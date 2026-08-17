use crate::fsx::{self, ReceivePlan};
use crate::protocol::{
    DATA_CHUNK_BYTES, FileEntry, FileHeader, Hello, IDLE_TIMEOUT, Manifest, ManifestReply, PROTOCOL_VERSION, Summary,
    read_frame, write_frame,
};
use crate::{Progress, SteamboatResult};
use anyhow::{Context, bail, ensure};
use std::path::{Path, PathBuf};
use tokio::fs as tokio_fs;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task;
use tokio::time::timeout;

#[derive(Debug)]
pub struct TransferOffer {
    pub peer: String,
    pub file_count: u64,
    pub total_bytes: u64,
}

#[derive(Debug)]
pub struct ReceiveReport {
    pub summary: Summary,
    pub renamed: Vec<(String, String)>,
    pub collided: Vec<String>,
}

#[derive(Debug)]
pub enum ReceiveOutcome {
    Completed(ReceiveReport),
    Declined,
}

/// Runs the receiving side of one transfer over an established stream.
///
/// `accept` is the confirmation seam: the CLI wires it to a prompt, tests
/// wire it to a closure.
pub async fn run_receiver(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
    dest: &Path,
    hostname: &str,
    accept: impl FnOnce(&TransferOffer) -> bool,
    progress: &dyn Progress,
) -> SteamboatResult<ReceiveOutcome> {
    let hello: Hello = timeout(IDLE_TIMEOUT, read_frame(&mut stream)).await??;
    let own_hello = Hello {
        version: PROTOCOL_VERSION,
        hostname: hostname.to_string(),
    };
    write_frame(&mut stream, &own_hello).await?;
    ensure!(
        hello.version == PROTOCOL_VERSION,
        "protocol version mismatch: sender speaks {}, this build speaks {PROTOCOL_VERSION}",
        hello.version
    );

    let manifest: Manifest = timeout(IDLE_TIMEOUT, read_frame(&mut stream)).await??;
    if let Err(reason) = crate::protocol::validate_manifest(&manifest) {
        write_frame(&mut stream, &ManifestReply::Invalid { reason: reason.clone() }).await?;
        bail!("invalid manifest from {}: {reason}", hello.hostname);
    }

    let offer = TransferOffer {
        peer: hello.hostname,
        file_count: manifest.files.len() as u64,
        total_bytes: manifest
            .files
            .iter()
            .map(|f| f.size)
            .sum(),
    };
    if !accept(&offer) {
        write_frame(&mut stream, &ManifestReply::Rejected).await?;

        return Ok(ReceiveOutcome::Declined);
    }

    let files = manifest.files.clone();
    let dest_owned = dest.to_path_buf();
    let plan = task::spawn_blocking(move || fsx::plan_receive(&files, &dest_owned)).await??;
    write_frame(
        &mut stream,
        &ManifestReply::Accepted {
            wanted: plan.wanted.clone(),
        },
    )
    .await?;
    let wanted_bytes: u64 = plan
        .wanted
        .iter()
        .map(|&i| manifest.files[i as usize].size)
        .sum();
    progress.transfer_start(plan.wanted.len() as u64, wanted_bytes);

    let summary = receive_files(&mut stream, &manifest, &plan, progress).await?;
    write_frame(&mut stream, &summary).await?;
    let report = ReceiveReport {
        summary,
        renamed: plan.renamed,
        collided: plan.collided,
    };

    Ok(ReceiveOutcome::Completed(report))
}

async fn receive_files(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    manifest: &Manifest,
    plan: &ReceivePlan,
    progress: &dyn Progress,
) -> SteamboatResult<Summary> {
    let mut received = 0;
    let mut failed = plan.collided.len() as u32;
    for &index in &plan.wanted {
        let header: FileHeader = timeout(IDLE_TIMEOUT, read_frame(stream)).await??;
        ensure!(
            header.index == index,
            "sender sent file {} but {index} was expected",
            header.index
        );
        let entry = &manifest.files[index as usize];
        progress.file_start(&entry.path);

        if receive_one_file(stream, entry, &plan.dest_paths[&index], progress).await? {
            received += 1;
        } else {
            failed += 1;
        }
    }

    Ok(Summary {
        received,
        skipped: plan.skipped,
        failed,
    })
}

