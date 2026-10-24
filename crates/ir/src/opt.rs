//! Optimization passes over lifted IR.
//!
//! Every pass preserves the machine-state semantics the IR
//! differential pins down. Dropping an unused flag def is equivalent
//! to clearing the op's set-flags bit: the dropped dest id is never
//! written, so state consumers keep the previous slot representative,
//! exactly like an S=0 op. Folding is restricted to unguarded,
//! value-only ops, whose dests cannot be conditionally skipped.

use std::collections::HashSet;

use crate::eval::{ShiftKind, add_with_carry, apply_shift, sub_with_borrow};
use crate::{Block, Condition, Function, Inst, Op, Terminator, Value, ValueId};

/// Flag slots in N, Z, C, V order.
const FLAG_SLOTS: [usize; 4] = [16, 17, 18, 19];

/// One pass over a function.
type Pass = fn(&mut Function);

/// Runs every pass in order over `function`.
pub fn optimize(function: &mut Function) {
    optimize_with_dumps(function, &mut |_, _| {});
}

/// Runs every pass, handing `dump` the pass name and the IR after each
/// one (plus `"lifted"` before anything runs) for `--print-after-all`
/// style traces.
pub fn optimize_with_dumps(function: &mut Function, dump: &mut dyn FnMut(&str, &Function)) {
    dump("lifted", function);
    let passes: [(&str, Pass); 3] = [
        ("dead-flags", dead_flag_elimination),
        ("const-prop", constant_propagation),
        ("copy-prop", copy_propagation),
    ];
    for (name, pass) in passes {
        pass(function);
        dump(name, function);
    }
}

/// An unconditional inst always executes; a decoded `AL` guard is the
/// same thing.
fn unconditional(inst: &Inst) -> bool {
    inst.guard.is_none_or(|cond| cond == Condition::Al)
}

/// The flag dests of one op in `[N, Z, C, V]` order, `None` for flags
/// the op does not write. Positions follow the per-variant dest
/// contracts documented on [`Op`].
fn flag_dests(op: &Op, dests: &[ValueId]) -> [Option<ValueId>; 4] {
    let d = |i: usize| dests.get(i).copied();
    match op {
        Op::Add {
            set_flags: true, ..
        }
        | Op::Sub {
            set_flags: true, ..
        }
        | Op::Adc {
            set_flags: true, ..
        }
        | Op::Sbc {
            set_flags: true, ..
        }
        | Op::Rsb {
            set_flags: true, ..
        }
        | Op::Rsc {
            set_flags: true, ..
        }
        | Op::And {
            set_flags: true, ..
        }
        | Op::Eor {
            set_flags: true, ..
        }
        | Op::Orr {
            set_flags: true, ..
        }
        | Op::Bic {
            set_flags: true, ..
        } => [d(1), d(2), d(3), d(4)],
        Op::Mov {
            set_flags: true, ..
        }
        | Op::Mvn {
            set_flags: true, ..
        } => [d(1), d(2), d(3), None],
        Op::Mul {
            set_flags: true, ..
        } => [d(1), d(2), None, None],
        Op::MultiplyLong {
            set_flags: true, ..
        } => [d(2), d(3), None, None],
        Op::Lsl {
            set_flags: true, ..
        }
        | Op::Lsr {
            set_flags: true, ..
        }
        | Op::Asr {
            set_flags: true, ..
        }
        | Op::Ror {
            set_flags: true, ..
        } => [d(1), d(2), d(3), None],
        // Carry-only expansion: the second dest is the shifter carry.
        Op::Lsl {
            set_flags: false, ..
        }
        | Op::Lsr {
            set_flags: false, ..
        }
        | Op::Asr {
            set_flags: false, ..
        }
        | Op::Ror {
            set_flags: false, ..
        }
        | Op::Rrx { .. } => [None, None, d(1), None],
        Op::Cmp { .. } | Op::Cmn { .. } => [d(0), d(1), d(2), d(3)],
        Op::Tst { .. } | Op::Teq { .. } => [d(0), d(1), d(2), None],
        _ => [None, None, None, None],
    }
}

