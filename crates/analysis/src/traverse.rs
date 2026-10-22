//! Recursive-descent traversal from seeded entry points.
//!
//! Walks every path a seed reaches, decoding with the shared decoder and
//! marking the flag map as it goes: `EXEC | WORD` on instruction bytes,
//! `LITERAL | WORD` on PC-relative load pools. Branch targets are new
//! paths, BL sites are recorded as calls, BX and MOV-to-PC follow
//! register values the path has proven (literal loads and immediate
//! MOVs), and bit 0 of a BX value switches ARM/THUMB state. Paths end on
//! undefined encodings, image bounds, return-like exits, and unknown
//! branch targets, each recorded as a [`Stop`].
//!
//! The traversal is conservative on purpose: it only marks bytes reached
//! by branch evidence, and it never speculates on register values it has
//! not seen the current path produce. Register proofs are per path and
//! reset at every taken jump, so a stale value can never cross a branch.
//! Return-like exits (BX LR, POP/LDM with PC, MOV PC from LR) are
//! conventionally well-behaved: LR is never proven, so they record
//! [`StopReason::Return`] instead of an unknown target.

use std::collections::HashSet;

use decode::{
    ArmInstr, Condition, DataOp, Register, ShifterOperand, ThumbHiOp, ThumbImmOp, ThumbInstr,
    ThumbShift, TransferOffset, decode_arm, decode_thumb,
};
use rom::Image;

use crate::flags::{FlagMap, Flags, Seed, State};

/// Why a traversal path ended without a defined successor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The encoding is architecturally undefined on the ARM7TDMI.
    Undefined,
    /// The path ran past the end of the image.
    OutOfBounds,
    /// A register-indirect branch whose target the path never proved.
    UnknownTarget,
    /// A return-like exit: BX LR, POP/LDM loading PC, or MOV PC from
    /// LR. LR is never proven, so the target is conventional.
    Return,
}

/// One recorded traversal end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stop {
    /// Canonical address where the path ended. For an out-of-bounds
    /// branch this is the unreachable target, not the branch site.
    pub addr: u32,
    /// State the path was in at `addr`.
    pub state: State,
    /// Why the path ended.
    pub reason: StopReason,
}

/// One recorded BL call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Call {
    /// Canonical address of the BL.
    pub site: u32,
    /// State of the caller (and, since BL preserves state, of the
    /// target too).
    pub state: State,
    /// Canonical address of the call target.
    pub target: u32,
}

/// One recorded software interrupt: a call into the BIOS that returns.
/// GBATEK "How BIOS Processes SWIs".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwiCall {
    /// Canonical address of the SWI instruction.
    pub site: u32,
    /// State of the SWI instruction.
    pub state: State,
    /// BIOS call number: the 8-bit number in THUMB state, the upper
    /// 8 bits of the 24-bit comment field in ARM state.
    pub number: u8,
}

/// One detected copy of code into EWRAM: a call matching the
/// CpuSet/CpuFastSet register convention (r0 source, r1 destination in
/// EWRAM, r2 control word, GBATEK "BIOS Memory Copy") followed by a BX
/// into the copied region. A "dynamically copied code" note consumed
/// by the hybrid fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CopyHint {
    /// Canonical address of the jump that entered the copied region.
    pub site: u32,
    /// Proven source address passed to the copy call.
    pub source: u32,
    /// Proven EWRAM destination of the copy.
    pub dest: u32,
    /// Copy size in bytes (control word count times 4).
    pub bytes: u32,
}

/// EWRAM, where multiboot images execute and cart code is copied to
/// (GBATEK "GBA Memory Map").
const EWRAM: std::ops::Range<u32> = 0x0200_0000..0x0204_0000;

/// Call arguments of an in-flight CpuSet/CpuFastSet, carried on the
/// fallthrough path after the call until a BX consumes them.
#[derive(Clone, Copy, Debug)]
struct CopyArgs {
    source: u32,
    dest: u32,
    words: u32,
}

/// One path waiting to be walked.
struct Path {
    addr: u32,
    state: State,
    regs: [Option<u32>; 16],
    pending: Option<CopyArgs>,
}

