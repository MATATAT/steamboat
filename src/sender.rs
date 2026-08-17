use crate::fsx::SourceFile;
use crate::protocol::{
    DATA_CHUNK_BYTES, FileEntry, FileHeader, Hello, IDLE_TIMEOUT, Manifest, ManifestReply, PROTOCOL_VERSION, Summary,
    read_frame, write_frame,
};
use crate::{Progress, SteamboatResult};
use anyhow::{Context, bail, ensure};
use tokio::fs as tokio_fs;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

/// Runs the sending side of one transfer over an established stream.
pub async fn run_sender(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
    hostname: &str,
    files: &[SourceFile],
    progress: &dyn Progress,
) -> SteamboatResult<Summary> {
    let own_hello = Hello {
        version: PROTOCOL_VERSION,
        hostname: hostname.to_string(),
    };
    write_frame(&mut stream, &own_hello).await?;
    let peer: Hello = timeout(IDLE_TIMEOUT, read_frame(&mut stream)).await??;
    ensure!(
        peer.version == PROTOCOL_VERSION,
        "protocol version mismatch: receiver {} speaks {}, this build speaks {PROTOCOL_VERSION}",
        peer.hostname,
        peer.version
    );

    let manifest = Manifest {
        files: files
            .iter()
            .map(FileEntry::from)
            .collect(),
    };
    write_frame(&mut stream, &manifest).await?;
    // No timeout here: a human on the receiving end is deciding.
    let reply: ManifestReply = read_frame(&mut stream).await?;
    let wanted = match reply {
        ManifestReply::Rejected => bail!("receiver {} declined the transfer", peer.hostname),
        ManifestReply::Invalid { reason } => {
            bail!("receiver {} rejected the manifest: {reason}", peer.hostname)
        }
        ManifestReply::Accepted { wanted } => wanted,
    };
    ensure!(
        wanted
            .iter()
            .all(|&i| (i as usize) < files.len()),
        "receiver requested an index outside the manifest"
    );

    let total: u64 = wanted
        .iter()
        .map(|&i| files[i as usize].size)
        .sum();
    progress.transfer_start(wanted.len() as u64, total);
    for &index in &wanted {
        send_one_file(&mut stream, index, &files[index as usize], progress).await?;
    }

    Ok(timeout(IDLE_TIMEOUT, read_frame(&mut stream)).await??)
}

async fn send_one_file(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    index: u32,
    source: &SourceFile,
    progress: &dyn Progress,
) -> SteamboatResult<()> {
    progress.file_start(&source.wire);
    write_frame(stream, &FileHeader { index }).await?;
    let mut file = tokio_fs::File::open(&source.abs)
        .await
        .with_context(|| format!("opening {:?}", source.abs))?;
    let mut remaining = source.size;
    let mut buf = vec![0u8; DATA_CHUNK_BYTES];
    while remaining > 0 {
        let take = remaining.min(buf.len() as u64) as usize;
        let read = file
            .read(&mut buf[..take])
            .await?;
        ensure!(read > 0, "{:?} shrank since it was hashed", source.abs);
        stream
            .write_all(&buf[..read])
            .await?;
        progress.chunk(read as u64);
        remaining -= read as u64;
    }
    stream.flush().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoProgress;
    use crate::fsx;
    use crate::receiver::run_receiver;
    use pretty_assertions::assert_eq;
    use std::fs;

    #[tokio::test]
    async fn transfers_files_end_to_end() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::create_dir_all(src.path().join("roms/snes")).unwrap();
        fs::write(
            src.path()
                .join("roms/snes/mario.sfc"),
            b"game data",
        )
        .unwrap();
        fs::write(
            src.path()
                .join("roms/empty.bin"),
            b"",
        )
        .unwrap();
        let files = fsx::walk_sources(&[src.path().join("roms")]).unwrap();

        let (sender_stream, receiver_stream) = tokio::io::duplex(1024 * 1024);
        let dest = dst.path().to_path_buf();
        let recv_task =
            tokio::spawn(async move { run_receiver(receiver_stream, &dest, "recv-host", |_| true, &NoProgress).await });
        let summary = run_sender(sender_stream, "send-host", &files, &NoProgress)
            .await
            .unwrap();

        assert_eq!(
            summary,
            Summary {
                received: 2,
                skipped: 0,
                failed: 0
            }
        );
        assert_eq!(
            fs::read(
                dst.path()
                    .join("roms/snes/mario.sfc")
            )
            .unwrap(),
            b"game data"
        );
        assert_eq!(
            fs::read(
                dst.path()
                    .join("roms/empty.bin")
            )
            .unwrap(),
            b""
        );
        recv_task
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn resend_skips_files_already_delivered() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::write(src.path().join("a.txt"), b"hello").unwrap();
        fs::write(dst.path().join("a.txt"), b"hello").unwrap();
        let files = fsx::walk_sources(&[src.path().join("a.txt")]).unwrap();

        let (sender_stream, receiver_stream) = tokio::io::duplex(1024 * 1024);
        let dest = dst.path().to_path_buf();
        let recv_task =
            tokio::spawn(async move { run_receiver(receiver_stream, &dest, "recv-host", |_| true, &NoProgress).await });
        let summary = run_sender(sender_stream, "send-host", &files, &NoProgress)
            .await
            .unwrap();

        assert_eq!(
            summary,
            Summary {
                received: 0,
                skipped: 1,
                failed: 0
            }
        );
        recv_task
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn declined_transfer_surfaces_as_an_error() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::write(src.path().join("a.txt"), b"hello").unwrap();
        let files = fsx::walk_sources(&[src.path().join("a.txt")]).unwrap();

        let (sender_stream, receiver_stream) = tokio::io::duplex(1024 * 1024);
        let dest = dst.path().to_path_buf();
        tokio::spawn(async move { run_receiver(receiver_stream, &dest, "recv-host", |_| false, &NoProgress).await });
        let result = run_sender(sender_stream, "send-host", &files, &NoProgress).await;

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("declined")
        );
    }
}