/// Values an op reads. Every operand field appears, including the
/// `carry_in` chains: liveness depends on seeing all of them.
pub(crate) fn operands(op: &Op) -> Vec<Value> {
    match op {
        Op::Add { a, b, .. }
        | Op::Sub { a, b, .. }
        | Op::Rsb { a, b, .. }
        | Op::Mul { a, b, .. }
        | Op::Cmp { a, b }
        | Op::Cmn { a, b } => vec![*a, *b],
        Op::Adc { a, b, carry_in, .. }
        | Op::Sbc { a, b, carry_in, .. }
        | Op::Rsc { a, b, carry_in, .. }
        | Op::And { a, b, carry_in, .. }
        | Op::Eor { a, b, carry_in, .. }
        | Op::Orr { a, b, carry_in, .. }
        | Op::Bic { a, b, carry_in, .. } => vec![*a, *b, *carry_in],
        Op::Tst { a, b, carry_in, .. } | Op::Teq { a, b, carry_in, .. } => {
            vec![*a, *b, *carry_in]
        }
        Op::Lsl {
            a,
            amount,
            carry_in,
            ..
        }
        | Op::Lsr {
            a,
            amount,
            carry_in,
            ..
        }
        | Op::Asr {
            a,
            amount,
            carry_in,
            ..
        }
        | Op::Ror {
            a,
            amount,
            carry_in,
            ..
        } => vec![*a, *amount, *carry_in],
        Op::Mov { a, carry_in, .. } | Op::Mvn { a, carry_in, .. } | Op::Rrx { a, carry_in, .. } => {
            vec![*a, *carry_in]
        }
        Op::MultiplyLong {
            a,
            b,
            acc_lo,
            acc_hi,
            ..
        } => vec![*a, *b, *acc_lo, *acc_hi],
        Op::Load { addr, .. } => vec![*addr],
        Op::Store { addr, value, .. } => vec![*addr, *value],
        Op::Intrinsic { args, .. } => args.clone(),
    }
}

/// Rewrites an op's operands through `subs` (one level; earlier
/// substitutions are already in the list).
fn substitute_op(op: &mut Op, subs: &[(ValueId, u32)]) {
    fn sub(value: &mut Value, subs: &[(ValueId, u32)]) {
        if let Value::Id(id) = *value {
            for (from, to) in subs {
                if *from == id {
                    *value = Value::Const(*to);
                    break;
                }
            }
        }
    }
    match op {
        Op::Add { a, b, .. }
        | Op::Sub { a, b, .. }
        | Op::Rsb { a, b, .. }
        | Op::Mul { a, b, .. }
        | Op::Cmp { a, b }
        | Op::Cmn { a, b } => {
            sub(a, subs);
            sub(b, subs);
        }
        Op::Adc { a, b, carry_in, .. }
        | Op::Sbc { a, b, carry_in, .. }
        | Op::Rsc { a, b, carry_in, .. }
        | Op::And { a, b, carry_in, .. }
        | Op::Eor { a, b, carry_in, .. }
        | Op::Orr { a, b, carry_in, .. }
        | Op::Bic { a, b, carry_in, .. } => {
            sub(a, subs);
            sub(b, subs);
            sub(carry_in, subs);
        }
        Op::Tst { a, b, carry_in, .. } | Op::Teq { a, b, carry_in, .. } => {
            sub(a, subs);
            sub(b, subs);
            sub(carry_in, subs);
        }
        Op::Lsl {
            a,
            amount,
            carry_in,
            ..
        }
        | Op::Lsr {
            a,
            amount,
            carry_in,
            ..
        }
        | Op::Asr {
            a,
            amount,
            carry_in,
            ..
        }
        | Op::Ror {
            a,
            amount,
            carry_in,
            ..
        } => {
            sub(a, subs);
            sub(amount, subs);
            sub(carry_in, subs);
        }
        Op::Mov { a, carry_in, .. } | Op::Mvn { a, carry_in, .. } | Op::Rrx { a, carry_in, .. } => {
            sub(a, subs);
            sub(carry_in, subs);
        }
        Op::MultiplyLong {
            a,
            b,
            acc_lo,
            acc_hi,
            ..
        } => {
            sub(a, subs);
            sub(b, subs);
            sub(acc_lo, subs);
            sub(acc_hi, subs);
        }
        Op::Load { addr, .. } => sub(addr, subs),
        Op::Store { addr, value, .. } => {
            sub(addr, subs);
            sub(value, subs);
        }
        Op::Intrinsic { args, .. } => {
            for arg in args {
                sub(arg, subs);
            }
        }
    }
}

