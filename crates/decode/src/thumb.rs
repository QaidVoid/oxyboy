//! The THUMB (16-bit) instruction model and decoder.
//!
//! Bit layouts follow GBATEK "THUMB Opcodes" (which match ARM ARM
//! appendix A4.2); each dispatch cites the GBATEK section it implements.

use crate::condition::Condition;
use crate::register::Register;
use crate::{sign_extend, thumb_bits};

/// Decodes one THUMB instruction halfword, consuming the next halfword
/// when the encoding is a `BL` pair.
///
/// Total over the input space: halfwords the ARM7TDMI would not execute
/// (permanently undefined spaces, a broken `BL` second halfword) decode to
/// [`ThumbInstr::Undefined`].
#[must_use]
pub fn decode_thumb(first: u16, next: u16) -> ThumbInstr {
    match first >> 12 {
        0x0 | 0x1 => decode_shift_add_subtract(first),
        0x2 | 0x3 => decode_immediate_operation(first),
        0x4 => decode_alu_high_or_pc_relative(first),
        0x5 => decode_load_store_register_offset(first),
        0x6 | 0x7 => decode_load_store_immediate_offset(first),
        0x8 => decode_load_store_halfword(first),
        0x9 => decode_load_store_stack(first),
        0xA => decode_load_address(first),
        0xB => decode_misc(first),
        0xC => decode_load_store_multiple(first),
        0xD => decode_conditional_branch_or_swi(first),
        0xE => {
            if first >> 11 == 0b11100 {
                decode_unconditional_branch(first)
            } else {
                // hw>>11 == 11101 is a permanently undefined space
                // (GBATEK THUMB opcode map).
                ThumbInstr::Undefined { encoding: first }
            }
        }
        _ => decode_long_branch_with_link(first, next),
    }
}

/// Dispatches the `0100` space: formats 4 (ALU), 5 (Hi register / BX)
/// and 6 (PC-relative load), selected by bits 11..10.
fn decode_alu_high_or_pc_relative(first: u16) -> ThumbInstr {
    if thumb_bits(first, 11, 10) >= 0b10 {
        decode_pc_relative_load(first)
    } else {
        decode_alu_or_high_register(first)
    }
}

/// Formats 1 (LSL/LSR/ASR) and 2 (ADD/SUB register or 3-bit immediate).
fn decode_shift_add_subtract(first: u16) -> ThumbInstr {
    if thumb_bits(first, 12, 11) == 0b11 {
        // Format 2: ADD/SUB register or immediate (GBATEK "THUMB.2 Move
        // Compare Add Subtract Immediate 3-Bit").
        ThumbInstr::AddSubtract {
            subtract: thumb_bits(first, 9, 9) == 1,
            immediate: thumb_bits(first, 10, 10) == 1,
            rn_or_imm3: thumb_bits(first, 8, 6) as u8,
            rs: Register::from_bits(thumb_bits(first, 5, 3) as u8),
            rd: Register::from_bits(thumb_bits(first, 2, 0) as u8),
        }
    } else {
        // Format 1: shift by 5-bit immediate (GBATEK "THUMB.1 Shift").
        ThumbInstr::ShiftedImmediate {
            kind: match thumb_bits(first, 12, 11) {
                0b00 => ThumbShift::Lsl,
                0b01 => ThumbShift::Lsr,
                _ => ThumbShift::Asr,
            },
            rd: Register::from_bits(thumb_bits(first, 2, 0) as u8),
            rm: Register::from_bits(thumb_bits(first, 5, 3) as u8),
            imm5: thumb_bits(first, 10, 6) as u8,
        }
    }
}

/// Format 3: MOV/CMP/ADD/SUB with 8-bit immediate.
fn decode_immediate_operation(first: u16) -> ThumbInstr {
    ThumbInstr::ImmediateOperation {
        op: match thumb_bits(first, 12, 11) {
            0b00 => ThumbImmOp::Mov,
            0b01 => ThumbImmOp::Cmp,
            0b10 => ThumbImmOp::Add,
            _ => ThumbImmOp::Sub,
        },
        rd: Register::from_bits(thumb_bits(first, 10, 8) as u8),
        imm8: thumb_bits(first, 7, 0) as u8,
    }
}

