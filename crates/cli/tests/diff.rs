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
