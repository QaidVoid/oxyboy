//! Golden disassembly text: one or more snapshots per instruction class.
//!
//! Every expectation is hand-checked against GBATEK/ARM ARM encodings;
//! byte-identity of the text is proven separately by the round-trip
//! harness (`roundtrip.rs`).

use decode::{arm_text, decode_arm, decode_thumb, thumb_text};

#[test]
fn arm_golden_text() {
    // (encoding, expected text), grouped by class.
    let cases: &[(u32, &str)] = &[
        // Data processing: immediate, register, shifted register,
        // register-specified shift, condition suffix, S suffix.
        (0xE3A0_0001, "mov r0, #1"),
        (0x03A0_0001, "moveq r0, #1"),
        (0xE3E0_20FF, "mvn r2, #255"),
        (0xE1A0_0000, "mov r0, r0"),
        (0xE1A0_0021, "mov r0, r1, lsr #32"),
        (0xE1A0_0061, "mov r0, r1, rrx"),
        (0xE084_3655, "add r3, r4, r5, asr r6"),
        (0xE284_3305, "add r3, r4, #335544320"), // rotate 3, value 0x05
        // Multiply, multiply long, swap.
        (0xE037_4392, "mlas r7, r2, r3, r4"),
        (0xE0B5_4796, "umlals r4, r5, r6, r7"),
        (0xE103_1092, "swp r1, r2, [r3]"),
        // PSR transfer.
        (0xE14F_0000, "mrs r0, spsr"),
        (0xE12F_F000, "msr cpsr_fsxc, r0"),
        (0xE361_F010, "msr spsr_c, #16"),
        // Single word/byte transfer: pre, pre+writeback, post, T forms,
        // shifted register offset with polarity.
        (0xE591_0000, "ldr r0, [r1, #0]"),
        (0xE5A1_0004, "str r0, [r1, #4]!"),
        (0xE4D2_0000, "ldrb r0, [r2], #0"),
        (0xE4B1_0004, "ldrt r0, [r1], #4"),
        (0xE421_0004, "strt r0, [r1], #-4"),
        (0xE731_0182, "ldr r0, [r1, -r2, lsl #3]!"),
        // Halfword/signed transfer.
        (0xE1F1_00B4, "ldrh r0, [r1, #4]!"),
        (0xE141_00B4, "strh r0, [r1, #-4]"),
        (0xE091_00F2, "ldrsh r0, [r1], r2"),
        (0xE194_30D5, "ldrsb r3, [r4, r5]"),
        // Block transfer: writeback, user-bank caret, low-high naming.
        (0xE8F0_8002, "ldmia r0!, {r1, pc}^"),
        (0xE9AD_000F, "stmib sp!, {r0, r1, r2, r3}"),
        (0xE8D0_0006, "ldmia r0, {r1, r2}^"),
        // Branch, branch with link, exchange.
        (0xEA00_0000, "b .+8"),
        (0xEB00_0000, "bl .+8"),
        (0xEAFF_FFFF, "b .+4"),
        (0x8AFF_FFFF, "bhi .+4"),
        (0xEBFF_FFFF, "bl .+4"),
        (0xE12F_FF13, "bx r3"),
        // SWI.
        (0xEF12_3456, "swi #0x123456"),
        // Undefined spaces keep raw encodings.
        (0xEE00_0000, ".inst 0xEE000000"),
        (0xFF00_0000, ".inst 0xFF000000"),
        (0xE180_0090, ".inst 0xE1800090"),
    ];
    for (word, expected) in cases {
        assert_eq!(
            arm_text(*word),
            *expected,
            "ARM {word:08X} rendered wrong text"
        );
    }

    // UNPREDICTABLE encodings render their nearest real syntax but are
    // excluded from the round-trip harness.
    let unpredictable: &[(u32, &str)] = &[
        (0xE081_0F12, "add r0, r1, r2, lsl pc"), // Rs = r15: UNPREDICTABLE
        (0xE100_0001, "tst r0, r1"),             // comparison with S = 0
        (0xE120_F000, "msr cpsr, r0"),           // empty field mask
        (0xE8A0_0001, "stmia r0!, {r0}"),        // writeback + base in list
        (0xE0F1_00B2, "ldrh r0, [r1], #2"),      // post-indexed + W bit
    ];
    for (word, expected) in unpredictable {
        assert_eq!(&arm_text(*word), expected, "word {word:08X}");
        assert!(
            decode_arm(*word).is_unpredictable(),
            "{word:08X} should be flagged unpredictable"
        );
    }
}

