use crate::SteamboatResult;
use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashSet;
use std::path::{Component, Path};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use unicode_normalization::UnicodeNormalization;

/// Encodes a relative native path as a wire path: forward-slash separated,
/// NFC-normalized.
///
/// # Errors
/// Fails on absolute paths, `.`/`..` components, or non-UTF-8 names.
pub fn to_wire(relative: &Path) -> SteamboatResult<String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => parts.push(
                name.to_str()
                    .with_context(|| format!("non-UTF-8 file name in {relative:?}"))?,
            ),
            other => bail!("unsupported path component {other:?} in {relative:?}"),
        }
    }
    ensure!(!parts.is_empty(), "empty path");

    Ok(parts
        .join("/")
        .nfc()
        .collect())
}

/// Validates a wire path and splits it into components.
///
/// # Errors
/// Fails on empty paths, empty components (leading/trailing/double slashes),
/// `.`/`..` components, or embedded NUL bytes.
pub fn wire_components(wire: &str) -> SteamboatResult<Vec<&str>> {
    ensure!(!wire.is_empty(), "empty wire path");
    let parts: Vec<&str> = wire.split('/').collect();
    for part in &parts {
        ensure!(!part.is_empty(), "empty component in wire path {wire:?}");
        ensure!(
            *part != "." && *part != "..",
            "traversal component in wire path {wire:?}"
        );
        ensure!(!part.contains('\0'), "NUL byte in wire path {wire:?}");
    }

    Ok(parts)
}

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;
pub const DATA_CHUNK_BYTES: usize = 64 * 1024;
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub version: u16,
    pub hostname: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ManifestReply {
    Rejected,
    Invalid { reason: String },
    Accepted { wanted: Vec<u32> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileHeader {
    pub index: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub received: u32,
    pub skipped: u32,
    pub failed: u32,
}

/// Writes one length-prefixed postcard frame.
pub async fn write_frame<T: Serialize, W: AsyncWrite + Unpin>(writer: &mut W, msg: &T) -> SteamboatResult<()> {
    let bytes = postcard::to_stdvec(msg)?;
    ensure!(
        bytes.len() <= MAX_FRAME_BYTES as usize,
        "outgoing frame too large: {} bytes",
        bytes.len()
    );
    writer
        .write_u32_le(bytes.len() as u32)
        .await?;
    writer
        .write_all(&bytes)
        .await?;
    writer.flush().await?;

    Ok(())
}

/// Reads one length-prefixed postcard frame.
pub async fn read_frame<T: DeserializeOwned, R: AsyncRead + Unpin>(reader: &mut R) -> SteamboatResult<T> {
    let len = reader.read_u32_le().await?;
    ensure!(len <= MAX_FRAME_BYTES, "incoming frame too large: {len} bytes");
    let mut buf = vec![0u8; len as usize];
    reader
        .read_exact(&mut buf)
        .await?;

    Ok(postcard::from_bytes(&buf)?)
}

/// Checks a received manifest before any filesystem work: every path must be
/// a valid wire path and paths must be unique. The `Err` string travels back
/// to the sender in `ManifestReply::Invalid`.
pub fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
    if manifest.files.is_empty() {
        return Err("empty manifest".into());
    }

    let mut seen = HashSet::new();
    for entry in &manifest.files {
        wire_components(&entry.path).map_err(|e| format!("bad path {:?}: {e}", entry.path))?;

        if !seen.insert(entry.path.as_str()) {
            return Err(format!("duplicate path {:?}", entry.path));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::path::Path;

    #[test]
    fn to_wire_joins_components_with_forward_slashes() {
        let path = Path::new("roms")
            .join("snes")
            .join("Mario.sfc");
        assert_eq!(to_wire(&path).unwrap(), "roms/snes/Mario.sfc");
    }

    #[test]
    fn to_wire_normalizes_to_nfc() {
        // "e" + combining acute (NFD) must become precomposed "é" (NFC)
        let path = Path::new("Poke\u{0301}mon.sfc").to_path_buf();
        assert_eq!(to_wire(&path).unwrap(), "Pok\u{00e9}mon.sfc");
    }

    #[test]
    fn to_wire_rejects_parent_and_root_components() {
        assert!(to_wire(Path::new("../escape.txt")).is_err());
        assert!(to_wire(Path::new("/abs/path.txt")).is_err());
    }

    #[test]
    fn wire_components_splits_a_valid_path() {
        assert_eq!(
            wire_components("roms/snes/Mario.sfc").unwrap(),
            vec!["roms", "snes", "Mario.sfc"]
        );
    }

    #[test]
    fn wire_components_rejects_traversal_and_malformed_paths() {
        for bad in ["", "/abs", "a//b", "..", "a/../b", ".", "a/./b", "a/", "a/b\0c"] {
            assert!(wire_components(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[tokio::test]
    async fn frames_round_trip_over_a_duplex_stream() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let sent = Manifest {
            files: vec![FileEntry {
                path: "a.txt".into(),
                size: 5,
                hash: [7; 32],
            }],
        };
        write_frame(&mut a, &sent)
            .await
            .unwrap();
        let got: Manifest = read_frame(&mut b)
            .await
            .unwrap();

        assert_eq!(got, sent);
    }

    #[tokio::test]
    async fn read_frame_rejects_oversized_length_prefix() {
        let (mut a, mut b) = tokio::io::duplex(64);
        tokio::io::AsyncWriteExt::write_u32_le(&mut a, MAX_FRAME_BYTES + 1)
            .await
            .unwrap();

        assert!(
            read_frame::<Hello, _>(&mut b)
                .await
                .is_err()
        );
    }

    #[test]
    fn validate_manifest_accepts_a_clean_manifest() {
        let manifest = Manifest {
            files: vec![
                FileEntry {
                    path: "a.txt".into(),
                    size: 1,
                    hash: [0; 32],
                },
                FileEntry {
                    path: "sub/b.txt".into(),
                    size: 2,
                    hash: [1; 32],
                },
            ],
        };

        assert!(validate_manifest(&manifest).is_ok());
    }

    #[test]
    fn validate_manifest_rejects_traversal_duplicates_and_empty() {
        let traversal = Manifest {
            files: vec![FileEntry {
                path: "../x".into(),
                size: 1,
                hash: [0; 32],
            }],
        };
        let duplicate = Manifest {
            files: vec![
                FileEntry {
                    path: "a.txt".into(),
                    size: 1,
                    hash: [0; 32],
                },
                FileEntry {
                    path: "a.txt".into(),
                    size: 2,
                    hash: [1; 32],
                },
            ],
        };
        let empty = Manifest { files: vec![] };

        assert!(validate_manifest(&traversal).is_err());
        assert!(validate_manifest(&duplicate).is_err());
        assert!(validate_manifest(&empty).is_err());
    }
}
