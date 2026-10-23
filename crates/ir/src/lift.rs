//! Function-level lifting pipeline: analysis boundaries to IR
//! [`Function`]s.
//!
//! The control flow comes from the analysis's flag map: a linear decode
//! walk per function region splits basic blocks at branch targets, and
//! terminators mirror the traversal's stop kinds (direct branch, call,
//! return, indirect exit). ARM and THUMB instruction semantics lower
//! through the ARM and THUMB lifters; the state
//! threading is real SSA values in slot order.

use std::collections::{BTreeMap, HashMap};

use analysis::{Canonical, FlagMap, Flags, State};
use decode::{
    ArmInstr, Condition, Register, ShifterOperand, ThumbHiOp, ThumbInstr, decode_arm, decode_thumb,
};

use crate::arm::ArmLift;
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
        next: ValueId(STATE_COUNT as u32),
    }
    .lift()
}

/// One undispatched block: decoded instructions plus a raw terminator
/// still naming addresses.
struct RawBlock {
    params: Vec<ValueId>,
    end_state: Vec<ValueId>,
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
    next: ValueId,
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
        // The block receives the full machine state on entry.
        let params: Vec<ValueId> = (0..STATE_COUNT)
            .map(|_| {
                let id = self.next;
                self.next = ValueId(id.0 + 1);
                id
            })
            .collect();
        let mut current = [params[0]; STATE_COUNT];
        current.copy_from_slice(&params);
        let mut lift = ArmLift {
            current,
            next: self.next,
            insts: Vec::new(),
            addr: from,
            guard: None,
        };
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
                                lift.addr = addr;
                                let lr = lift.write(Register::Lr);
                                lift.insts.push(crate::Inst {
                                    guard: None,
                                    dests: vec![lr],
                                    op: Op::Mov {
                                        set_flags: false,
                                        a: Value::Const(addr + 4),
                                        carry_in: Value::Const(0),
                                    },
                                });
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
                        other => {
                            lift.guard = guard;
                            lift.addr = addr;
                            lift.lift(other);
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
                            lift.addr = addr;
                            let lr = lift.write(Register::Lr);
                            lift.insts.push(crate::Inst {
                                guard: None,
                                dests: vec![lr],
                                op: Op::Mov {
                                    set_flags: false,
                                    a: Value::Const(addr + 4),
                                    carry_in: Value::Const(0),
                                },
                            });
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
                                lift.addr = addr;
                                lift.thumb_hi(op, rd, rs);
                                addr += 2;
                            }
                        },
                        ThumbInstr::PushPop {
                            pop: true,
                            include_pc_or_lr: true,
                            registers,
                        } => {
                            // The pop's loads must lift before the
                            // return so the pc slot carries them.
                            lift.addr = addr;
                            lift.thumb_pop(u16::from(registers), true);
                            break RawTerm::Return;
                        }
                        ThumbInstr::Undefined { .. } => break RawTerm::Unreachable,
                        other => {
                            lift.guard = None;
                            lift.addr = addr;
                            lift.thumb(other);
                            addr += 2;
                        }
                    }
                }
            }
        };
        leaders.retain(|&leader| self.in_region(leader));
        self.next = lift.next;
        self.raw.insert(
            from,
            RawBlock {
                params,
                end_state: lift.current.to_vec(),
                insts: lift.insts,
                term,
            },
        );
        leaders
    }

    /// Renders the walked blocks as IR: blocks in address order, the
    /// entry block carrying the incoming machine state, every edge
    /// passing the source block's (for now, identity) state values.
    fn render(self) -> Function {
        let mut next = self.next;
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

        let mut blocks = Vec::with_capacity(order.len() + helpers.len());
        for (_, raw) in order.iter() {
            let params = raw.params.clone();
            let current: Vec<Value> = raw.end_state.iter().copied().map(Value::Id).collect();
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
            let params: Vec<ValueId> = (0..STATE_COUNT)
                .map(|_| {
                    let allocated = next;
                    next = ValueId(allocated.0 + 1);
                    allocated
                })
                .collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Size;
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
                let Some((_, args)) = cont else {
                    panic!("expected a continuation");
                };
                assert_eq!(args.len(), STATE_COUNT);
                for (slot, arg) in args.iter().enumerate() {
                    let entry = Value::Id(function.blocks[0].params[slot]);
                    if slot == 14 {
                        assert_ne!(*arg, entry, "lr must be rewritten");
                    } else {
                        assert_eq!(*arg, entry);
                    }
                }
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
                let Some((_, args)) = cont else {
                    panic!("expected a continuation");
                };
                assert_eq!(args.len(), STATE_COUNT);
                for (slot, arg) in args.iter().enumerate() {
                    let entry = Value::Id(function.blocks[0].params[slot]);
                    if slot == 14 {
                        assert_ne!(*arg, entry, "lr must be rewritten");
                    } else {
                        assert_eq!(*arg, entry);
                    }
                }
            }
            other => panic!("expected call, got {other:?}"),
        }
        assert_eq!(function.blocks[1].insts.len(), 1);
        assert!(matches!(function.blocks[1].term, Terminator::Return { .. }));
    }

    /// One instance of every ARM class (including branch and bx,
    /// which the pipeline lifts as terminators), encodings verified
    /// arm-none-eabi-as.
    #[test]
    fn every_arm_class_lifts_without_unhandled() {
        use crate::Op;
        let words: [u32; 38] = [
            0xE081_0002, // add r0, r1, r2
            0xE091_0182, // adds r0, r1, r2, lsl #3
            0xE251_0005, // subs r0, r1, #5
            0xE061_0002, // rsb r0, r1, r2
            0xE0B1_0002, // adcs r0, r1, r2
            0xE0D1_0002, // sbcs r0, r1, r2
            0xE0F1_0002, // rscs r0, r1, r2
            0xE011_0262, // ands r0, r1, r2, ror #4
            0xE181_0002, // orr r0, r1, r2
            0xE021_0002, // eor r0, r1, r2
            0xE1C1_0002, // bic r0, r1, r2
            0xE3A0_0001, // mov r0, #1
            0xE1B0_0271, // movs r0, r1, ror r2
            0xE1E0_0002, // mvn r0, r2
            0xE111_0002, // tst r1, r2
            0xE131_0002, // teq r1, r2
            0xE351_0007, // cmp r1, #7
            0xE171_0002, // cmn r1, r2
            0xE000_0392, // mul r0, r2, r3
            0xE020_4392, // mla r0, r2, r3, r4
            0xE010_0392, // muls r0, r2, r3
            0xE085_4392, // umull r4, r5, r2, r3
            0xE0E5_4392, // smlal r4, r5, r2, r3
            0xE101_0092, // swp r0, r2, [r1]
            0xE141_0092, // swpb r0, r2, [r1]
            0xE10F_0000, // mrs r0, cpsr
            0xE128_F000, // msr cpsr_f, r0
            0xE591_0000, // ldr r0, [r1]
            0xE5A1_0004, // str r0, [r1, #4]!
            0xE4D1_0002, // ldrb r0, [r1], #2
            0xE711_0002, // ldr r0, [r1, -r2]
            0xE791_0102, // ldr r0, [r1, r2, lsl #2]
            0xE1C1_00B0, // strh r0, [r1]
            0xE1D1_00B2, // ldrh r0, [r1, #2]
            0xE191_00D2, // ldrsb r0, [r1, r2]
            0xE1D1_00F1, // ldrsh r0, [r1, #1]
            0xE8A1_000C, // stmia r1!, {r2, r3}
            0xEF00_0005, // swi 0x5
        ];
        let function = lift_arm(&words, words.len());
        for block in &function.blocks {
            for inst in &block.insts {
                assert_ne!(
                    inst.op,
                    Op::Intrinsic {
                        name: "unhandled",
                        args: Vec::new()
                    },
                    "every ARM class must lift"
                );
            }
        }
        let insts = &function.blocks[0].insts;
        // insts[0] is the first add, [1] the Lsl the shifter emitted
        // for `r2, lsl #3`, [2] the flag-binding Add itself.
        assert!(matches!(
            insts[2].op,
            Op::Add {
                set_flags: true,
                ..
            }
        ));
        assert_eq!(insts[2].dests.len(), 5, "adds binds value + nzcv");
        // Adc consumes the carry from the preceding Sub, not its own
        // freshly allocated c dest (dest allocation used to overwrite
        // the carry slot before carry_in read it).
        let sub_c = insts[3].dests[3];
        let Op::Adc { carry_in, .. } = &insts[5].op else {
            panic!("insts[5] should be the adcs");
        };
        assert_eq!(*carry_in, Value::Id(sub_c));
        // Post-indexed ldrb with W=0 still writes the base back: the
        // address add (base + 2) sits right before the load. The ldrb
        // is the last unsigned byte load (swpb's is earlier, ldrsb's
        // is signed).
        let ldrb = insts
            .iter()
            .rposition(|i| {
                matches!(
                    i.op,
                    Op::Load {
                        size: Size::Byte,
                        signed: false,
                        ..
                    }
                )
            })
            .expect("ldrb present");
        assert!(
            matches!(
                &insts[ldrb - 1].op,
                Op::Add {
                    b: Value::Const(2),
                    ..
                }
            ),
            "before ldrb: {:?}",
            insts[ldrb - 1].op
        );
        // The LDM with pc is a return terminator, SWI stays inline.
        assert!(matches!(function.blocks[0].term, Terminator::Unreachable));
        // Branch classes: conditional branch to two successors, bx as
        // indirect exit, plain branch as an in-region edge (b -3 lands
        // on the bx block).
        let control = lift_arm(&[0x0A00_0000, 0xE12F_FF10, 0xEAFF_FFFD], 3);
        assert_eq!(control.blocks.len(), 3);
        assert!(matches!(
            control.blocks[0].term,
            Terminator::BranchCond { .. }
        ));
        assert!(matches!(
            control.blocks[1].term,
            Terminator::IndirectExit { .. }
        ));
        assert!(matches!(control.blocks[2].term, Terminator::Branch { .. }));
    }

    /// The shifter carry shows up in lifted output: a shifted S-bit op
    /// emits the explicit shift op and threads its carry into the ALU
    /// op (pretty-print golden, spot-checked).
    #[test]
    fn shifter_carry_appears_in_lifted_output() {
        // ands r0, r1, r2, ror #4
        let function = lift_arm(&[0xE011_0262], 1);
        let printed = crate::print::fmt_function(&function);
        assert!(printed.contains("rors"), "{printed}");
        assert!(printed.contains("ands"), "{printed}");
        assert!(printed.contains(", c=v"), "{printed}");
    }

    /// One instance of every THUMB format, halfwords verified against
    /// arm-none-eabi-as.
    /// pop-pc ends the walk, so it closes the sweep; conditional and
    /// unconditional branches ride along as in-region edges.
    #[test]
    fn every_thumb_format_lifts_without_unhandled() {
        use crate::Op;
        let halves: [u16; 56] = [
            0x00D0, // lsls r0, r2, #3
            0x0C48, // lsrs r0, r1, #17
            0x17C8, // asrs r0, r1, #31
            0x1CC8, // adds r0, r1, #3
            0x1EC8, // subs r0, r1, #3
            0x20FF, // movs r0, #255
            0x2F07, // cmp r7, #7
            0x320A, // adds r2, #10
            0x3B01, // subs r3, #1
            0x4008, // ands r0, r1
            0x4048, // eors r0, r1
            0x4088, // lsls r0, r1
            0x40C8, // lsrs r0, r1
            0x4108, // asrs r0, r1
            0x4148, // adcs r0, r1
            0x4188, // sbcs r0, r1
            0x41C8, // rors r0, r1
            0x4208, // tst r0, r1
            0x4248, // negs r0, r1
            0x4288, // cmp r0, r1
            0x42C8, // cmn r0, r1
            0x4308, // orrs r0, r1
            0x4348, // muls r0, r1
            0x4388, // bics r0, r1
            0x43C8, // mvns r0, r1
            0x4441, // add r1, r8
            0x4540, // cmp r0, r8
            0x4641, // mov r1, r8
            0x4800, // ldr r0, [pc, #0]
            0x5088, // str r0, [r1, r2]
            0x5888, // ldr r0, [r1, r2]
            0x5488, // strb r0, [r1, r2]
            0x5C88, // ldrb r0, [r1, r2]
            0x5288, // strh r0, [r1, r2]
            0x5688, // ldsb r0, [r1, r2]
            0x5A88, // ldrh r0, [r1, r2]
            0x5E88, // ldsh r0, [r1, r2]
            0x6048, // str r0, [r1, #4]
            0x6848, // ldr r0, [r1, #4]
            0x7108, // strb r0, [r1, #4]
            0x7908, // ldrb r0, [r1, #4]
            0x8088, // strh r0, [r1, #4]
            0x8888, // ldrh r0, [r1, #4]
            0x9001, // str r0, [sp, #4]
            0x9801, // ldr r0, [sp, #4]
            0xA001, // add r0, pc, #4
            0xA801, // add r0, sp, #4
            0xB004, // add sp, #16
            0xB084, // sub sp, #16
            0xD100, // bne .+4 (in-region edge)
            0xE000, // b .+4 (in-region edge)
            0xDF05, // swi 5
            0xB403, // push {r0, r1}
            0xB500, // push {lr}
            0xBC01, // pop {r0}
            0xBD01, // pop {r0, pc}: loads then Return (walk ends)
        ];
        let function = lift_thumb(&halves, halves.len());
        for block in &function.blocks {
            for inst in &block.insts {
                assert_ne!(
                    inst.op,
                    Op::Intrinsic {
                        name: "unhandled",
                        args: Vec::new()
                    },
                    "every THUMB format must lift"
                );
            }
        }
        assert!(matches!(
            function.blocks.last().expect("blocks").term,
            Terminator::Return { .. }
        ));
        let insts = &function.blocks[0].insts;
        // negs lowers to rsb with b = 0 (Op::Rsb computes b - a).
        assert!(insts.iter().any(|inst| matches!(
            inst.op,
            Op::Rsb {
                set_flags: true,
                b: Value::Const(0),
                ..
            }
        )));
        // hi-register add copies without touching flags.
        assert!(insts.iter().any(|inst| matches!(
            inst.op,
            Op::Add {
                set_flags: false,
                ..
            }
        )));
        // The pc-relative load is a literal reference at lift time.
        let base = function
            .blocks
            .first()
            .map(|_| ())
            .and(insts.iter().find_map(|inst| match inst.op {
                Op::Load { addr, .. } => Some(addr),
                _ => None,
            }));
        assert!(matches!(base, Some(Value::Const(_))));
        // The SWI stays inline as an intrinsic (it sits in a block
        // after the bne split).
        assert!(function.blocks.iter().any(|block| {
            block
                .insts
                .iter()
                .any(|inst| matches!(inst.op, Op::Intrinsic { name: "swi", .. }))
        }));
    }

    /// The second BL halfword standing alone (F800, real games emit it
    /// as a cheap call) is an indirect exit through LR.
    #[test]
    fn thumb_bl_from_lr_is_an_indirect_exit() {
        let function = lift_thumb(&[0xF800], 1);
        assert_eq!(function.blocks.len(), 1);
        assert!(matches!(
            function.blocks[0].term,
            Terminator::IndirectExit { .. }
        ));
    }

    #[test]
    fn scratch_thumb_print() {
        let halves: Vec<u16> = vec![
            0x00D0, 0x4248, 0x4800, 0x4441, 0xB403, 0xB500, 0xBC01, 0xBD01,
        ];
        let f = lift_thumb(&halves, halves.len());
        print!("{}", crate::print::fmt_function(&f));
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
