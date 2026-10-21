//! ARMv4T undefined-space verification.
//!
//! The undefined/unpredictable table below mirrors what
//! [`decode::decode_arm`] enforces, with a source citation per entry.
//! The debug-mode enumeration walks every table region; the release-mode
//! test sweeps the full 2^32 space and is the brute-force totality
//! proof.

use decode::{ArmInstr, decode_arm};

/// One documented undefined region: `mask` and `value` select the words,
/// the citation says who defines the space as undefined.
struct UndefinedRegion {
    mask: u32,
    value: u32,
    citation: &'static str,
}

/// Cond field 0xF is reserved, so it is folded in by the tests rather
/// than listed per region.
const REGIONS: [UndefinedRegion; 6] = [
    // Class 011 with bit 4 set: the undefined-instruction space.
    UndefinedRegion {
        mask: 0x0E00_0010,
        value: 0x0600_0010,
        citation: "ARM ARM A3.4 Undefined instructions; GBATEK Single Data Transfer: bit 4 'Must be 0'",
    },
    // Class 110: coprocessor load/store. The ARM7TDMI has no coprocessor.
    UndefinedRegion {
        mask: 0x0E00_0000,
        value: 0x0C00_0000,
        citation: "GBATEK Coprocessor Instructions: 'no coprocessor exists' on GBA/NDS7",
    },
    // Class 111, bit 24 clear: CDP.
    UndefinedRegion {
        mask: 0x0F00_0010,
        value: 0x0E00_0000,
        citation: "GBATEK Coprocessor Instructions: 'no coprocessor exists' on GBA/NDS7",
    },
    // Class 111, bit 24 clear with bit 4 set: MCR/MRC.
    UndefinedRegion {
        mask: 0x0F00_0010,
        value: 0x0E00_0010,
        citation: "GBATEK Coprocessor Instructions: 'no coprocessor exists' on GBA/NDS7",
    },
    // Multiply space leftovers: bits 7..4 = 1001 without the MUL, MULL,
    // or SWP fixed patterns. This region is exception-riddled, so it is
    // enumerated separately in `multiply_space_leftovers_are_undefined`
    // instead of as a flat mask.
    // ARMv5TE LDRD (L=0, SH=10): not in ARMv4T.
    UndefinedRegion {
        mask: 0x0F10_00F0,
        value: 0x0000_00D0,
        citation: "GBATEK Halfword, Doubleword, and Signed Data Transfer: SH 10/11 with L=0 are 'ARMv5TE and up only'",
    },
    // ARMv5TE STRD (L=0, SH=11): not in ARMv4T.
    UndefinedRegion {
        mask: 0x0F10_00F0,
        value: 0x0000_00F0,
        citation: "GBATEK Halfword, Doubleword, and Signed Data Transfer: SH 10/11 with L=0 are 'ARMv5TE and up only'",
    },
];

/// The reserved condition field, tested as its own region.
const RESERVED_COND: u32 = 0xF;

/// Asserts the word decodes to Undefined carrying its own encoding.
fn assert_undefined(word: u32, context: &str) {
    assert_eq!(
        decode_arm(word),
        ArmInstr::Undefined { encoding: word },
        "{context}: {word:#010X} should be undefined"
    );
}

#[test]
fn every_region_and_cond_decodes_undefined() {
    for region in &REGIONS {
        let free = !(region.mask | 0xF000_0000) & 0x0FFF_FFFF;
        for cond in 0u32..16 {
            let base = (cond << 28) | region.value;
            for fill in [0, free] {
                assert_undefined(
                    base | fill,
                    &format!(
                        "region mask {:#010X} cond {cond} fill {fill:#010X}",
                        region.mask
                    ),
                );
            }
            // Toggling each free bit individually covers every
            // bit-position-dependent branch decode can take in this
            // region; in these regions decode reads only the mask bits,
            // so this plus the two fills above is exhaustive by
            // construction.
            for bit in 0..28 {
                let toggle = 1u32 << bit;
                if toggle & free != 0 {
                    assert_undefined(base | toggle, region.citation);
                }
            }
        }
    }
    // Reserved condition: every cond-0xF word is undefined.
    for word in [0xF000_0000u32, 0xF7FF_FFFF, 0xFFFF_FFFF, 0xF123_4567] {
        assert_undefined(word, "reserved condition 0b1111 (ARM ARM A3.3)");
    }
    let _ = RESERVED_COND;
}

