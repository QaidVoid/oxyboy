//! Table-driven tests for the ARM decoder: every enum variant and opcode
//! must be reachable through a known encoding.

use decode::{
    ArmInstr, DataOp, HalfwordKind, HalfwordOffset, PsrSource, Register, RegisterList,
    RegisterShift, ShiftAmount, ShiftKind, ShifterOperand, TransferOffset, decode_arm,
};

/// All 16 data-processing opcodes, decodeable with an immediate operand.
const DATA_OPS: [(u32, DataOp); 16] = [
    (0x0, DataOp::And),
    (0x1, DataOp::Eor),
    (0x2, DataOp::Sub),
    (0x3, DataOp::Rsb),
    (0x4, DataOp::Add),
    (0x5, DataOp::Adc),
    (0x6, DataOp::Sbc),
    (0x7, DataOp::Rsc),
    (0x8, DataOp::Tst),
    (0x9, DataOp::Teq),
    (0xA, DataOp::Cmp),
    (0xB, DataOp::Cmn),
    (0xC, DataOp::Orr),
    (0xD, DataOp::Mov),
    (0xE, DataOp::Bic),
    (0xF, DataOp::Mvn),
];

#[test]
fn data_processing_all_opcodes_immediate() {
    for (code, op) in DATA_OPS {
        // I bit set, S clear, opcode field variable, all registers zero.
        let word = 0xE200_0000 | (code << 21);
        match decode_arm(word) {
            ArmInstr::DataProcessing {
                opcode,
                set_flags,
                rd,
                rn,
                operand2,
            } => {
                assert_eq!(opcode, op);
                assert!(!set_flags);
                assert_eq!(rd, Register::R0);
                assert_eq!(rn, Register::R0);
                assert_eq!(
                    operand2,
                    ShifterOperand::Immediate {
                        rotate: 0,
                        value: 0
                    }
                );
            }
            other => panic!("{word:#010X} decoded as {other:?}"),
        }
    }
}

