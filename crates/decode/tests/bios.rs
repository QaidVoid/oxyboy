//! BIOS call table completeness and SWI text annotation.
//!
//! The expected number/name pairs are GBATEK 3.06, "BIOS Function
//! Summary", GBA column; the tests pin the whole documented range plus
//! the crash boundary.

use decode::{SWI_NAMES, arm_text, swi_name, thumb_text};

#[test]
fn table_covers_the_whole_documented_gba_range() {
    // GBATEK 3.06 documents GBA SWI 00h..2Ah (43 calls) and marks
    // 2Bh+ as crash entries.
    assert_eq!(SWI_NAMES.len(), 43);
    for (index, (number, name)) in SWI_NAMES.iter().enumerate() {
        assert_eq!(*number, index as u8, "table must be dense and ordered");
        assert!(!name.is_empty());
        assert!(name.is_ascii(), "names must be ASCII: {name}");
    }
}

#[test]
fn names_match_the_gbatek_gba_column() {
    for (number, expected) in [
        (0x00, "SoftReset"),
        (0x01, "RegisterRamReset"),
        (0x05, "VBlankIntrWait"),
        (0x06, "Div"),
        (0x07, "DivArm"),
        (0x0B, "CpuSet"),
        (0x0C, "CpuFastSet"),
        (0x11, "LZ77UnCompReadNormalWrite8bit"),
        (0x13, "HuffUnCompReadNormal"),
        (0x1F, "MidiKey2Freq"),
        (0x25, "MultiBoot"),
        (0x2A, "SoundGetJumpList"),
    ] {
        assert_eq!(swi_name(number), Some(expected));
    }
    // 2Bh+ is documented as "Crash (SWI xxh..FFh do jump to garbage
    // addresses)": not a call, no name.
    assert_eq!(swi_name(0x2B), None);
    assert_eq!(swi_name(0xFF), None);
}

#[test]
fn swi_text_carries_the_bios_name_comment() {
    // ARM state takes the call number from the upper 8 bits of the
    // comment field (GBATEK "How BIOS Processes SWIs": specify
    // SWI NN*10000h in ARM state).
    assert_eq!(arm_text(0xEF06_0000), "swi #0x60000 @ Div");
    assert_eq!(thumb_text(0xDF0B, 0), "swi #0x0B @ CpuSet");
    // Undocumented numbers render bare.
    assert_eq!(arm_text(0xEF2B_0000), "swi #0x2B0000");
    assert_eq!(thumb_text(0xDF2B, 0), "swi #0x2B");
}
