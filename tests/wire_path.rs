use pretty_assertions::assert_eq;
use std::path::{Path, PathBuf};
use steamboat::protocol;

#[test]
fn native_to_wire_to_native_round_trips() {
    let native = Path::new("roms")
        .join("snes")
        .join("Super Mario World.sfc");
    let wire = protocol::to_wire(&native).unwrap();
    let back: PathBuf = protocol::wire_components(&wire)
        .unwrap()
        .iter()
        .collect();

    assert_eq!(back, native);
}

#[test]
fn unicode_names_round_trip_in_nfc() {
    let native = Path::new("Pok\u{00e9}mon").join("Pok\u{00e9}mon Rouge.gb");
    let wire = protocol::to_wire(&native).unwrap();
    let back: PathBuf = protocol::wire_components(&wire)
        .unwrap()
        .iter()
        .collect();

    assert_eq!(back, native);
}
