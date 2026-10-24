//! AAPCS-lite signature recovery.
//!
//! Recovers one [`Signature`] per lifted function from its body plus
//! the call graph, with honest confidence:
//!
//! - Register arguments are `r0..r3` whose incoming value is read
//!   before any redefinition on some CFG path. The value-flow walk
//!   follows terminator arguments block to block, so a read two
//!   blocks deep still counts. The count is the highest read register
//!   plus one; gaps in the prefix stay as guessed `u32` placeholders,
//!   because AAPCS fills the registers in order.
//! - Types are usage-based: a value used as a load/store address is
//!   `Ptr`, a value compared with a signed condition or shifted with
//!   `asr` is `I32`, a byte/half load result is `u8`/`u16`, and
//!   everything else defaults to `u32`.
//! - The return type comes from `r0` at the `Return` terminators:
//!   redefined on some return path means a value of the
//!   redefinition's type; untouched on every path means `void`.
//! - Stack arguments are counted from entry-block word loads at
//!   `sp`-relative offsets 0..12; these are always guesses, because
//!   prologue spill loads look identical.
//! - Call sites contribute evidence across the graph. A written
//!   register at a call counts toward the callee's argument count
//!   (the minimum across callers wins when the body shows no reads),
//!   a forwarded parameter raises the caller's own count (the caller
//!   must have received it to pass it on), and pointer types
//!   propagate between caller and callee to a fixed point.
//!
//! A signature is [`Confidence::Measured`] when its count came from
//! body reads, every register parameter is contiguous, nothing was
//! inferred from forwarding or caller fallback, and no stack
//! parameters were guessed; anything else renders with the
//! `/* guessed */` marker via [`fmt_signature`].

use std::collections::BTreeMap;

use crate::opt::operands;
use crate::{Block, Function, Inst, Op, Reg, Terminator, Value, ValueId};

/// The basic recovered type of one argument or return value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    /// Unsigned 32-bit integer (the honest default).
    U32,
    /// Signed 32-bit integer.
    I32,
    /// Unsigned byte.
    U8,
    /// Unsigned halfword.
    U16,
    /// Pointer (any value used as a memory address).
    Ptr,
}

impl Type {
    /// The C-like spelling used in rendered signatures.
    pub fn name(self) -> &'static str {
        match self {
            Type::U32 => "u32",
            Type::I32 => "i32",
            Type::U8 => "u8",
            Type::U16 => "u16",
            Type::Ptr => "u32*",
        }
    }
}

/// How directly the signature is evidenced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    /// Count from body reads, contiguous registers, nothing guessed.
    Measured,
    /// Any component inferred without direct body evidence.
    Guessed,
}

/// One recovered function signature. `params` holds register
/// parameters first (`r0..r3`) and any stack parameters after them;
/// `registers` counts the register prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// Parameter types in calling order.
    pub params: Vec<Type>,
    /// How many of `params` live in `r0..r3`; the rest are stack
    /// parameters.
    pub registers: usize,
    /// The return type, or `None` for a void function.
    pub ret: Option<Type>,
    /// How directly the signature is evidenced.
    pub confidence: Confidence,
}

/// Recovers signatures for every function, keyed by function name.
pub fn recover(functions: &[Function]) -> BTreeMap<String, Signature> {
    let mut bodies: Vec<Body> = functions.iter().map(Body::recover).collect();
    propagate(functions, &mut bodies);
    functions
        .iter()
        .zip(&bodies)
        .map(|(function, body)| (function.name.clone(), body.finish()))
        .collect()
}

