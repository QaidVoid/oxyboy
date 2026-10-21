//! Table-driven tests for the THUMB decoder: every enum variant and
//! opcode must be reachable through a known encoding.

use decode::{
    Condition, Register, ThumbAluOp, ThumbHiOp, ThumbImmOp, ThumbInstr, ThumbMemOp, ThumbShift,
    ThumbSignedMemOp, decode_thumb,
};

/// All 16 format 4 ALU operations.
const ALU_OPS: [(u16, ThumbAluOp); 16] = [
    (0x0, ThumbAluOp::And),
    (0x1, ThumbAluOp::Eor),
    (0x2, ThumbAluOp::Lsl),
    (0x3, ThumbAluOp::Lsr),
    (0x4, ThumbAluOp::Asr),
    (0x5, ThumbAluOp::Adc),
    (0x6, ThumbAluOp::Sbc),
    (0x7, ThumbAluOp::Ror),
    (0x8, ThumbAluOp::Tst),
    (0x9, ThumbAluOp::Neg),
    (0xA, ThumbAluOp::Cmp),
    (0xB, ThumbAluOp::Cmn),
    (0xC, ThumbAluOp::Orr),
    (0xD, ThumbAluOp::Mul),
    (0xE, ThumbAluOp::Bic),
    (0xF, ThumbAluOp::Mvn),
];

/// All four format 3 immediate operations.
const IMM_OPS: [(u16, ThumbImmOp); 4] = [
    (0x0, ThumbImmOp::Mov),
    (0x1, ThumbImmOp::Cmp),
    (0x2, ThumbImmOp::Add),
    (0x3, ThumbImmOp::Sub),
];

/// All four format 5 Hi register operations.
const HI_OPS: [(u16, ThumbHiOp); 4] = [
    (0x0, ThumbHiOp::Add),
    (0x1, ThumbHiOp::Cmp),
    (0x2, ThumbHiOp::Mov),
    (0x3, ThumbHiOp::Bx),
];

/// Format 7 transfer kinds (STR, STRB, LDR, LDRB order).
const FORMAT7_MEM_OPS: [(u16, ThumbMemOp); 4] = [
    (0x0, ThumbMemOp::Str),
    (0x1, ThumbMemOp::Strb),
    (0x2, ThumbMemOp::Ldr),
    (0x3, ThumbMemOp::Ldrb),
];

/// Format 9 transfer kinds (STR, LDR, STRB, LDRB order).
const FORMAT9_MEM_OPS: [(u16, ThumbMemOp); 4] = [
    (0x0, ThumbMemOp::Str),
    (0x1, ThumbMemOp::Ldr),
    (0x2, ThumbMemOp::Strb),
    (0x3, ThumbMemOp::Ldrb),
];

/// Format 8 transfer kinds.
const SIGNED_MEM_OPS: [(u16, ThumbSignedMemOp); 4] = [
    (0x0, ThumbSignedMemOp::Strh),
    (0x1, ThumbSignedMemOp::Ldsb),
    (0x2, ThumbSignedMemOp::Ldrh),
    (0x3, ThumbSignedMemOp::Ldsh),
];

/// The three format 1 shift kinds.
const SHIFT_KINDS: [(u16, ThumbShift); 3] = [
    (0x0, ThumbShift::Lsl),
    (0x1, ThumbShift::Lsr),
    (0x2, ThumbShift::Asr),
];

/// All 14 branch conditions.
const CONDITIONS: [Condition; 14] = [
    Condition::Eq,
    Condition::Ne,
    Condition::Cs,
    Condition::Cc,
    Condition::Mi,
    Condition::Pl,
    Condition::Vs,
    Condition::Vc,
    Condition::Hi,
    Condition::Ls,
    Condition::Ge,
    Condition::Lt,
    Condition::Gt,
    Condition::Le,
];

#[test]
fn shifted_immediate_all_kinds() {
    for (op, kind) in SHIFT_KINDS {
        // lsl r0, r1, #2 with a variable shift kind.
        let halfword = (op << 11) | (2 << 6) | (1 << 3);
        match decode_thumb(halfword, 0) {
            ThumbInstr::ShiftedImmediate {
                kind: decoded,
                rd,
                rm,
                imm5,
            } => {
                assert_eq!(decoded, kind);
                assert_eq!(rd, Register::R0);
                assert_eq!(rm, Register::R1);
                assert_eq!(imm5, 2);
            }
            other => panic!("{halfword:#06X} decoded as {other:?}"),
        }
    }
    // lsr with imm5 = 0 encodes lsr #32 and is kept raw.
    assert!(matches!(
        decode_thumb(0x0800, 0),
        ThumbInstr::ShiftedImmediate {
            kind: ThumbShift::Lsr,
            imm5: 0,
            ..
        }
    ));
}

