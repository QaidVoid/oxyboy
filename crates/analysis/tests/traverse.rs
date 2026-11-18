//! Golden traversal over a synthetic ROM with a known layout.
//!
//! The ROM (assembled and objdump-verified with Binutils 2.32 from the
//! listing below) exercises every traversal decision: linear ARM, both
//! paths of a conditional branch, PC-relative literal pools, BL with a
//! fallthrough call, BX into THUMB via a pool word with bit 0 set, a
//! THUMB conditional branch, an undefined encoding, and two BX-LR
//! exits recorded as conventional returns.
//!
//! ```text
//!     .arm
//! 00: mov r0, #1
//! 04: ldr r1, [pc, #16]     @ pool at 0x1C
//! 08: bne arm_func          @ target 0x2C, fallthrough 0x0C
//! 0C: ldr r2, [pc, #16]     @ pool at 0x24
//! 10: bx r2                 @ pool word 0x08000041 -> THUMB at 0x40
//! 1C: .word 0x11223344      @ r1 literal
//! 24: .word thumb_start + 1 @ r2 literal
//! 2C: mov r3, #7
//! 30: bl arm_helper         @ target 0x54, fallthrough 0x34
//! 34: mov r4, #8
//! 38: bx lr                 @ unknown target
//! 40: movs r0, #8           @ THUMB
//! 42: cmp r0, #8
//! 44: beq thumb_tail        @ target 0x4C, fallthrough 0x46
//! 46: .hword 0xDE00         @ undefined
//! 4C: movs r1, #9
//! 4E: b 4e                  @ self loop
//! 54: mov r5, #1            @ ARM helper
//! 58: bx lr                 @ unknown target
//! ```

use analysis::{Call, FlagMap, Flags, Run, Seed, State, Stop, StopReason, SwiCall, traverse};
use rom::Image;
use testutil::synthetic::RomBuilder;

const E: Flags = Flags::EXEC;
const W: Flags = Flags::WORD;
const T: Flags = Flags::THUMB;
const L: Flags = Flags::LITERAL;
const F: Flags = Flags::FUNC_START;

const PROGRAM: [u8; 0x5C] = [
    // mov r0,#1; ldr r1,[pc,#16]; bne +0x24; ldr r2,[pc,#16]; bx r2
    0x01, 0x00, 0xA0, 0xE3, 0x10, 0x10, 0x9F, 0xE5, 0x07, 0x00, 0x00, 0x1A, 0x10, 0x20, 0x9F, 0xE5,
    0x12, 0xFF, 0x2F, 0xE1, // pads
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // r1 pool; pad
    0x44, 0x33, 0x22, 0x11, 0x00, 0x00, 0x00, 0x00,
    // r2 pool (thumb at 0x40 | bit 0); pad
    0x41, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00,
    // mov r3,#7; bl 0x54; mov r4,#8; bx lr
    0x07, 0x30, 0xA0, 0xE3, 0x07, 0x00, 0x00, 0xEB, 0x08, 0x40, 0xA0, 0xE3, 0x1E, 0xFF, 0x2F, 0xE1,
    // pad before the THUMB block
    0x00, 0x00, 0x00, 0x00, // movs r0,#8; cmp r0,#8; beq +4; .hword 0xDE00
    0x08, 0x20, 0x08, 0x28, 0x02, 0xD0, 0x00, 0xDE, // pads
    0x00, 0x00, 0x00, 0x00, // movs r1,#9; b self
    0x09, 0x21, 0xFE, 0xE7, // pads
    0x00, 0x00, 0x00, 0x00, // mov r5,#1; bx lr
    0x01, 0x50, 0xA0, 0xE3, 0x1E, 0xFF, 0x2F, 0xE1,
];

fn traversal_rom() -> Image {
    let mut bytes = RomBuilder::new().title("TRAV").build();
    bytes[..PROGRAM.len()].copy_from_slice(&PROGRAM);
    Image::from_bytes(bytes).expect("parse")
}

