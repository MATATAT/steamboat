use crate::SteamboatResult;
use crate::protocol::{self, FileEntry};
use anyhow::{Context, ensure};
use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub abs: PathBuf,
    pub wire: String,
    pub size: u64,
    pub hash: [u8; 32],
}

impl From<&SourceFile> for FileEntry {
    fn from(source: &SourceFile) -> Self {
        FileEntry {
            path: source.wire.clone(),
            size: source.size,
            hash: source.hash,
        }
    }
}

/// Streams a file through blake3.
pub fn hash_file(path: &Path) -> SteamboatResult<[u8; 32]> {
    let mut file = File::open(path).with_context(|| format!("opening {path:?}"))?;
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut file, &mut hasher)?;

    Ok(*hasher.finalize().as_bytes())
}

/// Expands CLI path arguments into hashed source files with wire paths.
///
/// A file argument contributes its bare file name; a directory argument is
/// walked recursively and contributes paths that include the directory's own
/// name (like `cp -r`).
///
/// # Errors
/// Fails if two sources map to the same wire path, or no files are found.
pub fn walk_sources(paths: &[PathBuf]) -> SteamboatResult<Vec<SourceFile>> {
    let mut seen = HashSet::new();
    let mut sources = Vec::new();
    for arg in paths {
        let arg = arg
            .canonicalize()
            .with_context(|| format!("resolving {arg:?}"))?;
        if arg.is_file() {
            push_source(
                &arg,
                arg.parent()
                    .unwrap_or(Path::new("")),
                &mut seen,
                &mut sources,
            )?;
        } else {
            let base = arg
                .parent()
                .unwrap_or(Path::new(""));
            for entry in WalkDir::new(&arg) {
                let entry = entry?;

                if entry.file_type().is_file() {
                    push_source(entry.path(), base, &mut seen, &mut sources)?;
                }
            }
        }
    }
    ensure!(!sources.is_empty(), "no files to send");

    Ok(sources)
}

fn push_source(
    abs: &Path,
    base: &Path,
    seen: &mut HashSet<String>,
    sources: &mut Vec<SourceFile>,
) -> SteamboatResult<()> {
    let relative = abs.strip_prefix(base)?;
    let wire = protocol::to_wire(relative)?;
    ensure!(seen.insert(wire.clone()), "duplicate path {wire:?} from {abs:?}");
    let size = abs.metadata()?.len();
    let hash = hash_file(abs)?;
    sources.push(SourceFile {
        abs: abs.to_path_buf(),
        wire,
        size,
        hash,
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::fs;

    #[test]
    fn hash_file_matches_blake3_of_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.bin");
        fs::write(&path, b"hello").unwrap();

        assert_eq!(hash_file(&path).unwrap(), *blake3::hash(b"hello").as_bytes());
    }

    #[test]
    fn walk_sources_uses_bare_name_for_file_args_and_dir_prefix_for_dir_args() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("single.txt"), b"s").unwrap();
        fs::create_dir_all(dir.path().join("roms/snes")).unwrap();
        fs::write(
            dir.path()
                .join("roms/snes/mario.sfc"),
            b"game",
        )
        .unwrap();

        let sources = walk_sources(&[dir.path().join("single.txt"), dir.path().join("roms")]).unwrap();
        let mut wires: Vec<&str> = sources
            .iter()
            .map(|s| s.wire.as_str())
            .collect();
        wires.sort_unstable();

        assert_eq!(wires, vec!["roms/snes/mario.sfc", "single.txt"]);
    }

    #[test]
    fn walk_sources_records_size_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"hello").unwrap();

        let sources = walk_sources(&[dir.path().join("a.txt")]).unwrap();

        assert_eq!(sources[0].size, 5);
        assert_eq!(sources[0].hash, *blake3::hash(b"hello").as_bytes());
    }

    #[test]
    fn walk_sources_rejects_duplicate_wire_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a")).unwrap();
        fs::create_dir_all(dir.path().join("b")).unwrap();
        fs::write(dir.path().join("a/x.txt"), b"1").unwrap();
        fs::write(dir.path().join("b/x.txt"), b"2").unwrap();

        let result = walk_sources(&[dir.path().join("a/x.txt"), dir.path().join("b/x.txt")]);

        assert!(result.is_err());
    }

    #[test]
    fn walk_sources_rejects_empty_input() {
        assert!(walk_sources(&[]).is_err());
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("empty")).unwrap();
        assert!(walk_sources(&[dir.path().join("empty")]).is_err());
    }
}