/// Renders one signature as C-like text: `u32* sub_08000000(u32 r0,
/// u32 r1) /* guessed */`. Stack parameters render as `u32 [sp]`,
/// `u32 [sp, #4]`, and so on.
pub fn fmt_signature(name: &str, signature: &Signature) -> String {
    let parts: Vec<String> = signature
        .params
        .iter()
        .enumerate()
        .map(|(index, param)| {
            if index < signature.registers {
                format!("{} r{index}", param.name())
            } else {
                match (index - signature.registers) * 4 {
                    0 => format!("{} [sp]", param.name()),
                    offset => format!("{} [sp, #{offset}]", param.name()),
                }
            }
        })
        .collect();
    let ret = signature.ret.map_or("void", Type::name);
    let guessed = match signature.confidence {
        Confidence::Guessed => " /* guessed */",
        Confidence::Measured => "",
    };
    if parts.is_empty() {
        return format!("{ret} {name}(void){guessed}");
    }
    format!("{ret} {name}({}){guessed}", parts.join(", "))
}

/// One static call site: the target's name, the values passed in
/// `r0..r3`, and which of those the caller redefined since entry.
#[derive(Clone)]
struct CallSite {
    target: String,
    values: [Option<ValueId>; 4],
    written: [bool; 4],
}

/// Per-function evidence gathered from one body scan plus the call
/// graph fixpoint.
struct Body {
    /// Whether the incoming `r0..r3` value is read before any
    /// redefinition.
    arg_reads: [bool; 4],
    /// Type evidence per register argument, from uses and
    /// propagation.
    arg_types: [Option<Type>; 4],
    /// Stack parameter count guessed from entry-block loads.
    stack_args: usize,
    /// `Some(None)` for a proven-void function, `Some(Some(t))` for a
    /// returning one, `None` when the body has no returns at all.
    ret: Option<Option<Type>>,
    /// Static call sites.
    calls: Vec<CallSite>,
    /// The highest register slot the function forwards to a callee.
    forwarded_count: usize,
    /// The minimum observed register count across all callers.
    caller_min_count: usize,
}

impl Body {
    fn recover(function: &Function) -> Body {
        let mut arg_reads = [false; 4];
        let mut arg_types: [Option<Type>; 4] = [None; 4];
        for slot in 0..4 {
            let (read, hint) = arg_flow(function, slot);
            arg_reads[slot] = read;
            arg_types[slot] = hint;
        }
        Body {
            arg_reads,
            arg_types,
            stack_args: stack_args(function),
            ret: return_type(function),
            calls: call_sites(function),
            forwarded_count: 0,
            caller_min_count: 0,
        }
    }

    /// The register count from body reads: the highest read slot plus
    /// one.
    fn body_count(&self) -> usize {
        (0..4)
            .filter(|slot| self.arg_reads[*slot])
            .map(|slot| slot + 1)
            .max()
            .unwrap_or(0)
    }

    /// Combines body and propagated evidence into the signature.
    fn finish(&self) -> Signature {
        let mut confidence = Confidence::Measured;
        let mut count = self.body_count();
        if self.forwarded_count > count {
            count = self.forwarded_count;
            confidence = Confidence::Guessed;
        }
        if count == 0 && self.caller_min_count > 0 {
            count = self.caller_min_count;
            confidence = Confidence::Guessed;
        }
        let mut params = Vec::with_capacity(count + self.stack_args);
        for slot in 0..count {
            if let Some(ev_type) = self.arg_types.get(slot).copied().flatten() {
                params.push(ev_type);
                continue;
            }
            if slot < 4 && !self.arg_reads[slot] {
                // A gap inside the register prefix: AAPCS fills the
                // registers in order, so the slot exists but its use
                // is unknown.
                confidence = Confidence::Guessed;
            }
            params.push(Type::U32);
        }
        if self.stack_args > 0 {
            confidence = Confidence::Guessed;
            params.extend(std::iter::repeat_n(Type::U32, self.stack_args));
        }
        Signature {
            params,
            registers: count,
            ret: self.ret.flatten(),
            confidence,
        }
    }
}

