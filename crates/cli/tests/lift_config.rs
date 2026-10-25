//! Config-declared functions must be lifted even when discovery never
//! walks a call to them: the config is a user assertion, and the
//! seed's FUNC_START mark is what `function_regions` reads.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use testutil::synthetic::RomBuilder;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxyboy-lift-cfg-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn oxyboy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oxyboy"))
}

/// A ROM parked at the entry (`b .`) with an undiscoverable leaf at
/// 0x02000040: `movs r0, #42; bx lr` (thumb, hand-assembled and
/// verified with arm-none-eabi-as). RomBuilder images are multiboot,
/// so the leaf's execution address is in the 0x02000000 window.
fn parked_rom(dir: &Path, name: &str) -> PathBuf {
    let mut bytes = RomBuilder::new().title("CFG").build();
    bytes[0..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
    bytes[0x40..0x44].copy_from_slice(&[0x2A, 0x20, 0x70, 0x47]);
    let path = dir.join(name);
    fs::write(&path, &bytes).expect("write ROM");
    path
}

fn lift_c(rom: &Path, config: Option<&Path>) -> String {
    let mut args = vec!["lift".to_string(), "--rom".to_string()];
    args.push(rom.display().to_string());
    args.push("--c".to_string());
    if let Some(config) = config {
        args.push("--config".to_string());
        args.push(config.display().to_string());
    }
    let output = oxyboy().args(&args).output().expect("run oxyboy lift");
    assert!(
        output.status.success(),
        "lift failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8 C text")
}

#[test]
fn a_config_declared_function_is_lifted_without_a_call_path() {
    let dir = temp_dir("seed");
    let rom = parked_rom(&dir, "parked.gba");
    let config = dir.join("names.conf");
    fs::write(&config, "thumb_func 0x02000040 secret_leaf\n").expect("write config");

    let without = lift_c(&rom, None);
    assert!(
        !without.contains("02000040"),
        "the leaf must not be lifted without the config"
    );

    let with = lift_c(&rom, Some(&config));
    assert!(
        with.contains("secret_leaf"),
        "the config-declared leaf must appear in the unit:\n{with}"
    );
}

#[test]
fn a_config_table_hint_is_accepted_alongside_seeds() {
    // The hint's site does not exist here; the config must still load
    // and the seed still land (they are independent config features).
    let dir = temp_dir("hint");
    let rom = parked_rom(&dir, "parked.gba");
    let config = dir.join("names.conf");
    fs::write(
        &config,
        "thumb_func 0x02000040 secret_leaf\nthumb_table 0x08000080 2\n",
    )
    .expect("write config");
    let unit = lift_c(&rom, Some(&config));
    assert!(unit.contains("secret_leaf"));
}