/// Rewrites every argument list a terminator carries.
fn substitute_term(term: &mut Terminator, subs: &[(ValueId, u32)]) {
    let lists: Vec<&mut Vec<Value>> = match term {
        Terminator::Branch { args, .. } => vec![args],
        Terminator::BranchCond {
            then_args,
            else_args,
            ..
        } => vec![then_args, else_args],
        Terminator::Call {
            cont: Some((_, args)),
            state,
            ..
        } => vec![args, state],
        Terminator::Return { state, .. } => vec![state],
        Terminator::IndirectExit { state, .. } => vec![state],
        _ => vec![],
    };
    for list in lists {
        for value in list.iter_mut() {
            if let Value::Id(id) = *value {
                for (from, to) in subs {
                    if *from == id {
                        *value = Value::Const(*to);
                        break;
                    }
                }
            }
        }
    }
}

/// Rewrites an inst's operands through `subs`.
fn substitute_inst(inst: &mut Inst, subs: &[(ValueId, u32)]) {
    substitute_op(&mut inst.op, subs);
    for value in &mut inst.state {
        if let Value::Id(id) = *value {
            for (from, to) in subs {
                if *from == id {
                    *value = Value::Const(*to);
                    break;
                }
            }
        }
    }
}

/// Dead-flag elimination: clears the set-flags bit of ops whose flag
/// (or shifter-carry) dests are never read, and removes pure ops whose
/// every dest is dead. Liveness runs backward across the whole CFG:
/// block arguments thread the state, so a terminator argument only
/// keeps a value alive when the successor's parameter is live. Guards
/// read the current flag representatives, which pins those defs.
pub fn dead_flag_elimination(function: &mut Function) {
    let facts = block_facts(function);
    let live_in = liveness(function, &facts);
    // Live-at-exit per block, from the converged successor live-ins.
    let exits: Vec<HashSet<u32>> = (0..function.blocks.len())
        .map(|index| exit_live(function, &facts, index, &live_in))
        .collect();
    let cond_reads: Vec<Vec<u32>> = (0..function.blocks.len())
        .map(|index| term_cond_reads(&function.blocks[index], &facts[index]))
        .collect();
    for (index, block) in function.blocks.iter_mut().enumerate() {
        let mut live = exits[index].clone();
        // The terminator's own condition reads the flags at block exit.
        for id in &cond_reads[index] {
            live.insert(*id);
        }
        for inst_index in (0..block.insts.len()).rev() {
            mark_guard_reads(
                block.insts[inst_index].guard,
                &facts[index].flag_reps_before[inst_index],
                &block.params,
                &mut live,
            );
            let inst = &mut block.insts[inst_index];
            let (keep, plain) = keep_count(&inst.op, &inst.dests, &live);
            if keep == 0 {
                if is_pure(&inst.op) {
                    block.insts.remove(inst_index);
                    continue;
                }
            } else if keep < inst.dests.len() {
                inst.dests.truncate(keep);
                if plain {
                    set_plain(inst);
                }
            }
            for id in &facts[index].reads[inst_index] {
                live.insert(id.0);
            }
        }
    }
}