#[test]
fn add_subtract_register_and_immediate() {
    // adds r0, r1, r2
    match decode_thumb(0x1888, 0) {
        ThumbInstr::AddSubtract {
            subtract,
            immediate,
            rn_or_imm3,
            rs,
            rd,
        } => {
            assert!(!subtract);
            assert!(!immediate);
            assert_eq!(rn_or_imm3, 2);
            assert_eq!(rs, Register::R1);
            assert_eq!(rd, Register::R0);
        }
        other => panic!("{other:?}"),
    }
    // subs r0, r1, #3
    match decode_thumb(0x1EC8, 0) {
        ThumbInstr::AddSubtract {
            subtract,
            immediate,
            rn_or_imm3,
            rs,
            rd,
        } => {
            assert!(subtract);
            assert!(immediate);
            assert_eq!(rn_or_imm3, 3);
            assert_eq!(rs, Register::R1);
            assert_eq!(rd, Register::R0);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn immediate_operation_all_opcodes() {
    for (op, expected) in IMM_OPS {
        // mov r1, #2 with a variable opcode.
        let halfword = 0x2000 | (op << 11) | (1 << 8) | 2;
        match decode_thumb(halfword, 0) {
            ThumbInstr::ImmediateOperation { op, rd, imm8 } => {
                assert_eq!(op, expected);
                assert_eq!(rd, Register::R1);
                assert_eq!(imm8, 2);
            }
            other => panic!("{halfword:#06X} decoded as {other:?}"),
        }
    }
}

#[test]
fn alu_operation_all_opcodes() {
    for (code, expected) in ALU_OPS {
        // and r0, r1 with a variable opcode.
        let halfword = 0x4000 | (code << 6) | (1 << 3);
        match decode_thumb(halfword, 0) {
            ThumbInstr::AluOperation { op, rd, rs } => {
                assert_eq!(op, expected);
                assert_eq!(rd, Register::R0);
                assert_eq!(rs, Register::R1);
            }
            other => panic!("{halfword:#06X} decoded as {other:?}"),
        }
    }
}

#[test]
fn high_register_operation_all_opcodes() {
    // add r0, r8
    assert_eq!(
        decode_thumb(0x4440, 0),
        ThumbInstr::HighRegisterOperation {
            op: ThumbHiOp::Add,
            rd: Register::R0,
            rs: Register::R8,
        }
    );
    // cmp r8, r1 (op=01, H1=1)
    assert_eq!(
        decode_thumb(0x4588, 0),
        ThumbInstr::HighRegisterOperation {
            op: ThumbHiOp::Cmp,
            rd: Register::R8,
            rs: Register::R1,
        }
    );
    // mov r9, r2 (op=10, H1=1)
    assert_eq!(
        decode_thumb(0x4691, 0),
        ThumbInstr::HighRegisterOperation {
            op: ThumbHiOp::Mov,
            rd: Register::R9,
            rs: Register::R2,
        }
    );
    // mov r8, r8 is the canonical NOP.
    assert_eq!(
        decode_thumb(0x46C0, 0),
        ThumbInstr::HighRegisterOperation {
            op: ThumbHiOp::Mov,
            rd: Register::R8,
            rs: Register::R8,
        }
    );
    // bx r3
    assert_eq!(
        decode_thumb(0x4718, 0),
        ThumbInstr::HighRegisterOperation {
            op: ThumbHiOp::Bx,
            rd: Register::R0,
            rs: Register::R3,
        }
    );
    for (code, expected) in HI_OPS {
        // ADD/CMP/MOV r0, r9 and BX r9: H2 set keeps format 5 defined.
        let halfword = 0x4400 | (code << 8) | 0x40 | 0x08;
        assert!(matches!(
            decode_thumb(halfword, 0),
            ThumbInstr::HighRegisterOperation { op, .. } if op == expected
        ));
    }
}

#[test]
fn high_register_undefined_low_low_and_bx_h1() {
    // add r0, r1 with no H bits set aliases format 4 and is undefined.
    assert_eq!(
        decode_thumb(0x4408, 0),
        ThumbInstr::Undefined { encoding: 0x4408 }
    );
    // bx with MSBd set is undefined (no BLX register on ARMv4T).
    assert_eq!(
        decode_thumb(0x4788, 0),
        ThumbInstr::Undefined { encoding: 0x4788 }
    );
}

#[test]
fn pc_relative_load() {
    assert_eq!(
        decode_thumb(0x4801, 0),
        ThumbInstr::PcRelativeLoad {
            rd: Register::R0,
            offset: 1
        }
    );
}

#[test]
fn load_store_register_offset_all_kinds() {
    for (code, expected) in FORMAT7_MEM_OPS {
        // str r0, [r1, r2] with a variable kind.
        let halfword = 0x5000 | (code << 10) | (2 << 6) | (1 << 3);
        match decode_thumb(halfword, 0) {
            ThumbInstr::LoadStoreRegisterOffset { kind, rd, rb, ro } => {
                assert_eq!(kind, expected);
                assert_eq!(rd, Register::R0);
                assert_eq!(rb, Register::R1);
                assert_eq!(ro, Register::R2);
            }
            other => panic!("{halfword:#06X} decoded as {other:?}"),
        }
    }
}

#[test]
fn load_store_signed_register_offset_all_kinds() {
    for (code, expected) in SIGNED_MEM_OPS {
        // strh r0, [r1, r2] with a variable kind.
        let halfword = 0x5000 | (code << 10) | 0x200 | (2 << 6) | (1 << 3);
        match decode_thumb(halfword, 0) {
            ThumbInstr::LoadStoreSignedRegisterOffset { kind, rd, rb, ro } => {
                assert_eq!(kind, expected);
                assert_eq!(rd, Register::R0);
                assert_eq!(rb, Register::R1);
                assert_eq!(ro, Register::R2);
            }
            other => panic!("{halfword:#06X} decoded as {other:?}"),
        }
    }
}

#[test]
fn load_store_immediate_offset_all_kinds() {
    for (code, expected) in FORMAT9_MEM_OPS {
        // str r0, [r1, #4] with a variable kind.
        let halfword = 0x6000 | (code << 11) | (4 << 6) | (1 << 3);
        match decode_thumb(halfword, 0) {
            ThumbInstr::LoadStoreImmediateOffset {
                kind,
                rd,
                rb,
                offset,
            } => {
                assert_eq!(kind, expected);
                assert_eq!(rd, Register::R0);
                assert_eq!(rb, Register::R1);
                assert_eq!(offset, 4);
            }
            other => panic!("{halfword:#06X} decoded as {other:?}"),
        }
    }
}

#[test]
fn load_store_halfword_immediate() {
    // strh r0, [r1, #2]
    assert_eq!(
        decode_thumb(0x8088, 0),
        ThumbInstr::LoadStoreHalfwordImmediate {
            load: false,
            rd: Register::R0,
            rb: Register::R1,
            offset: 2
        }
    );
    // ldrh r2, [r3, #4]
    assert_eq!(
        decode_thumb(0x891A, 0),
        ThumbInstr::LoadStoreHalfwordImmediate {
            load: true,
            rd: Register::R2,
            rb: Register::R3,
            offset: 4
        }
    );
}

#[test]
fn load_store_stack() {
    // str r0, [sp, #4]
    assert_eq!(
        decode_thumb(0x9001, 0),
        ThumbInstr::LoadStoreStack {
            load: false,
            rd: Register::R0,
            offset: 1
        }
    );
    // ldr r0, [sp, #4]
    assert!(matches!(
        decode_thumb(0x9801, 0),
        ThumbInstr::LoadStoreStack { load: true, .. }
    ));
}

#[test]
fn load_address() {
    // add r0, pc, #4
    assert_eq!(
        decode_thumb(0xA001, 0),
        ThumbInstr::LoadAddress {
            to_sp: false,
            rd: Register::R0,
            offset: 1
        }
    );
    // add r0, sp, #8
    assert_eq!(
        decode_thumb(0xA802, 0),
        ThumbInstr::LoadAddress {
            to_sp: true,
            rd: Register::R0,
            offset: 2
        }
    );
}

#[test]
fn adjust_stack_pointer() {
    // add sp, #16
    assert_eq!(
        decode_thumb(0xB004, 0),
        ThumbInstr::AdjustStackPointer {
            subtract: false,
            offset: 4
        }
    );
    // sub sp, #8
    assert_eq!(
        decode_thumb(0xB082, 0),
        ThumbInstr::AdjustStackPointer {
            subtract: true,
            offset: 2
        }
    );
}

#[test]
fn push_pop() {
    // push {r0, lr}
    assert_eq!(
        decode_thumb(0xB501, 0),
        ThumbInstr::PushPop {
            pop: false,
            include_pc_or_lr: true,
            registers: 0x01
        }
    );
    // pop {r1, pc}
    assert_eq!(
        decode_thumb(0xBD02, 0),
        ThumbInstr::PushPop {
            pop: true,
            include_pc_or_lr: true,
            registers: 0x02
        }
    );
    // push {r4} without LR.
    assert!(matches!(
        decode_thumb(0xB410, 0),
        ThumbInstr::PushPop {
            pop: false,
            include_pc_or_lr: false,
            ..
        }
    ));
}

#[test]
fn load_store_multiple() {
    // stmia r0!, {r1}
    assert_eq!(
        decode_thumb(0xC002, 0),
        ThumbInstr::LoadStoreMultiple {
            load: false,
            rb: Register::R0,
            registers: 0x02
        }
    );
    // ldmia r1!, {r0}
    assert_eq!(
        decode_thumb(0xC901, 0),
        ThumbInstr::LoadStoreMultiple {
            load: true,
            rb: Register::R1,
            registers: 0x01
        }
    );
}

#[test]
fn conditional_branch_all_conditions() {
    for (index, cond) in CONDITIONS.iter().enumerate() {
        // b<cond> +2 with a variable condition.
        let halfword = 0xD000 | ((index as u16) << 8) | 1;
        assert_eq!(
            decode_thumb(halfword, 0),
            ThumbInstr::ConditionalBranch {
                cond: *cond,
                offset: 2
            }
        );
    }
    // beq -4
    assert_eq!(
        decode_thumb(0xD0FE, 0),
        ThumbInstr::ConditionalBranch {
            cond: Condition::Eq,
            offset: -4
        }
    );
}

#[test]
fn software_interrupt() {
    assert_eq!(
        decode_thumb(0xDF11, 0),
        ThumbInstr::SoftwareInterrupt { value: 0x11 }
    );
}

#[test]
fn unconditional_branch() {
    assert_eq!(
        decode_thumb(0xE002, 0),
        ThumbInstr::UnconditionalBranch { offset: 4 }
    );
    assert_eq!(
        decode_thumb(0xE7FE, 0),
        ThumbInstr::UnconditionalBranch { offset: -4 }
    );
}

#[test]
fn long_branch_with_link() {
    // bl first=(hi=1), second=(lo=2): offset = (1 << 12) | (2 << 1).
    assert_eq!(
        decode_thumb(0xF001, 0xF802),
        ThumbInstr::LongBranchWithLink { offset: 4100 }
    );
    // Negative offset: hi has bit 10 set.
    let first = 0xF400; // hi11 = 0x400 sets bit 22 of the pair.
    let next = 0xF800;
    match decode_thumb(first, next) {
        ThumbInstr::LongBranchWithLink { offset } => assert!(offset < 0),
        other => panic!("{other:?}"),
    }
}

#[test]
fn undefined_spaces() {
    // Condition 0b1110 in the 1101 space.
    assert_eq!(
        decode_thumb(0xDE00, 0),
        ThumbInstr::Undefined { encoding: 0xDE00 }
    );
    // hw>>11 == 11101.
    assert_eq!(
        decode_thumb(0xE800, 0),
        ThumbInstr::Undefined { encoding: 0xE800 }
    );
    // Standalone second half of BL (hw>>11 == 11111).
    assert_eq!(
        decode_thumb(0xF800, 0),
        ThumbInstr::Undefined { encoding: 0xF800 }
    );
    // First half of BL without a valid second half.
    assert_eq!(
        decode_thumb(0xF000, 0x4770),
        ThumbInstr::Undefined { encoding: 0xF000 }
    );
    // First half with a BLX (ARM9) second half is undefined on ARMv4T.
    assert_eq!(
        decode_thumb(0xF000, 0xD800),
        ThumbInstr::Undefined { encoding: 0xF000 }
    );
    // 1011 space outside ADD SP and PUSH/POP.
    assert_eq!(
        decode_thumb(0xB600, 0),
        ThumbInstr::Undefined { encoding: 0xB600 }
    );
}

#[test]
fn decode_is_total_and_preserves_encoding() {
    // Every 16-bit halfword decodes to something; undefined words carry
    // their own encoding. 0 is a valid next halfword, so BL pairs decode
    // as Undefined here, which is fine.
    for halfword in 0..=u16::MAX {
        if let ThumbInstr::Undefined { encoding } = decode_thumb(halfword, 0) {
            assert_eq!(encoding, halfword);
        }
    }
}
