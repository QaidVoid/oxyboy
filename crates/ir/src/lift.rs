//! Function-level lifting pipeline: analysis boundaries to IR
//! [`Function`]s.
//!
//! The control flow comes from the analysis's flag map: a linear decode
//! walk per function region splits basic blocks at branch targets, and
//! terminators mirror the traversal's stop kinds (direct branch, call,
//! return, indirect exit). ARM and THUMB instruction semantics lower
//! through the ARM and THUMB lifters; the state
//! threading is real SSA values in slot order.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use analysis::{Canonical, FlagMap, Flags, JumpTable, State};
use decode::{
    ArmInstr, Condition, DataOp, Register, ShifterOperand, ThumbAluOp, ThumbHiOp, ThumbInstr,
    TransferOffset, decode_arm, decode_thumb,
};

use crate::arm::ArmLift;
use crate::{
    Block, BlockId, CycleCharge, Function, Inst, Op, SLOTS, Size, Terminator, Value, ValueId,
};

/// The interpreter's internal-cycle extras for one ARM instruction
/// beyond the opcode fetch and the data-access costs the host
/// callbacks charge (the `execute` cycle model in the runtime).
fn arm_internal(instr: &ArmInstr) -> u32 {
    match instr {
        ArmInstr::Multiply { .. } => 3,
        ArmInstr::MultiplyLong { .. } => 5,
        ArmInstr::Swap { .. } => 1,
        _ => 0,
    }
}

/// The interpreter's internal-cycle extras for one THUMB instruction
/// at a non-terminator site; terminator forms charge at their break
/// sites below.
fn thumb_internal(instr: &ThumbInstr) -> u32 {
    match instr {
        ThumbInstr::ShiftedImmediate { .. } => 1,
        ThumbInstr::AluOperation { op, .. } => match op {
            ThumbAluOp::Mul => 3,
            ThumbAluOp::Lsl | ThumbAluOp::Lsr | ThumbAluOp::Asr | ThumbAluOp::Ror => 1,
            _ => 0,
        },
        ThumbInstr::PcRelativeLoad { .. } => 1,
        _ => 0,
    }
}

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
    // Function starts: leading bytes of maximal FUNC_START runs (a run
    // longer than one byte is one mark spanning an instruction, not
    // several functions).
    let mut starts: Vec<u32> = Vec::new();
    let mut index = 0usize;
    while index < len {
        if map.get(base + index as u32).contains(Flags::FUNC_START) {
            starts.push(base + index as u32);
            while index < len && map.get(base + index as u32).contains(Flags::FUNC_START) {
                index += 1;
            }
        } else {
            index += 1;
        }
    }
    // Each region runs from its start to the next function's start (or
    // the image end): the bytes between belong to the function, so the
    // lifter sees its whole body and every branch inside it stays
    // in-region. The hybrid's region builder uses the same rule.
    let mut regions = Vec::new();
    for (position, &start) in starts.iter().enumerate() {
        let end = starts
            .get(position + 1)
            .copied()
            .unwrap_or(base + len as u32);
        let state = if map.get(start).contains(Flags::THUMB) {
            State::Thumb
        } else {
            State::Arm
        };
        regions.push(FunctionRegion { start, end, state });
    }
    regions
}

/// Lifts one function region to IR. `tables` are the resolved jump
/// tables from analysis whose sites may lie in this region; a dispatch
/// site with a resolved table lowers to a compare chain instead of a
/// raw indirect exit.
#[must_use]
pub fn lift_function(
    image: &rom::Image,
    map: &FlagMap,
    region: FunctionRegion,
    tables: &[JumpTable],
) -> Function {
    Lifter {
        image,
        map,
        canon: map.canonical(),
        region,
        tables,
        raw: BTreeMap::new(),
        next: ValueId(STATE_COUNT as u32),
        synthetic: 0,
    }
    .lift()
}

/// One undispatched block: decoded instructions plus a raw terminator
/// still naming addresses.
struct RawBlock {
    params: Vec<ValueId>,
    end_state: Vec<ValueId>,
    insts: Vec<Inst>,
    cycles: Vec<CycleCharge>,
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
    /// Return-like exit. `switches_state` marks an interworking
    /// return (`bx lr`); the pc slot then carries the lr value.
    Return { switches_state: bool },
    /// Register-indirect exit. `switches_state` marks an interworking
    /// branch (`bx rN`); plain `mov pc` style jumps keep the state.
    /// `pc_mask` mirrors the real instruction's target alignment.
    IndirectExit {
        reg: Register,
        switches_state: bool,
        pc_mask: u32,
    },
    /// Undefined encoding or walked into non-code.
    Unreachable,
}

