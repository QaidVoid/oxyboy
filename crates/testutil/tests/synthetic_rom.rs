//! End-to-end checks for the synthetic ROM builder and the golden helper.

use std::path::Path;

use testutil::golden::assert_matches;
use testutil::synthetic::RomBuilder;

#[test]
fn header_layout_is_gbatek_shaped() {
    let rom = RomBuilder::new()
        .title("TEST")
        .game_code("ABCD")
        .maker_code("01")
        .entry(0x0800_0000)
        .payload(&[1, 2, 3, 4])
        .build();
    assert_eq!(rom.len(), 512);
    assert_eq!(&rom[0x000..0x004], &0x0800_0000u32.to_le_bytes());
    assert!(rom[0x004..0x0A0].iter().all(|&b| b == 0xA5));
    assert_eq!(&rom[0x0A0..0x0A4], b"TEST");
    assert_eq!(&rom[0x0A4..0x0AC], &[0u8; 8]);
    assert_eq!(&rom[0x0AC..0x0B0], b"ABCD");
    assert_eq!(&rom[0x0B0..0x0B2], b"01");
    assert_eq!(rom[0x0B2], 0x96);
    let sum: u32 = rom[0x0A0..0x0BD].iter().map(|&b| u32::from(b)).sum();
    assert_eq!(
        (0u32.wrapping_sub(sum).wrapping_sub(0x19) & 0xFF) as u8,
        rom[0x0BD]
    );
    assert_eq!(&rom[0x100..0x104], &[1, 2, 3, 4]);
}

#[test]
fn long_fields_truncate_and_image_grows() {
    let rom = RomBuilder::new()
        .title("A VERY LONG TITLE")
        .game_code("TOOLONG")
        .maker_code("XYZ")
        .min_size(1024)
        .build();
    assert_eq!(rom.len(), 1024);
    assert_eq!(&rom[0x0A0..0x0AC], b"A VERY LONG ");
    assert_eq!(&rom[0x0AC..0x0B0], b"TOOL");
    assert_eq!(&rom[0x0B0..0x0B2], b"XY");
}

#[test]
fn payload_can_exceed_min_size() {
    let payload = vec![7u8; 300];
    let rom = RomBuilder::new().payload(&payload).min_size(0).build();
    assert_eq!(rom.len(), 0x100 + 300);
    assert_eq!(&rom[0x100..0x100 + 300], &payload[..]);
}

#[test]
fn golden_summary_matches() {
    let rom = RomBuilder::new().title("GOLDEN").game_code("GOLD").build();
    let title = String::from_utf8_lossy(&rom[0x0A0..0x0AC])
        .trim_matches('\0')
        .to_owned();
    let code = String::from_utf8_lossy(&rom[0x0AC..0x0B0])
        .trim_matches('\0')
        .to_owned();
    let summary = format!("title={title} code={code} len={}", rom.len());
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/synthetic-summary.txt");
    assert_matches(&path, &summary);
}
