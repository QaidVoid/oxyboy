//! Integration tests for `oxyboy run --headless`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use testutil::golden::assert_bytes_matches;
use testutil::synthetic::RomBuilder;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxyboy-run-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn oxyboy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oxyboy"))
}

/// Mode-3 paint program for the synthetic run ROM, embedded as machine
/// code so the test needs no assembler at runtime.
///
/// The machine boots at 0x08000000 in ARM state (no BIOS reset), so the
/// program starts with an ARM prologue that switches to THUMB. Assembled
/// from the source below with `arm-none-eabi-as` (Binutils 2.32) and
/// verified with `objdump`; regenerate with the same tools if you edit
/// the listing.
///
/// ```text
///     .arm
///     ldr r3, [pc, #0]      @ literal below
///     bx  r3
///     .word 0x0800000D      @ thumb_code, bit 0 set
///     .thumb
/// thumb_code:
///     ldr r0, =0x04000000   @ DISPCNT
///     ldr r1, =0x0403       @ mode 3, BG2 on
///     strh r1, [r0]
///     ldr r0, =0x06000000   @ VRAM pixel 0
///     ldr r1, =0x7FFF       @ white
///     strh r1, [r0]
///     ldr r1, =0x001F       @ red
///     strh r1, [r0, #2]
///     ldr r1, =0x7C00       @ blue
///     strh r1, [r0, #4]
/// idle:
///     b idle
///     .pool
/// ```
const MODE3_PROGRAM: [u8; 0x3C] = [
    // ldr r3, [pc]; bx r3; .word 0x0800000D
    0x00, 0x30, 0x9F, 0xE5, 0x13, 0xFF, 0x2F, 0xE1, 0x0D, 0x00, 0x00, 0x08,
    // ldr r0,[pc,#20]; ldr r1,[pc,#24]; strh r1,[r0]
    0x05, 0x48, 0x06, 0x49, 0x01, 0x80, // ldr r0,[pc,#24]; ldr r1,[pc,#24]; strh r1,[r0]
    0x06, 0x48, 0x06, 0x49, 0x01, 0x80, // ldr r1,[pc,#24]; strh r1,[r0,#2]
    0x06, 0x49, 0x41, 0x80, // ldr r1,[pc,#24]; strh r1,[r0,#4]
    0x06, 0x49, 0x81, 0x80, // b idle; pool padding
    0xFE, 0xE7, 0x00, 0x00, // literal pool: 0x04000000 0x00000403 0x06000000 0x00007FFF
    0x00, 0x00, 0x00, 0x04, 0x03, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0xFF, 0x7F, 0x00, 0x00,
    // literal pool: 0x0000001F 0x00007C00
    0x1F, 0x00, 0x00, 0x00, 0x00, 0x7C, 0x00, 0x00,
];

/// Writes a synthetic ROM whose code paints one white, one red, and one
/// blue pixel at VRAM 0x06000000..4 in mode 3, then idles.
///
/// The program overwrites the header's entry word and logo fill; the
/// loader does not validate either.
fn mode3_rom(dir: &Path, name: &str) -> PathBuf {
    let mut bytes = RomBuilder::new().title("RUN").build();
    bytes[..MODE3_PROGRAM.len()].copy_from_slice(&MODE3_PROGRAM);
    let path = dir.join(name);
    fs::write(&path, &bytes).expect("write ROM");
    path
}

#[test]
fn headless_dump_matches_golden() {
    let dir = temp_dir("dump");
    let rom = mode3_rom(&dir, "mode3.gba");
    let dump = dir.join("frame.png");

    let status = oxyboy()
        .args([
            "run",
            "--rom",
            rom.to_str().expect("utf-8 path"),
            "--headless",
            "--frames",
            "2",
            "--dump-frame",
            dump.to_str().expect("utf-8 path"),
            "--save-dir",
            dir.join("saves").to_str().expect("utf-8 path"),
        ])
        .status()
        .expect("spawn oxyboy");
    assert!(status.success(), "run failed: {status}");

    let png = fs::read(&dump).expect("read dumped frame");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/run-frame.png");
    assert_bytes_matches(golden, &png);
}

#[test]
fn headless_without_dump_exits_successfully() {
    let dir = temp_dir("nodump");
    let rom = mode3_rom(&dir, "mode3.gba");

    let status = oxyboy()
        .args([
            "run",
            "--rom",
            rom.to_str().expect("utf-8 path"),
            "--headless",
            "--frames",
            "3",
            "--save-dir",
            dir.join("saves").to_str().expect("utf-8 path"),
        ])
        .status()
        .expect("spawn oxyboy");
    assert!(status.success(), "run failed: {status}");
}

#[test]
fn missing_rom_fails_with_error() {
    let dir = temp_dir("missing");
    let status = oxyboy()
        .args([
            "run",
            "--rom",
            dir.join("nope.gba").to_str().expect("utf-8 path"),
            "--headless",
        ])
        .status()
        .expect("spawn oxyboy");
    assert_eq!(status.code(), Some(1), "missing ROM must exit 1");
}