/// Format 4 (ALU operations) and format 5 (Hi register operations / BX).
fn decode_alu_or_high_register(first: u16) -> ThumbInstr {
    if thumb_bits(first, 11, 10) == 0b00 {
        // Format 4: ALU operations, opcode in bits 9..6 (GBATEK "THUMB.4
        // ALU Operations").
        let op = match thumb_bits(first, 9, 6) {
            0x0 => ThumbAluOp::And,
            0x1 => ThumbAluOp::Eor,
            0x2 => ThumbAluOp::Lsl,
            0x3 => ThumbAluOp::Lsr,
            0x4 => ThumbAluOp::Asr,
            0x5 => ThumbAluOp::Adc,
            0x6 => ThumbAluOp::Sbc,
            0x7 => ThumbAluOp::Ror,
            0x8 => ThumbAluOp::Tst,
            0x9 => ThumbAluOp::Neg,
            0xA => ThumbAluOp::Cmp,
            0xB => ThumbAluOp::Cmn,
            0xC => ThumbAluOp::Orr,
            0xD => ThumbAluOp::Mul,
            0xE => ThumbAluOp::Bic,
            _ => ThumbAluOp::Mvn,
        };
        ThumbInstr::AluOperation {
            op,
            rd: Register::from_bits(thumb_bits(first, 2, 0) as u8),
            rs: Register::from_bits(thumb_bits(first, 5, 3) as u8),
        }
    } else {
        // Format 5: Hi register operations / BX (GBATEK "THUMB.5 Hi
        // Register Operations / Branch Exchange").
        let op = match thumb_bits(first, 9, 8) {
            0b00 => ThumbHiOp::Add,
            0b01 => ThumbHiOp::Cmp,
            0b10 => ThumbHiOp::Mov,
            _ => ThumbHiOp::Bx,
        };
        // When both operands are low registers the encoding aliases onto
        // format 4 ADD/CMP/MOV; the ARM7TDMI leaves it undefined.
        let both_low = thumb_bits(first, 7, 7) == 0 && thumb_bits(first, 6, 6) == 0;
        if op != ThumbHiOp::Bx && both_low {
            return ThumbInstr::Undefined { encoding: first };
        }
        // BX only reads a register when bit 6 (MSBs) selects the high
        // bank; H1 must be 0.
        if op == ThumbHiOp::Bx && thumb_bits(first, 7, 7) == 1 {
            return ThumbInstr::Undefined { encoding: first };
        }
        ThumbInstr::HighRegisterOperation {
            op,
            rd: Register::from_bits(
                (thumb_bits(first, 2, 0) | (thumb_bits(first, 7, 7) << 3)) as u8,
            ),
            rs: Register::from_bits(
                (thumb_bits(first, 5, 3) | (thumb_bits(first, 6, 6) << 3)) as u8,
            ),
        }
    }
}

/// Format 6: PC-relative load.
fn decode_pc_relative_load(first: u16) -> ThumbInstr {
    ThumbInstr::PcRelativeLoad {
        rd: Register::from_bits(thumb_bits(first, 10, 8) as u8),
        offset: thumb_bits(first, 7, 0) as u8,
    }
}

