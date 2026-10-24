//! Integration tests for `oxyboy diff`, the differential
//! recomp-versus-interpreter runner.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use testutil::synthetic::RomBuilder;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxyboy-diff-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn oxyboy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oxyboy"))
}

/// The same hand-assembled mode-3 paint program the run tests use:
/// ARM prologue, THUMB body, three VRAM stores, idle loop. The hybrid
/// must reproduce it exactly, so the runner reports `match`.
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

fn mode3_rom(dir: &Path, name: &str) -> PathBuf {
    let mut bytes = RomBuilder::new().title("DIF").build();
    bytes[..MODE3_PROGRAM.len()].copy_from_slice(&MODE3_PROGRAM);
    let path = dir.join(name);
    fs::write(&path, &bytes).expect("write ROM");
    path
}

#[test]
fn diff_reports_match_for_a_reproducible_rom() {
    let dir = temp_dir("match");
    let rom = mode3_rom(&dir, "mode3.gba");
    let case_dir = dir.join("cases");
    let output = oxyboy()
        .args([
            "diff",
            "--rom",
            rom.to_str().expect("utf-8 path"),
            "--frames",
            "2",
            "--case-dir",
            case_dir.to_str().expect("utf-8 path"),
        ])
        .output()
        .expect("run oxyboy diff");
    assert_eq!(output.status.code(), Some(0), "stdout: {:?}", output.stdout);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("match"), "stdout: {stdout}");
    // A match writes no case directory.
    assert!(!case_dir.join("mode3").exists());
}

#[test]
fn diff_rejects_a_missing_rom() {
    let output = oxyboy()
        .args(["diff", "--rom", "/nonexistent/missing.gba"])
        .output()
        .expect("run oxyboy diff");
    assert_eq!(output.status.code(), Some(1));
}

/// A multiboot image whose start code calls into its own EWRAM
/// mirror: `bl 0x02400000` (and a second call so the mirror start
/// bounds a region) re-enters the image through the mirror and walks
/// mirror strides without ever returning. Discovery follows the call
/// targets and seeds functions at the mirror addresses; the runner
/// must not translate them (the lifter reads literals at canonical
/// addresses, so a mirror region is not the mirror program) and must
/// let the interpreter own everything outside the image instead.
fn mirror_call_rom(dir: &Path, name: &str) -> PathBuf {
    let mut bytes = RomBuilder::new()
        .title("MBMIR")
        .entry(0xEA00_002E) // b 0x020000c0 (the header entry branch)
        .min_size(0x140)
        .build();
    let program: [u32; 3] = [
        0xEB0F_FFCE, // 0x020000c0: bl 0x02400000 (EWRAM mirror of the image)
        0xEB1F_FFFD, // 0x020000c4: bl 0x02800000 (bounds the mirror region)
        0xE12F_FF1E, // 0x020000c8: bx lr (lets discovery accept the mirrors)
    ];
    let mut code = Vec::new();
    for word in program {
        code.extend_from_slice(&word.to_le_bytes());
    }
    bytes[0xC0..0xC0 + code.len()].copy_from_slice(&code);
    let path = dir.join(name);
    fs::write(&path, &bytes).expect("write ROM");
    path
}

#[test]
fn diff_confines_regions_to_the_image_for_mirror_calls() {
    let dir = temp_dir("mirror");
    let rom = mirror_call_rom(&dir, "mbmirror.gba");
    let cases = dir.join("cases");
    let output = oxyboy()
        .args([
            "diff",
            "--rom",
            rom.to_str().expect("utf-8 path"),
            "--frames",
            "1",
            "--case-dir",
            cases.to_str().expect("utf-8 path"),
        ])
        .output()
        .expect("run oxyboy diff");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {stdout} stderr: {:?}",
        output.stderr
    );
    assert!(stdout.contains("match"), "stdout: {stdout}");
}
