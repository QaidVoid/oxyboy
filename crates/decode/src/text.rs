//! GNU as-compatible disassembly text in unified syntax.
//!
//! Rendering targets the GNU ARM assembler's unified syntax so that
//! text fed back to an assembler reproduces the original encoding;
//! [`crate::decode_arm`] / [`crate::decode_thumb`] plus these functions
//! and an assembler form the round-trip used by the test suite. GNU
//! `arm-none-eabi-as` is the reference backend; clang's integrated
//! assembler accepts the same input (one exception, documented on
//! [`thumb_text`]).
//!
//! Branches render as dot expressions relative to the instruction's own
//! address (`b .+8`, `beq .-2`), which keeps the text position
//! independent and is exact in both GNU as and clang: a branch's target
//! is its own address plus the pipeline displacement (8 bytes in ARM
//! state, 4 in THUMB state) plus the encoded offset. The numeric `#imm`
//! branch form is deliberately avoided: clang reads it as a byte offset
//! from PC + 4, while GNU as discards the constant entirely and branches
//! to itself.
//!
//! Encodings without a reproducible assembly form are rendered so a
//! reader never mistakes them for assemblable instructions:
//!
//! - [`crate::ArmInstr::Undefined`] renders as `.inst`, and
//!   [`crate::ThumbInstr::Undefined`] as `.2byte`, keeping raw bytes
//!   round-trippable.
//! - [`crate::ThumbInstr::LongBranchWithLinkFromLr`] renders as the
//!   pseudo-syntax `bl lr+N` (GBATEK THUMB.19 notation); no assembler
//!   accepts it.
//! - Encodings flagged by `is_unpredictable()` render as their nearest
//!   real syntax (for example `tst r0, r1` for a comparison with S
//!   clear); the round-trip harness skips them because the assembler
//!   would emit a different encoding.

use crate::arm::{
    ArmInstr, DataOp, HalfwordKind, HalfwordOffset, PsrSource, ShiftAmount, ShiftKind,
    ShifterOperand, TransferOffset, decode_arm,
};
use crate::bios::swi_name;
use crate::condition::Condition;
use crate::register::{Register, RegisterList};
use crate::thumb::{
    ThumbAluOp, ThumbHiOp, ThumbImmOp, ThumbInstr, ThumbMemOp, ThumbShift, ThumbSignedMemOp,
    decode_thumb,
};