/// Format 7: load/store with register offset (GBATEK "THUMB.7/8 Load
/// Store with Register Relative Offset").
fn decode_load_store_register_offset(first: u16) -> ThumbInstr {
    let kind = match thumb_bits(first, 11, 10) {
        0b00 => ThumbMemOp::Str,
        0b01 => ThumbMemOp::Strb,
        0b10 => ThumbMemOp::Ldr,
        _ => ThumbMemOp::Ldrb,
    };
    if thumb_bits(first, 9, 9) == 0 {
        ThumbInstr::LoadStoreRegisterOffset {
            kind,
            rd: Register::from_bits(thumb_bits(first, 2, 0) as u8),
            rb: Register::from_bits(thumb_bits(first, 5, 3) as u8),
            ro: Register::from_bits(thumb_bits(first, 8, 6) as u8),
        }
    } else {
        // Format 8: sign-extended byte/halfword load, halfword store.
        let kind = match thumb_bits(first, 11, 10) {
            0b00 => ThumbSignedMemOp::Strh,
            0b01 => ThumbSignedMemOp::Ldsb,
            0b10 => ThumbSignedMemOp::Ldrh,
            _ => ThumbSignedMemOp::Ldsh,
        };
        ThumbInstr::LoadStoreSignedRegisterOffset {
            kind,
            rd: Register::from_bits(thumb_bits(first, 2, 0) as u8),
            rb: Register::from_bits(thumb_bits(first, 5, 3) as u8),
            ro: Register::from_bits(thumb_bits(first, 8, 6) as u8),
        }
    }
}

/// Format 9: load/store with immediate offset.
fn decode_load_store_immediate_offset(first: u16) -> ThumbInstr {
    ThumbInstr::LoadStoreImmediateOffset {
        kind: match thumb_bits(first, 12, 11) {
            0b00 => ThumbMemOp::Str,
            0b01 => ThumbMemOp::Ldr,
            0b10 => ThumbMemOp::Strb,
            _ => ThumbMemOp::Ldrb,
        },
        rd: Register::from_bits(thumb_bits(first, 2, 0) as u8),
        rb: Register::from_bits(thumb_bits(first, 5, 3) as u8),
        // Byte transfers scale the 5-bit field by 1, word transfers by 4.
        offset: thumb_bits(first, 10, 6) as u8,
    }
}

/// Format 10: load/store halfword with immediate offset.
fn decode_load_store_halfword(first: u16) -> ThumbInstr {
    ThumbInstr::LoadStoreHalfwordImmediate {
        load: thumb_bits(first, 11, 11) == 1,
        rd: Register::from_bits(thumb_bits(first, 2, 0) as u8),
        rb: Register::from_bits(thumb_bits(first, 5, 3) as u8),
        offset: thumb_bits(first, 10, 6) as u8,
    }
}

/// Format 11: SP-relative load/store.
fn decode_load_store_stack(first: u16) -> ThumbInstr {
    ThumbInstr::LoadStoreStack {
        load: thumb_bits(first, 11, 11) == 1,
        rd: Register::from_bits(thumb_bits(first, 10, 8) as u8),
        offset: thumb_bits(first, 7, 0) as u8,
    }
}

/// Format 12: load address (SP or PC relative).
fn decode_load_address(first: u16) -> ThumbInstr {
    ThumbInstr::LoadAddress {
        to_sp: thumb_bits(first, 11, 11) == 1,
        rd: Register::from_bits(thumb_bits(first, 10, 8) as u8),
        offset: thumb_bits(first, 7, 0) as u8,
    }
}

/// Format 13, 14, 15 (misc: ADD SP, PUSH/POP, LDM/STM).
fn decode_misc(first: u16) -> ThumbInstr {
    if first >> 8 == 0b1011_0000 {
        // Format 13: ADD offset to SP (GBATEK "THUMB.13 Add Offset to
        // Stack Pointer").
        return ThumbInstr::AdjustStackPointer {
            subtract: thumb_bits(first, 7, 7) == 1,
            offset: thumb_bits(first, 6, 0) as u8,
        };
    }
    if thumb_bits(first, 10, 9) == 0b10 {
        // Format 14: PUSH/POP (GBATEK "THUMB.14 Push Pop Registers");
        // bit 11 selects PUSH/POP, bit 8 selects LR/PC.
        return ThumbInstr::PushPop {
            pop: thumb_bits(first, 11, 11) == 1,
            include_pc_or_lr: thumb_bits(first, 8, 8) == 1,
            registers: thumb_bits(first, 7, 0) as u8,
        };
    }
    ThumbInstr::Undefined { encoding: first }
}

