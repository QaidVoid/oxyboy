//! End-to-end check that `lift --c` output passes `gcc -fsyntax-only`
//! over the built test ROMs.

use std::process::Command;

use testutil::testrom::locate;

/// ROM names whose C output must compile; a subset is enough to pin
/// the harness and the emitter's declaration rules.
const ROMS: &[&str] = &[
    "switest",
    "mb-thumbptr",
    "mode3",
    "irqtest",
    "inputtest",
    "eeprom8ktest",
    "flash64test",
    "sramtest",
    "flash128test",
    "eeprom512test",
];

fn gcc_available() -> bool {
    Command::new("gcc")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn c_output_passes_gcc_syntax_over_corpus() {
    if !gcc_available() {
        println!("skipping: gcc not runnable");
        return;
    }
    let built: Vec<_> = ROMS.iter().filter_map(|name| locate(name).ok()).collect();
    if built.is_empty() {
        println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
        return;
    }
    for rom in built {
        let name = rom.file_stem().and_then(|s| s.to_str()).unwrap_or("rom");
        let output = Command::new(env!("CARGO_BIN_EXE_oxyboy"))
            .args(["lift", "--rom"])
            .arg(&rom)
            .arg("--c")
            .output()
            .unwrap_or_else(|e| panic!("run oxyboy for {name}: {e}"));
        assert!(
            output.status.success(),
            "lift --c failed for {name}: {:?}",
            output.stderr
        );
        let unit = String::from_utf8(output.stdout).expect("utf-8 C text");
        let path = std::env::temp_dir().join(format!("oxyboy_corpus_{name}.c"));
        std::fs::write(&path, &unit).unwrap_or_else(|e| panic!("write unit for {name}: {e}"));
        let check = Command::new("gcc")
            .arg("-fsyntax-only")
            .arg(&path)
            .output()
            .unwrap_or_else(|e| panic!("run gcc for {name}: {e}"));
        let _ = std::fs::remove_file(&path);
        assert!(
            check.status.success(),
            "gcc rejected {name}: {}\n{unit}",
            String::from_utf8_lossy(&check.stderr)
        );
    }
}