/// Walks the CFG following terminator arguments for one register
/// slot, reporting whether the incoming argument is read somewhere
/// and the strongest type its use sites suggest. A block stays
/// `from_entry` while its slot parameter still carries the incoming
/// value on some path (any-path semantics, like register liveness).
fn arg_flow(function: &Function, slot: usize) -> (bool, Option<Type>) {
    let addresses = address_values(function);
    let mut from_entry = vec![false; function.blocks.len()];
    from_entry[0] = true;
    let mut read = false;
    let mut hint: Option<Type> = None;
    let mut queue = vec![0usize];
    while let Some(index) = queue.pop() {
        if !from_entry[index] {
            continue;
        }
        let block = &function.blocks[index];
        let Some(arg) = block.params.get(slot).copied() else {
            continue;
        };
        let pointer = addresses.contains(&arg).then_some(Type::Ptr);
        for (inst_index, inst) in block.insts.iter().enumerate() {
            if operand_uses(inst, arg) {
                read = true;
                hint = merge_hint(hint, merge_hint(pointer, use_hint(block, inst_index, arg)));
            }
        }
        for (succ, args) in edges(block) {
            let unchanged = matches!(args.get(slot), Some(Value::Id(id)) if *id == arg);
            if unchanged && !from_entry[succ.0 as usize] {
                from_entry[succ.0 as usize] = true;
                queue.push(succ.0 as usize);
            }
        }
    }
    (read, hint)
}

/// Whether any operand of the inst reads `id`.
fn operand_uses(inst: &Inst, id: ValueId) -> bool {
    operands(&inst.op)
        .iter()
        .any(|value| matches!(value, Value::Id(used) if *used == id))
}