/// Result of a full traversal.
#[derive(Debug, Default)]
pub struct Traversal {
    /// Every recorded path end, in discovery order (duplicates removed).
    pub stops: Vec<Stop>,
    /// Every BL, in discovery order.
    pub calls: Vec<Call>,
    /// Every software interrupt, in discovery order. SWI is call-like
    /// but never terminal: the BIOS routine returns to the instruction
    /// after the SWI (GBATEK "How BIOS Processes SWIs").
    pub swi_calls: Vec<SwiCall>,
    /// Every detected copy-then-jump into EWRAM, in discovery order.
    pub copy_hints: Vec<CopyHint>,
}

/// Controls what a [`walk`] treats as a path boundary and whether it
/// writes marks.
#[derive(Clone, Copy, Debug)]
pub struct WalkOptions {
    /// Follow BL targets as new paths. When false (a function-body
    /// walk), BL records the call and the path continues at the
    /// fallthrough without recording an exit.
    pub follow_calls: bool,
    /// Write marks into the flag map.
    pub mark: bool,
}

impl WalkOptions {
    /// Global traversal: follow calls, mark the map.
    pub const fn global() -> Self {
        Self {
            follow_calls: true,
            mark: true,
        }
    }

    /// Function-body walk: treat BL as a call boundary, mark nothing.
    pub const fn function_body() -> Self {
        Self {
            follow_calls: false,
            mark: false,
        }
    }
}

/// Traverses from `seeds`, marking `map`, over `image` bytes.
pub fn traverse(image: &Image, map: &mut FlagMap, seeds: &[Seed]) -> Traversal {
    walk(image, map, seeds, WalkOptions::global())
}

/// Traverses from `seeds` with explicit options.
pub fn walk(image: &Image, map: &mut FlagMap, seeds: &[Seed], options: WalkOptions) -> Traversal {
    let bytes = image.bytes();
    let canon = map.canonical();
    let mut out = Traversal::default();
    let mut visited = HashSet::new();
    let mut work: Vec<Path> = seeds
        .iter()
        .map(|seed| Path {
            addr: seed.addr,
            state: seed.state,
            regs: [None; 16],
            pending: None,
        })
        .collect();
    while let Some(path) = work.pop() {
        let mut walk = PathWalk {
            bytes,
            canon,
            map,
            options,
            visited: &mut visited,
            work: &mut work,
            out: &mut out,
            pending: path.pending,
        };
        walk.visit(path.addr, path.state, path.regs);
    }
    out
}

/// One path walk over the shared analysis state.
struct PathWalk<'a, 'm> {
    bytes: &'a [u8],
    canon: crate::flags::Canonical,
    map: &'m mut FlagMap,
    options: WalkOptions,
    visited: &'a mut HashSet<(u32, State)>,
    work: &'a mut Vec<Path>,
    out: &'a mut Traversal,
    /// Copy arguments of an in-flight CpuSet/CpuFastSet call, if the
    /// path just returned from one.
    pending: Option<CopyArgs>,
}

impl PathWalk<'_, '_> {
    /// Call arguments for a copy hint when `r0..r2` match the
    /// CpuSet/CpuFastSet convention with an EWRAM destination.
    fn copy_args(&self, regs: &[Option<u32>; 16]) -> Option<CopyArgs> {
        let (Some(source), Some(dest), Some(control)) = (regs[0], regs[1], regs[2]) else {
            return None;
        };
        if !EWRAM.contains(&dest) || control == 0 {
            return None;
        }
        Some(CopyArgs {
            source,
            dest,
            // Control bits 0..20 hold the word count; bit 24 selects the
            // transfer width (GBATEK "BIOS Memory Copy").
            words: control & 0x001F_FFFF,
        })
    }

    /// Records a copy hint when this jump enters the region an
    /// in-flight copy call would have filled.
    fn enter_copied_region(&mut self, site: u32, target: u32) {
        if let Some(copy) = self.pending.take()
            && EWRAM.contains(&target)
            && target >= copy.dest
            && target < copy.dest + copy.words * 4
        {
            self.out.copy_hints.push(CopyHint {
                site,
                source: copy.source,
                dest: copy.dest,
                bytes: copy.words * 4,
            });
        }
    }

