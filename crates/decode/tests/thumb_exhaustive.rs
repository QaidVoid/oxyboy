//! Exhaustive THUMB space coverage: every halfword decodes, per-range
//! routing holds, and the documented illegal forms are flagged.

use decode::{ThumbInstr, decode_thumb};

#[test]
fn formats_one_to_nine_are_exhaustive_and_never_branch() {
    // Halfwords below 0xA000 route to formats 1 through 9. None of them
    // can decode to a jump, and the only undefined forms are the
    // high-register low-low and BX h1 aliases.
    for halfword in 0u16..0xA000 {
        match decode_thumb(halfword, 0xFFFF) {
            ThumbInstr::ConditionalBranch { .. }
            | ThumbInstr::SoftwareInterrupt { .. }
            | ThumbInstr::UnconditionalBranch { .. }
            | ThumbInstr::LongBranchWithLink { .. }
            | ThumbInstr::LongBranchWithLinkFromLr { .. } => {
                panic!("{halfword:#06X} decoded as a branch outside the jump formats")
            }
            _ => {}
        }
    }
}

#[test]
fn entire_space_is_exhaustive_over_next_halfwords() {
    // Every halfword decodes panic-free with several next-halfword
    // contexts (no-pair, valid BL second half, undefined-space value).
    for next in [0x0000u16, 0xFFFF, 0xD000] {
        for halfword in 0..=u16::MAX {
            if let ThumbInstr::Undefined { encoding } = decode_thumb(halfword, next) {
                assert_eq!(encoding, halfword);
            }
        }
    }
}

#[test]
fn standalone_bl_second_half_decodes_distinctly() {
    // 0xF800 is the documented "BL LR+0" (GBATEK THUMB.19 note).
    assert_eq!(
        decode_thumb(0xF800, 0x4770),
        ThumbInstr::LongBranchWithLinkFromLr { offset: 0 }
    );
    assert_eq!(
        decode_thumb(0xF801, 0),
        ThumbInstr::LongBranchWithLinkFromLr { offset: 2 }
    );
    assert_eq!(
        decode_thumb(0xFFFE, 0),
        ThumbInstr::LongBranchWithLinkFromLr { offset: -4 }
    );
    // Any next halfword is consumed without affecting the decode.
    assert_eq!(
        decode_thumb(0xFFFF, 0x1234),
        ThumbInstr::LongBranchWithLinkFromLr { offset: -2 }
    );
}

#[test]
fn illegal_rlist_forms_are_flagged() {
    // Empty register lists.
    assert!(decode_thumb(0xB400, 0).is_unpredictable()); // push {}
    assert!(decode_thumb(0xBC00, 0).is_unpredictable()); // pop {}
    assert!(decode_thumb(0xC000, 0).is_unpredictable()); // stmia r0!, {}
    assert!(decode_thumb(0xC800, 0).is_unpredictable()); // ldmia r0!, {}
    // LDM/STM with the base register in the list.
    assert!(decode_thumb(0xC801, 0).is_unpredictable()); // ldmia r0!, {r0}
    assert!(decode_thumb(0xC001, 0).is_unpredictable()); // stmia r0!, {r0}
    // Well-formed lists are not flagged.
    assert!(!decode_thumb(0xB501, 0).is_unpredictable()); // push {r0, lr}
    assert!(!decode_thumb(0xC002, 0).is_unpredictable()); // stmia r0!, {r1}
    assert!(!decode_thumb(0xC802, 0).is_unpredictable()); // ldmia r0!, {r1}
}

#[test]
fn condition_field_edge_cases() {
    // cond 0b1111 in the 1101 space is SWI, never a conditional branch.
    assert!(matches!(
        decode_thumb(0xDF00, 0),
        ThumbInstr::SoftwareInterrupt { value: 0 }
    ));
    // cond 0b1110 is the permanently undefined space.
    assert!(matches!(
        decode_thumb(0xDE00, 0),
        ThumbInstr::Undefined { encoding: 0xDE00 }
    ));
}

#[test]
fn bl_pair_is_one_instruction() {
    // The pair decodes once; the second halfword is consumed.
    assert_eq!(
        decode_thumb(0xF001, 0xF802),
        ThumbInstr::LongBranchWithLink { offset: 4100 }
    );
    // The same second halfword alone is the standalone form instead.
    assert_eq!(
        decode_thumb(0xF802, 0xF001),
        ThumbInstr::LongBranchWithLinkFromLr { offset: 4 }
    );
}