fn part_path(final_path: &Path) -> PathBuf {
    let mut part = final_path
        .as_os_str()
        .to_owned();
    part.push(".part");

    PathBuf::from(part)
}

/// Streams one file into `<final_path>.part`, verifying the hash before the
/// rename. Returns `Ok(false)` on a hash mismatch; the `.part` file is
/// removed on mismatch and on any I/O error.
async fn receive_one_file(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    entry: &FileEntry,
    final_path: &Path,
    progress: &dyn Progress,
) -> SteamboatResult<bool> {
    if let Some(parent) = final_path.parent() {
        tokio_fs::create_dir_all(parent).await?;
    }
    let part = part_path(final_path);
    let result = stream_to_part(stream, entry, &part, progress).await;
    match result {
        Ok(true) => {
            // Windows rename fails onto an existing file; remove first.
            tokio_fs::remove_file(final_path)
                .await
                .ok();
            tokio_fs::rename(&part, final_path).await?;

            Ok(true)
        }
        Ok(false) => {
            tokio_fs::remove_file(&part)
                .await
                .ok();

            Ok(false)
        }
        Err(e) => {
            tokio_fs::remove_file(&part)
                .await
                .ok();

            Err(e)
        }
    }
}

async fn stream_to_part(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    entry: &FileEntry,
    part: &Path,
    progress: &dyn Progress,
) -> SteamboatResult<bool> {
    let mut file = tokio_fs::File::create(part)
        .await
        .with_context(|| format!("creating {part:?}"))?;
    let mut hasher = blake3::Hasher::new();
    let mut remaining = entry.size;
    let mut buf = vec![0u8; DATA_CHUNK_BYTES];
    while remaining > 0 {
        let take = remaining.min(buf.len() as u64) as usize;
        let read = timeout(IDLE_TIMEOUT, stream.read(&mut buf[..take])).await??;
        ensure!(read > 0, "connection closed mid-file at {remaining} bytes remaining");
        file.write_all(&buf[..read])
            .await?;
        hasher.update(&buf[..read]);
        progress.chunk(read as u64);
        remaining -= read as u64;
    }
    file.flush().await?;
    drop(file);

    Ok(*hasher.finalize().as_bytes() == entry.hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoProgress;
    use crate::protocol::{
        FileEntry, FileHeader, Hello, Manifest, ManifestReply, PROTOCOL_VERSION, Summary, read_frame, write_frame,
    };
    use pretty_assertions::assert_eq;
    use std::fs;
    use tokio::io::{AsyncWriteExt, DuplexStream};

    async fn handshake(stream: &mut DuplexStream, manifest: &Manifest) -> ManifestReply {
        write_frame(
            stream,
            &Hello {
                version: PROTOCOL_VERSION,
                hostname: "tester".into(),
            },
        )
        .await
        .unwrap();
        let _peer: Hello = read_frame(stream)
            .await
            .unwrap();
        write_frame(stream, manifest)
            .await
            .unwrap();

        read_frame(stream)
            .await
            .unwrap()
    }

    fn manifest_of(entries: &[(&str, &[u8])]) -> Manifest {
        Manifest {
            files: entries
                .iter()
                .map(|(path, content)| FileEntry {
                    path: (*path).into(),
                    size: content.len() as u64,
                    hash: *blake3::hash(content).as_bytes(),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn receives_a_file_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sender, receiver_stream) = tokio::io::duplex(1024 * 1024);
        let dest = dir.path().to_path_buf();
        let task =
            tokio::spawn(async move { run_receiver(receiver_stream, &dest, "recv-host", |_| true, &NoProgress).await });

        let reply = handshake(&mut sender, &manifest_of(&[("sub/a.txt", b"hello")])).await;
        assert_eq!(reply, ManifestReply::Accepted { wanted: vec![0] });
        write_frame(&mut sender, &FileHeader { index: 0 })
            .await
            .unwrap();
        sender
            .write_all(b"hello")
            .await
            .unwrap();
        sender.flush().await.unwrap();
        let summary: Summary = read_frame(&mut sender)
            .await
            .unwrap();

        assert_eq!(
            summary,
            Summary {
                received: 1,
                skipped: 0,
                failed: 0
            }
        );
        assert_eq!(fs::read(dir.path().join("sub/a.txt")).unwrap(), b"hello");
        matches!(task.await.unwrap().unwrap(), ReceiveOutcome::Completed(_));
    }

    #[tokio::test]
    async fn skips_files_already_present_with_matching_hash() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let (mut sender, receiver_stream) = tokio::io::duplex(1024 * 1024);
        let dest = dir.path().to_path_buf();
        let task =
            tokio::spawn(async move { run_receiver(receiver_stream, &dest, "recv-host", |_| true, &NoProgress).await });

        let reply = handshake(&mut sender, &manifest_of(&[("a.txt", b"hello")])).await;
        assert_eq!(reply, ManifestReply::Accepted { wanted: vec![] });
        let summary: Summary = read_frame(&mut sender)
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
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn declined_offer_sends_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sender, receiver_stream) = tokio::io::duplex(1024 * 1024);
        let dest = dir.path().to_path_buf();
        let task =
            tokio::spawn(
                async move { run_receiver(receiver_stream, &dest, "recv-host", |_| false, &NoProgress).await },
            );

        let reply = handshake(&mut sender, &manifest_of(&[("a.txt", b"hello")])).await;

        assert_eq!(reply, ManifestReply::Rejected);
        matches!(task.await.unwrap().unwrap(), ReceiveOutcome::Declined);
    }

    #[tokio::test]
    async fn traversal_manifest_is_rejected_as_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sender, receiver_stream) = tokio::io::duplex(1024 * 1024);
        let dest = dir.path().to_path_buf();
        let task =
            tokio::spawn(async move { run_receiver(receiver_stream, &dest, "recv-host", |_| true, &NoProgress).await });

        let reply = handshake(&mut sender, &manifest_of(&[("../evil.txt", b"x")])).await;

        assert!(matches!(reply, ManifestReply::Invalid { .. }));
        assert!(task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn corrupt_data_counts_as_failed_and_leaves_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sender, receiver_stream) = tokio::io::duplex(1024 * 1024);
        let dest = dir.path().to_path_buf();
        let task =
            tokio::spawn(async move { run_receiver(receiver_stream, &dest, "recv-host", |_| true, &NoProgress).await });

        handshake(&mut sender, &manifest_of(&[("a.txt", b"hello")])).await;
        write_frame(&mut sender, &FileHeader { index: 0 })
            .await
            .unwrap();
        sender
            .write_all(b"jello")
            .await
            .unwrap();
        sender.flush().await.unwrap();
        let summary: Summary = read_frame(&mut sender)
            .await
            .unwrap();

        assert_eq!(
            summary,
            Summary {
                received: 0,
                skipped: 0,
                failed: 1
            }
        );
        assert!(
            !dir.path()
                .join("a.txt")
                .exists()
        );
        assert!(
            !dir.path()
                .join("a.txt.part")
                .exists()
        );
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn version_mismatch_fails_the_transfer() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sender, receiver_stream) = tokio::io::duplex(1024 * 1024);
        let dest = dir.path().to_path_buf();
        let task =
            tokio::spawn(async move { run_receiver(receiver_stream, &dest, "recv-host", |_| true, &NoProgress).await });

        write_frame(
            &mut sender,
            &Hello {
                version: 99,
                hostname: "tester".into(),
            },
        )
        .await
        .unwrap();

        assert!(task.await.unwrap().is_err());
    }
}
