//! End-to-end tests for `run --trace` and `trace-diff` over built
//! test ROMs. Skips loudly when the ROMs are not built; see
//! scripts/build-test-roms.sh.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use testutil::testrom;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxyboy-trace-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn oxyboy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oxyboy"))
}

#[test]
fn traced_headless_runs_are_byte_identical_and_diff_clean() {
    let Ok(rom) = testrom::locate("switest") else {
        println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
        return;
    };
    let dir = temp_dir("same");
    let a = dir.join("a.oxytrace");
    let b = dir.join("b.oxytrace");
    for out in [&a, &b] {
        let output = oxyboy()
            .args(["run", "--headless", "--frames", "1", "--trace"])
            .arg(out)
            .arg("--rom")
            .arg(&rom)
            .output()
            .expect("run oxyboy");
        assert!(output.status.success(), "run failed: {:?}", output.stderr);
    }
    let a_bytes = fs::read(&a).expect("trace a");
    let b_bytes = fs::read(&b).expect("trace b");
    assert!(!a_bytes.is_empty());
    assert_eq!(a_bytes, b_bytes, "two runs must trace identically");

    let output = oxyboy()
        .args(["trace-diff"])
        .arg(&a)
        .arg(&b)
        .output()
        .expect("run oxyboy");
    assert!(output.status.success(), "diff failed: {:?}", output.stderr);
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert!(stdout.contains("identical"), "stdout: {stdout}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn trace_diff_surfaces_the_first_diverging_pc() {
    let Ok(switest) = testrom::locate("switest") else {
        println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
        return;
    };
    let Ok(mode3) = testrom::locate("mode3") else {
        println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
        return;
    };
    let dir = temp_dir("diverge");
    let a = dir.join("a.oxytrace");
    let b = dir.join("b.oxytrace");
    for (out, rom) in [(&a, &switest), (&b, &mode3)] {
        let output = oxyboy()
            .args(["run", "--headless", "--frames", "1", "--trace"])
            .arg(out)
            .arg("--rom")
            .arg(rom)
            .output()
            .expect("run oxyboy");
        assert!(output.status.success(), "run failed: {:?}", output.stderr);
    }
    let output = oxyboy()
        .args(["trace-diff"])
        .arg(&a)
        .arg(&b)
        .output()
        .expect("run oxyboy");
    assert_eq!(output.status.code(), Some(1), "a difference must fail");
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert!(
        stdout.contains("first divergence at instruction 0"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("pc 0x"), "stdout: {stdout}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn trace_diff_reports_length_mismatch_and_bad_files() {
    let Ok(switest) = testrom::locate("switest") else {
        println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
        return;
    };
    let dir = temp_dir("length");
    let one = dir.join("one.oxytrace");
    let two = dir.join("two.oxytrace");
    for (out, frames) in [(&one, "1"), (&two, "2")] {
        let output = oxyboy()
            .args(["run", "--headless", "--frames", frames, "--trace"])
            .arg(out)
            .arg("--rom")
            .arg(&switest)
            .output()
            .expect("run oxyboy");
        assert!(output.status.success(), "run failed: {:?}", output.stderr);
    }
    let output = oxyboy()
        .args(["trace-diff"])
        .arg(&one)
        .arg(&two)
        .output()
        .expect("run oxyboy");
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert!(stdout.contains("the second is longer"), "stdout: {stdout}");

    let output = oxyboy()
        .args(["trace-diff", "/nonexistent/a.oxytrace"])
        .arg(&one)
        .output()
        .expect("run oxyboy");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(stderr.contains("No such file"), "stderr: {stderr}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn trace_requires_headless() {
    let Ok(switest) = testrom::locate("switest") else {
        println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
        return;
    };
    let dir = temp_dir("windowed");
    let out = dir.join("w.oxytrace");
    let output = oxyboy()
        .args(["run", "--trace"])
        .arg(&out)
        .arg("--rom")
        .arg(&switest)
        .output()
        .expect("run oxyboy");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(stderr.contains("--headless"), "stderr: {stderr}");
    fs::remove_dir_all(&dir).ok();
}
