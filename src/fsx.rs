use crate::SteamboatResult;
use crate::protocol::{self, FileEntry};
use anyhow::{Context, ensure};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
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

const WINDOWS_ILLEGAL: &[char] = &['\\', ':', '*', '?', '"', '<', '>', '|'];
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2",
    "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[derive(Debug, Default)]
pub struct ReceivePlan {
    pub wanted: Vec<u32>,
    pub skipped: u32,
    pub collided: Vec<String>,
    pub renamed: Vec<(String, String)>,
    pub dest_paths: HashMap<u32, PathBuf>,
}

/// Rewrites a single path component to be legal on Windows: illegal
/// characters and control bytes become `_`, trailing dots/spaces become `_`,
/// and reserved device names (`CON`, `COM1`, ...) get a `_` prefix.
pub fn sanitize_component(component: &str) -> String {
    let mut out: String = component
        .chars()
        .map(|c| {
            if WINDOWS_ILLEGAL.contains(&c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
        out.push('_');
    }
    let stem = out
        .split('.')
        .next()
        .unwrap_or(&out);

    if WINDOWS_RESERVED.contains(
        &stem
            .to_ascii_uppercase()
            .as_str(),
    ) {
        format!("_{out}")
    } else {
        out
    }
}

fn native_components(wire: &str) -> SteamboatResult<Vec<String>> {
    let components = protocol::wire_components(wire)?
        .into_iter()
        .map(|c| {
            if cfg!(windows) {
                sanitize_component(c)
            } else {
                c.to_string()
            }
        })
        .collect();

    Ok(components)
}

/// Converts a validated wire path to a native relative path, applying
/// Windows sanitization when running on Windows.
pub fn native_relative_path(wire: &str) -> SteamboatResult<PathBuf> {
    Ok(native_components(wire)?
        .iter()
        .collect())
}

/// Probes whether `dest`'s filesystem treats names case-insensitively
/// (NTFS, default APFS, ext4 with casefolding).
pub fn detect_case_insensitive(dest: &Path) -> SteamboatResult<bool> {
    let probe = dest.join(".steamboat-CaSe-probe");
    fs::write(&probe, b"")?;
    let insensitive = dest
        .join(".steamboat-case-probe")
        .exists();
    fs::remove_file(&probe).ok();

    Ok(insensitive)
}

/// Decides what to do with each manifest entry: skip (already present with a
/// matching hash), want, or report as a collision (two entries landing on
/// the same destination path). Blocking — hashes existing files.
pub fn plan_receive(files: &[FileEntry], dest: &Path) -> SteamboatResult<ReceivePlan> {
    let case_insensitive = detect_case_insensitive(dest)?;
    let mut groups: HashMap<String, Vec<u32>> = HashMap::new();
    for (index, entry) in files.iter().enumerate() {
        let mut key = native_components(&entry.path)?.join("/");

        if case_insensitive {
            key = key.to_lowercase();
        }
        groups
            .entry(key)
            .or_default()
            .push(index as u32);
    }
    let collided: HashSet<u32> = groups
        .into_values()
        .filter(|group| group.len() > 1)
        .flatten()
        .collect();

    let mut plan = ReceivePlan::default();
    for (index, entry) in files.iter().enumerate() {
        let index = index as u32;
        let native = native_relative_path(&entry.path)?;
        let native_wire_form = native
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");

        if native_wire_form != entry.path {
            plan.renamed
                .push((entry.path.clone(), native_wire_form));
        }
        let path = dest.join(&native);
        plan.dest_paths
            .insert(index, path.clone());

        if collided.contains(&index) {
            plan.collided
                .push(entry.path.clone());
        } else if path.is_file() && hash_file(&path)? == entry.hash {
            plan.skipped += 1;
        } else {
            plan.wanted.push(index);
        }
    }

    Ok(plan)
}

/// Deletes leftover `*.steamboat-part` files from an interrupted earlier run.
pub fn clean_orphan_parts(dest: &Path) -> SteamboatResult<u32> {
    let mut removed = 0;
    for entry in WalkDir::new(dest) {
        let entry = entry?;
        let is_part = entry.file_type().is_file()
            && entry
                .file_name()
                .to_string_lossy()
                .ends_with(".steamboat-part");

        if is_part {
            fs::remove_file(entry.path())?;
            removed += 1;
        }
    }

    Ok(removed)
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

    #[test]
    fn sanitize_component_rewrites_illegal_windows_names() {
        assert_eq!(sanitize_component("Mario: Special?"), "Mario_ Special_");
        assert_eq!(sanitize_component("a<b>c|d\"e\\f"), "a_b_c_d_e_f");
        assert_eq!(sanitize_component("trailing."), "trailing_");
        assert_eq!(sanitize_component("trailing "), "trailing_");
        assert_eq!(sanitize_component("CON"), "_CON");
        assert_eq!(sanitize_component("con.txt"), "_con.txt");
        assert_eq!(sanitize_component("COM7"), "_COM7");
        assert_eq!(sanitize_component("normal-name.sfc"), "normal-name.sfc");
        assert_eq!(sanitize_component("CONSOLE"), "CONSOLE");
    }

    #[test]
    fn plan_receive_skips_matching_files_and_wants_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("have.txt"), b"same").unwrap();
        let files = vec![
            FileEntry {
                path: "have.txt".into(),
                size: 4,
                hash: *blake3::hash(b"same").as_bytes(),
            },
            FileEntry {
                path: "stale.txt".into(),
                size: 3,
                hash: *blake3::hash(b"new").as_bytes(),
            },
            FileEntry {
                path: "missing.txt".into(),
                size: 1,
                hash: *blake3::hash(b"m").as_bytes(),
            },
        ];
        fs::write(dir.path().join("stale.txt"), b"old").unwrap();

        let plan = plan_receive(&files, dir.path()).unwrap();

        assert_eq!(plan.wanted, vec![1, 2]);
        assert_eq!(plan.skipped, 1);
        assert!(plan.collided.is_empty());
        assert!(
            plan.renamed.is_empty(),
            "no sanitization on this platform's clean names"
        );
        assert_eq!(plan.dest_paths[&2], dir.path().join("missing.txt"));
    }

    #[test]
    fn plan_receive_flags_case_collisions_on_insensitive_filesystems() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            FileEntry {
                path: "Mario.sfc".into(),
                size: 1,
                hash: [0; 32],
            },
            FileEntry {
                path: "mario.sfc".into(),
                size: 1,
                hash: [1; 32],
            },
            FileEntry {
                path: "zelda.sfc".into(),
                size: 1,
                hash: [2; 32],
            },
        ];

        let plan = plan_receive(&files, dir.path()).unwrap();

        if detect_case_insensitive(dir.path()).unwrap() {
            assert_eq!(plan.collided, vec!["Mario.sfc".to_string(), "mario.sfc".to_string()]);
            assert_eq!(plan.wanted, vec![2]);
        } else {
            assert!(plan.collided.is_empty());
            assert_eq!(plan.wanted, vec![0, 1, 2]);
        }
    }

    #[test]
    fn clean_orphan_parts_removes_only_part_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("keep.txt"), b"k").unwrap();
        fs::write(dir.path().join("keep.part"), b"legit user file").unwrap();
        fs::write(
            dir.path()
                .join("sub/orphan.bin.steamboat-part"),
            b"o",
        )
        .unwrap();

        assert_eq!(clean_orphan_parts(dir.path()).unwrap(), 1);
        assert!(
            dir.path()
                .join("keep.txt")
                .exists()
        );
        assert!(
            dir.path()
                .join("keep.part")
                .exists(),
            "a user file that happens to end in .part must not be deleted"
        );
        assert!(
            !dir.path()
                .join("sub/orphan.bin.steamboat-part")
                .exists()
        );
    }
}