/// Format 15: load/store multiple.
fn decode_load_store_multiple(first: u16) -> ThumbInstr {
    ThumbInstr::LoadStoreMultiple {
        load: thumb_bits(first, 11, 11) == 1,
        rb: Register::from_bits(thumb_bits(first, 10, 8) as u8),
        registers: thumb_bits(first, 7, 0) as u8,
    }
}

/// Format 16: conditional branch; 0xDEx space and SWI.
fn decode_conditional_branch_or_swi(first: u16) -> ThumbInstr {
    let cond_bits = thumb_bits(first, 11, 8) as u8;
    match cond_bits {
        // 0b1111 condition encodes SWI (format 17).
        0b1111 => ThumbInstr::SoftwareInterrupt {
            value: thumb_bits(first, 7, 0) as u8,
        },
        // 0b1110 is the permanently undefined space (GBATEK "THUMB.16
        // conditional branch": condition E is "Undefined, should not be
        // used").
        0b1110 => ThumbInstr::Undefined { encoding: first },
        _ => match Condition::from_bits(cond_bits) {
            Some(cond) => ThumbInstr::ConditionalBranch {
                cond,
                offset: (sign_extend(thumb_bits(first, 7, 0) as u32, 8) << 1) as i16,
            },
            None => ThumbInstr::Undefined { encoding: first },
        },
    }
}

/// Format 18: unconditional branch.
fn decode_unconditional_branch(first: u16) -> ThumbInstr {
    ThumbInstr::UnconditionalBranch {
        offset: (sign_extend(thumb_bits(first, 10, 0) as u32, 11) << 1) as i16,
    }
}

/// Format 19: long branch with link.
///
/// The first halfword supplies the high 11 offset bits, the second the
/// low 11; the pair is one instruction and `next` is consumed here. A
/// standalone second halfword decodes to
/// [`ThumbInstr::LongBranchWithLinkFromLr`], a real-world idiom documented
/// by GBATEK.
fn decode_long_branch_with_link(first: u16, next: u16) -> ThumbInstr {
    match (first >> 11, next >> 11) {
        (0b11110, 0b11111) => {
            let imm_hi = thumb_bits(first, 10, 0) as u32;
            let imm_lo = thumb_bits(next, 10, 0) as u32;
            ThumbInstr::LongBranchWithLink {
                // Target = PC + 4 + offset; offset is 23-bit sign
                // extended, halfword units.
                offset: sign_extend((imm_hi << 12) | (imm_lo << 1), 23),
            }
        }
        // Standalone second half: branch to LR + offset with link
        // (GBATEK THUMB.19 note: "Using only the 2nd half of BL as 'BL
        // LR+imm' is possible").
        (0b11111, _) => ThumbInstr::LongBranchWithLinkFromLr {
            offset: (sign_extend(thumb_bits(first, 10, 0) as u32, 11) << 1) as i16,
        },
        _ => ThumbInstr::Undefined { encoding: first },
    }
}

