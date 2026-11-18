//! Jump-table detection over assembler-verified switch ROMs.
//!
//! Each ROM exercises one documented idiom; every byte layout below was
//! assembled with Binutils 2.32 and dumped with objdump before being
//! embedded. Note: format 7's base field is three bits, so a THUMB
//! `ldr rX, [pc, rY]` is not encodable; the THUMB indexed idiom pulls
//! the table base from a literal pool first.

use analysis::{
    Analysis, FlagMap, Flags, JumpTable, State, Stop, StopReason, TableHint, Thresholds, analyze,
};
use rom::Image;
use testutil::synthetic::RomBuilder;

const E: Flags = Flags::EXEC;
const W: Flags = Flags::WORD;
const T: Flags = Flags::THUMB;

fn analyze_rom(rom: &Image, hints: &[TableHint]) -> (Analysis, FlagMap) {
    let mut map = FlagMap::from_image(rom);
    let seeds = map.seed(&[]);
    let analysis = analyze(rom, &mut map, &seeds, &seeds, Thresholds::default(), hints);
    (analysis, map)
}

fn arm_switch_rom() -> Image {
    // 00: mov r0, #1
    // 04: ldr r1, [pc, #8]          ; pool at 0x14
    // 08: mov r3, #0
    // 0C: ldr r2, [r1, r3, lsl #2]
    // 10: mov pc, r2
    // 14: .word 0x08000028          ; table base
    // 18: case0: mov r4, #1; bx lr
    // 20: case1: mov r5, #2; bx lr
    // 28: table: .word 0x08000018, 0x08000020
    let mut bytes = RomBuilder::new().title("ARMSW").build();
    bytes[0x00..0x30].copy_from_slice(&[
        0x01, 0x00, 0xA0, 0xE3, 0x08, 0x10, 0x9F, 0xE5, 0x00, 0x30, 0xA0, 0xE3, 0x03, 0x21, 0x91,
        0xE7, 0x02, 0xF0, 0xA0, 0xE1, 0x28, 0x00, 0x00, 0x08, 0x01, 0x40, 0xA0, 0xE3, 0x1E, 0xFF,
        0x2F, 0xE1, 0x02, 0x50, 0xA0, 0xE3, 0x1E, 0xFF, 0x2F, 0xE1, 0x18, 0x00, 0x00, 0x08, 0x20,
        0x00, 0x00, 0x08,
    ]);
    Image::from_bytes(bytes).expect("parse")
}

#[test]
fn arm_absolute_switch_is_found_and_marked() {
    let rom = arm_switch_rom();
    let (analysis, map) = analyze_rom(&rom, &[]);
    assert_eq!(
        analysis.tables,
        vec![JumpTable {
            site: 0x0800_0010,
            state: State::Arm,
            base: 0x0800_0028,
            entries: 2,
            targets: vec![(0x0800_0018, State::Arm), (0x0800_0020, State::Arm)],
        }]
    );
    // Table words are data (WORD, no EXEC); targets became code paths.
    assert_eq!(map.get(0x0800_0028), W);
    assert_eq!(map.get(0x0800_002C), W);
    assert_eq!(map.get(0x0800_0018), E | W);
    assert_eq!(map.get(0x0800_001C), E | W);
    assert_eq!(map.get(0x0800_0020), E | W);
    assert!(analysis.unresolved.is_empty());
    // No BLs or push sites exist: discovery keeps only the entry.
    assert_eq!(analysis.discovery.functions.len(), 1);
}

fn thumb_inline_rom() -> Image {
    // ARM shield at 0x00 branches into THUMB at 0x14.
    // 14: lsls r3, r0, #2
    // 16: ldr r2, [pc, #4]           ; base pointer word at 0x1C
    // 18: ldr r2, [r2, r3]
    // 1A: mov pc, r2
    // 1C: .word 0x08000024           ; table base
    // 20: .word 0
    // 24: table: .word 0x0800002D, 0x08000031
    // 2C: case0: movs r4, #1; bx lr
    // 30: case1: movs r5, #2; bx lr
    let mut bytes = RomBuilder::new().title("THSW").build();
    bytes[0x00..0x34].copy_from_slice(&[
        0x01, 0x00, 0xA0, 0xE3, 0x04, 0x00, 0x9F, 0xE5, 0x10, 0xFF, 0x2F, 0xE1, 0x00, 0x00, 0x00,
        0x00, 0x15, 0x00, 0x00, 0x08, 0x83, 0x00, 0x01, 0x4A, 0xD2, 0x58, 0x97, 0x46, 0x24, 0x00,
        0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x2D, 0x00, 0x00, 0x08, 0x31, 0x00, 0x00, 0x08, 0x01,
        0x24, 0x70, 0x47, 0x02, 0x25, 0x70, 0x47,
    ]);
    Image::from_bytes(bytes).expect("parse")
}