    /// Reads `size` bytes at a GBA address.
    fn read(&self, addr: u32, size: usize) -> Option<u32> {
        let offset = self.canon.offset(addr)?;
        if offset + size > self.bytes.len() {
            return None;
        }
        let mut raw = [0u8; 4];
        raw[..size].copy_from_slice(&self.bytes[offset..offset + size]);
        Some(u32::from_le_bytes(raw))
    }

    fn read_word(&self, addr: u32) -> Option<u32> {
        self.read(addr, 4)
    }

    /// Enqueues a path continuation if it has not been visited. The
    /// fork inherits the copy arguments: they belong to the caller's
    /// argument registers, which survive a call return.
    fn fork(
        &mut self,
        addr: u32,
        state: State,
        regs: [Option<u32>; 16],
        pending: Option<CopyArgs>,
    ) {
        if self.visited.insert((addr, state)) {
            self.work.push(Path {
                addr,
                state,
                regs,
                pending,
            });
        }
    }

    /// Continues the current walk inline at a taken jump, ending the path
    /// when the target was already visited.
    fn jump(&mut self, addr: u32, state: State) -> bool {
        self.visited.insert((addr, state))
    }

    fn stop(&mut self, addr: u32, state: State, reason: StopReason) {
        let stop = Stop {
            addr,
            state,
            reason,
        };
        if !self.out.stops.contains(&stop) {
            self.out.stops.push(stop);
        }
    }

    /// Marks `size` bytes of an instruction or pool word. Instruction
    /// marks (`flags` containing EXEC) carry the THUMB state bit so
    /// emission can pick the assembler dialect per run.
    fn mark(&mut self, addr: u32, size: usize, state: State, flags: Flags) {
        if self.options.mark {
            let flags = if flags.contains(Flags::EXEC) && state == State::Thumb {
                flags | Flags::THUMB
            } else {
                flags
            };
            self.map.mark_range(addr, addr + size as u32, flags);
        }
    }

