//! End-to-end tests for the `lift` subcommand over built test ROMs.

use std::process::Command;

use testutil::testrom::locate;

fn oxyboy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oxyboy"))
}

#[test]
fn lift_prints_functions_for_a_test_rom() {
    let Ok(rom) = locate("thumbmix") else {
        println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
        return;
    };
    let output = oxyboy()
        .args(["lift", "--rom"])
        .arg(&rom)
        .output()
        .expect("run oxyboy");
    assert!(output.status.success(), "lift failed: {:?}", output.stderr);
    let stdout = String::from_utf8(output.stdout).expect("utf-8 IR");
    assert!(stdout.contains("fn sub_"), "no function printed: {stdout}");
    assert!(stdout.contains("b0("), "no block printed: {stdout}");
}

#[test]
fn lift_cfg_flags_dump_the_analysis() {
    let Ok(rom) = locate("thumbmix") else {
        println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
        return;
    };
    let text = oxyboy()
        .args(["lift", "--rom"])
        .arg(&rom)
        .arg("--cfg")
        .output()
        .expect("run oxyboy");
    assert!(
        text.status.success(),
        "lift --cfg failed: {:?}",
        text.stderr
    );
    let stdout = String::from_utf8(text.stdout).expect("utf-8 dump");
    assert!(stdout.contains("entry: b0"), "no entry: {stdout}");
    assert!(stdout.contains("dominator tree:"), "no tree: {stdout}");
    assert!(
        stdout.contains("dominance frontiers:"),
        "no frontiers: {stdout}"
    );
    assert!(stdout.contains("loops:"), "no loops: {stdout}");

    let dot = oxyboy()
        .args(["lift", "--rom"])
        .arg(&rom)
        .arg("--cfg-dot")
        .output()
        .expect("run oxyboy");
    assert!(
        dot.status.success(),
        "lift --cfg-dot failed: {:?}",
        dot.stderr
    );
    let stdout = String::from_utf8(dot.stdout).expect("utf-8 dot");
    assert!(stdout.contains("digraph cfg {"), "no dot graph: {stdout}");
    assert!(stdout.contains("entry -> b0;"), "no entry edge: {stdout}");
}

#[test]
fn lift_print_after_all_shows_every_pass() {
    let Ok(rom) = locate("thumbmix") else {
        println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
        return;
    };
    let output = oxyboy()
        .args(["lift", "--rom"])
        .arg(&rom)
        .arg("--print-after-all")
        .output()
        .expect("run oxyboy");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8 IR");
    for pass in ["lifted", "dead-flags", "const-prop", "copy-prop"] {
        assert!(
            stdout.contains(&format!("=== {pass} ===")),
            "missing {pass}"
        );
    }
}

#[test]
fn lift_function_filter_misses_report_failure() {
    let Ok(rom) = locate("thumbmix") else {
        println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
        return;
    };
    let output = oxyboy()
        .args(["lift", "--rom"])
        .arg(&rom)
        .args(["--function", "0x0dead000"])
        .output()
        .expect("run oxyboy");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(stderr.contains("no matching function"), "stderr: {stderr}");
}