#[test]
fn data_processing_register_operand_immediate_shift() {
    // add r0, r1, r2, lsl #4
    match decode_arm(0xE081_0202) {
        ArmInstr::DataProcessing {
            opcode,
            operand2:
                ShifterOperand::Register {
                    rm,
                    shift:
                        RegisterShift {
                            kind,
                            amount: ShiftAmount::Immediate(amount),
                        },
                },
            ..
        } => {
            assert_eq!(opcode, DataOp::Add);
            assert_eq!(rm, Register::R2);
            assert_eq!(kind, ShiftKind::Lsl);
            assert_eq!(amount, 4);
        }
        other => panic!("{other:?}"),
    }
    // mov r0, r1, lsr #32 is encoded as lsr #0.
    match decode_arm(0xE1A0_0021) {
        ArmInstr::DataProcessing {
            operand2:
                ShifterOperand::Register {
                    shift:
                        RegisterShift {
                            kind,
                            amount: ShiftAmount::Immediate(amount),
                        },
                    ..
                },
            ..
        } => {
            assert_eq!(kind, ShiftKind::Lsr);
            assert_eq!(amount, 0);
        }
        other => panic!("{other:?}"),
    }
    // mov r0, r0, rrx is encoded as ror #0.
    match decode_arm(0xE1A0_0060) {
        ArmInstr::DataProcessing {
            operand2:
                ShifterOperand::Register {
                    rm,
                    shift:
                        RegisterShift {
                            kind,
                            amount: ShiftAmount::Immediate(amount),
                        },
                },
            ..
        } => {
            assert_eq!(rm, Register::R0);
            assert_eq!(kind, ShiftKind::Rrx);
            assert_eq!(amount, 0);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn data_processing_register_specified_shift_in_misc_space() {
    // add r0, r1, r2, lsl r3
    match decode_arm(0xE081_0312) {
        ArmInstr::DataProcessing {
            opcode,
            operand2:
                ShifterOperand::Register {
                    rm,
                    shift:
                        RegisterShift {
                            kind,
                            amount: ShiftAmount::Register(rs),
                        },
                },
            ..
        } => {
            assert_eq!(opcode, DataOp::Add);
            assert_eq!(rm, Register::R2);
            assert_eq!(kind, ShiftKind::Lsl);
            assert_eq!(rs, Register::R3);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn multiply_and_accumulate() {
    // mul r0, r1, r2 (Rd=0, Rm=1, Rs=2, accumulate operand zero)
    match decode_arm(0xE000_0291) {
        ArmInstr::Multiply {
            accumulate,
            set_flags,
            rd,
            rn,
            rs,
            rm,
        } => {
            assert!(!accumulate);
            assert!(!set_flags);
            assert_eq!(rd, Register::R0);
            assert_eq!(rn, Register::R0);
            assert_eq!(rs, Register::R2);
            assert_eq!(rm, Register::R1);
        }
        other => panic!("{other:?}"),
    }
    // mla r0, r1, r2, r3 (Rd=0, Rn=3, Rm=1, Rs=2)
    match decode_arm(0xE020_3291) {
        ArmInstr::Multiply { accumulate, rn, .. } => {
            assert!(accumulate);
            assert_eq!(rn, Register::R3);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn multiply_long_all_four_forms() {
    // umull r2, r3, r4, r5
    match decode_arm(0xE0C2_3495) {
        ArmInstr::MultiplyLong {
            unsigned,
            accumulate,
            rd_hi,
            rd_lo,
            rn,
            rm,
            ..
        } => {
            assert!(unsigned);
            assert!(!accumulate);
            assert_eq!(rd_hi, Register::R2);
            assert_eq!(rd_lo, Register::R3);
            assert_eq!(rn, Register::R4);
            assert_eq!(rm, Register::R5);
        }
        other => panic!("{other:?}"),
    }
    // umlal: A=1
    assert!(matches!(
        decode_arm(0xE0E2_3495),
        ArmInstr::MultiplyLong {
            unsigned: true,
            accumulate: true,
            ..
        }
    ));
    // smull: U=0
    assert!(matches!(
        decode_arm(0xE082_3495),
        ArmInstr::MultiplyLong {
            unsigned: false,
            accumulate: false,
            ..
        }
    ));
    // smlal: U=0, A=1
    assert!(matches!(
        decode_arm(0xE0A2_3495),
        ArmInstr::MultiplyLong {
            unsigned: false,
            accumulate: true,
            ..
        }
    ));
}

#[test]
fn swap_byte_and_word() {
    // swp r0, r1, [r2]
    match decode_arm(0xE102_0091) {
        ArmInstr::Swap { byte, rd, rn, rm } => {
            assert!(!byte);
            assert_eq!(rd, Register::R0);
            assert_eq!(rn, Register::R2);
            assert_eq!(rm, Register::R1);
        }
        other => panic!("{other:?}"),
    }
    // swpb
    assert!(matches!(
        decode_arm(0xE142_0091),
        ArmInstr::Swap { byte: true, .. }
    ));
}

#[test]
fn psr_read_write() {
    // mrs r0, cpsr
    assert_eq!(
        decode_arm(0xE10F_0000),
        ArmInstr::PsrRead {
            spsr: false,
            rd: Register::R0
        }
    );
    // mrs r1, spsr
    assert_eq!(
        decode_arm(0xE14F_1000),
        ArmInstr::PsrRead {
            spsr: true,
            rd: Register::R1
        }
    );
    // msr cpsr_fsxc, r0 (field mask in bits 19..16)
    match decode_arm(0xE12F_F000) {
        ArmInstr::PsrWrite {
            spsr,
            fields,
            source,
        } => {
            assert!(!spsr);
            assert_eq!(fields, 0xF);
            assert_eq!(source, PsrSource::Register(Register::R0));
        }
        other => panic!("{other:?}"),
    }
    // msr spsr_f, #0x12
    match decode_arm(0xE368_F012) {
        ArmInstr::PsrWrite {
            spsr,
            fields,
            source,
        } => {
            assert!(spsr);
            assert_eq!(fields, 0x8);
            assert_eq!(
                source,
                PsrSource::Immediate {
                    rotate: 0,
                    value: 0x12
                }
            );
        }
        other => panic!("{other:?}"),
    }
    // msr with a partial field mask still decodes.
    assert!(matches!(
        decode_arm(0xE321_F000),
        ArmInstr::PsrWrite { fields: 0x1, .. }
    ));
}

#[test]
fn load_store_immediate_offset() {
    // ldr r0, [r1, #4]
    assert_eq!(
        decode_arm(0xE591_0004),
        ArmInstr::LoadStore {
            load: true,
            byte: false,
            writeback: false,
            up: true,
            pre_index: true,
            rd: Register::R0,
            rn: Register::R1,
            offset: TransferOffset::Immediate(4),
        }
    );
    // strb r2, [r3], #-1 (P=0, U=0, B=1)
    assert_eq!(
        decode_arm(0xE443_2001),
        ArmInstr::LoadStore {
            load: false,
            byte: true,
            writeback: false,
            up: false,
            pre_index: false,
            rd: Register::R2,
            rn: Register::R3,
            offset: TransferOffset::Immediate(1),
        }
    );
}

#[test]
fn load_store_register_offset() {
    // ldr r0, [r1, r2, lsl #2] (I=1 -> bits 27..24 = 0111)
    assert_eq!(
        decode_arm(0xE791_0102),
        ArmInstr::LoadStore {
            load: true,
            byte: false,
            writeback: false,
            up: true,
            pre_index: true,
            rd: Register::R0,
            rn: Register::R1,
            offset: TransferOffset::Register {
                rm: Register::R2,
                shift: RegisterShift {
                    kind: ShiftKind::Lsl,
                    amount: ShiftAmount::Immediate(2),
                },
            },
        }
    );
}

#[test]
fn load_store_halfword_signed() {
    // ldrh r0, [r1, #8] (I=1: immediate offset)
    assert_eq!(
        decode_arm(0xE1D1_00B8),
        ArmInstr::LoadStoreHalfwordSigned {
            kind: HalfwordKind::Ldrh,
            writeback: false,
            up: true,
            pre_index: true,
            rd: Register::R0,
            rn: Register::R1,
            offset: HalfwordOffset::Immediate(8),
        }
    );
    // strh r0, [r1, r2]
    assert_eq!(
        decode_arm(0xE181_00B2),
        ArmInstr::LoadStoreHalfwordSigned {
            kind: HalfwordKind::Strh,
            writeback: false,
            up: true,
            pre_index: true,
            rd: Register::R0,
            rn: Register::R1,
            offset: HalfwordOffset::Register(Register::R2),
        }
    );
    // ldrsb r2, [r5]
    assert_eq!(
        decode_arm(0xE1D5_20D0),
        ArmInstr::LoadStoreHalfwordSigned {
            kind: HalfwordKind::Ldrsb,
            writeback: false,
            up: true,
            pre_index: true,
            rd: Register::R2,
            rn: Register::R5,
            offset: HalfwordOffset::Immediate(0),
        }
    );
    // ldrsh r3, [r5]
    assert_eq!(
        decode_arm(0xE1D5_30F0),
        ArmInstr::LoadStoreHalfwordSigned {
            kind: HalfwordKind::Ldrsh,
            writeback: false,
            up: true,
            pre_index: true,
            rd: Register::R3,
            rn: Register::R5,
            offset: HalfwordOffset::Immediate(0),
        }
    );
    // strd (SH=1x, L=0) is ARMv5TE and decodes as undefined here.
    assert_eq!(
        decode_arm(0xE1C1_00F0),
        ArmInstr::Undefined {
            encoding: 0xE1C1_00F0
        }
    );
}

#[test]
fn block_transfer() {
    // ldmia r0!, {r1, r2}
    assert_eq!(
        decode_arm(0xE8B0_0006),
        ArmInstr::BlockTransfer {
            load: true,
            psr_and_force_user: false,
            writeback: true,
            up: true,
            pre_index: false,
            rn: Register::R0,
            registers: RegisterList::from_bits(0x0006),
        }
    );
}

#[test]
fn branch_and_link() {
    assert_eq!(
        decode_arm(0xEA00_0002),
        ArmInstr::Branch {
            link: false,
            offset: 8
        }
    );
    assert_eq!(
        decode_arm(0xEBFF_FFFE),
        ArmInstr::Branch {
            link: true,
            offset: -8
        }
    );
}

#[test]
fn branch_exchange() {
    assert_eq!(
        decode_arm(0xE12F_FF13),
        ArmInstr::BranchExchange { rn: Register::R3 }
    );
}

#[test]
fn software_interrupt() {
    assert_eq!(
        decode_arm(0xEF12_3456),
        ArmInstr::SoftwareInterrupt { imm24: 0x12_3456 }
    );
}

#[test]
fn reserved_condition_is_undefined() {
    assert_eq!(
        decode_arm(0xF000_0000),
        ArmInstr::Undefined {
            encoding: 0xF000_0000
        }
    );
}

#[test]
fn coprocessor_and_undefined_spaces() {
    // cdp (class 111, bit 24 clear).
    assert_eq!(
        decode_arm(0xEE00_0000),
        ArmInstr::Undefined {
            encoding: 0xEE00_0000
        }
    );
    // mcr (class 111 with bit 4 set).
    assert_eq!(
        decode_arm(0xEE00_0010),
        ArmInstr::Undefined {
            encoding: 0xEE00_0010
        }
    );
    // Undefined instruction space: class 011 with I=1 and bit 4 set.
    assert_eq!(
        decode_arm(0xE7F0_0130),
        ArmInstr::Undefined {
            encoding: 0xE7F0_0130
        }
    );
}

#[test]
fn decode_is_total_and_preserves_encoding() {
    // Deterministic sweep over the 32-bit space; undefined words must
    // carry their own encoding.
    let mut word = 0xE000_0000u32;
    for _ in 0..100_000 {
        if let ArmInstr::Undefined { encoding } = decode_arm(word) {
            assert_eq!(encoding, word);
        }
        word = word.wrapping_mul(0x9E37_79B1).wrapping_add(0x3);
    }
}