/// One THUMB instruction halfword pair, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbInstr {
    /// Format 1: LSL/LSR/ASR by immediate.
    ShiftedImmediate {
        /// Shift kind; the encoding has no RRX.
        kind: ThumbShift,
        /// Destination.
        rd: Register,
        /// Source.
        rm: Register,
        /// 5-bit immediate shift amount.
        imm5: u8,
    },
    /// Format 2: ADD/SUB register or 3-bit immediate.
    AddSubtract {
        /// Subtract when set.
        subtract: bool,
        /// The third operand is a 3-bit immediate when set, a register
        /// otherwise.
        immediate: bool,
        /// Third operand register or 3-bit immediate.
        rn_or_imm3: u8,
        /// First operand.
        rs: Register,
        /// Destination.
        rd: Register,
    },
    /// Format 3: MOV/CMP/ADD/SUB with 8-bit immediate.
    ImmediateOperation {
        /// Operation.
        op: ThumbImmOp,
        /// Destination (or compared register).
        rd: Register,
        /// 8-bit immediate.
        imm8: u8,
    },
    /// Format 4: ALU operation.
    AluOperation {
        /// Operation.
        op: ThumbAluOp,
        /// Destination and first operand.
        rd: Register,
        /// Second operand.
        rs: Register,
    },
    /// Format 5: Hi register operation / BX.
    HighRegisterOperation {
        /// Operation.
        op: ThumbHiOp,
        /// First operand (and destination for ADD/MOV).
        rd: Register,
        /// Second operand.
        rs: Register,
    },
    /// Format 6: PC-relative load. Address = (PC and not 3) + 4 * offset.
    PcRelativeLoad {
        /// Destination.
        rd: Register,
        /// Unsigned word offset.
        offset: u8,
    },
    /// Format 7: STR/LDR/STRB/LDRB with register offset.
    LoadStoreRegisterOffset {
        /// Transfer kind.
        kind: ThumbMemOp,
        /// Transferred register.
        rd: Register,
        /// Base register.
        rb: Register,
        /// Offset register.
        ro: Register,
    },
    /// Format 8: STRH/LDSB/LDRH/LDSH with register offset.
    LoadStoreSignedRegisterOffset {
        /// Transfer kind.
        kind: ThumbSignedMemOp,
        /// Transferred register.
        rd: Register,
        /// Base register.
        rb: Register,
        /// Offset register.
        ro: Register,
    },
    /// Format 9: STR/LDR/STRB/LDRB with immediate offset.
    LoadStoreImmediateOffset {
        /// Transfer kind.
        kind: ThumbMemOp,
        /// Transferred register.
        rd: Register,
        /// Base register.
        rb: Register,
        /// 5-bit offset, scaled by 4 for word transfers.
        offset: u8,
    },
    /// Format 10: STRH/LDRH with immediate offset.
    LoadStoreHalfwordImmediate {
        /// Load when set, store when clear.
        load: bool,
        /// Transferred register.
        rd: Register,
        /// Base register.
        rb: Register,
        /// 5-bit offset, scaled by 2.
        offset: u8,
    },
    /// Format 11: STR/LDR relative to SP.
    LoadStoreStack {
        /// Load when set, store when clear.
        load: bool,
        /// Transferred register.
        rd: Register,
        /// 8-bit offset, scaled by 4.
        offset: u8,
    },
    /// Format 12: ADD offset to SP or PC into a register.
    LoadAddress {
        /// Base is SP when set, PC when clear.
        to_sp: bool,
        /// Destination.
        rd: Register,
        /// 8-bit offset, scaled by 4.
        offset: u8,
    },
    /// Format 13: ADD/SUB offset to SP itself.
    AdjustStackPointer {
        /// Subtract when set.
        subtract: bool,
        /// 7-bit offset, scaled by 4.
        offset: u8,
    },
    /// Format 14: PUSH/POP.
    PushPop {
        /// POP when set, PUSH when clear.
        pop: bool,
        /// Include PC (POP) or LR (PUSH).
        include_pc_or_lr: bool,
        /// Low register list, bit 0 = R0.
        registers: u8,
    },
    /// Format 15: LDMIA/STMIA.
    LoadStoreMultiple {
        /// Load when set, store when clear.
        load: bool,
        /// Base register.
        rb: Register,
        /// Low register list, bit 0 = R0.
        registers: u8,
    },
    /// Format 16: conditional branch. Target = PC + 4 + offset.
    ConditionalBranch {
        /// Branch condition.
        cond: Condition,
        /// Sign-extended offset, halfword units.
        offset: i16,
    },
    /// Format 17: SWI.
    SoftwareInterrupt {
        /// 8-bit comment field.
        value: u8,
    },
    /// Format 18: unconditional branch. Target = PC + 4 + offset.
    UnconditionalBranch {
        /// Sign-extended offset, halfword units.
        offset: i16,
    },
    /// Format 19: BL pair. Target = PC + 4 + offset.
    LongBranchWithLink {
        /// 23-bit sign-extended offset, halfword units.
        offset: i32,
    },
    /// Format 19 second halfword standing alone: branch to LR + offset
    /// with link. Not an official encoding, but real games emit it as a
    /// cheap call (GBATEK THUMB.19 note; for example Mario Golf Advance
    /// Tour uses F800h as `BL LR+0`).
    LongBranchWithLinkFromLr {
        /// 11-bit sign-extended offset, halfword units.
        offset: i16,
    },
    /// The ARM7TDMI would not execute this halfword.
    Undefined {
        /// The raw first halfword.
        encoding: u16,
    },
}