/// Live-at-exit for one block. Return, indirect-exit, and call state
/// lists are the outgoing machine state and are live directly; branch
/// arguments survive only when the successor's matching parameter is
/// live there.
fn exit_live(
    function: &Function,
    facts: &[BlockFacts],
    index: usize,
    live_in: &[HashSet<u32>],
) -> HashSet<u32> {
    let mut live: HashSet<u32> = HashSet::new();
    match &function.blocks[index].term {
        Terminator::Return { state, .. }
        | Terminator::IndirectExit { state, .. }
        | Terminator::Call { state, .. } => {
            for value in state {
                if let Value::Id(id) = value {
                    live.insert(id.0);
                }
            }
        }
        _ => {}
    }
    for (succ, args) in &facts[index].edges {
        for (arg, param) in args.iter().zip(function.blocks[*succ].params.iter()) {
            if live_in[*succ].contains(&param.0)
                && let Value::Id(id) = arg
            {
                live.insert(id.0);
            }
        }
    }
    live
}

/// Fixpoint liveness: per-block live-in sets over the whole CFG.
fn liveness(function: &Function, facts: &[BlockFacts]) -> Vec<HashSet<u32>> {
    let mut live_in: Vec<HashSet<u32>> = vec![HashSet::new(); function.blocks.len()];
    loop {
        let mut changed = false;
        for index in 0..function.blocks.len() {
            let mut live = exit_live(function, facts, index, &live_in);
            let block = &function.blocks[index];
            // The terminator's own condition reads the flags at block exit.
            for id in term_cond_reads(block, &facts[index]) {
                live.insert(id);
            }
            for inst_index in (0..block.insts.len()).rev() {
                let inst = &block.insts[inst_index];
                mark_guard_reads(
                    inst.guard,
                    &facts[index].flag_reps_before[inst_index],
                    &block.params,
                    &mut live,
                );
                for id in &facts[index].reads[inst_index] {
                    live.insert(id.0);
                }
            }
            if live != live_in[index] {
                live_in[index] = live;
                changed = true;
            }
        }
        if !changed {
            return live_in;
        }
    }
}

/// Marks the ids a guard reads: the in-block flag representatives, or
/// the block parameters for flags the block never defines.
fn mark_guard_reads(
    guard: Option<Condition>,
    reps_before: &[Option<ValueId>; 4],
    params: &[ValueId],
    live: &mut HashSet<u32>,
) {
    if let Some(cond) = guard
        && cond != Condition::Al
    {
        for (slot, rep) in FLAG_SLOTS.iter().zip(reps_before.iter()) {
            match rep {
                Some(id) => {
                    live.insert(id.0);
                }
                None => {
                    live.insert(params[*slot].0);
                }
            }
        }
    }
}

/// The ids a block-terminating condition reads: the same reads an
/// inst guard makes, but at block exit. Without this a conditional
/// branch is the only user of its `cmp` and the pass would drop the
/// comparison.
fn term_cond_reads(block: &Block, facts: &BlockFacts) -> Vec<u32> {
    let Terminator::BranchCond { cond, .. } = block.term else {
        return Vec::new();
    };
    let mut reps = facts.flag_reps_before.last().copied().unwrap_or([None; 4]);
    if let Some(inst) = block.insts.last() {
        for (slot, dest) in FLAG_SLOTS.iter().zip(flag_dests(&inst.op, &inst.dests)) {
            if let Some(id) = dest {
                reps[*slot - FLAG_SLOTS[0]] = Some(id);
            }
        }
    }
    let mut reads = Vec::new();
    for (slot, rep) in FLAG_SLOTS.iter().zip(reps.iter()) {
        match rep {
            Some(id) => reads.push(id.0),
            None => reads.push(block.params[*slot].0),
        }
    }
    if cond == Condition::Al {
        return Vec::new();
    }
    reads
}

/// Static per-block facts for the liveness walks.
struct BlockFacts {
    /// Flag-slot representatives before each inst.
    flag_reps_before: Vec<[Option<ValueId>; 4]>,
    /// Ids each inst reads.
    reads: Vec<Vec<ValueId>>,
    /// Successor blocks with their argument lists.
    edges: Vec<(usize, Vec<Value>)>,
}

