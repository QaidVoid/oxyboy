//! Function-level lifting pipeline: analysis boundaries to IR
//! [`Function`]s.
//!
//! The control flow comes from the analysis's flag map: a linear decode
//! walk per function region splits basic blocks at branch targets, and
//! terminators mirror the traversal's stop kinds (direct branch, call,
//! return, indirect exit). Per-instruction semantics are stubbed as
//! `unhandled` intrinsics until the ARM and THUMB
//! lifters replace them class by class; the state threading is
//! likewise identity for now, with per-instruction renames arriving
//! with the real lifters.

use std::collections::{BTreeMap, HashMap};

use analysis::{Canonical, FlagMap, Flags, State};
use decode::{
    ArmInstr, Condition, Register, ShifterOperand, ThumbHiOp, ThumbInstr, decode_arm, decode_thumb,
};

use crate::{Block, BlockId, Function, Inst, Op, SLOTS, Terminator, Value, ValueId};

/// The number of machine state slots.
const STATE_COUNT: usize = SLOTS.len();

/// One liftable function region: `[start, end)` in canonical
/// addresses, entered in `state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionRegion {
    /// Canonical address of the first instruction.
    pub start: u32,
    /// Exclusive canonical end (the next function start or image end).
    pub end: u32,
    /// Execution state at `start`.
    pub state: State,
}

/// Splits the image into function regions at FUNC_START runs.
#[must_use]
pub fn function_regions(image: &rom::Image, map: &FlagMap) -> Vec<FunctionRegion> {
    let len = image.bytes().len();
    let canon = map.canonical();
    let base = canon.base();
    let mut regions = Vec::new();
    let mut index = 0usize;
    while index < len {
        let run = map.get(base + index as u32);
        if run.contains(Flags::FUNC_START) {
            let state = if run.contains(Flags::THUMB) {
                State::Thumb
            } else {
                State::Arm
            };
            let mut end = index + 1;
            while end < len && map.get(base + end as u32).contains(Flags::FUNC_START) {
                end += 1;
            }
            regions.push(FunctionRegion {
                start: base + index as u32,
                end: base + end as u32,
                state,
            });
            index = end;
        } else {
            index += 1;
        }
    }
    regions
}

/// Lifts one function region to IR.
#[must_use]
pub fn lift_function(image: &rom::Image, map: &FlagMap, region: FunctionRegion) -> Function {
    Lifter {
        image,
        map,
        canon: map.canonical(),
        region,
        raw: BTreeMap::new(),
    }
    .lift()
}

/// One undispatched block: decoded instructions plus a raw terminator
/// still naming addresses.
struct RawBlock {
    insts: Vec<Inst>,
    term: RawTerm,
}

/// Terminator before block ids exist.
enum RawTerm {
    /// Direct jump to an in-region leader.
    Branch(u32),
    /// Conditional jump; either arm may leave the region.
    BranchCond {
        cond: Condition,
        then_target: u32,
        otherwise: u32,
    },
    /// Call to a static target with an optional in-region continuation.
    Call { target: u32, cont: Option<u32> },
    /// Return-like exit.
    Return,
    /// Register-indirect exit.
    IndirectExit(Register),
    /// Undefined encoding or walked into non-code.
    Unreachable,
}

/// The phase-1 walker.
struct Lifter<'a> {
    image: &'a rom::Image,
    map: &'a FlagMap,
    canon: Canonical,
    region: FunctionRegion,
    raw: BTreeMap<u32, RawBlock>,
}