#[test]
fn golden_layout_matches_the_expected_map() {
    let image = traversal_rom();
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    assert_eq!(
        seeds,
        vec![Seed {
            addr: 0x0800_0000,
            state: State::Arm,
        }]
    );

    let traversal = traverse(&image, &mut map, &seeds);

    // The BL is the only call; the two BX-LR exits end as conventional
    // returns and the undefined THUMB halfword ends the third path.
    assert_eq!(
        traversal.calls,
        vec![Call {
            site: 0x0800_0030,
            state: State::Arm,
            target: 0x0800_0054,
        }]
    );
    assert_eq!(
        traversal.stops,
        vec![
            Stop {
                addr: 0x0800_0058,
                state: State::Arm,
                reason: StopReason::Return,
            },
            Stop {
                addr: 0x0800_0038,
                state: State::Arm,
                reason: StopReason::Return,
            },
            Stop {
                addr: 0x0800_0046,
                state: State::Thumb,
                reason: StopReason::Undefined,
            },
        ]
    );

    let expected: Vec<Run> = [
        (0x00, 0x01, F | E | W),
        (0x01, 0x14, E | W),
        (0x14, 0x1C, Flags::EMPTY),
        (0x1C, 0x20, L | W),
        (0x20, 0x24, Flags::EMPTY),
        (0x24, 0x28, L | W),
        (0x28, 0x2C, Flags::EMPTY),
        (0x2C, 0x3C, E | W),
        (0x3C, 0x40, Flags::EMPTY),
        (0x40, 0x46, E | W | T),
        (0x46, 0x4C, Flags::EMPTY),
        (0x4C, 0x50, E | W | T),
        (0x50, 0x54, Flags::EMPTY),
        (0x54, 0x5C, E | W),
        (0x5C, 0x200, Flags::EMPTY),
    ]
    .iter()
    .map(|&(start, end, flags)| Run {
        addr: 0x0800_0000 + start,
        len: (end - start) as usize,
        flags,
    })
    .collect();
    let actual: Vec<Run> = map.runs().collect();
    assert_eq!(actual, expected, "flag map must match the golden layout");
}

#[test]
fn bx_bit_zero_selects_thumb_state() {
    // The BX at 0x10 reads the pool word 0x08000041: bit 0 set, so the
    // walked target 0x40 is THUMB code. If the state were wrong, the
    // two-byte instructions 0x40..0x46 would not be marked.
    let image = traversal_rom();
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    let _ = traverse(&image, &mut map, &seeds);
    assert_eq!(map.get(0x0800_0040), E | W | T);
    assert_eq!(map.get(0x0800_0044), E | W | T);
    assert_eq!(map.get(0x0800_0046), Flags::EMPTY);
}

#[test]
fn bx_bit_clear_stays_in_arm_state() {
    // Same layout with the pool bit cleared: the target walks as ARM.
    // The word at 0x40 (0x28082008) is a valid STM but the word at 0x44
    // (0xDE00D002) is undefined, so the ARM walk marks 0x40..0x43 and
    // stops at 0x44. The THUMB walk in the golden test instead marks
    // 0x40..0x45 and stops at 0x46: same bytes, different decode, which
    // is the bit-0 state switch made observable.
    let mut bytes = RomBuilder::new().title("TRVA").build();
    bytes[..PROGRAM.len()].copy_from_slice(&PROGRAM);
    bytes[0x24..0x28].copy_from_slice(&0x0800_0040u32.to_le_bytes());
    let image = Image::from_bytes(bytes).expect("parse");
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    let traversal = traverse(&image, &mut map, &seeds);
    assert_eq!(map.get(0x0800_0040), E | W);
    assert_eq!(map.get(0x0800_0043), E | W);
    assert_eq!(map.get(0x0800_0044), Flags::EMPTY);
    assert!(traversal.stops.contains(&Stop {
        addr: 0x0800_0044,
        state: State::Arm,
        reason: StopReason::Undefined,
    }));
}