/// Renders one ARM instruction word as GNU as unified-syntax text.
///
/// The condition field comes from `word` itself, so the result is exact:
/// `0x03A00001` renders as `moveq r0, #1`. Total over the input space;
/// undefined encodings render as `.inst` with the raw word.
#[must_use]
pub fn arm_text(word: u32) -> String {
    match decode_arm(word) {
        ArmInstr::Undefined { encoding } => format!(".inst 0x{encoding:08X}"),
        ArmInstr::DataProcessing {
            opcode,
            set_flags,
            rd,
            rn,
            operand2,
        } => {
            let cond = cond_suffix(word);
            let s = if set_flags { "s" } else { "" };
            let operand = arm_shifter(operand2);
            match opcode {
                // Comparisons have no destination operand (encoded
                // rd = 0) and are always flag-setting in their defined
                // form, so no S suffix is emitted.
                DataOp::Tst | DataOp::Teq | DataOp::Cmp | DataOp::Cmn => {
                    format!("{}{cond} {}, {}", dp_name(opcode), reg_name(rn), operand)
                }
                DataOp::Mov | DataOp::Mvn => {
                    format!("{}{cond}{s} {}, {}", dp_name(opcode), reg_name(rd), operand)
                }
                _ => format!(
                    "{}{cond}{s} {}, {}, {}",
                    dp_name(opcode),
                    reg_name(rd),
                    reg_name(rn),
                    operand
                ),
            }
        }
        ArmInstr::Multiply {
            accumulate,
            set_flags,
            rd,
            rn,
            rs,
            rm,
        } => {
            let cond = cond_suffix(word);
            let s = if set_flags { "s" } else { "" };
            if accumulate {
                format!(
                    "mla{cond}{s} {}, {}, {}, {}",
                    reg_name(rd),
                    reg_name(rm),
                    reg_name(rs),
                    reg_name(rn)
                )
            } else {
                format!(
                    "mul{cond}{s} {}, {}, {}",
                    reg_name(rd),
                    reg_name(rm),
                    reg_name(rs)
                )
            }
        }
        ArmInstr::MultiplyLong {
            unsigned,
            accumulate,
            set_flags,
            rd_hi,
            rd_lo,
            rn,
            rm,
        } => {
            let cond = cond_suffix(word);
            let s = if set_flags { "s" } else { "" };
            let base = match (unsigned, accumulate) {
                (true, false) => "umull",
                (true, true) => "umlal",
                (false, false) => "smull",
                (false, true) => "smlal",
            };
            format!(
                "{base}{cond}{s} {}, {}, {}, {}",
                reg_name(rd_lo),
                reg_name(rd_hi),
                reg_name(rm),
                reg_name(rn)
            )
        }
        ArmInstr::Swap { byte, rd, rn, rm } => {
            let b = if byte { "b" } else { "" };
            format!(
                "swp{b}{} {}, {}, [{}]",
                cond_suffix(word),
                reg_name(rd),
                reg_name(rm),
                reg_name(rn)
            )
        }
        ArmInstr::PsrRead { spsr, rd } => format!(
            "mrs{} {}, {}",
            cond_suffix(word),
            reg_name(rd),
            psr_name(spsr)
        ),
        ArmInstr::PsrWrite {
            spsr,
            fields,
            source,
        } => {
            let src = match source {
                PsrSource::Register(r) => reg_name(r).to_string(),
                PsrSource::Immediate { rotate, value } => {
                    format!("#{}", u32::from(value).rotate_right(u32::from(rotate) * 2))
                }
            };
            format!(
                "msr{} {}{}, {}",
                cond_suffix(word),
                psr_name(spsr),
                field_mask(fields),
                src
            )
        }
        ArmInstr::LoadStore {
            load,
            byte,
            writeback,
            up,
            pre_index,
            rd,
            rn,
            offset,
        } => {
            let cond = cond_suffix(word);
            // Post-indexed word/byte transfers with the W bit set are
            // the user-mode T forms (GBATEK "Single Data Transfer").
            let t = if !pre_index && writeback { "t" } else { "" };
            let base = match (load, byte) {
                (true, false) => "ldr",
                (false, false) => "str",
                (true, true) => "ldrb",
                (false, true) => "strb",
            };
            let operand = arm_transfer_offset(offset, up);
            format!(
                "{}{cond}{} {}, {}",
                base,
                t,
                reg_name(rd),
                addressing(rn, operand, pre_index, writeback)
            )
        }
        ArmInstr::LoadStoreHalfwordSigned {
            kind,
            writeback,
            up,
            pre_index,
            rd,
            rn,
            offset,
        } => {
            let base = match kind {
                HalfwordKind::Strh => "strh",
                HalfwordKind::Ldrh => "ldrh",
                HalfwordKind::Ldrsb => "ldrsb",
                HalfwordKind::Ldrsh => "ldrsh",
            };
            let operand = match offset {
                HalfwordOffset::Immediate(v) => signed_imm(u32::from(v), up),
                HalfwordOffset::Register(r) => signed_reg(r, up),
            };
            format!(
                "{}{} {}, {}",
                base,
                cond_suffix(word),
                reg_name(rd),
                addressing(rn, operand, pre_index, writeback)
            )
        }
        ArmInstr::BlockTransfer {
            load,
            psr_and_force_user,
            writeback,
            up,
            pre_index,
            rn,
            registers,
        } => {
            let base = match (load, pre_index, up) {
                (true, false, false) => "ldmda",
                (true, false, true) => "ldmia",
                (true, true, false) => "ldmdb",
                (true, true, true) => "ldmib",
                (false, false, false) => "stmda",
                (false, false, true) => "stmia",
                (false, true, false) => "stmdb",
                (false, true, true) => "stmib",
            };
            let bang = if writeback { "!" } else { "" };
            let caret = if psr_and_force_user { "^" } else { "" };
            format!(
                "{}{} {}{}, {{{}}}{}",
                base,
                cond_suffix(word),
                reg_name(rn),
                bang,
                list_text(registers),
                caret
            )
        }
        ArmInstr::Branch { link, offset } => {
            let base = if link { "bl" } else { "b" };
            // GNU as dot expression: the target is `.` plus the encoded
            // offset and the 8-byte pipeline displacement.
            let displacement = offset + 8;
            format!(
                "{}{} .{}{}",
                base,
                cond_suffix(word),
                sign(displacement),
                displacement.abs()
            )
        }
        ArmInstr::BranchExchange { rn } => {
            format!("bx{} {}", cond_suffix(word), reg_name(rn))
        }
        ArmInstr::SoftwareInterrupt { imm24 } => {
            // GBATEK "How BIOS Processes SWIs": ARM state interprets
            // only the upper 8 bits of the 24-bit comment field.
            let name = swi_name((imm24 >> 16) as u8);
            let comment = name.map(|name| format!(" @ {name}")).unwrap_or_default();
            format!("swi{} #0x{imm24:X}{comment}", cond_suffix(word))
        }
    }
}