/// Collects per-block facts: flag-rep snapshots (forward walk, slot
/// assignment from the per-op dest contracts) and operand lists.
fn block_facts(function: &Function) -> Vec<BlockFacts> {
    function
        .blocks
        .iter()
        .map(|block| {
            let mut flag_reps = [None; 4];
            let mut flag_reps_before = Vec::with_capacity(block.insts.len());
            let mut reads = Vec::with_capacity(block.insts.len());
            for inst in &block.insts {
                flag_reps_before.push(flag_reps);
                for (slot, dest) in FLAG_SLOTS.iter().zip(flag_dests(&inst.op, &inst.dests)) {
                    if let Some(id) = dest {
                        flag_reps[*slot - FLAG_SLOTS[0]] = Some(id);
                    }
                }
                reads.push(
                    operands(&inst.op)
                        .iter()
                        .chain(inst.state.iter())
                        .filter_map(|v| match v {
                            Value::Id(id) => Some(*id),
                            Value::Const(_) => None,
                        })
                        .collect(),
                );
            }
            let edges = successors(&block.term)
                .into_iter()
                .map(|(target, args)| (target.0 as usize, args))
                .collect();
            BlockFacts {
                flag_reps_before,
                reads,
                edges,
            }
        })
        .collect()
}

/// Successor blocks with their passed arguments.
fn successors(term: &Terminator) -> Vec<(crate::BlockId, Vec<Value>)> {
    match term {
        Terminator::Branch { target, args } => vec![(*target, args.clone())],
        Terminator::BranchCond {
            then_target,
            then_args,
            else_target,
            else_args,
            ..
        } => vec![
            (*then_target, then_args.clone()),
            (*else_target, else_args.clone()),
        ],
        Terminator::Call {
            cont: Some((target, args)),
            ..
        } => vec![(*target, args.clone())],
        _ => Vec::new(),
    }
}

/// How many leading dests survive when dead trailing flag/carry dests
/// are dropped, and whether the op must drop its set-flags bit. The
/// evaluator reads flag dests positionally: arithmetic ops and
/// expansion shifts are length-dispatched and may lose a trailing
/// suffix; movs/muls/mulls/THUMB format-1 shifts must keep the whole
/// flag tail or none of it.
fn keep_count(op: &Op, dests: &[ValueId], live: &HashSet<u32>) -> (usize, bool) {
    match op {
        // Comparisons are all flags: dead means removable.
        Op::Cmp { .. } | Op::Cmn { .. } | Op::Tst { .. } | Op::Teq { .. } => {
            (trailing(dests, 0, live), false)
        }
        // Arithmetic binds are length-aware: n, z, c, v drop
        // individually from the tail.
        Op::Add {
            set_flags: true, ..
        }
        | Op::Sub {
            set_flags: true, ..
        }
        | Op::Adc {
            set_flags: true, ..
        }
        | Op::Sbc {
            set_flags: true, ..
        }
        | Op::Rsb {
            set_flags: true, ..
        }
        | Op::Rsc {
            set_flags: true, ..
        }
        | Op::And {
            set_flags: true, ..
        }
        | Op::Eor {
            set_flags: true, ..
        }
        | Op::Orr {
            set_flags: true, ..
        }
        | Op::Bic {
            set_flags: true, ..
        } => (trailing(dests, 0, live), false),
        // All-or-nothing tails: the whole tail is dead, or none is.
        Op::Mov {
            set_flags: true, ..
        }
        | Op::Mvn {
            set_flags: true, ..
        }
        | Op::Mul {
            set_flags: true, ..
        } => all_or_nothing(dests, 1, live),
        Op::MultiplyLong {
            set_flags: true, ..
        } => all_or_nothing(dests, 2, live),
        Op::Lsl {
            set_flags: true, ..
        }
        | Op::Lsr {
            set_flags: true, ..
        }
        | Op::Asr {
            set_flags: true, ..
        }
        | Op::Ror {
            set_flags: true, ..
        } => all_or_nothing(dests, 1, live),
        // Carry-only expansion: the trailing carry dest drops alone.
        Op::Lsl {
            set_flags: false, ..
        }
        | Op::Lsr {
            set_flags: false, ..
        }
        | Op::Asr {
            set_flags: false, ..
        }
        | Op::Ror {
            set_flags: false, ..
        }
        | Op::Rrx { .. } => (trailing(dests, dests.len().saturating_sub(1), live), false),
        _ => (dests.len(), false),
    }
}