#[test]
fn adversarial_loops_terminate() {
    // A mov prefix keeps the loader's multiboot heuristic (which reads a
    // leading ARM branch as a multiboot entry) away from the ROM; the
    // b-self loop then ends the only path immediately.
    let mut bytes = RomBuilder::new().title("LOOP").build();
    bytes[0..4].copy_from_slice(&0xE3A0_0001u32.to_le_bytes()); // mov r0, #1
    bytes[4..8].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes()); // b 0x08000004
    let image = Image::from_bytes(bytes).expect("parse");
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    let traversal = traverse(&image, &mut map, &seeds);
    assert!(traversal.stops.is_empty());
    assert_eq!(map.get(0x0800_0000), F | E | W);
    assert_eq!(map.get(0x0800_0004), E | W);
    assert_eq!(map.get(0x0800_0008), Flags::EMPTY);

    // Alternating branches between two instructions.
    let mut bytes = RomBuilder::new().title("PING").build();
    // 0x00: mov r0, #1; 0x04: b 0x08000008; 0x08: b 0x08000004
    bytes[0..12].copy_from_slice(&[
        0x01, 0x00, 0xA0, 0xE3, 0xFF, 0xFF, 0xFF, 0xEA, 0xFD, 0xFF, 0xFF, 0xEA,
    ]);
    let image = Image::from_bytes(bytes).expect("parse");
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    let traversal = traverse(&image, &mut map, &seeds);
    assert!(traversal.stops.is_empty());
    assert_eq!(map.get(0x0800_0000), F | E | W);
    assert_eq!(map.get(0x0800_0004), E | W);
    assert_eq!(map.get(0x0800_0008), E | W);
}

fn branch_into_data_image() -> Image {
    let mut bytes = RomBuilder::new().title("DATA").build();
    bytes[0x00..0x04].copy_from_slice(&0xE59F_0008u32.to_le_bytes()); // ldr r0, [pc, #8] -> 0x10
    bytes[0x04..0x08].copy_from_slice(&0xE12F_FF10u32.to_le_bytes()); // bx r0
    bytes[0x10..0x14].copy_from_slice(&0x0800_0020u32.to_le_bytes()); // -> 0x20
    Image::from_bytes(bytes).expect("parse")
}

#[test]
fn branch_into_data_marks_code_with_evidence() {
    // Entry: ldr r0, [pc, #8]; bx r0 with the pool word pointing into a
    // run of zero bytes. The bytes are only marked because a branch
    // reaches them.
    let image = branch_into_data_image();
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    let _ = traverse(&image, &mut map, &seeds);
    assert_eq!(map.get(0x0800_0010), Flags::LITERAL | Flags::WORD);
    assert_eq!(map.get(0x0800_0020), E | W, "branch evidence marks data");
    assert_eq!(
        map.get(0x0800_0018),
        Flags::EMPTY,
        "untouched bytes stay empty"
    );
}

#[test]
fn out_of_bounds_branch_is_recorded_not_marked() {
    let mut bytes = RomBuilder::new().title("OOB").build();
    // mov r0, #1 (multiboot-heuristic shield), then b 0x08010000, far
    // past the 512-byte image.
    bytes[0..4].copy_from_slice(&0xE3A0_0001u32.to_le_bytes());
    bytes[4..8].copy_from_slice(&0xEA00_3FFDu32.to_le_bytes());
    let image = Image::from_bytes(bytes).expect("parse");
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    let traversal = traverse(&image, &mut map, &seeds);
    assert_eq!(
        traversal.stops,
        vec![Stop {
            addr: 0x0801_0000,
            state: State::Arm,
            reason: StopReason::OutOfBounds,
        }]
    );
    assert_eq!(map.get(0x0801_0000), Flags::EMPTY);
}

