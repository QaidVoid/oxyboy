//! Snapshot tests for the CLI surface.

use std::path::Path;
use std::process::Command;

use testutil::golden::assert_matches;

const SUBCOMMANDS: [&str; 10] = [
    "info",
    "disasm",
    "analyze",
    "lift",
    "recomp",
    "run",
    "diff",
    "corpus",
    "suite-test",
    "trace-diff",
];

fn oxyboy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oxyboy"))
}

#[test]
fn root_help_matches_golden() {
    let output = oxyboy().arg("--help").output().expect("run oxyboy");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8 help");
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/root-help.txt");
    assert_matches(&path, &stdout);
}

#[test]
fn every_subcommand_help_matches_golden() {
    for name in SUBCOMMANDS {
        let output = oxyboy()
            .args([name, "--help"])
            .output()
            .expect("run oxyboy");
        assert!(output.status.success(), "{name} --help failed");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 help");
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(format!("{name}-help.txt"));
        assert_matches(&path, &stdout);
    }
}

#[test]
fn every_subcommand_has_accurate_help() {
    // Subcommands flattening `RomArgs` share the `--rom`/`--verbose`
    // convention; the others take their own inputs.
    const ROM_SUBCOMMANDS: [&str; 8] = [
        "info",
        "disasm",
        "analyze",
        "lift",
        "recomp",
        "run",
        "diff",
        "suite-test",
    ];
    for name in SUBCOMMANDS {
        let output = oxyboy()
            .args([name, "--help"])
            .output()
            .expect("run oxyboy");
        assert!(output.status.success(), "{name} --help failed");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 help");
        if ROM_SUBCOMMANDS.contains(&name) {
            assert!(stdout.contains("--rom <FILE>"), "{name}: {stdout}");
            assert!(stdout.contains("--verbose"), "{name}: {stdout}");
        }
    }
}

#[test]
fn info_help_documents_database_flag() {
    let output = oxyboy()
        .args(["info", "--help"])
        .output()
        .expect("run oxyboy");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 help");
    assert!(stdout.contains("--db <FILE>"), "stdout: {stdout}");
    assert!(stdout.contains("OXYBOY_ROM_DB"), "stdout: {stdout}");
}

#[test]
fn bare_invocation_requires_a_subcommand() {
    let output = oxyboy().output().expect("run oxyboy");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(stderr.contains("subcommand"), "stderr: {stderr}");
}