/// The phase-1 walker.
struct Lifter<'a> {
    image: &'a rom::Image,
    map: &'a FlagMap,
    canon: Canonical,
    region: FunctionRegion,
    tables: &'a [JumpTable],
    raw: BTreeMap<u32, RawBlock>,
    next: ValueId,
    /// Seq for synthetic chain/fallback blocks; keys carry the high
    /// bit so they sort after every real address.
    synthetic: u32,
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

    /// The resolved jump table whose dispatch site is `addr`, if any.
    /// The table must match the region's state; `find_tables` sorts by
    /// site, so the lookup is a binary search.
    fn table_at(&self, addr: u32, state: State) -> Option<&JumpTable> {
        let index = self
            .tables
            .binary_search_by_key(&addr, |table| table.site)
            .ok()?;
        let table = &self.tables[index];
        (table.state == state && table.targets.len() >= 2).then_some(table)
    }

    /// Lowers a resolved jump-table dispatch into a compare chain: the
    /// exit value is masked into the pc slot, each chain block compares
    /// it (its own pc parameter) against one target and branches on
    /// equality, and the last fallback is the raw indirect exit over
    /// the already-masked pc slot. Returns the dispatcher's raw
    /// terminator plus the in-region case targets as leaders; the
    /// chain and fallback blocks land in `raw` under synthetic keys.
    fn lower_dispatch(
        &mut self,
        lift: &mut ArmLift,
        cycles: &mut Vec<CycleCharge>,
        addr: u32,
        value: Value,
        pc_mask: u32,
        table: &JumpTable,
    ) -> (RawTerm, Vec<u32>) {
        // The walk allocates through `lift.next`; keep both counters in
        // step while the chain blocks take their ids from `self.next`.
        self.next = lift.next;
        // The dispatcher masks the exit value into the pc slot so the
        // chain blocks compare their own pc parameter against each
        // target (block-argument SSA: no cross-block value references).
        let pc = lift.write(Register::Pc);
        lift.emit(
            Op::Mov {
                set_flags: false,
                a: value,
                carry_in: Value::Const(0),
            },
            vec![pc],
        );
        lift.mask_pc(pc_mask);
        cycles.push(CycleCharge { addr, cycles: 0 });

        let mut seen = BTreeSet::new();
        let targets: Vec<u32> = table
            .targets
            .iter()
            .map(|&(target, _)| target)
            .filter(|target| seen.insert(*target))
            .collect();
        let fallback = self.synthetic_key();
        let chain_keys: Vec<u32> = targets.iter().map(|_| self.synthetic_key()).collect();

        for (index, &target) in targets.iter().enumerate() {
            let otherwise = chain_keys.get(index + 1).copied().unwrap_or(fallback);
            let (params, flag_ids) = self.fresh_params();
            let mut end_state = params.clone();
            end_state[FLAG_SLOT..FLAG_SLOT + 4].copy_from_slice(&flag_ids);
            let compare = Inst {
                guard: None,
                dests: flag_ids,
                fallbacks: Vec::new(),
                op: Op::Cmp {
                    a: Value::Id(params[Register::Pc.bits() as usize]),
                    b: Value::Const(target),
                },
                pc: addr,
                state: Vec::new(),
            };
            self.raw.insert(
                chain_keys[index],
                RawBlock {
                    params,
                    end_state,
                    insts: vec![compare],
                    cycles: Vec::new(),
                    term: RawTerm::BranchCond {
                        cond: Condition::Eq,
                        then_target: target,
                        otherwise,
                    },
                },
            );
        }
        let fallback_params = self.fresh_params().0;
        self.raw.insert(
            fallback,
            RawBlock {
                end_state: fallback_params.clone(),
                params: fallback_params,
                insts: Vec::new(),
                cycles: Vec::new(),
                term: RawTerm::IndirectExit {
                    reg: Register::Pc,
                    switches_state: false,
                    pc_mask,
                },
            },
        );
        lift.next = self.next;
        // The dispatcher itself compares nothing: it hands the masked
        // value to the first chain block unconditionally. In-region
        // targets come back as leaders so their bodies lift too.
        let leaders = targets
            .into_iter()
            .filter(|&target| self.in_region(target))
            .collect();
        (RawTerm::Branch(chain_keys[0]), leaders)
    }

    /// One synthetic key for a chain or fallback block.
    fn synthetic_key(&mut self) -> u32 {
        self.synthetic += 1;
        0x8000_0000 | self.synthetic
    }

    /// Ends a guarded transfer: the transfer's raw terminator moves
    /// into a synthetic block reached only when `guard` holds, and a
    /// failed guard falls through to the next instruction (which the
    /// caller surfaces as a leader). Returns the dispatcher's raw
    /// terminator.
    fn lower_guarded(
        &mut self,
        lift: &mut ArmLift,
        addr: u32,
        guard: Condition,
        term: RawTerm,
    ) -> RawTerm {
        self.next = lift.next;
        let key = self.synthetic_key();
        let (params, flag_ids) = self.fresh_params();
        let mut end_state = params.clone();
        end_state[FLAG_SLOT..FLAG_SLOT + 4].copy_from_slice(&flag_ids);
        self.raw.insert(
            key,
            RawBlock {
                params,
                end_state,
                insts: Vec::new(),
                cycles: Vec::new(),
                term,
            },
        );
        lift.next = self.next;
        RawTerm::BranchCond {
            cond: guard,
            then_target: key,
            otherwise: addr.wrapping_add(4),
        }
    }

    /// Fresh block parameters plus the four flag-slot ids.
    fn fresh_params(&mut self) -> (Vec<ValueId>, Vec<ValueId>) {
        let params: Vec<ValueId> = (0..STATE_COUNT)
            .map(|_| {
                let allocated = self.next;
                self.next = ValueId(allocated.0 + 1);
                allocated
            })
            .collect();
        let flags = params[FLAG_SLOT..FLAG_SLOT + 4].to_vec();
        (params, flags)
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
            fallbacks: Vec::new(),
        };
        // One cycle charge per executed guest instruction, in order.
        let mut cycles: Vec<CycleCharge> = Vec::new();
        let mut addr = from;
        let term = loop {
            if addr >= self.region.end {
                // Clean run off the region end: the function returns,
                // carrying its final state in slot order. No interwork
                // happens on the way out.
                break RawTerm::Return {
                    switches_state: false,
                };
            }
            if !self.executed(addr) {
                break RawTerm::Unreachable;
            }
            match self.region.state {
                State::Arm => {
                    let word = self.word(addr);
                    let guard = Condition::from_bits((word >> 28) as u8);
                    match decode_arm(word) {
                        ArmInstr::Branch { link, offset } => {
                            cycles.push(CycleCharge { addr, cycles: 0 });
                            let target = addr.wrapping_add(8).wrapping_add_signed(offset);
                            if link {
                                lift.addr = addr;
                                lift.guard = guard.filter(|cond| *cond != Condition::Al);
                                let lr = lift.write(Register::Lr);
                                lift.emit(
                                    Op::Mov {
                                        set_flags: false,
                                        a: Value::Const(addr + 4),
                                        carry_in: Value::Const(0),
                                    },
                                    vec![lr],
                                );
                                let cont = addr + 4;
                                let linked = self.in_region(cont);
                                if linked {
                                    leaders.push(cont);
                                }
                                let call = RawTerm::Call {
                                    target,
                                    cont: linked.then_some(cont),
                                };
                                break match guard.filter(|cond| *cond != Condition::Al) {
                                    Some(cond) => self.lower_guarded(&mut lift, addr, cond, call),
                                    None => call,
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
                            cycles.push(CycleCharge { addr, cycles: 0 });
                            let term = if rn == Register::Lr {
                                RawTerm::Return {
                                    switches_state: true,
                                }
                            } else {
                                RawTerm::IndirectExit {
                                    reg: rn,
                                    switches_state: true,
                                    pc_mask: !1,
                                }
                            };
                            break match guard.filter(|cond| *cond != Condition::Al) {
                                Some(cond) => {
                                    leaders.push(addr + 4);
                                    self.lower_guarded(&mut lift, addr, cond, term)
                                }
                                None => term,
                            };
                        }
                        ArmInstr::DataProcessing {
                            rd: Register::Pc,
                            opcode: DataOp::Mov,
                            operand2: ShifterOperand::Register { rm, shift },
                            ..
                        } => {
                            // `mov pc, rX` is the common indirect jump. A
                            // resolved switch dispatch lowers to a
                            // compare chain over the table's targets;
                            // anything else (guarded forms included)
                            // keeps the raw indirect exit.
                            if guard.is_none_or(|c| c == Condition::Al)
                                && let Some(table) = self.table_at(addr, self.region.state)
                            {
                                let table = table.clone();
                                lift.guard = guard;
                                lift.addr = addr;
                                let (value, _) =
                                    lift.shifter(ShifterOperand::Register { rm, shift }, false);
                                let (term, case_leaders) = self.lower_dispatch(
                                    &mut lift,
                                    &mut cycles,
                                    addr,
                                    value,
                                    !3,
                                    &table,
                                );
                                leaders.extend(case_leaders);
                                break term;
                            }
                            cycles.push(CycleCharge { addr, cycles: 0 });
                            let term = RawTerm::IndirectExit {
                                reg: rm,
                                switches_state: false,
                                // ALU writes to pc word-align the target.
                                pc_mask: !3,
                            };
                            break match guard.filter(|cond| *cond != Condition::Al) {
                                Some(cond) => {
                                    leaders.push(addr + 4);
                                    self.lower_guarded(&mut lift, addr, cond, term)
                                }
                                None => term,
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
                            cycles.push(CycleCharge { addr, cycles: 0 });
                            let term = RawTerm::IndirectExit {
                                reg: match operand2 {
                                    ShifterOperand::Register { rm, .. } => rm,
                                    _ => Register::Pc,
                                },
                                switches_state: false,
                                // ALU writes to pc word-align the target.
                                pc_mask: !3,
                            };
                            break match guard.filter(|cond| *cond != Condition::Al) {
                                Some(cond) => {
                                    leaders.push(addr + 4);
                                    self.lower_guarded(&mut lift, addr, cond, term)
                                }
                                None => term,
                            };
                        }
                        ArmInstr::LoadStore {
                            load: true,
                            byte,
                            writeback,
                            up,
                            pre_index,
                            rd: Register::Pc,
                            rn,
                            offset,
                        } => {
                            // Lift the access (word-aligned pc mask
                            // included), charge the fetch, and return
                            // through the loaded pc slot.
                            cycles.push(CycleCharge { addr, cycles: 0 });
                            lift.guard = guard;
                            lift.addr = addr;
                            let offset = match offset {
                                TransferOffset::Immediate(imm) => Value::Const(imm),
                                TransferOffset::Register { rm, shift } => {
                                    lift.shifter(ShifterOperand::Register { rm, shift }, false)
                                        .0
                                }
                            };
                            lift.single_transfer(
                                true,
                                if byte { Size::Byte } else { Size::Word },
                                false,
                                writeback,
                                up,
                                pre_index,
                                Register::Pc,
                                rn,
                                offset,
                            );
                            let term = RawTerm::Return {
                                switches_state: false,
                            };
                            break match guard.filter(|cond| *cond != Condition::Al) {
                                Some(cond) => {
                                    leaders.push(addr + 4);
                                    self.lower_guarded(&mut lift, addr, cond, term)
                                }
                                None => term,
                            };
                        }
                        ArmInstr::BlockTransfer {
                            load: true,
                            psr_and_force_user: _,
                            writeback,
                            up,
                            pre_index,
                            rn,
                            registers,
                        } if registers.contains(Register::Pc) => {
                            cycles.push(CycleCharge { addr, cycles: 0 });
                            lift.guard = guard;
                            lift.addr = addr;
                            lift.block_transfer(true, writeback, up, pre_index, rn, registers);
                            let term = RawTerm::Return {
                                switches_state: false,
                            };
                            break match guard.filter(|cond| *cond != Condition::Al) {
                                Some(cond) => {
                                    leaders.push(addr + 4);
                                    self.lower_guarded(&mut lift, addr, cond, term)
                                }
                                None => term,
                            };
                        }
                        ArmInstr::Undefined { .. } => break RawTerm::Unreachable,
                        other => {
                            cycles.push(CycleCharge {
                                addr,
                                cycles: arm_internal(&other),
                            });
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
                            cycles.push(CycleCharge { addr, cycles: 0 });
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
                            cycles.push(CycleCharge { addr, cycles: 0 });
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
                            // One 32-bit instruction: the interpreter
                            // charges its fetch once plus the second
                            // halfword fetch and one internal cycle.
                            cycles.push(CycleCharge { addr, cycles: 2 });
                            let target = addr.wrapping_add(4).wrapping_add_signed(offset * 2);
                            lift.addr = addr;
                            let lr = lift.write(Register::Lr);
                            lift.insts.push(crate::Inst {
                                guard: None,
                                dests: vec![lr],
                                fallbacks: Vec::new(),
                                op: Op::Mov {
                                    set_flags: false,
                                    a: Value::Const(addr + 4),
                                    carry_in: Value::Const(0),
                                },
                                pc: addr,
                                state: Vec::new(),
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
                            cycles.push(CycleCharge { addr, cycles: 1 });
                            break RawTerm::IndirectExit {
                                reg: Register::Lr,
                                // Thumb `bl` does not interwork; the
                                // target state stays as it is.
                                switches_state: false,
                                pc_mask: !1,
                            };
                        }
                        ThumbInstr::HighRegisterOperation { op, rd, rs } => match (op, rd) {
                            (ThumbHiOp::Bx, _) => {
                                cycles.push(CycleCharge { addr, cycles: 0 });
                                break if rs == Register::Lr {
                                    RawTerm::Return {
                                        switches_state: true,
                                    }
                                } else {
                                    RawTerm::IndirectExit {
                                        reg: rs,
                                        switches_state: true,
                                        pc_mask: !1,
                                    }
                                };
                            }
                            (ThumbHiOp::Mov, Register::Pc) => {
                                // A resolved switch dispatch lowers to a
                                // compare chain; guarded THUMB does not
                                // exist, so only the table gates it.
                                if let Some(table) = self.table_at(addr, self.region.state) {
                                    let table = table.clone();
                                    lift.addr = addr;
                                    let value = Value::Id(lift.current[rs.bits() as usize]);
                                    let (term, case_leaders) = self.lower_dispatch(
                                        &mut lift,
                                        &mut cycles,
                                        addr,
                                        value,
                                        !1,
                                        &table,
                                    );
                                    leaders.extend(case_leaders);
                                    break term;
                                }
                                cycles.push(CycleCharge { addr, cycles: 0 });
                                break RawTerm::IndirectExit {
                                    reg: rs,
                                    switches_state: false,
                                    pc_mask: !1,
                                };
                            }
                            (ThumbHiOp::Add, Register::Pc) => {
                                cycles.push(CycleCharge { addr, cycles: 0 });
                                break RawTerm::IndirectExit {
                                    reg: rs,
                                    switches_state: false,
                                    pc_mask: !1,
                                };
                            }
                            _ => {
                                cycles.push(CycleCharge { addr, cycles: 0 });
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
                            cycles.push(CycleCharge { addr, cycles: 0 });
                            lift.addr = addr;
                            lift.thumb_pop(u16::from(registers), true);
                            break RawTerm::Return {
                                switches_state: false,
                            };
                        }
                        ThumbInstr::Undefined { .. } => break RawTerm::Unreachable,
                        other => {
                            cycles.push(CycleCharge {
                                addr,
                                cycles: thumb_internal(&other),
                            });
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
                cycles,
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
        for (addr, raw) in order.iter() {
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
                    state: current.clone(),
                },
                RawTerm::Return { switches_state } => {
                    let mut state = current.clone();
                    if *switches_state {
                        // `bx lr` resumes at lr; slot 15 tracks slot 14
                        // so the host resumes at the caller's address.
                        state[Register::Pc.bits() as usize] = state[Register::Lr.bits() as usize];
                    }
                    Terminator::Return {
                        state,
                        switches_state: *switches_state,
                    }
                }
                RawTerm::IndirectExit {
                    reg,
                    switches_state,
                    pc_mask,
                } => Terminator::IndirectExit {
                    target: current[reg.bits() as usize],
                    state: current.clone(),
                    switches_state: *switches_state,
                    pc_mask: *pc_mask,
                },
                RawTerm::Unreachable => Terminator::Unreachable,
            };
            blocks.push(Block {
                addr: *addr,
                params,
                insts: raw.insts.clone(),
                cycles: raw.cycles.clone(),
                term,
            });
        }
        // Helpers carry ids sequential after the real blocks, so
        // pushing in id order lands each at its index.
        let mut placed: Vec<(BlockId, u32)> =
            helpers.iter().map(|(&addr, &id)| (id, addr)).collect();
        placed.sort_unstable();
        for (id, addr) in placed {
            debug_assert_eq!(id.0 as usize, blocks.len(), "helper ids must be dense");
            let params: Vec<ValueId> = (0..STATE_COUNT)
                .map(|_| {
                    let allocated = next;
                    next = ValueId(allocated.0 + 1);
                    allocated
                })
                .collect();
            blocks.push(Block {
                addr,
                params,
                insts: Vec::new(),
                cycles: Vec::new(),
                term: Terminator::Call {
                    target: Value::Const(addr),
                    cont: None,
                    state: Vec::new(),
                },
            });
        }
        Function {
            name: format!("sub_{:08x}", self.region.start),
            thumb: self.region.state == State::Thumb,
            blocks,
        }
    }
}

/// Flag slots start after the sixteen register slots.
const FLAG_SLOT: usize = 16;

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
        lift_arm_with(words, count, &[])
    }

    /// `lift_arm` with resolved jump tables handed to the lifter.
    fn lift_arm_with(words: &[u32], count: usize, tables: &[JumpTable]) -> Function {
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
            tables,
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
            &[],
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
            Terminator::Return { state, .. } => assert_eq!(state.len(), STATE_COUNT),
            other => panic!("expected return, got {other:?}"),
        }
    }

    /// A conditional write followed by an unconditional read of the
    /// same register: the read consumes the write's dest, and the
    /// write carries the register's incoming value as its guard-fail
    /// fallback (the `movne rX / moveq rX` boolean idiom reads it).
    #[test]
    fn guarded_write_carries_the_previous_value_as_fallback() {
        // movne r2, #1; add r3, r2, r2; bx lr
        let function = lift_arm(&[0x13A0_2001, 0xE082_3002, 0xE12F_FF1E], 2);
        let entry = &function.blocks[0];
        assert_eq!(entry.insts.len(), 2);
        let (mov, add) = (&entry.insts[0], &entry.insts[1]);
        assert_eq!(mov.guard, Some(Condition::Ne));
        let dest = mov.dests[0];
        let Some(crate::Value::Id(prev)) = &mov.fallbacks[0] else {
            panic!("the movne must record a fallback")
        };
        // The fallback is r2's incoming block parameter (slot 2).
        assert_eq!(*prev, entry.params[2]);
        // The add reads the movne's dest, not the parameter.
        assert!(
            crate::opt::operands(&add.op).contains(&crate::Value::Id(dest)),
            "the unconditional read must see the guarded dest"
        );
    }

    /// The unconditional twin records no fallback: its dest always
    /// executes, so the else arm would be noise.
    #[test]
    fn unconditional_write_records_no_fallback() {
        // mov r2, #1; add r3, r2, r2; bx lr
        let function = lift_arm(&[0xE3A0_2001, 0xE082_3002, 0xE12F_FF1E], 2);
        let entry = &function.blocks[0];
        assert!(entry.insts[0].fallbacks.is_empty());
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
            Terminator::Call { target, cont, .. } => {
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
            Terminator::Call {
                target, cont: None, ..
            } => assert!(matches!(target, Value::Const(_))),
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
            Terminator::IndirectExit { target, .. } => {
                assert_eq!(*target, Value::Id(function.blocks[0].params[3]));
            }
            other => panic!("expected indirect exit, got {other:?}"),
        }
    }

    #[test]
    fn cycle_charges_carry_the_interpreter_extras() {
        // mul r0, r2, r3; swp r0, r2, [r1]; bx lr: three charges, the
        // multiply's latency and the swap's internal cycle included.
        let function = lift_arm(&[0xE000_0392, 0xE101_0092, 0xE12F_FF1E], 3);
        let entry = &function.blocks[0];
        let charges: Vec<(u32, u32)> = entry
            .cycles
            .iter()
            .map(|charge| (charge.addr, charge.cycles))
            .collect();
        assert_eq!(charges.len(), 3, "{charges:?}");
        assert_eq!(charges[0].1, 3, "multiply latency");
        assert_eq!(charges[1].1, 1, "swap internal cycle");
        assert_eq!(charges[2].1, 0, "bx is fetch-only");
        // Charges sit at the guest instructions' own addresses.
        assert_eq!(charges[0].0, entry.insts[0].pc);
        assert_eq!(charges[1].0, entry.insts.last().expect("swap insts").pc);
    }

    #[test]
    fn cycle_charges_cover_eliminated_and_terminator_instructions() {
        // bl +1 (fetch + 2 in THUMB) continuing into lsls r0, r1, #1
        // (+1 internal); bx lr. The bl's lr-write inst folds the bl's
        // charge; the terminator bx carries its own.
        let function = lift_thumb(&[0xF000, 0xF800, 0x0048, 0x4770], 4);
        let internals: Vec<u32> = function.blocks[0].cycles.iter().map(|c| c.cycles).collect();
        assert_eq!(internals, vec![2], "bl pair");
        let rest: Vec<u32> = function.blocks[1].cycles.iter().map(|c| c.cycles).collect();
        assert_eq!(rest, vec![1, 0], "movs then bx");
        // Ascending addresses, one per guest instruction in the block.
        assert!(
            function.blocks[1]
                .cycles
                .windows(2)
                .all(|w| w[0].addr < w[1].addr)
        );
        assert!(function.thumb);
    }

    #[test]
    fn thumb_pop_pc_is_a_return() {
        let function = lift_thumb(&[0xBD01], 1);
        assert!(matches!(function.blocks[0].term, Terminator::Return { .. }));
    }

    /// A resolved switch dispatch lowers to a compare chain
    /// over the table's targets; the raw indirect exit survives only
    /// as the out-of-range fallback.
    #[test]
    fn resolved_switch_lowers_to_a_compare_chain() {
        let base = 0x0800_0000u32;
        let words = [
            0xE1A0_F000, // base: mov pc, r0 (the dispatch)
            0xE1A0_0000,
            0xE1A0_0000,
            0xE1A0_0000, // padding
            0xE1A0_0000, // base+0x10: case 0
            0xE1A0_0000,
            0xE1A0_0000,
            0xE1A0_0000, // padding
            0xE1A0_0000, // base+0x20: case 1
        ];
        let table = JumpTable {
            site: base,
            state: State::Arm,
            base: base + 0x40,
            entries: 2,
            targets: vec![(base + 0x10, State::Arm), (base + 0x20, State::Arm)],
        };
        let function = lift_arm_with(&words, 9, &[table]);
        let entry = &function.blocks[0];
        // The dispatcher masked the exit value into the pc slot.
        assert!(matches!(entry.insts[0].op, Op::Mov { .. }));
        assert!(matches!(
            &entry.insts[1].op,
            Op::And {
                b: Value::Const(0xFFFF_FFFC),
                ..
            }
        ));
        // The dispatcher hands the masked value to the chain
        // unconditionally.
        let Terminator::Branch {
            target: first_chain,
            args: _,
        } = &entry.term
        else {
            panic!("expected the chain entry, got {:?}", entry.term);
        };
        let chain0 = &function.blocks[first_chain.0 as usize];
        let Terminator::BranchCond {
            cond: Condition::Eq,
            then_target,
            else_target,
            ..
        } = &chain0.term
        else {
            panic!("expected the first compare");
        };
        assert_eq!(function.blocks[then_target.0 as usize].addr, base + 0x10);
        let Op::Cmp {
            a: Value::Id(a),
            b: Value::Const(target),
        } = &chain0.insts[0].op
        else {
            panic!("chain block compares");
        };
        assert_eq!(*target, base + 0x10);
        assert_eq!(*a, chain0.params[Register::Pc.bits() as usize]);
        // The chain continues to case 1, then falls back.
        let chain1 = &function.blocks[else_target.0 as usize];
        let Terminator::BranchCond {
            cond: Condition::Eq,
            then_target: second,
            else_target: after,
            ..
        } = &chain1.term
        else {
            panic!("expected the second compare");
        };
        assert_eq!(function.blocks[second.0 as usize].addr, base + 0x20);
        let Op::Cmp {
            b: Value::Const(second_target),
            ..
        } = &chain1.insts[0].op
        else {
            panic!("chain block compares");
        };
        assert_eq!(*second_target, base + 0x20);
        // The chain ends in the raw indirect exit as the fallback.
        let fallback = &function.blocks[after.0 as usize];
        assert!(matches!(
            &fallback.term,
            Terminator::IndirectExit {
                target: Value::Id(_),
                switches_state: false,
                pc_mask: 0xFFFF_FFFC,
                ..
            }
        ));
        // Both cases are real blocks, lifted as leaders.
        assert!(function.blocks.iter().any(|b| b.addr == base + 0x10));
        assert!(function.blocks.iter().any(|b| b.addr == base + 0x20));
    }

    /// The THUMB dispatch lowers the same way, with the pop-style
    /// bit-0 mask instead of word alignment.
    #[test]
    fn thumb_switch_chain_masks_bit_zero() {
        // mov pc, r0 at the entry, nops after, two case bodies.
        let halves: [u16; 9] = [
            0x4687, // base: mov pc, r0
            0x1C00, 0x1C00, 0x1C00, // padding
            0x1C00, // base+8: case 0
            0x1C00, 0x1C00, 0x1C00, // padding
            0x1C00, // base+0x10: case 1
        ];
        let mut bytes = vec![0u8; 256];
        for (index, half) in halves.iter().enumerate() {
            bytes[index * 2..index * 2 + 2].copy_from_slice(&half.to_le_bytes());
        }
        let image = rom::Image::from_bytes(bytes).expect("parses");
        let mut map = FlagMap::from_image(&image);
        let canon_base = map.canonical().base();
        map.mark_range(canon_base, canon_base + 0x12, Flags::EXEC);
        map.set(canon_base, Flags::EXEC | Flags::FUNC_START | Flags::THUMB);
        let table = JumpTable {
            site: canon_base,
            state: State::Thumb,
            base: canon_base + 0x20,
            entries: 2,
            targets: vec![
                (canon_base + 8, State::Thumb),
                (canon_base + 0x10, State::Thumb),
            ],
        };
        let function = lift_function(
            &image,
            &map,
            FunctionRegion {
                start: canon_base,
                end: canon_base + 0x12,
                state: State::Thumb,
            },
            &[table],
        );
        let fallback = function
            .blocks
            .iter()
            .find(|b| {
                matches!(
                    b.term,
                    Terminator::IndirectExit {
                        pc_mask: 0xFFFF_FFFE,
                        ..
                    }
                )
            })
            .unwrap_or_else(|| {
                panic!(
                    "the bit-0 fallback:\n{}",
                    crate::print::fmt_function(&function)
                )
            });
        assert!(matches!(fallback.term, Terminator::IndirectExit { .. }));
        assert!(function.blocks.iter().any(|b| b.addr == canon_base + 8));
        assert!(function.blocks.iter().any(|b| b.addr == canon_base + 0x10));
    }

    /// A guarded call lowers to a trampoline: the lr write carries its
    /// guard-fail fallback, the call lives in a block reached only on
    /// the taken edge, and a failed guard falls through to the next
    /// instruction.
    #[test]
    fn guarded_call_falls_through_to_the_next_instruction() {
        let base = 0x0800_0000u32;
        let words = [
            0x1B00_0002, // base: blne base+0x10
            0xE3A0_0001, // base+4: mov r0, #1 (the fallthrough)
            0xE12F_FF10, // base+8: bx lr
            0xE1A0_0000, // padding
            0xE3A0_0002, // base+0x10: helper: mov r0, #2
            0xE12F_FF10, // bx lr
        ];
        let function = lift_arm_with(&words, 6, &[]);
        let entry = &function.blocks[0];
        // The lr write is guarded and falls back to the incoming lr.
        let write = &entry.insts[0];
        assert_eq!(write.guard, Some(Condition::Ne));
        assert_eq!(
            write.fallbacks,
            vec![Some(Value::Id(entry.params[Register::Lr.bits() as usize]))],
            "lr falls back to its incoming value"
        );
        // The block hands off: taken to the trampoline, else to the
        // next instruction.
        let Terminator::BranchCond {
            cond: Condition::Ne,
            then_target,
            else_target,
            ..
        } = &entry.term
        else {
            panic!("expected the guarded hand-off, got {:?}", entry.term);
        };
        assert_eq!(function.blocks[else_target.0 as usize].addr, base + 4);
        let trampoline = &function.blocks[then_target.0 as usize];
        let Terminator::Call {
            target: Value::Const(target),
            cont,
            state,
        } = &trampoline.term
        else {
            panic!("expected the trampoline call, got {:?}", trampoline.term);
        };
        assert_eq!(*target, base + 0x10);
        let (cont_id, _args) = cont.as_ref().expect("the continuation is in-region");
        assert_eq!(function.blocks[cont_id.0 as usize].addr, base + 4);
        // The callee's incoming lr is the trampoline's own parameter,
        // bound from the taken edge's state argument.
        assert_eq!(
            state[Register::Lr.bits() as usize],
            Value::Id(trampoline.params[Register::Lr.bits() as usize])
        );
    }

    /// A guarded `bx` ignores its exit when the condition fails and
    /// falls through; the taken edge exits through the trampoline.
    #[test]
    fn guarded_bx_falls_through_when_the_condition_fails() {
        let base = 0x0800_0000u32;
        let words = [
            0x112F_FF10, // base: bxne r0
            0xE3A0_0001, // base+4: mov r0, #1
            0xE12F_FF10, // base+8: bx lr
        ];
        let function = lift_arm_with(&words, 3, &[]);
        let entry = &function.blocks[0];
        assert!(entry.insts.is_empty(), "bx lifts no instructions");
        let Terminator::BranchCond {
            cond: Condition::Ne,
            then_target,
            else_target,
            ..
        } = &entry.term
        else {
            panic!("expected the guarded hand-off, got {:?}", entry.term);
        };
        assert_eq!(function.blocks[else_target.0 as usize].addr, base + 4);
        assert!(matches!(
            &function.blocks[then_target.0 as usize].term,
            Terminator::IndirectExit {
                switches_state: true,
                pc_mask: 0xFFFF_FFFE,
                ..
            }
        ));
    }

    /// A guarded pc-bearing return keeps the value threading: the
    /// taken edge passes the masked load result, which the
    /// trampoline's return sends out through its own pc parameter.
    #[test]
    fn guarded_pc_return_keeps_the_value_threading() {
        let base = 0x0800_0000u32;
        let words = [
            0x1590_F000, // base: ldrne pc, [r0]
            0xE3A0_0001, // base+4: mov r0, #1
            0xE12F_FF10, // base+8: bx lr
        ];
        let function = lift_arm_with(&words, 3, &[]);
        let entry = &function.blocks[0];
        let masked = entry
            .insts
            .iter()
            .find(|inst| matches!(inst.op, Op::And { .. }))
            .map(|inst| inst.dests[0])
            .expect("the pc mask");
        let Terminator::BranchCond {
            cond: Condition::Ne,
            then_target,
            else_target,
            then_args,
            ..
        } = &entry.term
        else {
            panic!("expected the guarded hand-off, got {:?}", entry.term);
        };
        assert_eq!(then_args[Register::Pc.bits() as usize], Value::Id(masked));
        assert_eq!(function.blocks[else_target.0 as usize].addr, base + 4);
        let trampoline = &function.blocks[then_target.0 as usize];
        let Terminator::Return {
            state,
            switches_state: false,
        } = &trampoline.term
        else {
            panic!("expected the trampoline return, got {:?}", trampoline.term);
        };
        assert_eq!(state[15], Value::Id(trampoline.params[15]));
    }

    /// `ldr pc, [sp], #4` lifts the load and the ARMv4
    /// word-align mask, and returns through the loaded pc slot.
    #[test]
    fn ldr_pc_lifts_the_load_align_mask_and_return() {
        let function = lift_arm(&[0xE49D_F004], 1);
        assert_eq!(function.blocks.len(), 1);
        let block = &function.blocks[0];
        assert!(matches!(block.term, Terminator::Return { .. }));
        let load_pos = block
            .insts
            .iter()
            .position(|inst| {
                matches!(
                    inst.op,
                    Op::Load {
                        size: Size::Word,
                        signed: false,
                        mode: crate::LoadMode::Ldr,
                        ..
                    }
                )
            })
            .expect("the word load");
        let loaded = block.insts[load_pos].dests[0];
        // The mask repoints pc at loaded & !3.
        let Op::And {
            a: Value::Id(a),
            b: Value::Const(0xFFFF_FFFC),
            ..
        } = &block.insts[load_pos + 1].op
        else {
            panic!(
                "expected the align mask, got {:?}",
                block.insts[load_pos + 1].op
            );
        };
        assert_eq!(*a, loaded);
        // Post-indexed writeback: sp += 4 after the access.
        assert!(matches!(
            &block.insts[load_pos + 2].op,
            Op::Add {
                b: Value::Const(4),
                ..
            }
        ));
        // The return state leaves pc at the masked value.
        let Terminator::Return { state, .. } = &block.term else {
            panic!("return terminator");
        };
        assert_eq!(
            state[Register::Pc.bits() as usize],
            Value::Id(block.insts[load_pos + 1].dests[0])
        );
    }

    /// `ldmdb r1!, {r2, r3, pc}` lifts every list load, the
    /// pc align mask, and the base writeback.
    #[test]
    fn ldm_pc_lifts_all_loads_mask_and_writeback() {
        let function = lift_arm(&[0xE931_800C], 1);
        let block = &function.blocks[0];
        assert!(matches!(block.term, Terminator::Return { .. }));
        let loads: Vec<&Inst> = block
            .insts
            .iter()
            .filter(|inst| {
                matches!(
                    inst.op,
                    Op::Load {
                        mode: crate::LoadMode::Block,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(loads.len(), 3, "one load per list entry");
        // The pc is the highest list bit, so its load is last and
        // feeds the align mask; the writeback follows it.
        let Op::And {
            a: Value::Id(a),
            b: Value::Const(0xFFFF_FFFC),
            ..
        } = &block.insts[block.insts.len() - 2].op
        else {
            panic!(
                "expected the align mask:\n{}",
                crate::print::fmt_function(&function)
            );
        };
        assert_eq!(*a, loads[2].dests[0]);
        // Writeback: r1 = base - 12 (the base is not in the list).
        assert!(matches!(
            &block.insts[block.insts.len() - 1].op,
            Op::Add {
                b: Value::Const(0xFFFF_FFF4),
                ..
            }
        ));
    }

    /// `pop {r0, pc}` clears bit 0 of the loaded pc
    /// (GBATEK THUMB.14: POP pc ignores bit 0).
    #[test]
    fn thumb_pop_pc_masks_bit_zero() {
        let function = lift_thumb(&[0xBD01], 1);
        let block = &function.blocks[0];
        let loads: Vec<&Inst> = block
            .insts
            .iter()
            .filter(|inst| {
                matches!(
                    inst.op,
                    Op::Load {
                        mode: crate::LoadMode::Block,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(loads.len(), 2, "r0 then pc");
        let mask_pos = block
            .insts
            .iter()
            .position(|inst| {
                matches!(
                    inst.op,
                    Op::And {
                        b: Value::Const(0xFFFF_FFFE),
                        ..
                    }
                )
            })
            .expect("the bit-0 mask");
        let Op::And {
            a: Value::Id(a), ..
        } = &block.insts[mask_pos].op
        else {
            panic!("mask");
        };
        assert_eq!(*a, loads[1].dests[0]);
    }

    /// ARM7 stores R15 as the instruction address plus 12, in the
    /// single and the block store paths alike (the generic pc read is
    /// address plus 8 and must not leak into stores).
    #[test]
    fn pc_stores_use_the_arm7_plus_twelve_value() {
        let base = 0x0800_0000u32;
        let function = lift_arm(&[0xE580_F000, 0xE880_8000], 2);
        let stores: Vec<&Inst> = function.blocks[0]
            .insts
            .iter()
            .filter(|inst| matches!(inst.op, Op::Store { .. }))
            .collect();
        assert_eq!(stores.len(), 2);
        for (index, inst) in stores.iter().enumerate() {
            let Op::Store { value, .. } = &inst.op else {
                panic!("store");
            };
            assert_eq!(*value, Value::Const(base + 4 * index as u32 + 12));
        }
    }

    #[test]
    fn thumb_long_branch_calls_and_continues() {
        // bl +0, movs r0, #1, bx lr.
        let function = lift_thumb(&[0xF000, 0xF800, 0x2001, 0x4770], 4);
        assert_eq!(function.blocks.len(), 2);
        match &function.blocks[0].term {
            Terminator::Call { target, cont, .. } => {
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
        let words: [u32; 39] = [
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
            0xE911_800C, // ldmdb r1!, {r2, r3, pc}: return terminator
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
        // address add (base + 2) follows the load, since the writeback
        // runs after the access. The ldrb is the last unsigned byte
        // load (swpb's is earlier, ldrsb's is signed).
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
                &insts[ldrb + 1].op,
                Op::Add {
                    b: Value::Const(2),
                    ..
                }
            ),
            "after ldrb: {:?}",
            insts[ldrb + 1].op
        );
        // The LDM with pc is a return terminator (it ends the walk),
        // and the SWI stays inline before it.
        assert!(matches!(function.blocks[0].term, Terminator::Return { .. }));
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
    /// emits the explicit shift op (carry-only) and threads its carry
    /// into the ALU op (pretty-print golden, spot-checked).
    #[test]
    fn shifter_carry_appears_in_lifted_output() {
        // ands r0, r1, r2, ror #4
        let function = lift_arm(&[0xE011_0262], 1);
        let printed = crate::print::fmt_function(&function);
        assert!(printed.contains("ror "), "{printed}");
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

    /// A conditional branch whose fall-through leaves the region gets
    /// an exit-helper block appended after the real ones (this used to
    /// index past the block vector and panic).
    #[test]
    fn out_of_region_fall_through_appends_an_exit_helper() {
        // cmp r0, #1; bne .  (the taken target is in region, the
        // fall-through is one past the region end)
        let function = lift_arm(&[0xE350_0001, 0x1AFF_FFFE], 2);
        assert_eq!(function.blocks.len(), 3);
        assert!(matches!(
            function.blocks[2].term,
            Terminator::Call { cont: None, .. }
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
        // Each region runs from its function's start to the next
        // function's start (or the image end): the bytes between
        // belong to the function, so the lifter sees its whole body.
        assert_eq!(
            regions,
            vec![
                FunctionRegion {
                    start: base,
                    end: base + 8,
                    state: State::Arm
                },
                FunctionRegion {
                    start: base + 8,
                    end: base + 256,
                    state: State::Thumb
                },
            ]
        );
    }
}
