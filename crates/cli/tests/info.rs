//! Integration tests for `oxyboy info`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rom::Hashes;
use testutil::golden::assert_matches;
use testutil::synthetic::RomBuilder;

const ZEROS_MD5: &str = "00000000000000000000000000000000";
const ZEROS_SHA1: &str = "0000000000000000000000000000000000000000";

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxyboy-info-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn oxyboy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oxyboy"))
}

fn write_image(dir: &Path, name: &str) -> Vec<u8> {
    let bytes = RomBuilder::new().title("GOLDEN").game_code("GOLD").build();
    fs::write(dir.join(name), &bytes).expect("write image");
    bytes
}

fn write_db(dir: &Path, entries: &str) {
    fs::write(dir.join("db.json"), format!(r#"{{"entries":[{entries}]}}"#))
        .expect("write database");
}

fn entry(name: &str, md5: &str, sha1: &str) -> String {
    format!(r#"{{"name":"{name}","status":"Homebrew","md5":"{md5}","sha1":"{sha1}"}}"#)
}

#[test]
fn known_image_matches_golden() {
    let dir = temp_dir("hit");
    let bytes = write_image(&dir, "demo.gba");
    let hashes = Hashes::compute(&bytes);
    write_db(
        &dir,
        &entry("Golden Demo", &hashes.md5_hex(), &hashes.sha1_hex()),
    );

    let output = oxyboy()
        .current_dir(&dir)
        .args(["info", "--rom", "demo.gba", "--db", "db.json"])
        .output()
        .expect("run oxyboy");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/info-hit.txt");
    assert_matches(&golden, &stdout);
}

#[test]
fn single_hash_match_is_not_a_hit() {
    let dir = temp_dir("half");
    let bytes = write_image(&dir, "demo.gba");
    let hashes = Hashes::compute(&bytes);
    write_db(
        &dir,
        &format!(
            "{},{}",
            entry("MD5 only", &hashes.md5_hex(), ZEROS_SHA1),
            entry("SHA1 only", ZEROS_MD5, &hashes.sha1_hex())
        ),
    );

    let output = oxyboy()
        .current_dir(&dir)
        .args(["info", "--rom", "demo.gba", "--db", "db.json"])
        .output()
        .expect("run oxyboy");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert!(
        stdout.contains("identification: unknown"),
        "stdout: {stdout}"
    );
}

#[test]
fn unknown_image_with_database_still_exits_zero() {
    let dir = temp_dir("unknown");
    write_image(&dir, "demo.gba");
    write_db(&dir, &entry("Other Game", ZEROS_MD5, ZEROS_SHA1));

    let output = oxyboy()
        .current_dir(&dir)
        .args(["info", "--rom", "demo.gba", "--db", "db.json"])
        .output()
        .expect("run oxyboy");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert!(
        stdout.contains("identification: unknown"),
        "stdout: {stdout}"
    );
}

#[test]
fn no_database_reports_and_exits_zero() {
    let dir = temp_dir("nodb");
    write_image(&dir, "demo.gba");

    let output = oxyboy()
        .current_dir(&dir)
        .env_remove("OXYBOY_ROM_DB")
        .args(["info", "--rom", "demo.gba"])
        .output()
        .expect("run oxyboy");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert!(
        stdout.contains("identification: no database supplied"),
        "stdout: {stdout}"
    );
}

#[test]
fn environment_variable_selects_database() {
    let dir = temp_dir("env");
    let bytes = write_image(&dir, "demo.gba");
    let hashes = Hashes::compute(&bytes);
    write_db(
        &dir,
        &entry("Golden Demo", &hashes.md5_hex(), &hashes.sha1_hex()),
    );

    let output = oxyboy()
        .current_dir(&dir)
        .env("OXYBOY_ROM_DB", "db.json")
        .args(["info", "--rom", "demo.gba"])
        .output()
        .expect("run oxyboy");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert!(
        stdout.contains("identification: Golden Demo (Homebrew)"),
        "stdout: {stdout}"
    );
}

#[test]
fn malformed_database_fails_loudly() {
    let dir = temp_dir("baddb");
    write_image(&dir, "demo.gba");
    write_db(&dir, &entry("Bad Hashes", "zz", "00"));

    let output = oxyboy()
        .current_dir(&dir)
        .args(["info", "--rom", "demo.gba", "--db", "db.json"])
        .output()
        .expect("run oxyboy");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(stderr.contains("database entry 0"), "stderr: {stderr}");
}

#[test]
fn invalid_checksum_is_flagged_not_fatal() {
    let dir = temp_dir("checksum");
    let mut bytes = write_image(&dir, "demo.gba");
    bytes[0x0BD] ^= 0xFF;
    fs::write(dir.join("demo.gba"), &bytes).expect("rewrite image");

    let output = oxyboy()
        .current_dir(&dir)
        .args(["info", "--rom", "demo.gba"])
        .output()
        .expect("run oxyboy");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert!(stdout.contains("checksum: INVALID"), "stdout: {stdout}");
}