#[test]
fn return_like_exits_record_conventional_returns() {
    // A THUMB `push {lr}; pop {r0, pc}` body and an ARM `mov pc, lr`
    // body both end as Return stops: LR is never proven, so these are
    // conventional function exits, not unknown targets.
    let mut bytes = RomBuilder::new().title("RETN").build();
    // 0x40 (THUMB): push {lr}; movs r0, #1; pop {r0, pc}
    bytes[0x40..0x46].copy_from_slice(&[0xB5, 0x00, 0x01, 0x20, 0x01, 0xBD]);
    // 0x20 (ARM): mov pc, lr
    bytes[0x20..0x24].copy_from_slice(&[0x0E, 0xF0, 0xA0, 0xE1]);
    let image = Image::from_bytes(bytes).expect("parse");
    let mut map = FlagMap::from_image(&image);
    let seeds = vec![
        Seed {
            addr: 0x0800_0040,
            state: State::Thumb,
        },
        Seed {
            addr: 0x0800_0020,
            state: State::Arm,
        },
    ];
    let traversal = traverse(&image, &mut map, &seeds);
    assert!(traversal.stops.contains(&Stop {
        addr: 0x0800_0044,
        state: State::Thumb,
        reason: StopReason::Return,
    }));
    assert!(traversal.stops.contains(&Stop {
        addr: 0x0800_0020,
        state: State::Arm,
        reason: StopReason::Return,
    }));
    assert_eq!(map.get(0x0800_0040), E | W | T);
    assert_eq!(map.get(0x0800_0044), E | W | T);
    assert_eq!(map.get(0x0800_0046), Flags::EMPTY);
}

#[test]
fn swi_is_recorded_and_registers_survive_it() {
    // ```text
    // 00: ldr r2, [pc, #8]   @ pool at 0x10 -> 0x08000020
    // 04: swi 0x060000       @ Div; r2 must stay proven
    // 08: mov pc, r2         @ follows the proof: jump to 0x20
    // 10: .word 0x08000020
    // 20: bx lr
    // ```
    let mut bytes = RomBuilder::new().title("SWIK").build();
    bytes[0x00..0x24].copy_from_slice(&[
        0x08, 0x20, 0x9F, 0xE5, 0x00, 0x00, 0x06, 0xEF, 0x02, 0xF0, 0xA0, 0xE1, 0x00, 0x00, 0xA0,
        0xE1, 0x20, 0x00, 0x00, 0x08, 0x00, 0x00, 0xA0, 0xE1, 0x00, 0x00, 0xA0, 0xE1, 0x00, 0x00,
        0xA0, 0xE1, 0x1E, 0xFF, 0x2F, 0xE1,
    ]);
    let image = Image::from_bytes(bytes).expect("parse");
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    let traversal = traverse(&image, &mut map, &seeds);
    // SWI is call-like: recorded with its BIOS number, and the path
    // continued past it to the BX-LR.
    assert_eq!(
        traversal.swi_calls,
        vec![SwiCall {
            site: 0x0800_0004,
            state: State::Arm,
            number: 0x06,
        }]
    );
    assert_eq!(
        traversal.stops,
        vec![Stop {
            addr: 0x0800_0020,
            state: State::Arm,
            reason: StopReason::Return,
        }]
    );
    assert_eq!(map.get(0x0800_0004), E | W);
    assert_eq!(map.get(0x0800_0008), E | W);
    assert_eq!(map.get(0x0800_0020), E | W);
}

