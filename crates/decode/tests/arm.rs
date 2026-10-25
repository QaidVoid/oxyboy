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
    // umull r2, r3, r4, r5 (U = 0 selects unsigned; verified against
    // GNU as encodings)
    match decode_arm(0xE082_3495) {
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
    // umlal: U = 0, A = 1
    assert!(matches!(
        decode_arm(0xE0A2_3495),
        ArmInstr::MultiplyLong {
            unsigned: true,
            accumulate: true,
            ..
        }
    ));
    // smull: U = 1
    assert!(matches!(
        decode_arm(0xE0C2_3495),
        ArmInstr::MultiplyLong {
            unsigned: false,
            accumulate: false,
            ..
        }
    ));
    // smlal: U = 1, A = 1
    assert!(matches!(
        decode_arm(0xE0E2_3495),
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
fn bad_comparison_rd15_stays_data_processing() {
    // TST/TEQ/CMP/CMN with S=1 and Rd=15 decode as ordinary data
    // processing; their ARMv2-era CMPP/TSTP heritage is an execution
    // rule in the CPU (see write_cpsr_control_from_result), not a
    // decode alias. The external ARM suite pins the exact word
    // `cmp pc, pc, r0` (0xE15FF000) as a mode change.
    assert!(matches!(
        decode_arm(0xE15F_F000),
        ArmInstr::DataProcessing {
            opcode: DataOp::Cmp,
            set_flags: true,
            rd: Register::Pc,
            rn: Register::Pc,
            ..
        }
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

#[test]
fn every_opcode_with_every_operand_form() {
    // Three shifter forms per opcode: immediate, register with immediate
    // shift, register with register-specified shift.
    let immediate = |code: u32| 0xE200_0000 | (code << 21);
    let reg_imm_shift = |code: u32| 0xE000_0000 | (code << 21) | (0x202);
    let reg_reg_shift = |code: u32| 0xE000_0000 | (code << 21) | 0x312;

    for (code, op) in DATA_OPS {
        for (word, form) in [
            (immediate(code), "immediate"),
            (reg_imm_shift(code), "register immediate shift"),
            (reg_reg_shift(code), "register specified shift"),
        ] {
            match decode_arm(word) {
                ArmInstr::DataProcessing { opcode, .. } => assert_eq!(opcode, op, "{form}"),
                other => panic!("{word:#010X} ({form}) decoded as {other:?}"),
            }
        }
    }
}

#[test]
fn immediate_rotation_is_captured() {
    // add r0, r1, #0xFF, ROR #16 -> rotate field 8.
    match decode_arm(0xE281_08FF) {
        ArmInstr::DataProcessing {
            operand2: ShifterOperand::Immediate { rotate, value },
            ..
        } => {
            assert_eq!(rotate, 8);
            assert_eq!(value, 0xFF);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn register_shift_captures_each_type() {
    // op2 = r2 shifted by r3; shift type varies over bits 6..5.
    let word = |shift_type: u32| 0xE081_0002 | (0x300) | (shift_type << 5) | 0x10;
    for (shift_type, kind) in [
        (0b00, ShiftKind::Lsl),
        (0b01, ShiftKind::Lsr),
        (0b10, ShiftKind::Asr),
        (0b11, ShiftKind::Ror),
    ] {
        match decode_arm(word(shift_type)) {
            ArmInstr::DataProcessing {
                operand2:
                    ShifterOperand::Register {
                        rm,
                        shift:
                            RegisterShift {
                                kind: decoded,
                                amount: ShiftAmount::Register(rs),
                            },
                    },
                ..
            } => {
                assert_eq!(rm, Register::R2);
                assert_eq!(decoded, kind);
                assert_eq!(rs, Register::R3);
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn immediate_shift_captures_each_type() {
    // op2 = r2 shifted by an immediate; shift type varies over bits 6..5.
    let word = |shift_type: u32, amount: u32| 0xE081_0002 | (amount << 7) | (shift_type << 5);
    for (shift_type, kind) in [
        (0b00, ShiftKind::Lsl),
        (0b01, ShiftKind::Lsr),
        (0b10, ShiftKind::Asr),
    ] {
        match decode_arm(word(shift_type, 4)) {
            ArmInstr::DataProcessing {
                operand2:
                    ShifterOperand::Register {
                        shift:
                            RegisterShift {
                                kind: decoded,
                                amount: ShiftAmount::Immediate(amount),
                            },
                        ..
                    },
                ..
            } => {
                assert_eq!(decoded, kind);
                assert_eq!(amount, 4);
            }
            other => panic!("{other:?}"),
        }
    }
    // ROR with nonzero immediate decodes as Ror; ROR with zero immediate
    // is RRX.
    assert!(matches!(
        decode_arm(word(0b11, 5)),
        ArmInstr::DataProcessing {
            operand2: ShifterOperand::Register {
                shift: RegisterShift {
                    kind: ShiftKind::Ror,
                    ..
                },
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        decode_arm(word(0b11, 0)),
        ArmInstr::DataProcessing {
            operand2: ShifterOperand::Register {
                shift: RegisterShift {
                    kind: ShiftKind::Rrx,
                    ..
                },
                ..
            },
            ..
        }
    ));
}

#[test]
fn s_bit_is_captured() {
    // adds vs add: bit 20 only.
    for (word, set_flags) in [(0xE081_0202u32, false), (0xE091_0202, true)] {
        match decode_arm(word) {
            ArmInstr::DataProcessing {
                set_flags: flags, ..
            } => {
                assert_eq!(flags, set_flags);
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn r15_registers_are_recorded_as_pc() {
    // add pc, pc, pc, lsl pc is legal to encode: Rn = Rd = Rm = R15 read
    // as PC, and the shifter is unpredictable because the shift amount
    // register is R15.
    let word = 0xE08F_FF1F;
    match decode_arm(word) {
        ArmInstr::DataProcessing {
            rd,
            rn,
            operand2:
                ShifterOperand::Register {
                    rm,
                    shift:
                        RegisterShift {
                            amount: ShiftAmount::Register(rs),
                            ..
                        },
                },
            ..
        } => {
            assert_eq!(rd, Register::Pc);
            assert_eq!(rn, Register::Pc);
            assert_eq!(rm, Register::Pc);
            assert_eq!(rs, Register::Pc);
            assert!(decode_arm(word).is_unpredictable());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn r15_as_shift_amount_is_flagged_but_r15_as_operand_is_not() {
    // mov r0, pc, lsl r1: Rm = R15 is legal (reads PC + 8), shift amount
    // register is R1.
    let legal = 0xE1A0_011F;
    assert!(!decode_arm(legal).is_unpredictable());

    // mov r0, r1, lsl pc: shift amount register is R15, UNPREDICTABLE.
    let unpredictable = 0xE1A0_0F11;
    assert!(decode_arm(unpredictable).is_unpredictable());

    // Unpredictability does not leak into unrelated variants.
    assert!(!decode_arm(0xE591_0004).is_unpredictable());
    assert!(!decode_arm(0xEA00_0002).is_unpredictable());
}

#[test]
fn single_transfer_all_addressing_modes() {
    // LDR/STR/LDRB/STRB over every P/U/B/W/L combination, with both the
    // immediate and the register offset form.
    for pre_index in [false, true] {
        for up in [false, true] {
            for byte in [false, true] {
                for writeback in [false, true] {
                    for load in [false, true] {
                        let flags = u32::from(pre_index) << 24
                            | u32::from(up) << 23
                            | u32::from(byte) << 22
                            | u32::from(writeback) << 21
                            | u32::from(load) << 20;
                        let base = 0xE000_0000 | 0x0400_0000 | flags | (0x5 << 16) | (0x3 << 12);

                        let imm_word = base | 0x42;
                        assert_eq!(
                            decode_arm(imm_word),
                            ArmInstr::LoadStore {
                                load,
                                byte,
                                writeback,
                                up,
                                pre_index,
                                rd: Register::R3,
                                rn: Register::R5,
                                offset: TransferOffset::Immediate(0x42),
                            },
                            "P={pre_index} U={up} B={byte} W={writeback} L={load} imm"
                        );

                        // Register offset: class 011 with bit 4 clear and
                        // an LSL #2 shift on R2.
                        let reg_word = base | 0x0600_0000 | (2 << 7) | 0x2;
                        assert_eq!(
                            decode_arm(reg_word),
                            ArmInstr::LoadStore {
                                load,
                                byte,
                                writeback,
                                up,
                                pre_index,
                                rd: Register::R3,
                                rn: Register::R5,
                                offset: TransferOffset::Register {
                                    rm: Register::R2,
                                    shift: RegisterShift {
                                        kind: ShiftKind::Lsl,
                                        amount: ShiftAmount::Immediate(2),
                                    },
                                },
                            },
                            "P={pre_index} U={up} B={byte} W={writeback} L={load} reg"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn post_indexed_user_mode_t_form_is_defined() {
    // P=0 with bit 21 set is LDRT/STRT/LDRBT/STRBT: defined on ARMv4T
    // (GBATEK "Single Data Transfer": bit 21 = T when P=0) and decodes
    // with writeback set, but is not flagged unpredictable.
    let word = 0xE6F5_2001; // ldrbt r2, [r5], #-1 (class 011, P=0, U, B, T, L)
    match decode_arm(word) {
        ArmInstr::LoadStore {
            load,
            byte,
            writeback,
            pre_index,
            ..
        } => {
            assert!(load);
            assert!(byte);
            assert!(writeback);
            assert!(!pre_index);
            assert!(!decode_arm(word).is_unpredictable());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn halfword_post_indexing_with_writeback_is_flagged() {
    // P=0 with W=1 in the halfword space: bit 21 must be zero
    // (GBATEK); decodable but flagged unpredictable.
    let word = 0xE0B1_00B2; // strh with P=0, U=1, W=1
    match decode_arm(word) {
        ArmInstr::LoadStoreHalfwordSigned {
            pre_index,
            writeback,
            ..
        } => {
            assert!(!pre_index);
            assert!(writeback);
            assert!(decode_arm(word).is_unpredictable());
        }
        other => panic!("{other:?}"),
    }
    // Every other P/W combination is fine.
    for (pre_index, writeback) in [(false, false), (true, false), (true, true)] {
        let flags = u32::from(pre_index) << 24 | u32::from(writeback) << 21;
        let word = 0xE180_0000 | 0x0080_0000 | flags | 0xB2;
        assert!(
            !decode_arm(word).is_unpredictable(),
            "P={pre_index} W={writeback}"
        );
    }
}

#[test]
fn halfword_sh_zero_overlap_resolves_to_swap_or_undefined() {
    // SH = 00 with L = 0 is where SWP lives (GBATEK halfword section:
    // "Reserved for SWP instruction"). Words with the full SWP bit
    // pattern decode as Swap; everything else is undefined.
    // swp r0, r1, [r2]
    assert!(matches!(
        decode_arm(0xE102_0091),
        ArmInstr::Swap { byte: false, .. }
    ));
    // Same shape but with L set: SWP has no load form.
    assert_eq!(
        decode_arm(0xE112_0091),
        ArmInstr::Undefined {
            encoding: 0xE112_0091
        }
    );
    // SH = 00 without the SWP bits 11..4 pattern.
    assert_eq!(
        decode_arm(0xE181_0090),
        ArmInstr::Undefined {
            encoding: 0xE181_0090
        }
    );
}

#[test]
fn block_transfer_all_modes() {
    // LDM/STM over every P/U/S/W/L combination.
    for pre_index in [false, true] {
        for up in [false, true] {
            for psr in [false, true] {
                for writeback in [false, true] {
                    for load in [false, true] {
                        let flags = u32::from(pre_index) << 24
                            | u32::from(up) << 23
                            | u32::from(psr) << 22
                            | u32::from(writeback) << 21
                            | u32::from(load) << 20;
                        let word = 0xE000_0000 | 0x0800_0000 | flags | (0x7 << 16) | 0x0082; // {r1, r7}: includes the base R7
                        match decode_arm(word) {
                            ArmInstr::BlockTransfer {
                                load: l,
                                psr_and_force_user: s,
                                writeback: w,
                                up: u,
                                pre_index: p,
                                rn,
                                registers,
                            } => {
                                assert_eq!(l, load);
                                assert_eq!(s, psr);
                                assert_eq!(w, writeback);
                                assert_eq!(u, up);
                                assert_eq!(p, pre_index);
                                assert_eq!(rn, Register::R7);
                                assert_eq!(registers, RegisterList::from_bits(0x0082));
                            }
                            other => panic!("{word:#010X} decoded as {other:?}"),
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn block_transfer_base_in_list_with_writeback_is_flagged() {
    // ldmia r7!, {r1, r7}: base in list plus writeback.
    let word = 0xE8B7_0082;
    assert!(decode_arm(word).is_unpredictable());
    // Without writeback the combination is not flagged.
    assert!(!decode_arm(0xE897_0082).is_unpredictable());
    // Base not in the list is fine even with writeback.
    assert!(!decode_arm(0xE8B7_0002).is_unpredictable());
}

#[test]
fn swap_all_forms() {
    for (byte, word) in [(false, 0xE102_0091), (true, 0xE142_0091)] {
        match decode_arm(word) {
            ArmInstr::Swap {
                byte: b,
                rd,
                rn,
                rm,
            } => {
                assert_eq!(b, byte);
                assert_eq!(rd, Register::R0);
                assert_eq!(rn, Register::R2);
                assert_eq!(rm, Register::R1);
            }
            other => panic!("{word:#010X} decoded as {other:?}"),
        }
    }
}

#[test]
fn bx_mask_fields_are_enforced() {
    // bx r3 under a condition (bits 11..4 must be 11110001).
    assert_eq!(
        decode_arm(0x012F_FF13),
        ArmInstr::BranchExchange { rn: Register::R3 }
    );
    // Same space but bits 11..4 != 11110001 falls through to data
    // processing rather than decoding as BX.
    assert!(matches!(
        decode_arm(0x012F_FF20),
        ArmInstr::DataProcessing { .. }
    ));
}

#[test]
fn psr_forms_and_swi_cover_s04_classes() {
    // MRS/MSR immediate and register forms (both R settings).
    assert!(matches!(
        decode_arm(0xE10F_0000),
        ArmInstr::PsrRead { spsr: false, .. }
    ));
    assert!(matches!(
        decode_arm(0xE14F_0000),
        ArmInstr::PsrRead { spsr: true, .. }
    ));
    assert!(matches!(
        decode_arm(0xE129_F000),
        ArmInstr::PsrWrite {
            source: PsrSource::Register(_),
            ..
        }
    ));
    assert!(matches!(
        decode_arm(0xE32F_F000),
        ArmInstr::PsrWrite {
            source: PsrSource::Immediate { .. },
            ..
        }
    ));
    // MUL/MLA/SWP/B/BL/SWI decode (fields asserted in dedicated tests).
    assert!(matches!(decode_arm(0xE000_0291), ArmInstr::Multiply { .. }));
    assert!(matches!(decode_arm(0xE020_3291), ArmInstr::Multiply { .. }));
    assert!(matches!(decode_arm(0xE102_0091), ArmInstr::Swap { .. }));
    assert!(matches!(decode_arm(0xEA00_0002), ArmInstr::Branch { .. }));
    assert!(matches!(decode_arm(0xEB00_0002), ArmInstr::Branch { .. }));
    assert!(matches!(
        decode_arm(0xEF00_0000),
        ArmInstr::SoftwareInterrupt { .. }
    ));
}
