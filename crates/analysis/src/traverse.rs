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

/// Result of a full traversal.
#[derive(Debug, Default)]
pub struct Traversal {
    /// Every recorded path end, in discovery order (duplicates removed).
    pub stops: Vec<Stop>,
    /// Every BL, in discovery order.
    pub calls: Vec<Call>,
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
    let mut work: Vec<(u32, State, [Option<u32>; 16])> = seeds
        .iter()
        .map(|seed| (seed.addr, seed.state, [None; 16]))
        .collect();
    while let Some((addr, state, regs)) = work.pop() {
        let mut path = PathWalk {
            bytes,
            canon,
            map,
            options,
            visited: &mut visited,
            work: &mut work,
            out: &mut out,
        };
        path.visit(addr, state, regs);
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
    work: &'a mut Vec<(u32, State, [Option<u32>; 16])>,
    out: &'a mut Traversal,
}

impl PathWalk<'_, '_> {
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

    /// Enqueues a path continuation if it has not been visited.
    fn fork(&mut self, addr: u32, state: State, regs: [Option<u32>; 16]) {
        if self.visited.insert((addr, state)) {
            self.work.push((addr, state, regs));
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

    /// Marks `size` bytes of an instruction or pool word.
    fn mark(&mut self, addr: u32, size: usize, flags: Flags) {
        if self.options.mark {
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
                    self.mark(addr, 4, Flags::EXEC | Flags::WORD);
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
                                self.fork(addr.wrapping_add(4), state, regs);
                            } else if cond != Some(Condition::Al) {
                                self.fork(addr.wrapping_add(4), state, regs);
                            }
                            regs = [None; 16];
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
                                    regs = [None; 16];
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
                            self.mark(target, 4, Flags::LITERAL | Flags::WORD);
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
                    self.mark(addr, 2, Flags::EXEC | Flags::WORD);
                    match instr {
                        ThumbInstr::ConditionalBranch { cond: _, offset } => {
                            let target = addr
                                .wrapping_add(4)
                                .wrapping_add((i32::from(offset) * 2) as u32);
                            self.fork(addr.wrapping_add(2), state, regs);
                            regs = [None; 16];
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
                            self.fork(addr.wrapping_add(4), state, regs);
                            regs = [None; 16];
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
                            self.mark(target, 4, Flags::LITERAL | Flags::WORD);
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
                                    regs = [None; 16];
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
