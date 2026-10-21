//! Integration tests for image loading and classification.

use std::path::PathBuf;

use rom::{Image, ImageKind, LoadError, MAX_ROM_SIZE};
use testutil::synthetic::RomBuilder;

#[test]
fn loads_valid_cart_image() {
    let bytes = RomBuilder::new()
        .title("LOADTEST")
        .game_code("LOAD")
        .maker_code("ZZ")
        .payload(&[0xAA; 16])
        .build();
    let image = Image::from_bytes(bytes).expect("valid image");
    assert_eq!(image.kind(), ImageKind::Cart);
    assert!(image.size_is_multiple_of_4());
    assert!(image.header().checksum_valid());
    assert_eq!(image.header().title_str(), Some("LOADTEST"));
    assert_eq!(image.header().game_code_str(), Some("LOAD"));
    assert_eq!(image.header().maker_code_str(), Some("ZZ"));
    assert_eq!(image.bytes().len(), 512);
}

#[test]
fn flags_stale_checksum_without_rejecting() {
    let mut bytes = RomBuilder::new().build();
    bytes[0x0BD] ^= 0xFF;
    let image = Image::from_bytes(bytes).expect("image still loads");
    assert!(!image.header().checksum_valid());
}

#[test]
fn rejects_oversized_images() {
    let bytes = vec![0u8; MAX_ROM_SIZE + 1];
    match Image::from_bytes(bytes) {
        Err(LoadError::TooLarge { size }) => assert_eq!(size, MAX_ROM_SIZE + 1),
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[test]
fn accepts_exactly_32_mib() {
    let bytes = RomBuilder::new().min_size(MAX_ROM_SIZE).build();
    let image = Image::from_bytes(bytes).expect("32 MiB is the limit");
    assert_eq!(image.bytes().len(), MAX_ROM_SIZE);
}

#[test]
fn flags_non_multiple_of_four_size() {
    let bytes = RomBuilder::new().min_size(513).build();
    let image = Image::from_bytes(bytes).expect("odd size still loads");
    assert!(!image.size_is_multiple_of_4());
}

#[test]
fn detects_multiboot_shape() {
    // b 0x020000C0 assembled at the multiboot base 0x02000000.
    let bytes = RomBuilder::new().entry(0xEA00_002E).build();
    let image = Image::from_bytes(bytes).expect("valid image");
    assert_eq!(image.kind(), ImageKind::Multiboot);
}

#[test]
fn multiboot_size_boundary() {
    let small = RomBuilder::new()
        .entry(0xEA00_002E)
        .min_size(256 * 1024)
        .build();
    assert_eq!(
        Image::from_bytes(small).expect("valid").kind(),
        ImageKind::Multiboot
    );
    let large = RomBuilder::new()
        .entry(0xEA00_002E)
        .min_size(256 * 1024 + 4)
        .build();
    assert_eq!(
        Image::from_bytes(large).expect("valid").kind(),
        ImageKind::Cart
    );
}

#[test]
fn small_cart_entry_branch_is_ambiguous() {
    // Documented ambiguity: the same branch word assembled for the cart
    // base decodes into EWRAM at the multiboot base, so a small cart image
    // with a short entry branch is classified as multiboot.
    // b 0x080000C0 assembled at 0x08000000: word 0xEA00002E.
    let bytes = RomBuilder::new().entry(0xEA00_002E).build();
    assert_eq!(
        Image::from_bytes(bytes).expect("valid").kind(),
        ImageKind::Multiboot
    );
}

#[test]
fn loads_from_file_and_reports_io_errors() {
    let bytes = RomBuilder::new().title("FILELOAD").build();
    let path: PathBuf =
        std::env::temp_dir().join(format!("oxyboy-rom-test-{}.gba", std::process::id()));
    std::fs::write(&path, &bytes).expect("write temp image");

    let image = Image::from_file(&path).expect("file loads");
    assert_eq!(image.header().title_str(), Some("FILELOAD"));

    std::fs::remove_file(&path).expect("remove temp image");
    match Image::from_file(&path) {
        Err(LoadError::Io(_)) => {}
        other => panic!("expected Io error, got {other:?}"),
    }
}