#[test]
fn thumb_golden_text() {
    // (first halfword, next halfword, expected text), grouped by
    // GBATEK format number.
    let cases: &[(u16, u16, &str)] = &[
        // 1: shifted immediate.
        (0x0008, 0, "lsls r0, r1, #0"),
        (0x00C3, 0, "lsls r3, r0, #3"),
        (0x0848, 0, "lsrs r0, r1, #1"),
        (0x1048, 0, "asrs r0, r1, #1"),
        (0x17C8, 0, "asrs r0, r1, #31"),
        // 2: add/subtract register and 3-bit immediate.
        (0x1808, 0, "adds r0, r1, r0"),
        (0x1FC8, 0, "subs r0, r1, #7"),
        // 3: 8-bit immediate operations.
        (0x2001, 0, "movs r0, #1"),
        (0x2800, 0, "cmp r0, #0"),
        (0x3001, 0, "adds r0, #1"),
        (0x3801, 0, "subs r0, #1"),
        // 4: ALU operations.
        (0x4008, 0, "ands r0, r1"),
        (0x4248, 0, "rsbs r0, r1, #0"),
        (0x4352, 0, "muls r2, r2"),
        (0x4208, 0, "tst r0, r1"),
        (0x43C8, 0, "mvns r0, r1"),
        // 5: hi register operations and BX.
        (0x4480, 0, "add r8, r0"),
        (0x4588, 0, "cmp r8, r1"),
        (0x469C, 0, "mov r12, r3"),
        (0x4718, 0, "bx r3"),
        (0x4770, 0, "bx lr"),
        // 6: PC-relative load.
        (0x4801, 0, "ldr r0, [pc, #4]"),
        // 7/8: register-offset transfers, plain and signed.
        (0x5088, 0, "str r0, [r1, r2]"),
        (0x5C88, 0, "ldrb r0, [r1, r2]"),
        (0x5A88, 0, "ldrh r0, [r1, r2]"),
        (0x5288, 0, "strh r0, [r1, r2]"),
        (0x5688, 0, "ldrsb r0, [r1, r2]"),
        // 9: immediate-offset word/byte transfers.
        (0x6088, 0, "str r0, [r1, #8]"),
        (0x78C8, 0, "ldrb r0, [r1, #3]"),
        // 10: immediate-offset halfword transfers.
        (0x80C8, 0, "strh r0, [r1, #6]"),
        (0x8888, 0, "ldrh r0, [r1, #4]"),
        // 11: SP-relative transfers.
        (0x9001, 0, "str r0, [sp, #4]"),
        (0x9404, 0, "str r4, [sp, #16]"),
        (0x9802, 0, "ldr r0, [sp, #8]"),
        // 12: load address.
        (0xA803, 0, "add r0, sp, #12"),
        (0xA001, 0, "add r0, pc, #4"),
        // 13: add offset to SP.
        (0xB008, 0, "add sp, #32"),
        (0xB088, 0, "sub sp, #32"),
        // 14: push/pop.
        (0xB510, 0, "push {r4, lr}"),
        (0xBD10, 0, "pop {r4, pc}"),
        (0xB410, 0, "push {r4}"),
        (0xB500, 0, "push {lr}"),
        // 15: load/store multiple.
        (0xCB03, 0, "ldmia r3!, {r0, r1}"),
        (0xC303, 0, "stmia r3!, {r0, r1}"),
        // 16-18: branches and SWI.
        (0xD0FF, 0, "beq .+2"),
        (0xD1FD, 0, "bne .-2"),
        (0xE7FE, 0, "b .+0"),
        (0xE003, 0, "b .+10"),
        (0xDF01, 0, "swi #0x01"),
        // 19: long branch with link, and the standalone second half.
        (0xF7FF, 0xFFCE, "bl .-96"),
        (0xF000, 0xF800, "bl .+4"),
        (0xF800, 0, "bl lr+0"),
        // Undefined spaces keep raw encodings.
        (0xDE00, 0, ".2byte 0xDE00"),
        (0xE800, 0, ".2byte 0xE800"),
    ];
    for (first, next, expected) in cases {
        let actual = thumb_text(*first, *next);
        assert_eq!(
            actual, *expected,
            "THUMB {first:04X} (next {next:04X}) rendered wrong text"
        );
    }

    // UNPREDICTABLE encodings: empty rlists without LR/PC and rlists
    // containing the base.
    for (first, next, expected) in [
        (0xB400u16, 0u16, "push {}"),
        (0xC001u16, 0u16, "stmia r0!, {r0}"),
    ] {
        assert_eq!(thumb_text(first, next), expected, "halfword {first:04X}");
        assert!(decode_thumb(first, next).is_unpredictable());
    }
}
