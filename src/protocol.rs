use crate::SteamboatResult;
use anyhow::{Context, bail, ensure};
use std::path::{Component, Path};
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
}
