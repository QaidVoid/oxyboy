//! The ARM (32-bit) instruction model and decoder.
//!
//! Bit layouts follow GBATEK "ARM Opcodes" (which match ARM ARM section
//! A4.1); each dispatch cites the GBATEK section it implements.

use crate::condition::Condition;
use crate::register::{Register, RegisterList};
use crate::{bits, sign_extend};

/// Decodes one ARM instruction word.
///
/// Total over the input space: words the ARM7TDMI would not execute
/// (reserved condition `0b1111`, coprocessor space, undefined-instruction
/// spaces) decode to [`ArmInstr::Undefined`].
#[must_use]
pub fn decode_arm(word: u32) -> ArmInstr {
    // Condition 0b1111 is reserved on ARMv4T (ARM ARM A3.3); the ARM7TDMI
    // TRM marks it UNPREDICTABLE.
    let Some(_cond) = Condition::from_bits(bits(word, 31, 28) as u8) else {
        return ArmInstr::Undefined { encoding: word };
    };

    match bits(word, 27, 25) {
        0b000 => decode_class_000(word),
        0b001 => decode_class_001(word),
        0b010 | 0b011 => decode_single_data_transfer(word),
        0b100 => decode_block_transfer(word),
        0b101 => decode_branch(word),
        0b110 => ArmInstr::Undefined { encoding: word },
        _ => decode_class_111(word),
    }
}

/// Class `000` (bit 25 clear), routed by bits 7..4 (GBATEK ALU,
/// Multiply, Halfword Signed Transfer, PSR Transfer, and Branch Exchange
/// sections).
fn decode_class_000(word: u32) -> ArmInstr {
    match bits(word, 7, 4) {
        // Multiply and swap space.
        0b1001 => decode_multiply_space(word),
        // Halfword and signed transfer space (SH bits 6..5 nonzero).
        0b1011 | 0b1101 | 0b1111 => decode_halfword_transfer(word),
        // Misc space: BX shares it with data processing register-
        // specified shifts (GBATEK "Branch and Exchange").
        0b0001 => {
            if (word & 0x0FFF_FFF0) == 0x012F_FF10 {
                ArmInstr::BranchExchange {
                    rn: Register::from_bits(bits(word, 3, 0) as u8),
                }
            } else {
                decode_data_processing(word)
            }
        }
        // Bit 4 clear: MRS and MSR register occupy the TST/TEQ/CMP/CMN
        // with S=0 area of data processing (GBATEK "PSR Transfer": PSR
        // transfers "occupy an unused area ... of ALU opcodes").
        _ => {
            if (word & 0x0FBF_0FFF) == 0x010F_0000 {
                ArmInstr::PsrRead {
                    spsr: bit(word, 22) == 1,
                    rd: Register::from_bits(bits(word, 15, 12) as u8),
                }
            } else if (word & 0x0FB0_FFF0) == 0x0120_F000 {
                decode_msr(word)
            } else {
                decode_data_processing(word)
            }
        }
    }
}

/// Class `001`: data processing with immediate operand, except MSR
/// immediate, which shares the space (GBATEK "PSR Transfer").
fn decode_class_001(word: u32) -> ArmInstr {
    // MSR immediate: bits 15..12 = 1111, field mask free (GBATEK "PSR
    // Transfer").
    if (word & 0x0FB0_F000) == 0x0320_F000 {
        return decode_msr(word);
    }
    decode_data_processing(word)
}

fn decode_msr(word: u32) -> ArmInstr {
    ArmInstr::PsrWrite {
        spsr: bit(word, 22) == 1,
        fields: bits(word, 19, 16) as u8,
        source: if bit(word, 25) == 1 {
            PsrSource::Immediate {
                rotate: bits(word, 11, 8) as u8,
                value: bits(word, 7, 0) as u8,
            }
        } else {
            PsrSource::Register(Register::from_bits(bits(word, 3, 0) as u8))
        },
    }
}

