//! Integration tests for `oxyboy corpus`, the regression-corpus
//! runner.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use testutil::synthetic::RomBuilder;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxyboy-corpus-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn oxyboy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oxyboy"))
}

/// The hand-assembled mode-3 paint program the run tests use; any
/// deterministic ROM works for the corpus checks.
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
    let mut bytes = RomBuilder::new().title("CRP").build();
    bytes[..MODE3_PROGRAM.len()].copy_from_slice(&MODE3_PROGRAM);
    let path = dir.join(name);
    fs::write(&path, &bytes).expect("write ROM");
    path
}

fn write_manifest(path: &Path, rom: &Path, budget_ms: u64, hash: Option<&str>) {
    let hash_field = hash
        .map(|h| format!(", \"hash\": \"{h}\""))
        .unwrap_or_default();
    let rom = rom.to_str().expect("utf-8 path");
    let text = format!(
        "{{\"entries\": [{{\"name\": \"mode3\", \"rom\": \"{rom}\", \
         \"frames\": 2, \"budget_ms\": {budget_ms}{hash_field}}}]}}\n"
    );
    fs::write(path, text).expect("write manifest");
}

fn corpus(manifest: &Path, extra: &[&str]) -> (i32, String, String) {
    let output = oxyboy()
        .arg("corpus")
        .arg("--manifest")
        .arg(manifest.to_str().expect("utf-8 path"))
        .args(extra)
        .output()
        .expect("run oxyboy corpus");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn corpus_update_pins_hash_then_enforces_it() {
    let dir = temp_dir("pin");
    let rom = mode3_rom(&dir, "mode3.gba");
    let manifest = dir.join("corpus.json");
    write_manifest(&manifest, &rom, 60_000, None);

    let (code, _stdout, _) = corpus(&manifest, &["--update"]);
    assert_eq!(code, 0, "update run failed");
    let text = fs::read_to_string(&manifest).expect("read manifest");
    assert!(text.contains("\"hash\""), "hash not pinned: {text}");

    let (code, stdout, _) = corpus(&manifest, &[]);
    assert_eq!(code, 0, "enforce run failed");
    assert!(stdout.contains("all green"), "stdout: {stdout}");
}

#[test]
fn corpus_fails_naming_a_hash_mismatch() {
    let dir = temp_dir("pin-mismatch");
    let rom = mode3_rom(&dir, "mode3.gba");
    let manifest = dir.join("corpus.json");
    write_manifest(&manifest, &rom, 60_000, Some("0000000000000000"));

    let (code, _, stderr) = corpus(&manifest, &[]);
    assert_eq!(code, 1);
    assert!(stderr.contains("mode3"), "stderr: {stderr}");
    assert!(stderr.contains("differs from pinned"), "stderr: {stderr}");
}

#[test]
fn corpus_fails_naming_a_budget_regression() {
    let dir = temp_dir("budget");
    let rom = mode3_rom(&dir, "mode3.gba");
    let manifest = dir.join("corpus.json");
    write_manifest(&manifest, &rom, 0, None);

    let (code, _, stderr) = corpus(&manifest, &[]);
    assert_eq!(code, 1);
    assert!(stderr.contains("mode3"), "stderr: {stderr}");
    assert!(stderr.contains("exceeds budget"), "stderr: {stderr}");
}

#[test]
fn corpus_skips_missing_roms_by_name() {
    let dir = temp_dir("skip");
    let manifest = dir.join("corpus.json");
    write_manifest(&manifest, &dir.join("absent.gba"), 60_000, None);

    let (code, stdout, _) = corpus(&manifest, &[]);
    assert_eq!(code, 0);
    assert!(stdout.contains("skipped"), "stdout: {stdout}");
    assert!(stdout.contains("mode3"), "stdout: {stdout}");
}
