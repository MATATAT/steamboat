use pretty_assertions::assert_eq;
use std::fs;
use std::path::Path;
use steamboat::protocol::{FileHeader, Hello, Manifest, ManifestReply, PROTOCOL_VERSION, Summary, write_frame};
use steamboat::receiver::{ReceiveOutcome, run_receiver};
use steamboat::sender::run_sender;
use steamboat::{NoProgress, fsx, protocol};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

async fn spawn_receiver(
    dest: &Path,
) -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<steamboat::SteamboatResult<ReceiveOutcome>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let dest = dest.to_path_buf();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        run_receiver(stream, &dest, "recv-host", |_| true, &NoProgress).await
    });

    (addr, task)
}

#[tokio::test]
async fn clean_transfer_over_tcp() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    fs::create_dir_all(src.path().join("set/nested")).unwrap();
    fs::write(src.path().join("set/one.bin"), vec![1u8; 200_000]).unwrap();
    fs::write(
        src.path()
            .join("set/nested/two.bin"),
        b"two",
    )
    .unwrap();
    fs::write(
        src.path()
            .join("set/empty.bin"),
        b"",
    )
    .unwrap();
    let files = fsx::walk_sources(&[src.path().join("set")]).unwrap();

    let (addr, recv_task) = spawn_receiver(dst.path()).await;
    let stream = TcpStream::connect(addr)
        .await
        .unwrap();
    let summary = run_sender(stream, "send-host", &files, &NoProgress)
        .await
        .unwrap();

    assert_eq!(
        summary,
        Summary {
            received: 3,
            skipped: 0,
            failed: 0
        }
    );
    assert_eq!(fs::read(dst.path().join("set/one.bin")).unwrap(), vec![1u8; 200_000]);
    assert_eq!(
        fs::read(
            dst.path()
                .join("set/nested/two.bin")
        )
        .unwrap(),
        b"two"
    );
    recv_task
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn retry_sends_only_missing_or_stale_files() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    fs::create_dir_all(src.path().join("set")).unwrap();
    fs::write(
        src.path()
            .join("set/good.bin"),
        b"correct",
    )
    .unwrap();
    fs::write(
        src.path()
            .join("set/stale.bin"),
        b"new contents",
    )
    .unwrap();
    fs::write(
        src.path()
            .join("set/missing.bin"),
        b"never arrived",
    )
    .unwrap();
    // Simulate a previously interrupted transfer at the destination.
    fs::create_dir_all(dst.path().join("set")).unwrap();
    fs::write(
        dst.path()
            .join("set/good.bin"),
        b"correct",
    )
    .unwrap();
    fs::write(
        dst.path()
            .join("set/stale.bin"),
        b"old corrupt junk",
    )
    .unwrap();
    let files = fsx::walk_sources(&[src.path().join("set")]).unwrap();

    let (addr, recv_task) = spawn_receiver(dst.path()).await;
    let stream = TcpStream::connect(addr)
        .await
        .unwrap();
    let summary = run_sender(stream, "send-host", &files, &NoProgress)
        .await
        .unwrap();

    assert_eq!(
        summary,
        Summary {
            received: 2,
            skipped: 1,
            failed: 0
        }
    );
    assert_eq!(
        fs::read(
            dst.path()
                .join("set/stale.bin")
        )
        .unwrap(),
        b"new contents"
    );
    assert_eq!(
        fs::read(
            dst.path()
                .join("set/missing.bin")
        )
        .unwrap(),
        b"never arrived"
    );
    recv_task
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn mid_stream_disconnect_keeps_completed_files_and_no_part_files() {
    let dst = tempfile::tempdir().unwrap();
    let (addr, recv_task) = spawn_receiver(dst.path()).await;

    // A hand-rolled sender that completes one file then dies mid-second-file.
    let mut stream = TcpStream::connect(addr)
        .await
        .unwrap();
    write_frame(
        &mut stream,
        &Hello {
            version: PROTOCOL_VERSION,
            hostname: "flaky".into(),
        },
    )
    .await
    .unwrap();
    let _peer: Hello = protocol::read_frame(&mut stream)
        .await
        .unwrap();
    let manifest = Manifest {
        files: vec![
            protocol::FileEntry {
                path: "done.bin".into(),
                size: 4,
                hash: *blake3::hash(b"done").as_bytes(),
            },
            protocol::FileEntry {
                path: "cut.bin".into(),
                size: 100_000,
                hash: *blake3::hash(&vec![9u8; 100_000]).as_bytes(),
            },
        ],
    };
    write_frame(&mut stream, &manifest)
        .await
        .unwrap();
    let reply: ManifestReply = protocol::read_frame(&mut stream)
        .await
        .unwrap();
    assert_eq!(reply, ManifestReply::Accepted { wanted: vec![0, 1] });
    write_frame(&mut stream, &FileHeader { index: 0 })
        .await
        .unwrap();
    stream
        .write_all(b"done")
        .await
        .unwrap();
    write_frame(&mut stream, &FileHeader { index: 1 })
        .await
        .unwrap();
    stream
        .write_all(&vec![9u8; 10_000])
        .await
        .unwrap();
    stream.flush().await.unwrap();
    drop(stream);

    assert!(
        recv_task
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(fs::read(dst.path().join("done.bin")).unwrap(), b"done");
    assert!(
        !dst.path()
            .join("cut.bin")
            .exists()
    );
    assert!(
        !dst.path()
            .join("cut.bin.steamboat-part")
            .exists()
    );
}