#[test]
fn swi_clobbers_r0_r1_r3_and_thumb_number_is_direct() {
    // ARM: `swi 0x060000` returns garbage in r0/r1/r3 (GBATEK), so the
    // r0 pool proof must not survive it: `mov pc, r0` is an unknown
    // target. THUMB: the number is the 8-bit halfword directly.
    let mut bytes = RomBuilder::new().title("SWIX").build();
    // 0x00 ARM: mov r1,#7; ldr r0,[pc,#8]; swi 0x060000; mov pc, r0
    // 0x14 pool: 0x08000020
    bytes[0x00..0x18].copy_from_slice(&[
        0x07, 0x10, 0xA0, 0xE3, 0x08, 0x00, 0x9F, 0xE5, 0x00, 0x00, 0x06, 0xEF, 0x00, 0xF0, 0xA0,
        0xE1, 0x20, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00,
    ]);
    // 0x40 THUMB: swi #0x06; bx r0
    bytes[0x40..0x44].copy_from_slice(&[0x06, 0xDF, 0x00, 0x47]);
    let image = Image::from_bytes(bytes).expect("parse");
    let mut map = FlagMap::from_image(&image);
    let seeds = vec![
        Seed {
            addr: 0x0800_0000,
            state: State::Arm,
        },
        Seed {
            addr: 0x0800_0040,
            state: State::Thumb,
        },
    ];
    let traversal = traverse(&image, &mut map, &seeds);
    let mut swi_calls = traversal.swi_calls;
    swi_calls.sort_by_key(|call| call.site);
    assert_eq!(
        swi_calls,
        vec![
            SwiCall {
                site: 0x0800_0008,
                state: State::Arm,
                number: 0x06,
            },
            SwiCall {
                site: 0x0800_0040,
                state: State::Thumb,
                number: 0x06,
            },
        ]
    );
    assert!(traversal.stops.contains(&Stop {
        addr: 0x0800_000C,
        state: State::Arm,
        reason: StopReason::UnknownTarget,
    }));
    assert!(traversal.stops.contains(&Stop {
        addr: 0x0800_0042,
        state: State::Thumb,
        reason: StopReason::UnknownTarget,
    }));
    // Both SWI sites executed (marked) but did not end their paths.
    assert_eq!(map.get(0x0800_0008), E | W);
    assert_eq!(map.get(0x0800_0040), E | W | T);
    assert_eq!(map.get(0x0800_0042), E | W | T);
}

/// Places little-endian ARM `words` at ROM offset `at` and walks from
/// there in ARM state.
fn walk_arm(title: &str, words: &[(usize, u32)], at: u32) -> (FlagMap, analysis::Traversal) {
    let mut bytes = RomBuilder::new().title(title).build();
    for (offset, word) in words {
        bytes[*offset..*offset + 4].copy_from_slice(&word.to_le_bytes());
    }
    let image = Image::from_bytes(bytes).expect("parse");
    let mut map = FlagMap::from_image(&image);
    let seeds = vec![Seed {
        addr: 0x0800_0000 + at,
        state: State::Arm,
    }];
    let traversal = traverse(&image, &mut map, &seeds);
    (map, traversal)
}

#[test]
fn conditional_pc_writes_fall_through() {
    let (map, _) = walk_arm(
        "CPCW",
        &[
            (0x20, 0x012F_FF1E), // bxeq lr
            (0x24, 0x18BD_8010), // ldmnefd sp!, {r4, pc}
            (0x28, 0xE3A0_0001), // mov r0, #1
            (0x2C, 0xE12F_FF1E), // bx lr
        ],
        0x20,
    );
    for addr in [0x0800_0024, 0x0800_0028, 0x0800_002C] {
        assert_eq!(map.get(addr), E | W, "{addr:#x}");
    }
}

#[test]
fn ldr_pc_literal_veneer_follows_the_target() {
    let (map, traversal) = walk_arm(
        "LDPC",
        &[
            (0x20, 0xE51F_F004), // ldr pc, [pc, #-4]
            (0x24, 0x0800_0060), // .word target
            (0x60, 0xE3A0_0001), // mov r0, #1
            (0x64, 0xE12F_FF1E), // bx lr
        ],
        0x20,
    );
    assert_eq!(map.get(0x0800_0024), L | W, "the pool word is data");
    assert_eq!(map.get(0x0800_0060), E | W, "the veneer target is code");
    assert!(traversal.stops.contains(&Stop {
        addr: 0x0800_0064,
        state: State::Arm,
        reason: StopReason::Return,
    }));
}