/// THUMB format 1 shift kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbShift {
    /// Logical shift left.
    Lsl,
    /// Logical shift right; immediate amount 0 means 32.
    Lsr,
    /// Arithmetic shift right; immediate amount 0 means 32.
    Asr,
}

/// THUMB format 3 operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbImmOp {
    /// `MOV` immediate.
    Mov,
    /// `CMP` immediate.
    Cmp,
    /// `ADD` immediate.
    Add,
    /// `SUB` immediate.
    Sub,
}

/// THUMB format 4 ALU operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbAluOp {
    /// `AND`.
    And,
    /// `EOR`.
    Eor,
    /// `LSL` by register.
    Lsl,
    /// `LSR` by register.
    Lsr,
    /// `ASR` by register.
    Asr,
    /// `ADC`.
    Adc,
    /// `SBC`.
    Sbc,
    /// `ROR` by register.
    Ror,
    /// `TST`.
    Tst,
    /// `NEG` (RSD).
    Neg,
    /// `CMP`.
    Cmp,
    /// `CMN`.
    Cmn,
    /// `ORR`.
    Orr,
    /// `MUL`.
    Mul,
    /// `BIC`.
    Bic,
    /// `MVN`.
    Mvn,
}

/// THUMB format 5 Hi register operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbHiOp {
    /// `ADD`.
    Add,
    /// `CMP`.
    Cmp,
    /// `MOV`.
    Mov,
    /// `BX`: branch and exchange.
    Bx,
}

/// THUMB word/byte memory transfer kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbMemOp {
    /// `STR`.
    Str,
    /// `LDR`.
    Ldr,
    /// `STRB`.
    Strb,
    /// `LDRB`.
    Ldrb,
}

/// THUMB halfword and signed-byte memory transfer kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbSignedMemOp {
    /// `STRH`.
    Strh,
    /// `LDSB`, sign-extended byte load.
    Ldsb,
    /// `LDRH`, zero-extended halfword load.
    Ldrh,
    /// `LDSH`, sign-extended halfword load.
    Ldsh,
}

impl ThumbInstr {
    /// True when the encoding is architecturally UNPREDICTABLE on the
    /// ARM7TDMI.
    ///
    /// Currently this covers PUSH/POP with an empty register list and no
    /// LR/PC bit (`push {lr}` and `pop {pc}` with an otherwise empty
    /// list are defined), and load/store multiple with an empty list or
    /// with the base register in the list (ARM ARM A7.7; GBATEK THUMB.15
    /// documents only the ARM7's de facto "Strange Effects on Invalid
    /// Rlist's" behavior: an empty list transfers R15 and adds 40h to
    /// the base, and a list containing the base stores or skips
    /// writeback depending on position).
    #[must_use]
    pub fn is_unpredictable(&self) -> bool {
        match self {
            ThumbInstr::PushPop {
                include_pc_or_lr,
                registers,
                ..
            } => *registers == 0 && !include_pc_or_lr,
            ThumbInstr::LoadStoreMultiple { rb, registers, .. } => {
                *registers == 0 || u16::from(*registers) & (1u16 << rb.bits()) != 0
            }
            _ => false,
        }
    }
}