fn decode_data_processing(word: u32) -> ArmInstr {
    let opcode = match bits(word, 24, 21) {
        0x0 => DataOp::And,
        0x1 => DataOp::Eor,
        0x2 => DataOp::Sub,
        0x3 => DataOp::Rsb,
        0x4 => DataOp::Add,
        0x5 => DataOp::Adc,
        0x6 => DataOp::Sbc,
        0x7 => DataOp::Rsc,
        0x8 => DataOp::Tst,
        0x9 => DataOp::Teq,
        0xA => DataOp::Cmp,
        0xB => DataOp::Cmn,
        0xC => DataOp::Orr,
        0xD => DataOp::Mov,
        0xE => DataOp::Bic,
        _ => DataOp::Mvn,
    };
    let operand2 = if bit(word, 25) == 1 {
        // Immediate: rotate (0..30 in steps of 2) applied to the 8-bit
        // value.
        ShifterOperand::Immediate {
            rotate: bits(word, 11, 8) as u8,
            value: bits(word, 7, 0) as u8,
        }
    } else {
        let rm = Register::from_bits(bits(word, 3, 0) as u8);
        let shift = if bit(word, 4) == 0 {
            // Immediate shift amount; LSR/ASR #0 mean #32, ROR #0 is RRX.
            let amount = bits(word, 11, 7) as u8;
            match bits(word, 6, 5) {
                0b00 if amount == 0 => RegisterShift {
                    kind: ShiftKind::Lsl,
                    amount: ShiftAmount::Immediate(0),
                },
                0b00 => RegisterShift {
                    kind: ShiftKind::Lsl,
                    amount: ShiftAmount::Immediate(amount),
                },
                0b01 => RegisterShift {
                    kind: ShiftKind::Lsr,
                    amount: ShiftAmount::Immediate(amount),
                },
                0b10 => RegisterShift {
                    kind: ShiftKind::Asr,
                    amount: ShiftAmount::Immediate(amount),
                },
                _ if amount == 0 => RegisterShift {
                    kind: ShiftKind::Rrx,
                    amount: ShiftAmount::Immediate(0),
                },
                _ => RegisterShift {
                    kind: ShiftKind::Ror,
                    amount: ShiftAmount::Immediate(amount),
                },
            }
        } else {
            // Register-specified shift amount; Rs in bits 11..8.
            RegisterShift {
                kind: match bits(word, 6, 5) {
                    0b00 => ShiftKind::Lsl,
                    0b01 => ShiftKind::Lsr,
                    0b10 => ShiftKind::Asr,
                    _ => ShiftKind::Ror,
                },
                amount: ShiftAmount::Register(Register::from_bits(bits(word, 11, 8) as u8)),
            }
        };
        ShifterOperand::Register { rm, shift }
    };
    ArmInstr::DataProcessing {
        opcode,
        set_flags: bit(word, 20) == 1,
        rd: Register::from_bits(bits(word, 15, 12) as u8),
        rn: Register::from_bits(bits(word, 19, 16) as u8),
        operand2,
    }
}

/// Multiply space, bits 7..4 == 1001 (GBATEK "Multiply", "Single Data
/// Swap").
fn decode_multiply_space(word: u32) -> ArmInstr {
    if bits(word, 27, 22) == 0 {
        // MUL (A=0) / MLA (A=1).
        return ArmInstr::Multiply {
            accumulate: bit(word, 21) == 1,
            set_flags: bit(word, 20) == 1,
            rd: Register::from_bits(bits(word, 19, 16) as u8),
            rn: Register::from_bits(bits(word, 15, 12) as u8),
            rs: Register::from_bits(bits(word, 11, 8) as u8),
            rm: Register::from_bits(bits(word, 3, 0) as u8),
        };
    }
    if bits(word, 27, 23) == 0b00001 {
        // Long multiplies: U selects UMULL/UMLAL vs SMULL/SMLAL, A selects
        // accumulate. Included in ARMv4T (ARMv3M and up).
        return ArmInstr::MultiplyLong {
            unsigned: bit(word, 22) == 1,
            accumulate: bit(word, 21) == 1,
            set_flags: bit(word, 20) == 1,
            rd_hi: Register::from_bits(bits(word, 19, 16) as u8),
            rd_lo: Register::from_bits(bits(word, 15, 12) as u8),
            rn: Register::from_bits(bits(word, 11, 8) as u8),
            rm: Register::from_bits(bits(word, 3, 0) as u8),
        };
    }
    if bits(word, 27, 23) == 0b00010 && bits(word, 21, 20) == 0 && bits(word, 11, 4) == 0x09 {
        // SWP: bits 27..23 fixed 00010, bits 21..20 zero, bits 11..4
        // fixed 00001001 (GBATEK "Single Data Swap").
        return ArmInstr::Swap {
            byte: bit(word, 22) == 1,
            rd: Register::from_bits(bits(word, 15, 12) as u8),
            rn: Register::from_bits(bits(word, 19, 16) as u8),
            rm: Register::from_bits(bits(word, 3, 0) as u8),
        };
    }
    ArmInstr::Undefined { encoding: word }
}