#[test]
fn thumb_inline_switch_is_found() {
    let rom = thumb_inline_rom();
    let (analysis, map) = analyze_rom(&rom, &[]);
    assert_eq!(
        analysis.tables,
        vec![JumpTable {
            site: 0x0800_001A,
            state: State::Thumb,
            base: 0x0800_0024,
            entries: 2,
            targets: vec![(0x0800_002C, State::Thumb), (0x0800_0030, State::Thumb)],
        }]
    );
    assert_eq!(map.get(0x0800_0024), W);
    assert_eq!(map.get(0x0800_002C), E | W | T);
    assert!(analysis.unresolved.is_empty());
}

fn thumb_relative_rom() -> Image {
    // ARM shield branches into THUMB at 0x14.
    // 14: ldr r2, [pc, #4]           ; entry words at 0x1C
    // 16: add r2, pc                 ; anchor 0x1A
    // 18: mov pc, r2
    // 1A: nop
    // 1C: table: .word 0x0A, 0x0E (case0 - 0x1A, case1 - 0x1A)
    // 24: case0: movs r4, #1; bx lr
    // 28: case1: movs r5, #2; bx lr
    let mut bytes = RomBuilder::new().title("THRL").build();
    bytes[0x00..0x2C].copy_from_slice(&[
        0x01, 0x00, 0xA0, 0xE3, 0x04, 0x00, 0x9F, 0xE5, 0x10, 0xFF, 0x2F, 0xE1, 0x00, 0x00, 0x00,
        0x00, 0x15, 0x00, 0x00, 0x08, 0x01, 0x4A, 0x7A, 0x44, 0x97, 0x46, 0xC0, 0x46, 0x0A, 0x00,
        0x00, 0x00, 0x0E, 0x00, 0x00, 0x00, 0x01, 0x24, 0x70, 0x47, 0x02, 0x25, 0x70, 0x47,
    ]);
    Image::from_bytes(bytes).expect("parse")
}

#[test]
fn thumb_relative_jump_resolves_its_single_target() {
    // The immediate-offset load always reads the word at 0x1C, so only
    // case0 is reachable; the next word is not a table entry.
    let rom = thumb_relative_rom();
    let (analysis, map) = analyze_rom(&rom, &[]);
    assert_eq!(
        analysis.tables,
        vec![JumpTable {
            site: 0x0800_0018,
            state: State::Thumb,
            base: 0x0800_001C,
            entries: 1,
            targets: vec![(0x0800_0024, State::Thumb)],
        }]
    );
    assert_eq!(map.get(0x0800_0024), E | W | T);
    assert_eq!(map.get(0x0800_0028), Flags::EMPTY);
    assert!(analysis.unresolved.is_empty());
}

fn broken_window_rom() -> Image {
    // Same shape as the ARM switch but `mov r1, #9` clobbers the table
    // base before the indexed load, so the detection window goes Bot.
    let mut bytes = RomBuilder::new().title("BRKSW").build();
    bytes[0x00..0x30].copy_from_slice(&[
        0x01, 0x00, 0xA0, 0xE3, 0x08, 0x10, 0x9F, 0xE5, 0x09, 0x10, 0xA0, 0xE3, 0x03, 0x21, 0x91,
        0xE7, 0x02, 0xF0, 0xA0, 0xE1, 0x28, 0x00, 0x00, 0x08, 0x01, 0x40, 0xA0, 0xE3, 0x1E, 0xFF,
        0x2F, 0xE1, 0x02, 0x50, 0xA0, 0xE3, 0x1E, 0xFF, 0x2F, 0xE1, 0x18, 0x00, 0x00, 0x08, 0x20,
        0x00, 0x00, 0x08,
    ]);
    Image::from_bytes(bytes).expect("parse")
}

