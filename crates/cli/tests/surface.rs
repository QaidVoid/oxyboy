//! End-to-end tests for the CLI surface: every subcommand
//! performs its real function on a corpus ROM, with `--json` and the
//! config workflow wired through.

use std::process::Command;

use testutil::testrom::locate;

fn oxyboy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oxyboy"))
}

fn switest() -> Option<std::path::PathBuf> {
    match locate("switest") {
        Ok(rom) => Some(rom),
        Err(_) => {
            println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
            None
        }
    }
}

#[test]
fn disasm_emits_assembly_to_stdout_and_out_file() {
    let Some(rom) = switest() else { return };
    let output = oxyboy()
        .args(["disasm", "--rom"])
        .arg(&rom)
        .output()
        .expect("run oxyboy");
    assert!(
        output.status.success(),
        "disasm failed: {:?}",
        output.stderr
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 asm");
    assert!(stdout.contains(".syntax unified"), "{stdout}");
    assert!(stdout.contains("sub_02000000"), "no labels: {stdout}");
    assert!(
        stdout.contains("swi"),
        "analyzed code not rendered: {stdout}"
    );

    let out = std::env::temp_dir().join(format!("oxyboy_disasm_{}.s", std::process::id()));
    let written = oxyboy()
        .args(["disasm", "--rom"])
        .arg(&rom)
        .arg("--out")
        .arg(&out)
        .output()
        .expect("run oxyboy");
    assert!(written.status.success(), "disasm --out failed");
    let text = std::fs::read_to_string(&out).expect("reads asm file");
    let _ = std::fs::remove_file(&out);
    assert_eq!(text, stdout, "--out content differs from stdout");
}

#[test]
fn analyze_reports_layout_in_text_and_stable_json() {
    let Some(rom) = switest() else { return };
    let text = oxyboy()
        .args(["analyze", "--rom"])
        .arg(&rom)
        .output()
        .expect("run oxyboy");
    assert!(text.status.success(), "analyze failed: {:?}", text.stderr);
    let stdout = String::from_utf8(text.stdout).expect("utf-8 report");
    assert!(stdout.contains("functions: 1"), "{stdout}");
    assert!(stdout.contains("0x02000000 arm"), "{stdout}");
    assert!(stdout.contains("swi calls: 4"), "{stdout}");

    let json = oxyboy()
        .args(["analyze", "--rom"])
        .arg(&rom)
        .arg("--json")
        .output()
        .expect("run oxyboy");
    assert!(json.status.success(), "analyze --json failed");
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).expect("valid JSON");
    let functions = value["functions"].as_array().expect("functions array");
    assert_eq!(functions.len(), 1);
    assert_eq!(functions[0]["addr"], "0x02000000");
    assert_eq!(functions[0]["state"], "arm");
    let swi = value["swi_calls"].as_array().expect("swi array");
    assert_eq!(swi.len(), 4);
    assert!(swi[0]["number"].is_u64(), "number must be numeric");
    for key in ["rejected", "tables", "copy_hints", "unresolved"] {
        assert!(value[key].is_array(), "missing stable field {key}");
    }
}

#[test]
fn analyze_config_out_feeds_named_functions_into_lift() {
    let Some(rom) = switest() else { return };
    let conf = std::env::temp_dir().join(format!("oxyboy_confout_{}.conf", std::process::id()));
    let analyzed = oxyboy()
        .args(["analyze", "--rom"])
        .arg(&rom)
        .arg("--config-out")
        .arg(&conf)
        .output()
        .expect("run oxyboy");
    assert!(analyzed.status.success(), "analyze --config-out failed");
    let mut config = std::fs::read_to_string(&conf).expect("reads config");
    config.push_str("arm_func 0x02000100 div_helper\n");
    std::fs::write(&conf, &config).expect("writes config");

    let unit = oxyboy()
        .args(["lift", "--rom"])
        .arg(&rom)
        .arg("--c")
        .arg("--config")
        .arg(&conf)
        .output()
        .expect("run oxyboy");
    let _ = std::fs::remove_file(&conf);
    assert!(unit.status.success(), "lift --config failed");
    let stdout = String::from_utf8(unit.stdout).expect("utf-8 C");
    assert!(
        stdout.contains("u32 div_helper(u32"),
        "named function not lifted as its own function:\n{stdout}"
    );
}

#[test]
fn recomp_emits_c_source_and_json_summary() {
    let Some(rom) = switest() else { return };
    let output = oxyboy()
        .args(["recomp", "--rom"])
        .arg(&rom)
        .output()
        .expect("run oxyboy");
    assert!(
        output.status.success(),
        "recomp failed: {:?}",
        output.stderr
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 C");
    assert!(stdout.contains("oxy_fn_02000000"), "{stdout}");
    assert!(stdout.contains("struct oxy_state"), "{stdout}");
    assert!(stdout.contains("oxy_ops"), "{stdout}");

    let out = std::env::temp_dir().join(format!("oxyboy_recomp_{}.c", std::process::id()));
    let written = oxyboy()
        .args(["recomp", "--rom"])
        .arg(&rom)
        .arg("--out")
        .arg(&out)
        .output()
        .expect("run oxyboy");
    assert!(written.status.success(), "recomp --out failed");
    assert!(out.exists(), "--out file not written");
    let _ = std::fs::remove_file(&out);

    let json = oxyboy()
        .args(["recomp", "--rom"])
        .arg(&rom)
        .arg("--json")
        .output()
        .expect("run oxyboy");
    assert!(json.status.success(), "recomp --json failed");
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).expect("valid JSON");
    assert_eq!(value["entries"][0], "0x02000000");
    assert_eq!(value["symbols"][0], "oxy_fn_02000000");
    assert!(value["c_bytes"].as_u64().unwrap() > 0);
}

#[test]
fn info_json_has_stable_fields() {
    let Some(rom) = switest() else { return };
    let output = oxyboy()
        .args(["info", "--rom"])
        .arg(&rom)
        .arg("--json")
        .output()
        .expect("run oxyboy");
    assert!(
        output.status.success(),
        "info --json failed: {:?}",
        output.stderr
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    for key in [
        "file",
        "size",
        "kind",
        "entry_point",
        "title",
        "game_code",
        "maker_code",
        "checksum_valid",
        "md5",
        "sha1",
        "identification",
    ] {
        assert!(value.get(key).is_some(), "missing stable field {key}");
    }
    assert_eq!(value["kind"], "multiboot");
    assert_eq!(value["entry_point"], "0xEA00003E");
}

#[test]
fn subcommands_report_missing_rom_with_exit_1() {
    for name in ["info", "disasm", "analyze", "lift", "recomp"] {
        let output = oxyboy()
            .args([name, "--rom", "no-such-rom.gba"])
            .output()
            .expect("run oxyboy");
        assert_eq!(output.status.code(), Some(1), "{name} must exit 1");
        let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
        assert!(!stderr.is_empty(), "{name} must explain the failure");
    }
}