/// Halfword and signed-byte transfer space: bits 7 and 4 set, shift
/// opcode bits 6..5 nonzero (GBATEK "Halfword, Doubleword, and Signed
/// Data Transfer").
fn decode_halfword_transfer(word: u32) -> ArmInstr {
    let kind = match (bit(word, 20), bits(word, 6, 5)) {
        // LDRD/STRD are ARMv5TE and up; SH == 00 is the undefined space.
        (0, 0b01) => HalfwordKind::Strh,
        (1, 0b01) => HalfwordKind::Ldrh,
        (1, 0b10) => HalfwordKind::Ldrsb,
        (1, 0b11) => HalfwordKind::Ldrsh,
        _ => return ArmInstr::Undefined { encoding: word },
    };
    // Bit 22 = I: 0 = register offset, 1 = immediate offset (GBATEK
    // "Halfword, Doubleword, and Signed Data Transfer"); the register
    // form requires bits 11..8 = 0.
    let offset = if bit(word, 22) == 1 {
        HalfwordOffset::Immediate(((bits(word, 11, 8) << 4) | bits(word, 3, 0)) as u8)
    } else if bits(word, 11, 8) == 0 {
        HalfwordOffset::Register(Register::from_bits(bits(word, 3, 0) as u8))
    } else {
        return ArmInstr::Undefined { encoding: word };
    };
    ArmInstr::LoadStoreHalfwordSigned {
        kind,
        writeback: bit(word, 21) == 1,
        up: bit(word, 23) == 1,
        pre_index: bit(word, 24) == 1,
        rd: Register::from_bits(bits(word, 15, 12) as u8),
        rn: Register::from_bits(bits(word, 19, 16) as u8),
        offset,
    }
}

/// Single data transfer, classes 010/011 (GBATEK "Single Data Transfer").
fn decode_single_data_transfer(word: u32) -> ArmInstr {
    // I=1 with bit 4 set is the undefined-instruction space (GBATEK: bit 4
    // "Must be 0").
    if bit(word, 25) == 1 && bit(word, 4) == 1 {
        return ArmInstr::Undefined { encoding: word };
    }
    let offset = if bit(word, 25) == 1 {
        // Register offset, shifted by immediate.
        TransferOffset::Register {
            rm: Register::from_bits(bits(word, 3, 0) as u8),
            shift: RegisterShift {
                kind: match bits(word, 6, 5) {
                    0b00 => ShiftKind::Lsl,
                    0b01 => ShiftKind::Lsr,
                    0b10 => ShiftKind::Asr,
                    _ if bits(word, 11, 7) == 0 => ShiftKind::Rrx,
                    _ => ShiftKind::Ror,
                },
                amount: ShiftAmount::Immediate(bits(word, 11, 7) as u8),
            },
        }
    } else {
        TransferOffset::Immediate(bits(word, 11, 0))
    };
    ArmInstr::LoadStore {
        load: bit(word, 20) == 1,
        byte: bit(word, 22) == 1,
        writeback: bit(word, 21) == 1,
        up: bit(word, 23) == 1,
        pre_index: bit(word, 24) == 1,
        rd: Register::from_bits(bits(word, 15, 12) as u8),
        rn: Register::from_bits(bits(word, 19, 16) as u8),
        offset,
    }
}

/// Block data transfer, class 100 (GBATEK "Block Data Transfer").
fn decode_block_transfer(word: u32) -> ArmInstr {
    ArmInstr::BlockTransfer {
        load: bit(word, 20) == 1,
        psr_and_force_user: bit(word, 22) == 1,
        writeback: bit(word, 21) == 1,
        up: bit(word, 23) == 1,
        pre_index: bit(word, 24) == 1,
        rn: Register::from_bits(bits(word, 19, 16) as u8),
        registers: RegisterList::from_bits(word as u16),
    }
}

/// Branch and branch with link, class 101 (GBATEK "Branch and Branch with
/// Link").
fn decode_branch(word: u32) -> ArmInstr {
    ArmInstr::Branch {
        link: bit(word, 24) == 1,
        // Target = PC + 8 + offset; the offset is kept position
        // independent.
        offset: sign_extend(bits(word, 23, 0), 24) * 4,
    }
}

/// Class `111`: SWI executes; the coprocessor space is undefined on the
/// ARM7TDMI, which has no coprocessor (GBATEK "Coprocessor
/// Instructions": "The opcodes are irrelevant for GBA/NDS7 because no
/// coprocessor exists").
fn decode_class_111(word: u32) -> ArmInstr {
    if bit(word, 24) == 1 {
        ArmInstr::SoftwareInterrupt {
            imm24: bits(word, 23, 0),
        }
    } else {
        // CDP (bit 4 clear) and MCR/MRC (bit 4 set).
        ArmInstr::Undefined { encoding: word }
    }
}

