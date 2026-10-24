//! End-to-end tests for the debug tooling: the scripted
//! debugger session and the IO access log with filters.

use std::process::Command;

use testutil::testrom::locate;

fn oxyboy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oxyboy"))
}

fn rom_or_skip(name: &str) -> Option<std::path::PathBuf> {
    match locate(name) {
        Ok(rom) => Some(rom),
        Err(_) => {
            println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
            None
        }
    }
}

#[test]
fn debug_script_breaks_steps_dumps_and_peeks() {
    let Some(rom) = rom_or_skip("switest") else {
        return;
    };
    let script = std::env::temp_dir().join(format!("oxyboy_dbg_{}.txt", std::process::id()));
    std::fs::write(
        &script,
        "# scripted session\nbreak 0x02000110\nrun\nregs\npeek 0x02000000 4\nstep 2\nquit\n",
    )
    .expect("writes script");
    let output = oxyboy()
        .args(["debug", "--rom"])
        .arg(&rom)
        .arg("--script")
        .arg(&script)
        .output()
        .expect("run oxyboy");
    let _ = std::fs::remove_file(&script);
    assert!(output.status.success(), "debug failed: {:?}", output.stderr);
    let stdout = String::from_utf8(output.stdout).expect("utf-8 session");
    assert!(stdout.contains("> break 0x02000110"), "{stdout}");
    assert!(stdout.contains("breakpoint 0 at 0x02000110"), "{stdout}");
    assert!(stdout.contains("break at 0x02000110"), "{stdout}");
    assert!(stdout.contains("r14="), "{stdout}");
    assert!(stdout.contains("cpsr=0x"), "{stdout}");
    // The multiboot copy maps the header at 0x02000000; its first
    // word is the entry branch.
    assert!(stdout.contains("0x02000000: 0xea00003e"), "{stdout}");
    assert!(stdout.contains("> step 2"), "{stdout}");
    assert!(stdout.contains("pc 0x02000118"), "{stdout}");
}

#[test]
fn debug_run_without_breakpoint_reports_frame_budget() {
    let Some(rom) = rom_or_skip("switest") else {
        return;
    };
    let script = std::env::temp_dir().join(format!("oxyboy_dbg2_{}.txt", std::process::id()));
    std::fs::write(&script, "run 1\nquit\n").expect("writes script");
    let output = oxyboy()
        .args(["debug", "--rom"])
        .arg(&rom)
        .arg("--script")
        .arg(&script)
        .output()
        .expect("run oxyboy");
    let _ = std::fs::remove_file(&script);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8 session");
    assert!(stdout.contains("ran 1 frames"), "{stdout}");
}

#[test]
fn io_log_writes_csv_and_range_filters_apply() {
    let Some(rom) = rom_or_skip("mode3") else {
        return;
    };
    let full = std::env::temp_dir().join(format!("oxyboy_io_{}.csv", std::process::id()));
    let output = oxyboy()
        .args(["run", "--rom"])
        .arg(&rom)
        .args(["--headless", "--frames", "2", "--io-log"])
        .arg(&full)
        .output()
        .expect("run oxyboy");
    assert!(
        output.status.success(),
        "run --io-log failed: {:?}",
        output.stderr
    );
    let csv = std::fs::read_to_string(&full).expect("reads csv");
    let _ = std::fs::remove_file(&full);
    let mut lines = csv.lines();
    assert_eq!(lines.next(), Some("pc,addr,rw,width,value"));
    let rows: Vec<&str> = lines.collect();
    assert!(
        rows.iter().any(|row| row.contains(",0x04000000,w,2,")),
        "no DISPCNT write in:\n{csv}"
    );

    let narrow = std::env::temp_dir().join(format!("oxyboy_io2_{}.csv", std::process::id()));
    let filtered = oxyboy()
        .args(["run", "--rom"])
        .arg(&rom)
        .args([
            "--headless",
            "--frames",
            "2",
            "--io-from",
            "0x04000100",
            "--io-to",
            "0x040001ff",
            "--io-log",
        ])
        .arg(&narrow)
        .output()
        .expect("run oxyboy");
    assert!(filtered.status.success());
    let csv = std::fs::read_to_string(&narrow).expect("reads csv");
    let _ = std::fs::remove_file(&narrow);
    for row in csv.lines().skip(1) {
        let addr = row.split(',').nth(1).expect("addr column");
        let value = u32::from_str_radix(addr.trim_start_matches("0x"), 16).expect("hex addr");
        assert!(
            (0x0400_0100..=0x0400_01FF).contains(&value),
            "row outside filter: {row}"
        );
    }
}

#[test]
fn io_log_requires_headless() {
    let Some(rom) = rom_or_skip("mode3") else {
        return;
    };
    let output = oxyboy()
        .args(["run", "--rom"])
        .arg(&rom)
        .args(["--frames", "1", "--io-log", "/tmp/oxyboy_windowed.csv"])
        .output()
        .expect("run oxyboy");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(stderr.contains("--headless"), "{stderr}");
}