/// Renders one THUMB instruction as GNU as unified-syntax text.
///
/// `next` is consumed for `BL` pairs, mirroring [`crate::decode_thumb`].
///
/// The one known backend gap: `LoadAddress` with `to_sp == false`
/// renders as `add rd, pc, #imm`, which GNU as assembles to the exact
/// encoding but clang's integrated assembler rejects (it only offers the
/// `adr` pseudo-instruction, whose aligned-PC semantics cannot express
/// arbitrary offset fields). The round-trip harness accounts for this.
#[must_use]
pub fn thumb_text(first: u16, next: u16) -> String {
    match decode_thumb(first, next) {
        ThumbInstr::Undefined { encoding } => format!(".2byte 0x{encoding:04X}"),
        ThumbInstr::ShiftedImmediate { kind, rd, rm, imm5 } => {
            let base = match kind {
                ThumbShift::Lsl => "lsls",
                ThumbShift::Lsr => "lsrs",
                ThumbShift::Asr => "asrs",
            };
            // LSR/ASR amount 0 means 32 (GBATEK THUMB.1); LSL 0 is a
            // plain register move.
            let amount = if imm5 == 0 && kind != ThumbShift::Lsl {
                32
            } else {
                u32::from(imm5)
            };
            format!("{base} {}, {}, #{}", reg_name(rd), reg_name(rm), amount)
        }
        ThumbInstr::AddSubtract {
            subtract,
            immediate,
            rn_or_imm3,
            rs,
            rd,
        } => {
            let base = if subtract { "subs" } else { "adds" };
            let operand = if immediate {
                format!("#{}", rn_or_imm3)
            } else {
                reg_name(Register::from_bits(rn_or_imm3)).to_string()
            };
            format!("{base} {}, {}, {}", reg_name(rd), reg_name(rs), operand)
        }
        ThumbInstr::ImmediateOperation { op, rd, imm8 } => {
            let base = match op {
                ThumbImmOp::Mov => "movs",
                ThumbImmOp::Cmp => "cmp",
                ThumbImmOp::Add => "adds",
                ThumbImmOp::Sub => "subs",
            };
            format!("{base} {}, #{}", reg_name(rd), imm8)
        }
        ThumbInstr::AluOperation { op, rd, rs } => {
            let (base, extra) = match op {
                ThumbAluOp::And => ("ands", ""),
                ThumbAluOp::Eor => ("eors", ""),
                ThumbAluOp::Lsl => ("lsls", ""),
                ThumbAluOp::Lsr => ("lsrs", ""),
                ThumbAluOp::Asr => ("asrs", ""),
                ThumbAluOp::Adc => ("adcs", ""),
                ThumbAluOp::Sbc => ("sbcs", ""),
                ThumbAluOp::Ror => ("rors", ""),
                ThumbAluOp::Tst => ("tst", ""),
                // NEG has no unified mnemonic; RSBS #0 assembles to it.
                ThumbAluOp::Neg => ("rsbs", ", #0"),
                ThumbAluOp::Cmp => ("cmp", ""),
                ThumbAluOp::Cmn => ("cmn", ""),
                ThumbAluOp::Orr => ("orrs", ""),
                ThumbAluOp::Mul => ("muls", ""),
                ThumbAluOp::Bic => ("bics", ""),
                ThumbAluOp::Mvn => ("mvns", ""),
            };
            format!("{base} {}, {}{}", reg_name(rd), reg_name(rs), extra)
        }
        ThumbInstr::HighRegisterOperation { op, rd, rs } => match op {
            ThumbHiOp::Add => format!("add {}, {}", reg_name(rd), reg_name(rs)),
            ThumbHiOp::Cmp => format!("cmp {}, {}", reg_name(rd), reg_name(rs)),
            ThumbHiOp::Mov => format!("mov {}, {}", reg_name(rd), reg_name(rs)),
            ThumbHiOp::Bx => format!("bx {}", reg_name(rs)),
        },
        ThumbInstr::PcRelativeLoad { rd, offset } => {
            format!("ldr {}, [pc, #{}]", reg_name(rd), u32::from(offset) * 4)
        }
        ThumbInstr::LoadStoreRegisterOffset { kind, rd, rb, ro } => {
            format!(
                "{} {}, [{}, {}]",
                mem_name(kind),
                reg_name(rd),
                reg_name(rb),
                reg_name(ro)
            )
        }
        ThumbInstr::LoadStoreSignedRegisterOffset { kind, rd, rb, ro } => {
            let base = match kind {
                ThumbSignedMemOp::Strh => "strh",
                ThumbSignedMemOp::Ldsb => "ldrsb",
                ThumbSignedMemOp::Ldrh => "ldrh",
                ThumbSignedMemOp::Ldsh => "ldrsh",
            };
            format!(
                "{base} {}, [{}, {}]",
                reg_name(rd),
                reg_name(rb),
                reg_name(ro)
            )
        }
        ThumbInstr::LoadStoreImmediateOffset {
            kind,
            rd,
            rb,
            offset,
        } => {
            // Word transfers scale the 5-bit field by 4, byte transfers
            // by 1 (GBATEK THUMB.9).
            let scale = match kind {
                ThumbMemOp::Str | ThumbMemOp::Ldr => 4,
                ThumbMemOp::Strb | ThumbMemOp::Ldrb => 1,
            };
            format!(
                "{} {}, [{}, #{}]",
                mem_name(kind),
                reg_name(rd),
                reg_name(rb),
                u32::from(offset) * scale
            )
        }
        ThumbInstr::LoadStoreHalfwordImmediate {
            load,
            rd,
            rb,
            offset,
        } => {
            let base = if load { "ldrh" } else { "strh" };
            format!(
                "{base} {}, [{}, #{}]",
                reg_name(rd),
                reg_name(rb),
                u32::from(offset) * 2
            )
        }
        ThumbInstr::LoadStoreStack { load, rd, offset } => {
            let base = if load { "ldr" } else { "str" };
            format!("{base} {}, [sp, #{}]", reg_name(rd), u32::from(offset) * 4)
        }
        ThumbInstr::LoadAddress { to_sp, rd, offset } => {
            let base = if to_sp { "sp" } else { "pc" };
            format!("add {}, {}, #{}", reg_name(rd), base, u32::from(offset) * 4)
        }
        ThumbInstr::AdjustStackPointer { subtract, offset } => {
            let base = if subtract { "sub" } else { "add" };
            format!("{base} sp, #{}", u32::from(offset) * 4)
        }
        ThumbInstr::PushPop {
            pop,
            include_pc_or_lr,
            registers,
        } => {
            let base = if pop { "pop" } else { "push" };
            let mut names: Vec<&'static str> = (0..8)
                .filter(|bit| registers & (1 << bit) != 0)
                .map(|bit| low_reg_name(bit))
                .collect();
            if include_pc_or_lr {
                names.push(if pop { "pc" } else { "lr" });
            }
            format!("{base} {{{}}}", names.join(", "))
        }
        ThumbInstr::LoadStoreMultiple {
            load,
            rb,
            registers,
        } => {
            let base = if load { "ldmia" } else { "stmia" };
            let names: Vec<&'static str> = (0..8)
                .filter(|bit| registers & (1 << bit) != 0)
                .map(|bit| low_reg_name(bit))
                .collect();
            format!("{base} {}!, {{{}}}", reg_name(rb), names.join(", "))
        }
        ThumbInstr::ConditionalBranch { cond, offset } => {
            branch_text("b", cond.mnemonic(), i32::from(offset))
        }
        ThumbInstr::SoftwareInterrupt { value } => {
            let comment = swi_name(value)
                .map(|name| format!(" @ {name}"))
                .unwrap_or_default();
            format!("swi #0x{value:02X}{comment}")
        }
        ThumbInstr::UnconditionalBranch { offset } => branch_text("b", "", i32::from(offset)),
        ThumbInstr::LongBranchWithLink { offset } => branch_text("bl", "", offset),
        ThumbInstr::LongBranchWithLinkFromLr { offset } => {
            // Standalone BL second halfword: no assembler encoding
            // exists; render in GBATEK THUMB.19 notation.
            format!("bl lr{}{}", sign(i32::from(offset)), offset.abs())
        }
    }
}