/// Reads one bit of `word`.
fn bit(word: u32, index: u32) -> u32 {
    (word >> index) & 1
}

/// One ARM instruction word, decoded.
///
/// Field names mirror the encoding; execution semantics live in the
/// runtime and lifter, not here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArmInstr {
    /// Data processing (ALU): AND..MVN with register or immediate second
    /// operand.
    DataProcessing {
        /// Opcode, bits 24..21.
        opcode: DataOp,
        /// S bit: set condition codes.
        set_flags: bool,
        /// Destination register; unused by TST/TEQ/CMP/CMN (encoded 0).
        rd: Register,
        /// First operand register.
        rn: Register,
        /// Second operand, immediate or (shifted) register.
        operand2: ShifterOperand,
    },
    /// MUL/MLA, 32-bit multiply with optional accumulate.
    Multiply {
        /// A bit: accumulate into `rn`.
        accumulate: bool,
        /// S bit.
        set_flags: bool,
        /// Destination.
        rd: Register,
        /// Accumulate operand.
        rn: Register,
        /// Shifted multiplier.
        rs: Register,
        /// Multiplicand.
        rm: Register,
    },
    /// UMULL/UMLAL/SMULL/SMLAL, 64-bit multiply.
    MultiplyLong {
        /// U bit: unsigned when set.
        unsigned: bool,
        /// A bit: accumulate.
        accumulate: bool,
        /// S bit.
        set_flags: bool,
        /// High half of the result.
        rd_hi: Register,
        /// Low half of the result.
        rd_lo: Register,
        /// Accumulate low half.
        rn: Register,
        /// Multiplicand.
        rm: Register,
    },
    /// SWP/SWPB, atomic swap.
    Swap {
        /// B bit: byte swap when set.
        byte: bool,
        /// Destination.
        rd: Register,
        /// Memory address register.
        rn: Register,
        /// Store value.
        rm: Register,
    },
    /// MRS: read CPSR or SPSR into a register.
    PsrRead {
        /// R bit: SPSR when set, CPSR when clear.
        spsr: bool,
        /// Destination.
        rd: Register,
    },
    /// MSR: write a register or immediate into CPSR/SPSR fields.
    PsrWrite {
        /// R bit: SPSR when set, CPSR when clear.
        spsr: bool,
        /// Field mask, bits 19..16: bit 3 = flags, 2 = status, 1 =
        /// extension, 0 = control.
        fields: u8,
        /// Source operand.
        source: PsrSource,
    },
    /// LDR/STR/LDRB/STRB, single word or byte transfer.
    LoadStore {
        /// L bit: load when set.
        load: bool,
        /// B bit: byte when set.
        byte: bool,
        /// Bit 21: the W bit when pre-indexed; the T bit (force user
        /// access) when post-indexed. Post-indexing always writes back.
        writeback: bool,
        /// U bit: add the offset when set.
        up: bool,
        /// P bit: pre-indexed when set.
        pre_index: bool,
        /// Transferred register.
        rd: Register,
        /// Base register.
        rn: Register,
        /// Offset, immediate or shifted register.
        offset: TransferOffset,
    },
    /// STRH/LDRH/LDRSB/LDRSH, halfword and signed transfer.
    LoadStoreHalfwordSigned {
        /// SH bits 6..5 with the L bit.
        kind: HalfwordKind,
        /// W bit.
        writeback: bool,
        /// U bit.
        up: bool,
        /// P bit.
        pre_index: bool,
        /// Transferred register.
        rd: Register,
        /// Base register.
        rn: Register,
        /// Offset, immediate or register.
        offset: HalfwordOffset,
    },
    /// LDM/STM, block transfer.
    BlockTransfer {
        /// L bit: load when set.
        load: bool,
        /// S bit: user-bank transfer / PSR restore.
        psr_and_force_user: bool,
        /// W bit.
        writeback: bool,
        /// U bit.
        up: bool,
        /// P bit.
        pre_index: bool,
        /// Base register.
        rn: Register,
        /// Register list, bit 0 = R0.
        registers: RegisterList,
    },
    /// B/BL. Target = instruction address + 8 + `offset`.
    Branch {
        /// L bit: link into R14 when set.
        link: bool,
        /// Sign-extended 24-bit offset, already scaled by 4.
        offset: i32,
    },
    /// BX: branch to `rn`, switching to THUMB state when bit 0 is set.
    BranchExchange {
        /// Target register.
        rn: Register,
    },
    /// SWI: supervisor call, comment field only.
    SoftwareInterrupt {
        /// The 24-bit comment field.
        imm24: u32,
    },
    /// The ARM7TDMI would not execute this word: reserved condition,
    /// undefined-instruction space, or absent coprocessor space.
    Undefined {
        /// The raw word.
        encoding: u32,
    },
}

