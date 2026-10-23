//! Traced-oracle determinism: two headless runs of the same ROM must
//! produce byte-identical traces, and records
//! must carry the writes the run performed.
//!
//! Skips loudly when the ROM is not built; see scripts/build-test-roms.sh.

use rom::Image;
use runtime::ppu::{CYCLES_PER_LINE, LINES_PER_FRAME};
use runtime::system::System;
use trace::{Reader, Writer};

fn run_traced(frames: u32) -> Vec<u8> {
    let Some(path) = testutil::testrom::require("switest") else {
        return Vec::new();
    };
    let image = Image::from_file(&path).expect("script-built ROM must parse");
    let mut system = System::new(image.bytes().to_vec(), 0x0800_0000);
    let mut bytes = Vec::new();
    let mut writer = Writer::new(&mut bytes).expect("header");
    let budget = u64::from(frames)
        * u64::from(CYCLES_PER_LINE)
        * u64::try_from(LINES_PER_FRAME).unwrap_or(0);
    let mut spent = 0;
    while spent < budget {
        let (cycles, record) = system.step_recorded();
        writer.write(&record).expect("record");
        spent += u64::from(cycles);
    }
    writer.finish().expect("flush");
    bytes
}

#[test]
fn two_runs_produce_byte_identical_traces() {
    let Some(_) = testutil::testrom::require("switest") else {
        println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
        return;
    };
    let a = run_traced(2);
    let b = run_traced(2);
    assert!(!a.is_empty(), "the run must produce records");
    assert_eq!(a, b, "two runs of the same ROM must trace identically");
}

#[test]
fn records_carry_pc_state_and_memory_writes() {
    let Some(_) = testutil::testrom::require("switest") else {
        println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
        return;
    };
    let bytes = run_traced(1);
    let mut reader = Reader::new(&bytes[..]).expect("header");

    // The first record: the cart entry branch, ARM state, branching
    // over the header to the start label at 0x100.
    let first = reader.next_record().expect("read").expect("record");
    assert_eq!(first.pc, 0x0800_0100, "entry branch target");
    assert_eq!(first.flags, 0, "ARM state, not halted");

    // Somewhere in the trace the HLE CpuFastSet fill must appear as
    // bus writes to the fill destination, and the LZ77 output as byte
    // or word writes into EWRAM.
    let mut fill_writes = 0;
    let mut lz77_writes = 0;
    while let Some(record) = reader.next_record().expect("read") {
        for write in &record.writes {
            if (0x0200_0100..0x0200_0120).contains(&write.addr) {
                fill_writes += 1;
            }
            if (0x0200_0000..0x0200_0010).contains(&write.addr) {
                lz77_writes += 1;
            }
        }
    }
    assert!(fill_writes > 0, "no CpuFastSet fill writes recorded");
    assert!(lz77_writes > 0, "no LZ77 output writes recorded");
}

#[test]
fn trace_diff_reports_no_difference_between_identical_runs() {
    let Some(_) = testutil::testrom::require("switest") else {
        println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
        return;
    };
    let a = run_traced(1);
    let b = run_traced(1);
    assert_eq!(
        trace::diff(Reader::new(&a[..]).unwrap(), Reader::new(&b[..]).unwrap()).expect("read"),
        trace::Diff::Identical
    );
}