impl Lifter<'_> {
    /// Walks every leader, then renders the sorted blocks as IR.
    fn lift(mut self) -> Function {
        let mut work = vec![self.region.start];
        while let Some(addr) = work.pop() {
            if self.raw.contains_key(&addr) {
                continue;
            }
            work.extend(self.walk(addr));
        }
        self.render()
    }

    /// Whether `addr` lies inside the region.
    fn in_region(&self, addr: u32) -> bool {
        addr >= self.region.start && addr < self.region.end
    }

    /// Whether the byte at `addr` is marked executed.
    fn executed(&self, addr: u32) -> bool {
        self.map.get(addr).contains(Flags::EXEC)
    }

    /// The 32-bit word at `addr`, zero past the image end.
    fn word(&self, addr: u32) -> u32 {
        match self.canon.offset(addr) {
            Some(index) => {
                let bytes = self.image.bytes();
                bytes
                    .get(index..index + 4)
                    .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
                    .map_or(0, u32::from_le_bytes)
            }
            None => 0,
        }
    }

    /// The halfword at `addr`, zero past the image end.
    fn half(&self, addr: u32) -> u16 {
        match self.canon.offset(addr) {
            Some(index) => {
                let bytes = self.image.bytes();
                bytes
                    .get(index..index + 2)
                    .and_then(|slice| <[u8; 2]>::try_from(slice).ok())
                    .map_or(0, u16::from_le_bytes)
            }
            None => 0,
        }
    }

    /// Decodes and classifies from `from` to the first terminator,
    /// returning the leader addresses the terminator targets.
    fn walk(&mut self, from: u32) -> Vec<u32> {
        let mut leaders = Vec::new();
        let mut insts = Vec::new();
        let mut addr = from;
        let term = loop {
            if addr >= self.region.end || !self.executed(addr) {
                break RawTerm::Unreachable;
            }
            match self.region.state {
                State::Arm => {
                    let word = self.word(addr);
                    let guard = Condition::from_bits((word >> 28) as u8);
                    match decode_arm(word) {
                        ArmInstr::Branch { link, offset } => {
                            let target = addr.wrapping_add(8).wrapping_add_signed(offset);
                            if link {
                                let cont = addr + 4;
                                let linked = self.in_region(cont);
                                if linked {
                                    leaders.push(cont);
                                }
                                break RawTerm::Call {
                                    target,
                                    cont: linked.then_some(cont),
                                };
                            }
                            if let Some(cond) = guard.filter(|cond| *cond != Condition::Al) {
                                let fall = addr + 4;
                                leaders.push(fall);
                                if self.in_region(target) {
                                    leaders.push(target);
                                }
                                break RawTerm::BranchCond {
                                    cond,
                                    then_target: target,
                                    otherwise: fall,
                                };
                            }
                            if self.in_region(target) {
                                leaders.push(target);
                                break RawTerm::Branch(target);
                            }
                            break RawTerm::Call { target, cont: None };
                        }
                        ArmInstr::BranchExchange { rn } => {
                            break if rn == Register::Lr {
                                RawTerm::Return
                            } else {
                                RawTerm::IndirectExit(rn)
                            };
                        }
                        ArmInstr::DataProcessing {
                            rd: Register::Pc,
                            operand2,
                            ..
                        } => {
                            // `mov pc, rX` is the common indirect jump;
                            // other ALU writes to pc keep the pc value
                            // as the exit target until the real lifter
                            // computes the sum.
                            break RawTerm::IndirectExit(match operand2 {
                                ShifterOperand::Register { rm, .. } => rm,
                                _ => Register::Pc,
                            });
                        }
                        ArmInstr::LoadStore { load, rd, .. } if load && rd == Register::Pc => {
                            break RawTerm::Return;
                        }
                        ArmInstr::BlockTransfer {
                            load, registers, ..
                        } if load && registers.contains(Register::Pc) => {
                            break RawTerm::Return;
                        }
                        ArmInstr::Undefined { .. } => break RawTerm::Unreachable,
                        _ => {
                            insts.push(stub(guard));
                            addr += 4;
                        }
                    }
                }
                State::Thumb => {
                    let first = self.half(addr);
                    let next = self.half(addr + 2);
                    match decode_thumb(first, next) {
                        ThumbInstr::ConditionalBranch { cond, offset } => {
                            let target = addr
                                .wrapping_add(4)
                                .wrapping_add_signed(i32::from(offset) * 2);
                            let fall = addr + 2;
                            leaders.push(fall);
                            if self.in_region(target) {
                                leaders.push(target);
                            }
                            break RawTerm::BranchCond {
                                cond,
                                then_target: target,
                                otherwise: fall,
                            };
                        }
                        ThumbInstr::UnconditionalBranch { offset } => {
                            let target = addr
                                .wrapping_add(4)
                                .wrapping_add_signed(i32::from(offset) * 2);
                            if self.in_region(target) {
                                leaders.push(target);
                                break RawTerm::Branch(target);
                            }
                            break RawTerm::Call { target, cont: None };
                        }
                        ThumbInstr::LongBranchWithLink { offset } => {
                            let target = addr.wrapping_add(4).wrapping_add_signed(offset * 2);
                            let cont = addr + 4;
                            let linked = self.in_region(cont);
                            if linked {
                                leaders.push(cont);
                            }
                            break RawTerm::Call {
                                target,
                                cont: linked.then_some(cont),
                            };
                        }
                        ThumbInstr::LongBranchWithLinkFromLr { .. } => {
                            break RawTerm::IndirectExit(Register::Lr);
                        }
                        ThumbInstr::HighRegisterOperation { op, rd, rs } => match (op, rd) {
                            (ThumbHiOp::Bx, _) => {
                                break if rs == Register::Lr {
                                    RawTerm::Return
                                } else {
                                    RawTerm::IndirectExit(rs)
                                };
                            }
                            (ThumbHiOp::Add | ThumbHiOp::Mov, Register::Pc) => {
                                break RawTerm::IndirectExit(rs);
                            }
                            _ => {
                                insts.push(stub(None));
                                addr += 2;
                            }
                        },
                        ThumbInstr::PushPop {
                            pop,
                            include_pc_or_lr,
                            ..
                        } if pop && include_pc_or_lr => break RawTerm::Return,
                        ThumbInstr::Undefined { .. } => break RawTerm::Unreachable,
                        _ => {
                            insts.push(stub(None));
                            addr += 2;
                        }
                    }
                }
            }
        };
        leaders.retain(|&leader| self.in_region(leader));
        self.raw.insert(from, RawBlock { insts, term });
        leaders
    }

    /// Renders the walked blocks as IR: blocks in address order, the
    /// entry block carrying the incoming machine state, every edge
    /// passing the source block's (for now, identity) state values.
    fn render(self) -> Function {
        let mut ids: HashMap<u32, BlockId> = HashMap::new();
        let order: Vec<(u32, RawBlock)> = self.raw.into_iter().collect();
        for (index, (addr, _)) in order.iter().enumerate() {
            ids.insert(*addr, BlockId(index as u32));
        }
        // Exit helpers for transfers leaving the region, numbered
        // after the real blocks.
        let outside: Vec<u32> = order
            .iter()
            .flat_map(|(_, raw)| match &raw.term {
                RawTerm::Branch(to) => vec![*to],
                RawTerm::BranchCond {
                    then_target,
                    otherwise,
                    ..
                } => vec![*then_target, *otherwise],
                _ => Vec::new(),
            })
            .filter(|addr| !ids.contains_key(addr))
            .collect();
        let mut helpers: BTreeMap<u32, BlockId> = BTreeMap::new();
        for addr in outside {
            let next = (order.len() + helpers.len()) as u32;
            helpers.entry(addr).or_insert(BlockId(next));
        }

        let mut next_id = ValueId(STATE_COUNT as u32);
        let mut blocks = Vec::with_capacity(order.len() + helpers.len());
        let mut make_params = || -> (Vec<ValueId>, Vec<Value>) {
            let mut params = Vec::with_capacity(STATE_COUNT);
            for _ in 0..STATE_COUNT {
                params.push(next_id);
                next_id = ValueId(next_id.0 + 1);
            }
            let current: Vec<Value> = params.iter().copied().map(Value::Id).collect();
            (params, current)
        };
        let (entry_params, entry_current) = make_params();
        for (index, (_, raw)) in order.iter().enumerate() {
            let (params, current) = if index == 0 {
                (entry_params.clone(), entry_current.clone())
            } else {
                make_params()
            };
            let term = match &raw.term {
                RawTerm::Branch(to) => Terminator::Branch {
                    target: block_or_helper(&ids, &helpers, *to),
                    args: current.clone(),
                },
                RawTerm::BranchCond {
                    cond,
                    then_target,
                    otherwise,
                } => Terminator::BranchCond {
                    cond: *cond,
                    then_target: block_or_helper(&ids, &helpers, *then_target),
                    then_args: current.clone(),
                    else_target: block_or_helper(&ids, &helpers, *otherwise),
                    else_args: current.clone(),
                },
                RawTerm::Call { target, cont } => Terminator::Call {
                    target: Value::Const(*target),
                    cont: cont.map(|addr| (ids[&addr], current.clone())),
                },
                RawTerm::Return => Terminator::Return {
                    state: current.clone(),
                },
                RawTerm::IndirectExit(reg) => Terminator::IndirectExit {
                    target: current[reg.bits() as usize],
                },
                RawTerm::Unreachable => Terminator::Unreachable,
            };
            blocks.push(Block {
                params,
                insts: raw.insts.clone(),
                term,
            });
        }
        for (addr, &id) in &helpers {
            let (params, _) = make_params();
            blocks[id.0 as usize] = Block {
                params,
                insts: Vec::new(),
                term: Terminator::Call {
                    target: Value::Const(*addr),
                    cont: None,
                },
            };
        }
        Function {
            name: format!("sub_{:08x}", self.region.start),
            blocks,
        }
    }
}