#[test]
fn broken_windows_stay_unresolved_until_hinted() {
    let rom = broken_window_rom();

    // Without a hint the indirect jump stays flagged as unresolved and
    // nothing is marked.
    let (analysis, map) = analyze_rom(&rom, &[]);
    assert!(analysis.tables.is_empty());
    assert_eq!(
        analysis.unresolved,
        vec![Stop {
            addr: 0x0800_0010,
            state: State::Arm,
            reason: StopReason::UnknownTarget,
        }]
    );
    assert_eq!(map.get(0x0800_0028), Flags::EMPTY);

    // The hint declares the table outright: targets become paths, the
    // table words are marked, and the unexplained site stays listed.
    let hints = [TableHint {
        addr: 0x0800_0028,
        entries: 2,
        state: State::Arm,
    }];
    let (analysis, map) = analyze_rom(&rom, &hints);
    assert_eq!(
        analysis.tables,
        vec![JumpTable {
            site: 0x0800_0028,
            state: State::Arm,
            base: 0x0800_0028,
            entries: 2,
            targets: vec![(0x0800_0018, State::Arm), (0x0800_0020, State::Arm)],
        }]
    );
    assert_eq!(map.get(0x0800_0028), W);
    assert_eq!(map.get(0x0800_0018), E | W);
    assert_eq!(
        analysis.unresolved,
        vec![Stop {
            addr: 0x0800_0010,
            state: State::Arm,
            reason: StopReason::UnknownTarget,
        }]
    );
}

#[test]
fn pointer_like_data_stays_unanalyzed() {
    // Entry returns immediately; the rest of the image is filled with
    // ROM-span words. Nothing may mark them or become a function.
    let mut bytes = RomBuilder::new().title("DATAFP").build();
    bytes[0x00..0x08].copy_from_slice(&[
        0x01, 0x00, 0xA0, 0xE3, // 00: mov r0, #1
        0x1E, 0xFF, 0x2F, 0xE1, // 04: bx lr
    ]);
    for offset in (0x08..0x100).step_by(4) {
        bytes[offset..offset + 4].copy_from_slice(&0x0800_0040u32.to_le_bytes());
    }
    let rom = Image::from_bytes(bytes).expect("parse");
    let (analysis, map) = analyze_rom(&rom, &[]);
    assert!(analysis.tables.is_empty());
    assert!(analysis.unresolved.is_empty());
    assert_eq!(analysis.discovery.functions.len(), 1);
    assert!(analysis.discovery.rejected.is_empty());
    assert_eq!(map.get(0x0800_0008), Flags::EMPTY, "data stays empty");
    assert_eq!(map.get(0x0800_00FC), Flags::EMPTY, "data stays empty");
}

/// A range-checked ARM switch whose two-entry table is followed by a
/// literal pool word that also looks like a code address.
fn bounded_switch_rom() -> Image {
    let words: [(usize, u32); 13] = [
        (0x00, 0xE3A0_0001), // mov r0, #1
        (0x04, 0xE350_0001), // cmp r0, #1
        (0x08, 0x8A00_0006), // bhi default (0x28)
        (0x0C, 0xE59F_1008), // ldr r1, [pc, #8]   ; pool at 0x1C
        (0x10, 0xE791_2100), // ldr r2, [r1, r0, lsl #2]
        (0x14, 0xE1A0_F002), // mov pc, r2
        (0x1C, 0x0800_0030), // table base
        (0x20, 0xE3A0_4001), // case0: mov r4, #1
        (0x24, 0xE12F_FF1E), // bx lr
        (0x28, 0xE3A0_4003), // default/case1: mov r4, #3
        (0x2C, 0xE12F_FF1E), // bx lr
        (0x30, 0x0800_0020), // table: case0
        (0x34, 0x0800_0028), //        case1
    ];
    let mut bytes = RomBuilder::new().title("BNDSW").build();
    for (offset, word) in words {
        bytes[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
    }
    // A pool word right after the table, pointing at bytes that decode.
    bytes[0x38..0x3C].copy_from_slice(&0x0800_0040u32.to_le_bytes());
    bytes[0x40..0x44].copy_from_slice(&0xE3A0_5005u32.to_le_bytes());
    Image::from_bytes(bytes).expect("parse")
}

#[test]
fn range_check_caps_the_table() {
    let rom = bounded_switch_rom();
    let (analysis, map) = analyze_rom(&rom, &[]);
    assert_eq!(
        analysis.tables,
        vec![JumpTable {
            site: 0x0800_0014,
            state: State::Arm,
            base: 0x0800_0030,
            entries: 2,
            targets: vec![(0x0800_0020, State::Arm), (0x0800_0028, State::Arm)],
        }]
    );
    assert_eq!(
        map.get(0x0800_0040),
        Flags::EMPTY,
        "the pool word is not a case"
    );
}