/// The condition mnemonic suffix for an ARM word, empty for `al`.
fn cond_suffix(word: u32) -> &'static str {
    match Condition::from_bits((word >> 28) as u8) {
        Some(Condition::Al) | None => "",
        Some(cond) => cond.mnemonic(),
    }
}

/// Register names as GNU as prints them (R13/R14/R15 as sp/lr/pc).
fn reg_name(register: Register) -> &'static str {
    match register {
        Register::R0 => "r0",
        Register::R1 => "r1",
        Register::R2 => "r2",
        Register::R3 => "r3",
        Register::R4 => "r4",
        Register::R5 => "r5",
        Register::R6 => "r6",
        Register::R7 => "r7",
        Register::R8 => "r8",
        Register::R9 => "r9",
        Register::R10 => "r10",
        Register::R11 => "r11",
        Register::R12 => "r12",
        Register::Sp => "sp",
        Register::Lr => "lr",
        Register::Pc => "pc",
    }
}

/// Low-register name for THUMB list encodings.
fn low_reg_name(bits: u32) -> &'static str {
    reg_name(Register::from_bits(bits as u8))
}

/// The data-processing mnemonic.
fn dp_name(opcode: DataOp) -> &'static str {
    match opcode {
        DataOp::And => "and",
        DataOp::Eor => "eor",
        DataOp::Sub => "sub",
        DataOp::Rsb => "rsb",
        DataOp::Add => "add",
        DataOp::Adc => "adc",
        DataOp::Sbc => "sbc",
        DataOp::Rsc => "rsc",
        DataOp::Tst => "tst",
        DataOp::Teq => "teq",
        DataOp::Cmp => "cmp",
        DataOp::Cmn => "cmn",
        DataOp::Orr => "orr",
        DataOp::Mov => "mov",
        DataOp::Bic => "bic",
        DataOp::Mvn => "mvn",
    }
}