/// Drops dead dests one by one from the tail, never below `head`.
fn trailing(dests: &[ValueId], head: usize, live: &HashSet<u32>) -> usize {
    let mut keep = dests.len();
    while keep > head && !live.contains(&dests[keep - 1].0) {
        keep -= 1;
    }
    keep
}

/// Keeps everything unless every dest after `head` is dead, in which
/// case only the head prefix survives and set-flags must be cleared.
fn all_or_nothing(dests: &[ValueId], head: usize, live: &HashSet<u32>) -> (usize, bool) {
    if dests[head..].iter().all(|id| !live.contains(&id.0)) {
        (head, true)
    } else {
        (dests.len(), false)
    }
}

/// Ops with no effect beyond their dests.
fn is_pure(op: &Op) -> bool {
    // Loads are impure: a dead load still performs its bus access,
    // which charges waitstates and advances the sequential tracker.
    !matches!(
        op,
        Op::Store { .. } | Op::Intrinsic { .. } | Op::Load { .. }
    )
}

/// Clears the set-flags bit after an all-or-nothing truncation.
fn set_plain(inst: &mut Inst) {
    match &mut inst.op {
        Op::Add { set_flags, .. }
        | Op::Sub { set_flags, .. }
        | Op::Adc { set_flags, .. }
        | Op::Sbc { set_flags, .. }
        | Op::Rsb { set_flags, .. }
        | Op::Rsc { set_flags, .. }
        | Op::And { set_flags, .. }
        | Op::Eor { set_flags, .. }
        | Op::Orr { set_flags, .. }
        | Op::Bic { set_flags, .. }
        | Op::Mov { set_flags, .. }
        | Op::Mvn { set_flags, .. }
        | Op::Mul { set_flags, .. }
        | Op::MultiplyLong { set_flags, .. }
        | Op::Lsl { set_flags, .. }
        | Op::Lsr { set_flags, .. }
        | Op::Asr { set_flags, .. }
        | Op::Ror { set_flags, .. }
        | Op::Rrx { set_flags, .. } => *set_flags = false,
        _ => {}
    }
}

/// Constant propagation: folds unguarded, single-dest value ops whose
/// operands are all constants (flag-writing ops stay: their dests feed
/// the end state). Later operands and terminator arguments see the
/// folded constants.
pub fn constant_propagation(function: &mut Function) {
    for block in &mut function.blocks {
        let mut subs: Vec<(ValueId, u32)> = Vec::new();
        let mut folded: HashSet<usize> = HashSet::new();
        for index in 0..block.insts.len() {
            substitute_inst(&mut block.insts[index], &subs);
            let inst = &block.insts[index];
            if unconditional(inst)
                && inst.dests.len() == 1
                && !matches!(inst.op, Op::Store { .. } | Op::Intrinsic { .. })
                && operands(&inst.op)
                    .iter()
                    .all(|v| matches!(v, Value::Const(_)))
                && let Some(value) = eval_const(&inst.op)
            {
                subs.push((inst.dests[0], value));
                folded.insert(index);
            }
        }
        substitute_term(&mut block.term, &subs);
        let old = std::mem::take(&mut block.insts);
        block.insts = old
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !folded.contains(i))
            .map(|(_, inst)| inst)
            .collect();
    }
}