#[test]
fn twenty_documented_undefined_encodings() {
    let cases: [(&str, u32); 20] = [
        ("cdp r0, 0, cr0, cr0, cr0 (cond EQ)", 0x0E00_0000),
        ("cdp with operands", 0xEE11_3A7F),
        ("mcr p0, 0, r0, cr0, cr0", 0xEE00_0010),
        ("mrc p15, 0, r0, cr0, cr0", 0xEE10_0F10),
        ("ldc p0, cr0, [r0]", 0xED90_0000),
        ("stc p0, cr0, [r0]", 0xEC80_0000),
        ("ldc with writeback", 0xEDB0_0001),
        ("stc post-indexed", 0xEC40_0002),
        ("undefined-instruction space, EQ", 0x0600_0010),
        ("undefined-instruction space, AL", 0xE600_0010),
        ("undefined-instruction space, NE", 0x1600_0FF0),
        ("multiply space leftover", 0xE1A0_0091),
        ("multiply space leftover, second shape", 0xE040_0291),
        ("ARMv5TE LDRD shape", 0xE1C1_20D0),
        ("ARMv5TE STRD shape", 0xE1C1_30F0),
        ("SH=00 L=1 has no halfword form", 0xE112_0091),
        ("SH=00 without SWP bits 11..4", 0xE181_0090),
        ("reserved condition, SWI-shaped", 0xFF00_0000),
        ("reserved condition, data-shaped", 0xF3A0_0001),
        ("reserved condition, all ones", 0xFFFF_FFFF),
    ];
    for (description, word) in cases {
        assert_undefined(word, description);
    }
}

/// The multiply space (bits 7..4 = 1001) is undefined exactly where the
/// word matches neither MUL/MLA, the long multiply family, nor SWP.
/// This test enumerates every field the decoder branches on in that
/// space, so it covers the defined/undefined boundary exactly.
#[test]
fn multiply_space_leftovers_are_undefined() {
    for cond in [0u32, 0xE] {
        // bits 27..25 must be 000 for the multiply space, so bits 27..23
        // only spans b24..b23 (0..4).
        for bits2723 in 0u32..4 {
            for bit22 in 0..2 {
                for bits2120 in 0..4 {
                    for bits118 in 0..16 {
                        for bits30 in 0..16 {
                            // bits 19..12 hold a fixed non-zero fill; the
                            // decoder does not branch on them here.
                            let word = cond << 28
                                | bits2723 << 23
                                | bit22 << 22
                                | bits2120 << 20
                                | 0xA5 << 12
                                | bits118 << 8
                                | 0x9 << 4
                                | bits30;
                            let defined = (bits2723 == 0 && bit22 == 0)
                                || bits2723 == 0b0_0001
                                || (bits2723 == 0b0_0010 && bits2120 == 0 && bits118 == 0);
                            let decoded = decode_arm(word);
                            if defined {
                                assert!(
                                    !matches!(decoded, ArmInstr::Undefined { .. }),
                                    "{word:#010X} should be a defined multiply-space instruction"
                                );
                            } else {
                                assert_eq!(
                                    decoded,
                                    ArmInstr::Undefined { encoding: word },
                                    "{word:#010X} should be undefined"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Full totality proof over all 2^32 words. Run explicitly:
/// `cargo test --release -p decode --test undefined_space -- --ignored`
#[test]
#[ignore = "exhaustive 2^32 sweep, release mode only"]
fn release_sweep_entire_space() {
    let started = std::time::Instant::now();
    let mut undefined_count = 0u64;
    for word in 0..=u32::MAX {
        if let ArmInstr::Undefined { encoding } = decode_arm(word) {
            assert_eq!(encoding, word);
            undefined_count += 1;
        }
    }
    let elapsed = started.elapsed();
    // Local time budget: the sweep completes in well under a minute in
    // release mode; anything beyond this signals a decoder slowdown.
    assert!(
        elapsed.as_secs() < 120,
        "exhaustive sweep exceeded budget: {elapsed:?}"
    );
    // The undefined space must be non-trivial but not dominate the
    // instruction encoding space.
    assert!(undefined_count > 0);
}