/// The data-processing second operand.
fn arm_shifter(operand: ShifterOperand) -> String {
    match operand {
        ShifterOperand::Immediate { rotate, value } => {
            format!("#{}", u32::from(value).rotate_right(u32::from(rotate) * 2))
        }
        ShifterOperand::Register { rm, shift } => {
            format!("{}{}", reg_name(rm), shift_suffix(shift))
        }
    }
}

/// The `, lsl #3` part of a shifted-register operand, empty for LSL #0.
fn shift_suffix(shift: crate::arm::RegisterShift) -> String {
    match shift {
        crate::arm::RegisterShift {
            kind: ShiftKind::Lsl,
            amount: ShiftAmount::Immediate(0),
        } => String::new(),
        crate::arm::RegisterShift {
            kind: ShiftKind::Rrx,
            ..
        } => ", rrx".to_string(),
        // LSR/ASR #0 mean #32; ROR #0 is RRX, handled above.
        crate::arm::RegisterShift {
            kind,
            amount: ShiftAmount::Immediate(0),
        } => format!(", {} #32", shift_name(kind)),
        crate::arm::RegisterShift {
            kind,
            amount: ShiftAmount::Immediate(amount),
        } => format!(", {} #{}", shift_name(kind), amount),
        crate::arm::RegisterShift {
            kind,
            amount: ShiftAmount::Register(rs),
        } => format!(", {} {}", shift_name(kind), reg_name(rs)),
    }
}