/// Evaluates a constant-only op to its dest value. `None` when the op
/// is not foldable (loads, stores, intrinsics, comparison forms).
fn eval_const(op: &Op) -> Option<u32> {
    let mut consts = Vec::new();
    for value in operands(op) {
        match value {
            Value::Const(c) => consts.push(c),
            Value::Id(_) => return None,
        }
    }
    let v = |i: usize| consts[i];
    match op {
        Op::Add { .. } => Some(v(0).wrapping_add(v(1))),
        Op::Sub { .. } => Some(v(0).wrapping_sub(v(1))),
        Op::Rsb { .. } => Some(v(1).wrapping_sub(v(0))),
        Op::Adc { carry_in, .. } => {
            Some(add_with_carry(v(0), v(1), *carry_in != Value::Const(0)).0)
        }
        Op::Sbc { carry_in, .. } => {
            Some(sub_with_borrow(v(0), v(1), *carry_in != Value::Const(0)).0)
        }
        Op::Rsc { carry_in, .. } => {
            Some(sub_with_borrow(v(1), v(0), *carry_in != Value::Const(0)).0)
        }
        Op::And { .. } => Some(v(0) & v(1)),
        Op::Eor { .. } => Some(v(0) ^ v(1)),
        Op::Orr { .. } => Some(v(0) | v(1)),
        Op::Bic { .. } => Some(v(0) & !v(1)),
        Op::Mov { .. } => Some(v(0)),
        Op::Mvn { .. } => Some(!v(0)),
        Op::Mul { .. } => Some(v(0).wrapping_mul(v(1))),
        Op::Lsl { carry_in, .. } => Some(
            apply_shift(
                v(0),
                v(1) & 0xFF,
                *carry_in != Value::Const(0),
                ShiftKind::Lsl,
            )
            .0,
        ),
        Op::Lsr { carry_in, .. } => Some(
            apply_shift(
                v(0),
                v(1) & 0xFF,
                *carry_in != Value::Const(0),
                ShiftKind::Lsr,
            )
            .0,
        ),
        Op::Asr { carry_in, .. } => Some(
            apply_shift(
                v(0),
                v(1) & 0xFF,
                *carry_in != Value::Const(0),
                ShiftKind::Asr,
            )
            .0,
        ),
        Op::Ror { carry_in, .. } => Some(
            apply_shift(
                v(0),
                v(1) & 0xFF,
                *carry_in != Value::Const(0),
                ShiftKind::Ror,
            )
            .0,
        ),
        Op::Rrx { carry_in, .. } => {
            let cin = *carry_in != Value::Const(0);
            Some((v(0) >> 1) | (u32::from(cin) << 31))
        }
        _ => None,
    }
}

/// Copy propagation: rewrites later uses of a plain unguarded
/// `mov dest, source` with the source value. The move itself stays:
/// its dest may be the end state's representative.
pub fn copy_propagation(function: &mut Function) {
    for block in &mut function.blocks {
        let mut copies: Vec<(ValueId, Value)> = Vec::new();
        for index in 0..block.insts.len() {
            {
                let inst = &mut block.insts[index];
                rewrite_copies(inst, &copies);
            }
            let inst = &block.insts[index];
            if unconditional(inst)
                && inst.dests.len() == 1
                && let Op::Mov {
                    set_flags: false,
                    a,
                    ..
                } = &inst.op
            {
                copies.push((inst.dests[0], *a));
            }
        }
        rewrite_term_copies(&mut block.term, &copies);
    }
}

