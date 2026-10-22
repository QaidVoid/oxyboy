//! Recursive-descent traversal from seeded entry points.
//!
//! Walks every path a seed reaches, decoding with the shared decoder and
//! marking the flag map as it goes: `EXEC | WORD` on instruction bytes,
//! `LITERAL | WORD` on PC-relative load pools. Branch targets are new
//! paths, BL sites are recorded as calls, BX and MOV-to-PC follow
//! register values the path has proven (literal loads and immediate
//! MOVs), and bit 0 of a BX value switches ARM/THUMB state. Paths end on
//! undefined encodings, image bounds, and unknown branch targets, each
//! recorded as a [`Stop`].
//!
//! The traversal is conservative on purpose: it only marks bytes reached
//! by branch evidence, and it never speculates on register values it has
//! not seen the current path produce. Register proofs are per path and
//! reset at every taken jump, so a stale value can never cross a branch.

use std::collections::HashSet;

use decode::{
    ArmInstr, Condition, DataOp, Register, ShifterOperand, ThumbHiOp, ThumbImmOp, ThumbInstr,
    TransferOffset, decode_arm, decode_thumb,
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

/// Result of a full traversal.
#[derive(Debug, Default)]
pub struct Traversal {
    /// Every recorded path end, in discovery order (duplicates removed).
    pub stops: Vec<Stop>,
    /// `(call site, target)` pairs for every BL, in discovery order.
    pub calls: Vec<(u32, u32)>,
}

/// Traverses from `seeds`, marking `map`, over `image` bytes.
pub fn traverse(image: &Image, map: &mut FlagMap, seeds: &[Seed]) -> Traversal {
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
        self.map.mark_range(addr, addr + size as u32, flags);
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
                            if link {
                                self.out.calls.push((addr, target));
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
                        ArmInstr::BranchExchange { rn } => match regs[rn.bits() as usize] {
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
                        },
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
                            rd: Register::Pc, ..
                        } => {
                            self.stop(addr, state, StopReason::UnknownTarget);
                            return;
                        }
                        ArmInstr::BlockTransfer {
                            load, registers, ..
                        } if load && registers.contains(Register::Pc) => {
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
                            self.out.calls.push((addr, target));
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
                        } => match regs[rs.bits() as usize] {
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
                        },
                        ThumbInstr::HighRegisterOperation {
                            op: ThumbHiOp::Mov,
                            rd: Register::Pc,
                            rs,
                        } => match regs[rs.bits() as usize] {
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
                        },
                        ThumbInstr::HighRegisterOperation {
                            op: ThumbHiOp::Mov | ThumbHiOp::Add,
                            rd,
                            rs,
                        } if rd != Register::Pc => {
                            if let Some(value) = regs[rs.bits() as usize] {
                                regs[rd.bits() as usize] = Some(value);
                            }
                            addr = addr.wrapping_add(2);
                        }
                        ThumbInstr::PushPop {
                            pop: true,
                            include_pc_or_lr: true,
                            ..
                        } => return,
                        ThumbInstr::ImmediateOperation {
                            op: ThumbImmOp::Mov,
                            rd,
                            imm8,
                        } => {
                            regs[rd.bits() as usize] = Some(u32::from(imm8));
                            addr = addr.wrapping_add(2);
                        }
                        ThumbInstr::LoadStoreMultiple {
                            load: true,
                            registers,
                            ..
                        } if registers & (1 << Register::Pc.bits()) != 0 => return,
                        _ => addr = addr.wrapping_add(2),
                    }
                }
            }
        }
    }
}