/// Every value that flows into a load/store address, directly or
/// through `add`/`mov` chains: the pointer evidence set.
fn address_values(function: &Function) -> std::collections::HashSet<ValueId> {
    let mut set = std::collections::HashSet::new();
    for block in &function.blocks {
        for inst in &block.insts {
            let addrs: &[Value] = match &inst.op {
                Op::Load { addr, .. } => std::slice::from_ref(addr),
                Op::Store { addr, .. } => std::slice::from_ref(addr),
                _ => &[],
            };
            for value in addrs {
                if let Value::Id(id) = value {
                    set.insert(*id);
                }
            }
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        for block in &function.blocks {
            for inst in &block.insts {
                let Some(dest) = inst.dests.first() else {
                    continue;
                };
                if !set.contains(dest) {
                    continue;
                }
                let bases: &[Value] = match &inst.op {
                    Op::Add { a, b, .. } => &[*a, *b],
                    Op::Mov { a, .. } => std::slice::from_ref(a),
                    _ => &[],
                };
                for value in bases {
                    if let Value::Id(id) = value
                        && set.insert(*id)
                    {
                        changed = true;
                    }
                }
            }
        }
    }
    set
}

/// The successor edges of one block with their argument lists.
fn edges(block: &Block) -> Vec<(crate::BlockId, &[Value])> {
    match &block.term {
        Terminator::Branch { target, args } => vec![(*target, args)],
        Terminator::BranchCond {
            then_target,
            then_args,
            else_target,
            else_args,
            ..
        } => vec![(*then_target, then_args), (*else_target, else_args)],
        Terminator::Call {
            cont: Some((cont, args)),
            ..
        } => vec![(*cont, args)],
        _ => Vec::new(),
    }
}

/// Merges one hint into the running evidence, preferring `Ptr` over
/// `I32` over nothing.
fn merge_hint(current: Option<Type>, hint: Option<Type>) -> Option<Type> {
    match (current, hint) {
        (_, Some(Type::Ptr)) | (Some(Type::Ptr), _) => Some(Type::Ptr),
        (_, Some(Type::I32)) | (Some(Type::I32), _) => Some(Type::I32),
        (current, _) => current,
    }
}

/// The type one use site of `arg` suggests.
fn use_hint(block: &Block, index: usize, arg: ValueId) -> Option<Type> {
    let inst = &block.insts[index];
    match &inst.op {
        Op::Load { addr, .. } | Op::Store { addr, .. } if uses_value(addr, arg) => Some(Type::Ptr),
        Op::Asr { a, .. } if uses_value(a, arg) => Some(Type::I32),
        Op::Cmp { a, b } if uses_value(a, arg) || uses_value(b, arg) => {
            signed_branch(block, index).then_some(Type::I32)
        }
        _ => None,
    }
}

fn uses_value(value: &Value, id: ValueId) -> bool {
    matches!(value, Value::Id(used) if *used == id)
}

/// Whether the block branches on a signed condition. A compared value
/// feeding such a branch is being treated as signed.
fn signed_branch(block: &Block, index: usize) -> bool {
    if index + 1 != block.insts.len() {
        return false;
    }
    matches!(
        &block.term,
        Terminator::BranchCond {
            cond: decode::Condition::Ge
                | decode::Condition::Lt
                | decode::Condition::Gt
                | decode::Condition::Le,
            ..
        }
    )
}

/// Guesses stack parameters from entry-block word loads at small
/// sp-relative offsets.
fn stack_args(function: &Function) -> usize {
    let entry = &function.blocks[0];
    let Some(sp) = entry.params.get(Reg::Sp as usize).copied() else {
        return 0;
    };
    let mut max_slot = 0usize;
    let mut found = false;
    for inst in &entry.insts {
        let Op::Load {
            addr,
            size: crate::Size::Word,
            signed: false,
            mode: crate::LoadMode::Ldr,
        } = &inst.op
        else {
            continue;
        };
        let Some(offset) = sp_offset(addr, sp, entry) else {
            continue;
        };
        if offset % 4 == 0 && offset <= 12 {
            max_slot = max_slot.max(offset as usize / 4 + 1);
            found = true;
        }
    }
    if found { max_slot } else { 0 }
}

/// The byte offset from `sp` that `addr` computes, when it is `sp`
/// itself or a small `sp + k` addition defined in the entry block.
fn sp_offset(addr: &Value, sp: ValueId, entry: &Block) -> Option<u32> {
    let Value::Id(id) = addr else {
        return None;
    };
    if *id == sp {
        return Some(0);
    }
    let def = entry
        .insts
        .iter()
        .find(|inst| inst.dests.first().is_some_and(|dest| *dest == *id))?;
    match &def.op {
        Op::Add {
            a: Value::Id(base),
            b: Value::Const(offset),
            ..
        }
        | Op::Add {
            a: Value::Const(offset),
            b: Value::Id(base),
            ..
        } if *base == sp => Some(*offset),
        _ => None,
    }
}

/// The return evidence: `Some(None)` for proven-void, `Some(Some(t))`
/// for a returning function, `None` when the body has no returns.
fn return_type(function: &Function) -> Option<Option<Type>> {
    let incoming = function.blocks[0].params.get(Reg::R0 as usize).copied();
    let mut any_return = false;
    let mut value: Option<Type> = None;
    for block in &function.blocks {
        let Terminator::Return { state, .. } = &block.term else {
            continue;
        };
        any_return = true;
        match state.first() {
            Some(Value::Id(id)) if Some(*id) == incoming => {}
            Some(Value::Id(id)) => value = Some(def_type(function, *id)),
            Some(Value::Const(_)) => value = Some(Type::U32),
            None => {}
        }
    }
    if value.is_some() {
        Some(value)
    } else if any_return {
        Some(None)
    } else {
        None
    }
}

/// The type of one computed value from its single definition.
fn def_type(function: &Function, id: ValueId) -> Type {
    for block in &function.blocks {
        for inst in &block.insts {
            if !inst.dests.first().is_some_and(|dest| *dest == id) {
                continue;
            }
            return match &inst.op {
                Op::Load {
                    size: crate::Size::Byte,
                    signed: false,
                    ..
                } => Type::U8,
                Op::Load {
                    size: crate::Size::Half,
                    signed: false,
                    ..
                } => Type::U16,
                Op::Load { signed: true, .. } | Op::Asr { .. } => Type::I32,
                Op::And {
                    b: Value::Const(mask),
                    ..
                } if *mask == 0xFF => Type::U8,
                Op::And {
                    b: Value::Const(mask),
                    ..
                } if *mask == 0xFFFF => Type::U16,
                _ => Type::U32,
            };
        }
    }
    Type::U32
}

/// The static call sites of one function.
fn call_sites(function: &Function) -> Vec<CallSite> {
    let mut sites = Vec::new();
    for block in &function.blocks {
        let Terminator::Call { target, state, .. } = &block.term else {
            continue;
        };
        let Value::Const(addr) = target else {
            continue;
        };
        let mut values = [None; 4];
        let mut written = [false; 4];
        for (slot, value) in values.iter_mut().enumerate() {
            match state.get(slot) {
                Some(Value::Id(id)) => {
                    *value = Some(*id);
                    written[slot] = !block.params.get(slot).is_some_and(|param| param == id);
                }
                Some(Value::Const(_)) => written[slot] = true,
                None => written[slot] = false,
            }
        }
        sites.push(CallSite {
            target: format!("sub_{addr:08x}"),
            values,
            written,
        });
    }
    sites
}

/// The call-graph fixpoint: counts and pointer types propagate
/// between callers and callees until stable.
fn propagate(functions: &[Function], bodies: &mut [Body]) {
    let index_of: BTreeMap<&str, usize> = functions
        .iter()
        .enumerate()
        .map(|(index, function)| (function.name.as_str(), index))
        .collect();
    let mut caller_min = vec![usize::MAX; functions.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for (caller_index, caller) in functions.iter().enumerate() {
            let sites = bodies[caller_index].calls.clone();
            for site in &sites {
                let Some(&callee_index) = index_of.get(site.target.as_str()) else {
                    continue;
                };
                // Count evidence for the callee: written registers,
                // bounded by the minimum across callers. A forwarded
                // register whose value the callee actually reads
                // raises the caller's own count, since the caller
                // must have received it to pass it on.
                let mut observed = 0usize;
                let mut forwarded = 0usize;
                for slot in 0..4 {
                    if site.written[slot] {
                        observed = slot + 1;
                    } else if bodies[callee_index].arg_reads[slot] {
                        forwarded = forwarded.max(slot + 1);
                    }
                }
                if observed > 0 && observed < caller_min[callee_index] {
                    caller_min[callee_index] = observed;
                    changed = true;
                }
                if forwarded > bodies[caller_index].forwarded_count {
                    bodies[caller_index].forwarded_count = forwarded;
                    changed = true;
                }
                // Type propagation in both directions, for forwarded
                // parameters.
                for (slot, value) in site.values.iter().enumerate() {
                    let Some(value) = value else {
                        continue;
                    };
                    if caller.blocks[0].params.get(slot) != Some(value) {
                        continue;
                    }
                    let hint = bodies[caller_index].arg_types[slot];
                    let merged = merge_hint(bodies[callee_index].arg_types[slot], hint);
                    if merged != bodies[callee_index].arg_types[slot] {
                        bodies[callee_index].arg_types[slot] = merged;
                        changed = true;
                    }
                    let back = bodies[callee_index].arg_types[slot];
                    let merged = merge_hint(bodies[caller_index].arg_types[slot], back);
                    if merged != bodies[caller_index].arg_types[slot] {
                        bodies[caller_index].arg_types[slot] = merged;
                        changed = true;
                    }
                }
            }
        }
    }
    for (body, min) in bodies.iter_mut().zip(caller_min) {
        body.caller_min_count = if min == usize::MAX { 0 } else { min };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Block, BlockId, Size};
    use decode::Condition;

    const SP: usize = Reg::Sp as usize;

    /// The SSA id of block `block`'s parameter for `slot`.
    fn pid(block: usize, slot: usize) -> ValueId {
        ValueId((block * 20 + slot) as u32)
    }

    fn pval(block: usize, slot: usize) -> Value {
        Value::Id(pid(block, slot))
    }

    /// Def ids live above the param id space.
    fn did(n: u32) -> ValueId {
        ValueId(10_000 + n)
    }

    fn def_inst(dest: ValueId, op: Op) -> Inst {
        Inst {
            guard: None,
            dests: vec![dest],
            op,
            pc: 0,
            state: Vec::new(),
        }
    }

    fn bare_inst(op: Op) -> Inst {
        Inst {
            guard: None,
            dests: Vec::new(),
            op,
            pc: 0,
            state: Vec::new(),
        }
    }

    /// The 20-slot outgoing state of `block`, with overrides.
    fn out_state(block: usize, overrides: &[(usize, Value)]) -> Vec<Value> {
        let mut values: Vec<Value> = (0..20).map(|slot| pval(block, slot)).collect();
        for (slot, value) in overrides {
            values[*slot] = *value;
        }
        values
    }

    fn blk(insts: Vec<Inst>, term: Terminator) -> Block {
        Block {
            addr: 0,
            params: Vec::new(),
            insts,
            term,
        }
    }

    /// Assigns the 20-slot parameter lists by block index.
    fn fn_of(name: &str, blocks: Vec<Block>) -> Function {
        let mut blocks = blocks;
        for (index, block) in blocks.iter_mut().enumerate() {
            block.params = (0..20).map(|slot| pid(index, slot)).collect();
        }
        Function {
            name: name.to_string(),
            blocks,
        }
    }

    #[test]
    fn leaf_two_args_returns_sum() {
        let function = fn_of(
            "sub_08000000",
            vec![blk(
                vec![def_inst(
                    did(1),
                    Op::Add {
                        set_flags: false,
                        a: pval(0, 0),
                        b: pval(0, 1),
                    },
                )],
                Terminator::Return {
                    state: out_state(0, &[(0, Value::Id(did(1)))]),
                    switches_state: false,
                },
            )],
        );
        let signatures = recover(&[function]);
        let signature = &signatures["sub_08000000"];
        assert_eq!(
            signature,
            &Signature {
                params: vec![Type::U32, Type::U32],
                registers: 2,
                ret: Some(Type::U32),
                confidence: Confidence::Measured,
            }
        );
        assert_eq!(
            fmt_signature("sub_08000000", signature),
            "u32 sub_08000000(u32 r0, u32 r1)"
        );
    }

    #[test]
    fn pointer_arg_and_word_load_return() {
        let function = fn_of(
            "sub_08000000",
            vec![blk(
                vec![def_inst(
                    did(1),
                    Op::Load {
                        addr: pval(0, 0),
                        size: Size::Word,
                        signed: false,
                        mode: crate::LoadMode::Ldr,
                    },
                )],
                Terminator::Return {
                    state: out_state(0, &[(0, Value::Id(did(1)))]),
                    switches_state: false,
                },
            )],
        );
        let signatures = recover(&[function]);
        assert_eq!(
            fmt_signature("sub_08000000", &signatures["sub_08000000"]),
            "u32 sub_08000000(u32* r0)"
        );
    }

    #[test]
    fn struct_ptr_offset_reads_byte_field() {
        let function = fn_of(
            "sub_08000000",
            vec![blk(
                vec![
                    def_inst(
                        did(1),
                        Op::Add {
                            set_flags: false,
                            a: pval(0, 0),
                            b: Value::Const(4),
                        },
                    ),
                    def_inst(
                        did(2),
                        Op::Load {
                            addr: Value::Id(did(1)),
                            size: Size::Byte,
                            signed: false,
                            mode: crate::LoadMode::Ldr,
                        },
                    ),
                ],
                Terminator::Return {
                    state: out_state(0, &[(0, Value::Id(did(2)))]),
                    switches_state: false,
                },
            )],
        );
        let signatures = recover(&[function]);
        assert_eq!(
            fmt_signature("sub_08000000", &signatures["sub_08000000"]),
            "u8 sub_08000000(u32* r0)"
        );
    }

    #[test]
    fn four_register_arguments() {
        let function = fn_of(
            "sub_08000000",
            vec![blk(
                vec![
                    def_inst(
                        did(1),
                        Op::Add {
                            set_flags: false,
                            a: pval(0, 0),
                            b: pval(0, 1),
                        },
                    ),
                    def_inst(
                        did(2),
                        Op::Add {
                            set_flags: false,
                            a: pval(0, 2),
                            b: pval(0, 3),
                        },
                    ),
                    def_inst(
                        did(3),
                        Op::Add {
                            set_flags: false,
                            a: Value::Id(did(1)),
                            b: Value::Id(did(2)),
                        },
                    ),
                ],
                Terminator::Return {
                    state: out_state(0, &[(0, Value::Id(did(3)))]),
                    switches_state: false,
                },
            )],
        );
        let signatures = recover(&[function]);
        assert_eq!(
            fmt_signature("sub_08000000", &signatures["sub_08000000"]),
            "u32 sub_08000000(u32 r0, u32 r1, u32 r2, u32 r3)"
        );
    }

    #[test]
    fn void_leaf_is_measured() {
        let function = fn_of(
            "sub_08000000",
            vec![blk(
                Vec::new(),
                Terminator::Return {
                    state: out_state(0, &[]),
                    switches_state: false,
                },
            )],
        );
        let signatures = recover(&[function]);
        let signature = &signatures["sub_08000000"];
        assert_eq!(signature.confidence, Confidence::Measured);
        assert_eq!(
            fmt_signature("sub_08000000", signature),
            "void sub_08000000(void)"
        );
    }

    #[test]
    fn gap_in_register_prefix_is_guessed() {
        let function = fn_of(
            "sub_08000000",
            vec![blk(
                vec![def_inst(
                    did(1),
                    Op::Add {
                        set_flags: false,
                        a: pval(0, 0),
                        b: pval(0, 2),
                    },
                )],
                Terminator::Return {
                    state: out_state(0, &[(0, Value::Id(did(1)))]),
                    switches_state: false,
                },
            )],
        );
        let signatures = recover(&[function]);
        assert_eq!(
            fmt_signature("sub_08000000", &signatures["sub_08000000"]),
            "u32 sub_08000000(u32 r0, u32 r1, u32 r2) /* guessed */"
        );
    }

    #[test]
    fn stack_argument_is_a_guess() {
        let function = fn_of(
            "sub_08000000",
            vec![blk(
                vec![def_inst(
                    did(1),
                    Op::Load {
                        addr: pval(0, SP),
                        size: Size::Word,
                        signed: false,
                        mode: crate::LoadMode::Ldr,
                    },
                )],
                Terminator::Return {
                    state: out_state(0, &[(0, Value::Id(did(1)))]),
                    switches_state: false,
                },
            )],
        );
        let signatures = recover(&[function]);
        assert_eq!(
            fmt_signature("sub_08000000", &signatures["sub_08000000"]),
            "u32 sub_08000000(u32 [sp]) /* guessed */"
        );
    }

    #[test]
    fn signed_compare_makes_i32() {
        let function = fn_of(
            "sub_08000000",
            vec![
                blk(
                    vec![bare_inst(Op::Cmp {
                        a: pval(0, 0),
                        b: Value::Const(10),
                    })],
                    Terminator::BranchCond {
                        cond: Condition::Lt,
                        then_target: BlockId(1),
                        then_args: out_state(0, &[]),
                        else_target: BlockId(2),
                        else_args: out_state(0, &[]),
                    },
                ),
                blk(
                    Vec::new(),
                    Terminator::Return {
                        state: out_state(0, &[]),
                        switches_state: false,
                    },
                ),
                blk(
                    Vec::new(),
                    Terminator::Return {
                        state: out_state(0, &[]),
                        switches_state: false,
                    },
                ),
            ],
        );
        let signatures = recover(&[function]);
        assert_eq!(
            fmt_signature("sub_08000000", &signatures["sub_08000000"]),
            "void sub_08000000(i32 r0)"
        );
    }

    #[test]
    fn argument_read_two_blocks_deep_counts() {
        let function = fn_of(
            "sub_08000000",
            vec![
                blk(
                    Vec::new(),
                    Terminator::Branch {
                        target: BlockId(1),
                        args: out_state(0, &[]),
                    },
                ),
                blk(
                    Vec::new(),
                    Terminator::Branch {
                        target: BlockId(2),
                        args: out_state(1, &[]),
                    },
                ),
                blk(
                    vec![def_inst(
                        did(1),
                        Op::Add {
                            set_flags: false,
                            a: pval(2, 0),
                            b: Value::Const(2),
                        },
                    )],
                    Terminator::Return {
                        state: out_state(2, &[(0, Value::Id(did(1)))]),
                        switches_state: false,
                    },
                ),
            ],
        );
        let signatures = recover(&[function]);
        assert_eq!(
            fmt_signature("sub_08000000", &signatures["sub_08000000"]),
            "u32 sub_08000000(u32 r0)"
        );
    }

    #[test]
    fn wrapper_forwards_pointer_type_to_caller() {
        // g reads r0 as a pointer; f does nothing but call g with its
        // own r0, so f's r0 becomes a pointer by propagation.
        let g = fn_of(
            "sub_08000100",
            vec![blk(
                vec![def_inst(
                    did(1),
                    Op::Load {
                        addr: pval(0, 0),
                        size: Size::Word,
                        signed: false,
                        mode: crate::LoadMode::Ldr,
                    },
                )],
                Terminator::Return {
                    state: out_state(0, &[(0, Value::Id(did(1)))]),
                    switches_state: false,
                },
            )],
        );
        let f = fn_of(
            "sub_08000000",
            vec![
                blk(
                    Vec::new(),
                    Terminator::Call {
                        target: Value::Const(0x0800_0100),
                        cont: Some((BlockId(1), out_state(0, &[]))),
                        state: out_state(0, &[]),
                    },
                ),
                blk(
                    Vec::new(),
                    Terminator::Return {
                        state: out_state(0, &[]),
                        switches_state: false,
                    },
                ),
            ],
        );
        let signatures = recover(&[f, g]);
        assert_eq!(
            fmt_signature("sub_08000000", &signatures["sub_08000000"]),
            "void sub_08000000(u32* r0) /* guessed */"
        );
        assert_eq!(
            fmt_signature("sub_08000100", &signatures["sub_08000100"]),
            "u32 sub_08000100(u32* r0)"
        );
    }

    #[test]
    fn caller_written_registers_fallback_for_void_callee() {
        // h's body never reads anything; the sole caller writes r0 and
        // r1 before the call, so h takes two arguments by fallback.
        let h = fn_of(
            "sub_08000100",
            vec![blk(
                Vec::new(),
                Terminator::Return {
                    state: out_state(0, &[]),
                    switches_state: false,
                },
            )],
        );
        let k = fn_of(
            "sub_08000000",
            vec![blk(
                vec![
                    def_inst(
                        did(1),
                        Op::Add {
                            set_flags: false,
                            a: pval(0, 0),
                            b: Value::Const(1),
                        },
                    ),
                    def_inst(
                        did(2),
                        Op::Add {
                            set_flags: false,
                            a: pval(0, 1),
                            b: Value::Const(1),
                        },
                    ),
                ],
                Terminator::Call {
                    target: Value::Const(0x0800_0100),
                    cont: None,
                    state: out_state(0, &[(0, Value::Id(did(1))), (1, Value::Id(did(2)))]),
                },
            )],
        );
        let signatures = recover(&[k, h]);
        assert_eq!(
            fmt_signature("sub_08000100", &signatures["sub_08000100"]),
            "void sub_08000100(u32 r0, u32 r1) /* guessed */"
        );
        assert_eq!(
            fmt_signature("sub_08000000", &signatures["sub_08000000"]),
            "void sub_08000000(u32 r0, u32 r1)"
        );
    }
}
