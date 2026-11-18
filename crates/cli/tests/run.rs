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

/// Multiboot paint program placed at EWRAM 0xC0 and entered through
/// the header's entry branch (the same listing the runtime multiboot
/// tests document, assembler-verified with Binutils 2.32).
const MB_HEADER_ENTRY: u32 = 0xEA00_002E; // b 0x020000c0
const MB_PROGRAM: [u32; 17] = [
    0xE59F_0024,
    0xE59F_1024,
    0xE1C0_10B0,
    0xE59F_0020,
    0xE59F_1020,
    0xE1C0_10B0,
    0xE59F_101C,
    0xE1C0_10B2,
    0xE59F_1018,
    0xE1C0_10B4,
    0xEAFF_FFFE,
    0x0400_0000,
    0x0000_0403,
    0x0600_0000,
    0x0000_7FFF,
    0x0000_001F,
    0x0000_7C00,
];

/// Writes a multiboot-classified synthetic ROM: the entry branch
/// decodes into EWRAM, so the loader classifies it as multiboot and
/// the boot copies the image to 0x02000000.
fn multiboot_rom(dir: &Path, name: &str) -> PathBuf {
    let mut bytes = RomBuilder::new()
        .title("MBRUN")
        .entry(MB_HEADER_ENTRY)
        .min_size(0x140)
        .build();
    let mut code = Vec::new();
    for word in MB_PROGRAM {
        code.extend_from_slice(&word.to_le_bytes());
    }
    bytes[0xC0..0xC0 + code.len()].copy_from_slice(&code);
    let path = dir.join(name);
    fs::write(&path, &bytes).expect("write ROM");
    path
}

#[test]
fn multiboot_rom_boots_headless_to_its_frame() {
    let dir = temp_dir("mb");
    let rom = multiboot_rom(&dir, "mbrun.gba");
    let dump = dir.join("frame.png");

    let status = oxyboy()
        .args([
            "run",
            "--rom",
            rom.to_str().expect("utf-8 path"),
            "--headless",
            "--frames",
            "1",
            "--dump-frame",
            dump.to_str().expect("utf-8 path"),
            "--save-dir",
            dir.join("saves").to_str().expect("utf-8 path"),
        ])
        .status()
        .expect("spawn oxyboy");
    assert!(status.success(), "run failed: {status}");

    let file = fs::File::open(&dump).expect("open dumped frame");
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().expect("png info");
    let size = reader.output_buffer_size().expect("frame size");
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf).expect("decode frame");
    assert_eq!(info.width, 240);
    assert_eq!(&buf[0..4], &[255, 255, 255, 255], "pixel 0: white");
    assert_eq!(&buf[4..8], &[255, 0, 0, 255], "pixel 1: red");
    assert_eq!(&buf[8..12], &[0, 0, 255, 255], "pixel 2: blue");
    assert_eq!(&buf[40..44], &[0, 0, 0, 255], "backdrop untouched");
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

/// The mode-3 program in a cart that declares SRAM backup media.
fn sram_rom(dir: &Path, name: &str) -> PathBuf {
    let mut bytes = RomBuilder::new()
        .game_code("SVTS")
        .payload(b"SRAM_V113")
        .build();
    bytes[..MODE3_PROGRAM.len()].copy_from_slice(&MODE3_PROGRAM);
    let path = dir.join(name);
    fs::write(&path, &bytes).expect("write ROM");
    path
}

/// Runs `oxyboy run --headless` with the user data dirs pointed into
/// `dir`, returning the exit status and stderr.
fn run_headless(dir: &Path, rom: &Path, extra: &[&str]) -> (std::process::ExitStatus, String) {
    let output = oxyboy()
        .env("XDG_DATA_HOME", dir.join("xdg"))
        .env("HOME", dir.join("home"))
        .args([
            "run",
            "--rom",
            rom.to_str().expect("utf-8 path"),
            "--headless",
        ])
        .args(extra)
        .output()
        .expect("spawn oxyboy");
    (
        output.status,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn headless_runs_save_only_with_an_explicit_dir() {
    let dir = temp_dir("headless-save");
    let rom = sram_rom(&dir, "sram.gba");
    let (status, _) = run_headless(&dir, &rom, &[]);
    assert!(status.success(), "run failed: {status}");
    assert!(!dir.join("xdg").exists(), "no default save dir");
    assert!(!dir.join("home").exists(), "no HOME fallback");

    let saves = dir.join("saves");
    let (status, _) = run_headless(&dir, &rom, &["--save-dir", saves.to_str().expect("utf-8")]);
    assert!(status.success(), "run failed: {status}");
    assert_eq!(
        fs::read(saves.join("SVTS.sav")).expect("save").len(),
        0x8000
    );
}

#[test]
fn cart_without_backup_media_writes_no_save() {
    let dir = temp_dir("no-media");
    let rom = mode3_rom(&dir, "mode3.gba");
    let saves = dir.join("saves");
    let (status, _) = run_headless(&dir, &rom, &["--save-dir", saves.to_str().expect("utf-8")]);
    assert!(status.success(), "run failed: {status}");
    let files = fs::read_dir(&saves).map_or(0, |entries| entries.count());
    assert_eq!(files, 0, "nothing to persist");
}

#[test]
fn unreadable_save_is_left_untouched() {
    let dir = temp_dir("bad-save");
    let rom = sram_rom(&dir, "sram.gba");
    let saves = dir.join("saves");
    fs::create_dir_all(&saves).expect("create saves");
    fs::write(saves.join("SVTS.sav"), b"not a save").expect("write save");
    let (status, stderr) =
        run_headless(&dir, &rom, &["--save-dir", saves.to_str().expect("utf-8")]);
    assert!(status.success(), "run failed: {status}");
    assert!(stderr.contains("saving is disabled"), "{stderr}");
    assert_eq!(
        fs::read(saves.join("SVTS.sav")).expect("save"),
        b"not a save"
    );
}