/// Data-processing opcode, bits 24..21 (GBATEK "Data Processing (ALU)").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataOp {
    /// `AND`: logical and.
    And,
    /// `EOR`: exclusive or.
    Eor,
    /// `SUB`: subtract.
    Sub,
    /// `RSB`: reverse subtract.
    Rsb,
    /// `ADD`: add.
    Add,
    /// `ADC`: add with carry.
    Adc,
    /// `SBC`: subtract with carry.
    Sbc,
    /// `RSC`: reverse subtract with carry.
    Rsc,
    /// `TST`: test.
    Tst,
    /// `TEQ`: test equality.
    Teq,
    /// `CMP`: compare.
    Cmp,
    /// `CMN`: compare negated.
    Cmn,
    /// `ORR`: logical or.
    Orr,
    /// `MOV`: move.
    Mov,
    /// `BIC`: bit clear.
    Bic,
    /// `MVN`: move not.
    Mvn,
}

/// The data-processing second operand (GBATEK "Op2").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShifterOperand {
    /// 8-bit immediate rotated right by `2 * rotate`.
    Immediate {
        /// Rotation, 0..15, applied as even right rotations.
        rotate: u8,
        /// The 8-bit immediate.
        value: u8,
    },
    /// Register, optionally shifted. An unshifted register decodes as
    /// `LSL #0`, the encoding the assembler emits for a plain `Rm`.
    Register {
        /// The operand register.
        rm: Register,
        /// The shift applied to `rm`.
        shift: RegisterShift,
    },
}

/// A register shift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisterShift {
    /// Shift kind; [`ShiftKind::Rrx`] is encoded as `ROR #0`.
    pub kind: ShiftKind,
    /// Shift amount.
    pub amount: ShiftAmount,
}

/// Shift kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShiftKind {
    /// Logical shift left.
    Lsl,
    /// Logical shift right; immediate amount 0 means 32.
    Lsr,
    /// Arithmetic shift right; immediate amount 0 means 32.
    Asr,
    /// Rotate right.
    Ror,
    /// Rotate right with extend, encoded as `ROR #0`.
    Rrx,
}

/// Shift amount: immediate or register specified.
///
/// Register-specified amounts are only encodable in the data-processing
/// class, and LSL is the only kind defined with a register amount there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShiftAmount {
    /// Immediate amount, 0..31 (0 has per-kind special meanings).
    Immediate(u8),
    /// Register-specified amount (bottom byte of the register).
    Register(Register),
}

/// Shift type field, bits 6..5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShiftType {
    /// Logical shift left.
    Lsl,
    /// Logical shift right.
    Lsr,
    /// Arithmetic shift right.
    Asr,
    /// Rotate right.
    Ror,
}

/// Offset for single data transfers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferOffset {
    /// 12-bit immediate offset.
    Immediate(u32),
    /// Register offset, shifted by an immediate amount only (ARMv4
    /// constraint; register-specified amounts are not encodable here).
    Register {
        /// Offset register.
        rm: Register,
        /// Shift applied to the offset register.
        shift: RegisterShift,
    },
}

/// Transfer kind for the halfword/signed space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HalfwordKind {
    /// `STRH`.
    Strh,
    /// `LDRH`, zero-extended load.
    Ldrh,
    /// `LDRSB`, sign-extended byte load.
    Ldrsb,
    /// `LDRSH`, sign-extended halfword load.
    Ldrsh,
}

/// Offset for the halfword/signed space: 8-bit immediate or plain
/// register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HalfwordOffset {
    /// 8-bit immediate (bits 11..8 and 3..0).
    Immediate(u8),
    /// Offset register.
    Register(Register),
}

/// Source operand of `MSR`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PsrSource {
    /// Source register.
    Register(Register),
    /// 8-bit immediate rotated right by `2 * rotate`.
    Immediate {
        /// Rotation, 0..15.
        rotate: u8,
        /// The 8-bit immediate.
        value: u8,
    },
}
