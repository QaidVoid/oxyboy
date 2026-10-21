//! Snapshot tests for the CLI surface.

use std::path::Path;
use std::process::Command;

use testutil::golden::assert_matches;

const SUBCOMMANDS: [&str; 6] = ["info", "disasm", "analyze", "lift", "recomp", "run"];

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
fn every_subcommand_has_accurate_help() {
    for name in SUBCOMMANDS {
        let output = oxyboy()
            .args([name, "--help"])
            .output()
            .expect("run oxyboy");
        assert!(output.status.success(), "{name} --help failed");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 help");
        assert!(
            stdout.contains("--rom <FILE>"),
            "{name} --help is missing --rom: {stdout}"
        );
        assert!(
            stdout.contains("--verbose"),
            "{name} --help is missing --verbose: {stdout}"
        );
        assert!(
            stdout.to_ascii_lowercase().contains(name),
            "{name} --help does not mention the subcommand: {stdout}"
        );
    }
}

#[test]
fn unimplemented_subcommands_fail_loudly() {
    for name in SUBCOMMANDS {
        let output = oxyboy()
            .args([name, "--rom", "unused.gba"])
            .output()
            .expect("run oxyboy");
        assert_eq!(
            output.status.code(),
            Some(1),
            "{name} must exit 1 until implemented"
        );
        let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
        assert!(
            stderr.contains("not implemented"),
            "{name} stderr must say it is not implemented: {stderr}"
        );
    }
}

#[test]
fn bare_invocation_requires_a_subcommand() {
    let output = oxyboy().output().expect("run oxyboy");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(stderr.contains("subcommand"), "stderr: {stderr}");
}