/// The block id for `addr`, or an exit helper.
fn block_or_helper(
    ids: &HashMap<u32, BlockId>,
    helpers: &BTreeMap<u32, BlockId>,
    addr: u32,
) -> BlockId {
    ids.get(&addr).copied().unwrap_or_else(|| helpers[&addr])
}

/// The placeholder instruction standing in for real per-class lifting.
fn stub(guard: Option<Condition>) -> Inst {
    Inst {
        guard,
        dests: Vec::new(),
        op: Op::Intrinsic {
            name: "unhandled",
            args: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use analysis::Flags;

    /// Builds a small image, marks `count` ARM words executed, and
    /// lifts them as one region from the canonical base.
    fn lift_arm(words: &[u32], count: usize) -> Function {
        let mut bytes = vec![0u8; 256];
        for (index, word) in words.iter().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        let image = rom::Image::from_bytes(bytes).expect("parses");
        let mut map = FlagMap::from_image(&image);
        let base = map.canonical().base();
        map.mark_range(base, base + count as u32 * 4, Flags::EXEC);
        map.set(base, Flags::EXEC | Flags::FUNC_START);
        lift_function(
            &image,
            &map,
            FunctionRegion {
                start: base,
                end: base + count as u32 * 4,
                state: State::Arm,
            },
        )
    }

    /// The ARM twin over THUMB halfwords.
    fn lift_thumb(halves: &[u16], count: usize) -> Function {
        let mut bytes = vec![0u8; 256];
        for (index, half) in halves.iter().enumerate() {
            bytes[index * 2..index * 2 + 2].copy_from_slice(&half.to_le_bytes());
        }
        let image = rom::Image::from_bytes(bytes).expect("parses");
        let mut map = FlagMap::from_image(&image);
        let base = map.canonical().base();
        map.mark_range(base, base + count as u32 * 2, Flags::EXEC);
        map.set(base, Flags::EXEC | Flags::FUNC_START | Flags::THUMB);
        lift_function(
            &image,
            &map,
            FunctionRegion {
                start: base,
                end: base + count as u32 * 2,
                state: State::Thumb,
            },
        )
    }

    #[test]
    fn straight_line_arm_is_one_returning_block() {
        let function = lift_arm(&[0xE280_0001, 0xE12F_FF1E], 2);
        assert_eq!(function.blocks.len(), 1);
        let entry = &function.blocks[0];
        assert_eq!(entry.params.len(), STATE_COUNT);
        // Only the add lands in the block; bx lr is the terminator.
        assert_eq!(entry.insts.len(), 1);
        match &entry.term {
            Terminator::Return { state } => assert_eq!(state.len(), STATE_COUNT),
            other => panic!("expected return, got {other:?}"),
        }
    }

    #[test]
    fn conditional_branch_makes_two_successors() {
        // bne +0 (target = base+8), addcs, bx lr.
        let function = lift_arm(&[0x1A00_0000, 0x2280_0001, 0xE12F_FF1E], 3);
        assert_eq!(function.blocks.len(), 3);
        let entry = &function.blocks[0];
        assert_eq!(entry.insts.len(), 0);
        match &entry.term {
            Terminator::BranchCond {
                cond,
                then_target,
                else_target,
                ..
            } => {
                assert_eq!(*cond, Condition::Ne);
                // Leaders sort by address: base, base+4, base+8.
                assert_eq!(*then_target, BlockId(2));
                assert_eq!(*else_target, BlockId(1));
            }
            other => panic!("expected conditional branch, got {other:?}"),
        }
        // The fallthrough add carries its decoded condition as a guard.
        assert_eq!(
            function.blocks[1].insts[0].guard,
            Some(Condition::Cs),
            "{:?}",
            function.blocks[1].insts[0]
        );
        assert!(matches!(function.blocks[1].term, Terminator::Return { .. }));
        assert!(matches!(function.blocks[2].term, Terminator::Return { .. }));
    }

    #[test]
    fn bl_makes_a_call_with_a_continuation() {
        // bl base+0x10 (outside), add, bx lr; region covers three words.
        let function = lift_arm(&[0xEB00_0002, 0xE280_0001, 0xE12F_FF1E], 3);
        assert_eq!(function.blocks.len(), 2);
        match &function.blocks[0].term {
            Terminator::Call { target, cont } => {
                assert!(matches!(target, Value::Const(word) if word % 4 == 0));
                assert_eq!(
                    *cont,
                    Some((
                        BlockId(1),
                        function.blocks[0]
                            .params
                            .iter()
                            .copied()
                            .map(Value::Id)
                            .collect()
                    ))
                );
            }
            other => panic!("expected call, got {other:?}"),
        }
        // The add; the bx lr is the terminator.
        assert_eq!(function.blocks[1].insts.len(), 1);
    }

    #[test]
    fn branch_outside_the_region_exits_through_a_call_helper() {
        // b base+0xC with a two-word region: the jump leaves the
        // region, so the block exits through a call-shaped terminator.
        let function = lift_arm(&[0xEA00_0001, 0xE280_0001], 2);
        assert_eq!(function.blocks.len(), 1);
        match &function.blocks[0].term {
            Terminator::Call { target, cont: None } => assert!(matches!(target, Value::Const(_))),
            other => panic!("expected exit call, got {other:?}"),
        }
    }

    #[test]
    fn unconditional_branch_in_region_makes_one_edge() {
        // b base+8 (skipping the add), add, bx lr.
        let function = lift_arm(&[0xEA00_0000, 0xE280_0001, 0xE12F_FF1E], 3);
        assert_eq!(function.blocks.len(), 2);
        match &function.blocks[0].term {
            Terminator::Branch { target, args } => {
                assert_eq!(*target, BlockId(1));
                assert_eq!(args.len(), STATE_COUNT);
            }
            other => panic!("expected branch, got {other:?}"),
        }
        assert!(matches!(function.blocks[1].term, Terminator::Return { .. }));
    }

    #[test]
    fn undefined_encoding_is_unreachable() {
        let function = lift_arm(&[0xF000_0000], 1);
        assert_eq!(function.blocks.len(), 1);
        assert!(matches!(function.blocks[0].term, Terminator::Unreachable));
    }

    #[test]
    fn thumb_bx_register_is_an_indirect_exit() {
        let function = lift_thumb(&[0x4718], 1);
        assert_eq!(function.blocks.len(), 1);
        match &function.blocks[0].term {
            Terminator::IndirectExit { target } => {
                assert_eq!(*target, Value::Id(function.blocks[0].params[3]));
            }
            other => panic!("expected indirect exit, got {other:?}"),
        }
    }

    #[test]
    fn thumb_pop_pc_is_a_return() {
        let function = lift_thumb(&[0xBD01], 1);
        assert!(matches!(function.blocks[0].term, Terminator::Return { .. }));
    }

    #[test]
    fn thumb_long_branch_calls_and_continues() {
        // bl +0, movs r0, #1, bx lr.
        let function = lift_thumb(&[0xF000, 0xF800, 0x2001, 0x4770], 4);
        assert_eq!(function.blocks.len(), 2);
        match &function.blocks[0].term {
            Terminator::Call { target, cont } => {
                assert!(matches!(target, Value::Const(word) if *word % 2 == 0));
                assert_eq!(
                    *cont,
                    Some((
                        BlockId(1),
                        function.blocks[0]
                            .params
                            .iter()
                            .copied()
                            .map(Value::Id)
                            .collect()
                    ))
                );
            }
            other => panic!("expected call, got {other:?}"),
        }
        assert_eq!(function.blocks[1].insts.len(), 1);
        assert!(matches!(function.blocks[1].term, Terminator::Return { .. }));
    }

    #[test]
    fn regions_split_at_func_starts() {
        let bytes = vec![0u8; 256];
        let image = rom::Image::from_bytes(bytes).expect("parses");
        let mut map = FlagMap::from_image(&image);
        let base = map.canonical().base();
        map.mark_range(base, base + 4, Flags::EXEC | Flags::FUNC_START);
        map.mark_range(
            base + 8,
            base + 12,
            Flags::EXEC | Flags::FUNC_START | Flags::THUMB,
        );
        let regions = function_regions(&image, &map);
        assert_eq!(
            regions,
            vec![
                FunctionRegion {
                    start: base,
                    end: base + 4,
                    state: State::Arm
                },
                FunctionRegion {
                    start: base + 8,
                    end: base + 12,
                    state: State::Thumb
                },
            ]
        );
    }
}