    /// Begins a walk. Returns when the path ends; the walk owns `regs`
    /// from here on.
    fn visit(&mut self, addr: u32, state: State, mut regs: [Option<u32>; 16]) {
        let mut addr = addr;
        let mut state = state;
        loop {
            match state {
                State::Arm => {
                    let Some(word) = self.read_word(addr) else {
                        self.stop(addr, state, StopReason::OutOfBounds);
                        return;
                    };
                    if matches!(decode_arm(word), ArmInstr::Undefined { .. }) {
                        self.stop(addr, state, StopReason::Undefined);
                        return;
                    }
                    let cond = Condition::from_bits((word >> 28) as u8);
                    self.mark(addr, 4, state, Flags::EXEC | Flags::WORD);
                    match decode_arm(word) {
                        ArmInstr::Branch { link, offset } => {
                            let target = addr.wrapping_add(8).wrapping_add(offset as u32);
                            if link && !self.options.follow_calls {
                                self.out.calls.push(Call {
                                    site: addr,
                                    state,
                                    target,
                                });
                                addr = addr.wrapping_add(4);
                                continue;
                            }
                            if link {
                                self.out.calls.push(Call {
                                    site: addr,
                                    state,
                                    target,
                                });
                                let args = self.copy_args(&regs);
                                self.fork(addr.wrapping_add(4), state, regs, args);
                            } else if cond != Some(Condition::Al) {
                                let pending = self.pending;
                                self.fork(addr.wrapping_add(4), state, regs, pending);
                            }
                            regs = [None; 16];
                            self.pending = None;
                            if !self.jump(target, state) {
                                return;
                            }
                            addr = target;
                        }
                        ArmInstr::BranchExchange { rn } => {
                            if rn == Register::Lr && regs[rn.bits() as usize].is_none() {
                                self.stop(addr, state, StopReason::Return);
                                return;
                            }
                            match regs[rn.bits() as usize] {
                                Some(value) => {
                                    state = if value & 1 == 1 {
                                        State::Thumb
                                    } else {
                                        State::Arm
                                    };
                                    self.enter_copied_region(addr, value & !1);
                                    regs = [None; 16];
                                    self.pending = None;
                                    if !self.jump(value & !1, state) {
                                        return;
                                    }
                                    addr = value & !1;
                                }
                                None => {
                                    self.stop(addr, state, StopReason::UnknownTarget);
                                    return;
                                }
                            }
                        }
                        ArmInstr::LoadStore {
                            load: true,
                            pre_index: true,
                            rn: Register::Pc,
                            rd,
                            offset: TransferOffset::Immediate(imm),
                            up,
                            ..
                        } => {
                            let target = addr.wrapping_add(8).wrapping_add_signed(if up {
                                imm as i32
                            } else {
                                -(imm as i32)
                            });
                            self.mark(target, 4, State::Arm, Flags::LITERAL | Flags::WORD);
                            regs[rd.bits() as usize] = self.read_word(target);
                            addr = addr.wrapping_add(4);
                        }
                        ArmInstr::DataProcessing {
                            opcode: DataOp::Mov,
                            rd: Register::Pc,
                            operand2: ShifterOperand::Immediate { rotate, value },
                            ..
                        } => {
                            regs = [None; 16];
                            self.pending = None;
                            let target = u32::from(value).rotate_right(u32::from(rotate) * 2);
                            if !self.jump(target, state) {
                                return;
                            }
                            addr = target;
                        }
                        ArmInstr::DataProcessing {
                            opcode: DataOp::Mov,
                            rd: Register::Pc,
                            operand2:
                                ShifterOperand::Register {
                                    rm: Register::Lr, ..
                                },
                            ..
                        } => {
                            self.stop(addr, state, StopReason::Return);
                            return;
                        }
                        ArmInstr::DataProcessing {
                            opcode: DataOp::Mov,
                            rd: Register::Pc,
                            operand2: ShifterOperand::Register { rm, .. },
                            ..
                        } => {
                            // MOV to PC branches without interworking:
                            // ARMv4 data-processing writes to R15 clear
                            // PC bits 1:0 and never switch state (ARM
                            // ARM "Data Processing": only BX reads bit
                            // 0 for state).
                            match regs[rm.bits() as usize] {
                                Some(value) => {
                                    regs = [None; 16];
                                    self.pending = None;
                                    let target = value & !3;
                                    if !self.jump(target, state) {
                                        return;
                                    }
                                    addr = target;
                                }
                                None => {
                                    self.stop(addr, state, StopReason::UnknownTarget);
                                    return;
                                }
                            }
                        }
                        ArmInstr::DataProcessing {
                            rd: Register::Pc, ..
                        } => {
                            self.stop(addr, state, StopReason::UnknownTarget);
                            return;
                        }
                        ArmInstr::BlockTransfer {
                            load, registers, ..
                        } if load && registers.contains(Register::Pc) => {
                            self.stop(addr, state, StopReason::Return);
                            return;
                        }
                        ArmInstr::DataProcessing {
                            opcode: DataOp::Mov,
                            rd,
                            operand2: ShifterOperand::Immediate { rotate, value },
                            ..
                        } => {
                            regs[rd.bits() as usize] =
                                Some(u32::from(value).rotate_right(u32::from(rotate) * 2));
                            addr = addr.wrapping_add(4);
                        }
                        ArmInstr::SoftwareInterrupt { imm24 } => {
                            self.out.swi_calls.push(SwiCall {
                                site: addr,
                                state,
                                // ARM state takes the number from the
                                // upper 8 bits of the comment field
                                // (GBATEK "How BIOS Processes SWIs").
                                number: (imm24 >> 16) as u8,
                            });
                            // r0, r1, r3 return garbage or the result;
                            // all other registers are preserved.
                            regs[0] = None;
                            regs[1] = None;
                            regs[3] = None;
                            addr = addr.wrapping_add(4);
                        }
                        // Any other register-writing instruction produces a
                        // value the traversal does not model, so the
                        // destination's stale proof is invalidated.
                        ArmInstr::DataProcessing { rd, .. }
                        | ArmInstr::Multiply { rd, .. }
                        | ArmInstr::Swap { rd, .. }
                        | ArmInstr::PsrRead { rd, .. }
                        | ArmInstr::LoadStore { rd, .. }
                        | ArmInstr::LoadStoreHalfwordSigned { rd, .. } => {
                            regs[rd.bits() as usize] = None;
                            addr = addr.wrapping_add(4);
                        }
                        ArmInstr::MultiplyLong { rd_hi, rd_lo, .. } => {
                            regs[rd_hi.bits() as usize] = None;
                            regs[rd_lo.bits() as usize] = None;
                            addr = addr.wrapping_add(4);
                        }
                        _ => addr = addr.wrapping_add(4),
                    }
                }
                State::Thumb => {
                    let Some(half) = self.read(addr, 2) else {
                        self.stop(addr, state, StopReason::OutOfBounds);
                        return;
                    };
                    let next = self.read(addr.wrapping_add(2), 2).unwrap_or(0) as u16;
                    let instr = decode_thumb(half as u16, next);
                    if matches!(instr, ThumbInstr::Undefined { .. }) {
                        self.stop(addr, state, StopReason::Undefined);
                        return;
                    }
                    self.mark(addr, 2, state, Flags::EXEC | Flags::WORD);
                    match instr {
                        ThumbInstr::ConditionalBranch { cond: _, offset } => {
                            let target = addr
                                .wrapping_add(4)
                                .wrapping_add((i32::from(offset) * 2) as u32);
                            let pending = self.pending;
                            self.fork(addr.wrapping_add(2), state, regs, pending);
                            regs = [None; 16];
                            self.pending = None;
                            if !self.jump(target, state) {
                                return;
                            }
                            addr = target;
                        }
                        ThumbInstr::UnconditionalBranch { offset } => {
                            let target = addr
                                .wrapping_add(4)
                                .wrapping_add((i32::from(offset) * 2) as u32);
                            regs = [None; 16];
                            self.pending = None;
                            if !self.jump(target, state) {
                                return;
                            }
                            addr = target;
                        }
                        ThumbInstr::LongBranchWithLink { offset } => {
                            let target = addr.wrapping_add(4).wrapping_add((offset * 2) as u32);
                            self.out.calls.push(Call {
                                site: addr,
                                state,
                                target,
                            });
                            if !self.options.follow_calls {
                                addr = addr.wrapping_add(4);
                                continue;
                            }
                            let args = self.copy_args(&regs);
                            self.fork(addr.wrapping_add(4), state, regs, args);
                            regs = [None; 16];
                            self.pending = None;
                            if !self.jump(target, state) {
                                return;
                            }
                            addr = target;
                        }
                        ThumbInstr::LongBranchWithLinkFromLr { .. } => {
                            self.stop(addr, state, StopReason::UnknownTarget);
                            return;
                        }
                        ThumbInstr::PcRelativeLoad { rd, offset } => {
                            let target = (addr.wrapping_add(4) & !3) + u32::from(offset) * 4;
                            self.mark(target, 4, State::Arm, Flags::LITERAL | Flags::WORD);
                            regs[rd.bits() as usize] = self.read_word(target);
                            addr = addr.wrapping_add(2);
                        }
                        ThumbInstr::HighRegisterOperation {
                            op: ThumbHiOp::Bx,
                            rd: _,
                            rs,
                        } => {
                            if rs == Register::Lr && regs[rs.bits() as usize].is_none() {
                                self.stop(addr, state, StopReason::Return);
                                return;
                            }
                            match regs[rs.bits() as usize] {
                                Some(value) => {
                                    state = if value & 1 == 1 {
                                        State::Thumb
                                    } else {
                                        State::Arm
                                    };
                                    self.enter_copied_region(addr, value & !1);
                                    regs = [None; 16];
                                    self.pending = None;
                                    if !self.jump(value & !1, state) {
                                        return;
                                    }
                                    addr = value & !1;
                                }
                                None => {
                                    self.stop(addr, state, StopReason::UnknownTarget);
                                    return;
                                }
                            }
                        }
                        ThumbInstr::HighRegisterOperation {
                            op: ThumbHiOp::Mov,
                            rd: Register::Pc,
                            rs,
                        } => {
                            if rs == Register::Lr && regs[rs.bits() as usize].is_none() {
                                self.stop(addr, state, StopReason::Return);
                                return;
                            }
                            match regs[rs.bits() as usize] {
                                Some(value) => {
                                    regs = [None; 16];
                                    self.pending = None;
                                    let target = value & !1;
                                    if !self.jump(target, state) {
                                        return;
                                    }
                                    addr = target;
                                }
                                None => {
                                    self.stop(addr, state, StopReason::UnknownTarget);
                                    return;
                                }
                            }
                        }
                        ThumbInstr::HighRegisterOperation {
                            op: ThumbHiOp::Mov | ThumbHiOp::Add,
                            rd,
                            rs,
                        } if rd != Register::Pc => {
                            regs[rd.bits() as usize] = regs[rs.bits() as usize];
                            addr = addr.wrapping_add(2);
                        }
                        ThumbInstr::PushPop {
                            pop: true,
                            include_pc_or_lr: true,
                            ..
                        } => {
                            self.stop(addr, state, StopReason::Return);
                            return;
                        }
                        ThumbInstr::ShiftedImmediate { kind, rd, rm, imm5 } => {
                            regs[rd.bits() as usize] =
                                regs[rm.bits() as usize].map(|value| match kind {
                                    ThumbShift::Lsl => value << imm5,
                                    ThumbShift::Lsr => {
                                        if imm5 == 0 {
                                            0
                                        } else {
                                            value >> imm5
                                        }
                                    }
                                    ThumbShift::Asr => {
                                        if imm5 == 0 {
                                            (value as i32 >> 31) as u32
                                        } else {
                                            ((value as i32) >> imm5) as u32
                                        }
                                    }
                                });
                            addr = addr.wrapping_add(2);
                        }
                        ThumbInstr::ImmediateOperation {
                            op: ThumbImmOp::Mov,
                            rd,
                            imm8,
                        } => {
                            regs[rd.bits() as usize] = Some(u32::from(imm8));
                            addr = addr.wrapping_add(2);
                        }
                        ThumbInstr::SoftwareInterrupt { value } => {
                            self.out.swi_calls.push(SwiCall {
                                site: addr,
                                state,
                                number: value,
                            });
                            regs[0] = None;
                            regs[1] = None;
                            regs[3] = None;
                            addr = addr.wrapping_add(2);
                        }
                        // Mirror of the ARM rule: instructions that write a
                        // register without producing a modeled value drop
                        // its stale proof.
                        ThumbInstr::AddSubtract { rd, .. }
                        | ThumbInstr::ImmediateOperation { rd, .. }
                        | ThumbInstr::AluOperation { rd, .. }
                        | ThumbInstr::LoadStoreRegisterOffset { rd, .. }
                        | ThumbInstr::LoadStoreSignedRegisterOffset { rd, .. }
                        | ThumbInstr::LoadStoreImmediateOffset { rd, .. }
                        | ThumbInstr::LoadStoreHalfwordImmediate { rd, .. }
                        | ThumbInstr::LoadStoreStack { rd, .. }
                        | ThumbInstr::LoadAddress { rd, .. } => {
                            regs[rd.bits() as usize] = None;
                            addr = addr.wrapping_add(2);
                        }
                        ThumbInstr::PushPop {
                            pop: true,
                            include_pc_or_lr: false,
                            registers,
                        }
                        | ThumbInstr::LoadStoreMultiple {
                            load: true,
                            registers,
                            ..
                        } => {
                            for (bit, reg) in regs.iter_mut().enumerate().take(8) {
                                if registers & (1 << bit) != 0 {
                                    *reg = None;
                                }
                            }
                            addr = addr.wrapping_add(2);
                        }
                        _ => addr = addr.wrapping_add(2),
                    }
                }
            }
        }
    }
}