/// Shift kind mnemonic.
fn shift_name(kind: ShiftKind) -> &'static str {
    match kind {
        ShiftKind::Lsl => "lsl",
        ShiftKind::Lsr => "lsr",
        ShiftKind::Asr => "asr",
        ShiftKind::Ror => "ror",
        // RRX is matched before reaching this function.
        ShiftKind::Rrx => "rrx",
    }
}

/// The word/byte transfer offset, signed for immediate values.
fn arm_transfer_offset(offset: TransferOffset, up: bool) -> String {
    match offset {
        TransferOffset::Immediate(v) => signed_imm(v, up),
        TransferOffset::Register { rm, shift } => signed_reg(rm, up) + &shift_suffix(shift),
    }
}

/// `#12` or `#-12` for an immediate transfer offset.
fn signed_imm(value: u32, up: bool) -> String {
    if up {
        format!("#{value}")
    } else {
        format!("#-{value}")
    }
}

/// `r2` or `-r2` for a register transfer offset.
fn signed_reg(rm: Register, up: bool) -> String {
    if up {
        reg_name(rm).to_string()
    } else {
        format!("-{}", reg_name(rm))
    }
}

/// GNU addressing grammar: `[base, offset]`, `[base, offset]!`, or
/// post-indexed `[base], offset`.
fn addressing(rn: Register, offset: String, pre_index: bool, writeback: bool) -> String {
    match (pre_index, writeback) {
        (true, true) => format!("[{}, {}]!", reg_name(rn), offset),
        (true, false) => format!("[{}, {}]", reg_name(rn), offset),
        // Post-indexed: writeback is implicit in the grammar.
        (false, _) => format!("[{}], {}", reg_name(rn), offset),
    }
}

/// CPSR or SPSR.
fn psr_name(spsr: bool) -> &'static str {
    if spsr { "spsr" } else { "cpsr" }
}

/// The MSR field mask in GNU `_fsxc` letter order; an empty mask renders
/// bare (the encoding is UNPREDICTABLE; see
/// `ArmInstr::is_unpredictable`).
fn field_mask(fields: u8) -> String {
    let mut out = String::new();
    if fields & 0b1000 != 0 {
        out.push('f');
    }
    if fields & 0b0100 != 0 {
        out.push('s');
    }
    if fields & 0b0010 != 0 {
        out.push('x');
    }
    if fields & 0b0001 != 0 {
        out.push('c');
    }
    if out.is_empty() {
        out
    } else {
        format!("_{out}")
    }
}

/// An ARM register list as ascending names.
fn list_text(list: RegisterList) -> String {
    (0..16)
        .filter(|bit| list.bits() & (1 << bit) != 0)
        .map(|bit| low_reg_name(bit))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Word/byte transfer mnemonic.
fn mem_name(kind: ThumbMemOp) -> &'static str {
    match kind {
        ThumbMemOp::Str => "str",
        ThumbMemOp::Ldr => "ldr",
        ThumbMemOp::Strb => "strb",
        ThumbMemOp::Ldrb => "ldrb",
    }
}

/// A THUMB branch as a dot expression: the target is the instruction's
/// own address plus the 4-byte pipeline displacement plus the encoded
/// halfword offset doubled to bytes, so the printed displacement is
/// `4 + offset * 2`.
fn branch_text(base: &str, cond: &str, offset: i32) -> String {
    let displacement = offset * 2 + 4;
    format!("{base}{cond} .{}{}", sign(displacement), displacement.abs())
}

/// `+` or `-` for signed offsets.
fn sign(value: i32) -> &'static str {
    if value < 0 { "-" } else { "+" }
}