/// Rewrites an op's operands through the copy list.
fn rewrite_copies(inst: &mut Inst, copies: &[(ValueId, Value)]) {
    fn sub(value: &mut Value, copies: &[(ValueId, Value)]) {
        if let Value::Id(id) = *value {
            for (from, to) in copies {
                if *from == id {
                    *value = *to;
                    break;
                }
            }
        }
    }
    match &mut inst.op {
        Op::Add { a, b, .. }
        | Op::Sub { a, b, .. }
        | Op::Rsb { a, b, .. }
        | Op::Mul { a, b, .. }
        | Op::Cmp { a, b }
        | Op::Cmn { a, b } => {
            sub(a, copies);
            sub(b, copies);
        }
        Op::Adc { a, b, carry_in, .. }
        | Op::Sbc { a, b, carry_in, .. }
        | Op::Rsc { a, b, carry_in, .. }
        | Op::And { a, b, carry_in, .. }
        | Op::Eor { a, b, carry_in, .. }
        | Op::Orr { a, b, carry_in, .. }
        | Op::Bic { a, b, carry_in, .. } => {
            sub(a, copies);
            sub(b, copies);
            sub(carry_in, copies);
        }
        Op::Tst { a, b, carry_in, .. } | Op::Teq { a, b, carry_in, .. } => {
            sub(a, copies);
            sub(b, copies);
            sub(carry_in, copies);
        }
        Op::Lsl {
            a,
            amount,
            carry_in,
            ..
        }
        | Op::Lsr {
            a,
            amount,
            carry_in,
            ..
        }
        | Op::Asr {
            a,
            amount,
            carry_in,
            ..
        }
        | Op::Ror {
            a,
            amount,
            carry_in,
            ..
        } => {
            sub(a, copies);
            sub(amount, copies);
            sub(carry_in, copies);
        }
        Op::Mov { a, carry_in, .. } | Op::Mvn { a, carry_in, .. } | Op::Rrx { a, carry_in, .. } => {
            sub(a, copies);
            sub(carry_in, copies);
        }
        Op::MultiplyLong {
            a,
            b,
            acc_lo,
            acc_hi,
            ..
        } => {
            sub(a, copies);
            sub(b, copies);
            sub(acc_lo, copies);
            sub(acc_hi, copies);
        }
        Op::Load { addr, .. } => sub(addr, copies),
        Op::Store { addr, value, .. } => {
            sub(addr, copies);
            sub(value, copies);
        }
        Op::Intrinsic { args, .. } => {
            for arg in args {
                sub(arg, copies);
            }
        }
    }
}

fn rewrite_term_copies(term: &mut Terminator, copies: &[(ValueId, Value)]) {
    let lists: Vec<&mut Vec<Value>> = match term {
        Terminator::Branch { args, .. } => vec![args],
        Terminator::BranchCond {
            then_args,
            else_args,
            ..
        } => vec![then_args, else_args],
        Terminator::Call {
            cont: Some((_, args)),
            state,
            ..
        } => vec![args, state],
        Terminator::Return { state, .. } => vec![state],
        Terminator::IndirectExit { state, .. } => vec![state],
        _ => vec![],
    };
    for list in lists {
        for value in list.iter_mut() {
            if let Value::Id(id) = *value {
                for (from, to) in copies {
                    if *from == id {
                        *value = *to;
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lift::{FunctionRegion, lift_function};
    use analysis::{FlagMap, Flags, State};

    /// Lifts `words` as one ARM region from the canonical base.
    fn lift_arm(words: &[u32]) -> Function {
        let mut bytes = vec![0u8; 256];
        for (index, word) in words.iter().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        let image = rom::Image::from_bytes(bytes).expect("parses");
        let mut map = FlagMap::from_image(&image);
        let base = map.canonical().base();
        map.mark_range(base, base + words.len() as u32 * 4, Flags::EXEC);
        lift_function(
            &image,
            &map,
            FunctionRegion {
                start: base,
                end: base + words.len() as u32 * 4,
                state: State::Arm,
            },
        )
    }

    /// A conditional branch reads the comparison's flags; the pass
    /// must not drop the `cmp` as dead (the recomp corpus
    /// regressed exactly here).
    #[test]
    fn branch_condition_keeps_its_comparison() {
        // cmp r0, #1; bne .
        let mut function = lift_arm(&[0xE350_0001, 0x1AFF_FFFE]);
        optimize(&mut function);
        assert!(
            function.blocks[0]
                .insts
                .iter()
                .any(|inst| matches!(inst.op, Op::Cmp { .. })),
            "the comparison feeding a branch condition must survive"
        );
    }

    /// A load whose dest is immediately overwritten is still impure:
    /// its bus access charges waitstates and advances the sequential
    /// tracker, so removing it would skew every later access cost.
    #[test]
    fn dead_loads_survive_optimization() {
        // ldr r0, [r1]; mov r0, #7; bx lr
        let mut function = lift_arm(&[0xE591_0000, 0xE3A0_0007, 0xE12F_FF11]);
        optimize(&mut function);
        assert!(
            function.blocks[0]
                .insts
                .iter()
                .any(|inst| matches!(inst.op, Op::Load { .. })),
            "a dead load must survive: its bus access charges cycles"
        );
    }
}
