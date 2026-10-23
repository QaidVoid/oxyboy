//! Translates lifted IR functions into C11 source.
//!
//! One C file per module: the ABI header followed by one function per
//! IR function, `void oxy_fn_<start>(struct oxy_state*, const struct
//! oxy_ops*)`. The translation is a direct transcription of the IR
//! evaluator's semantics (`ir::eval`): guards become `if` conditions
//! over the state's flags, slot destinations write the state struct
//! and their SSA local, every other SSA value becomes a C local, and
//! block edges copy the twenty state slots into the target block's
//! parameter locals and the state struct.
//!
//! What executes natively stops at the IR model's edges: a block
//! containing an [`Op::Intrinsic`] exits before running any effect,
//! calls to functions outside the module exit with the resume PC, and
//! backward branches check the host's instruction budget. The driver
//! resumes interpretation at `st->pc` in all those cases, so nothing
//! ever re-executes natively.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use decode::Condition;
use ir::{Block, BlockId, Function, Inst, LoadMode, Op, STATE_ARITY, Size, Terminator, Value};

use crate::abi::header;

pub mod abi;
pub mod jit;

pub use jit::Module;

/// Translated module: the C source and the translated entry
/// addresses (the symbol for entry `a` is `oxy_fn_{a:08x}`).
pub struct ModuleSource {
    /// The complete C11 translation unit.
    pub c_source: String,
    /// The translated entry addresses, ascending.
    pub entries: Vec<u32>,
}

/// The C symbol of the function translated from `start`.
pub fn symbol(start: u32) -> String {
    format!("oxy_fn_{start:08x}")
}

/// Translates `functions` (entry address, lifted IR) into one C
/// source. Calls between the functions compile to direct C calls;
/// every other control transfer exits to the host driver.
pub fn emit_module(functions: &[(u32, &Function)]) -> ModuleSource {
    let translated: BTreeSet<u32> = functions.iter().map(|(a, _)| *a).collect();
    let mut out = String::new();
    out.push_str(header());
    // Forward declarations: call order follows the input list, not
    // definition order.
    for (start, _) in functions {
        let _ = writeln!(
            out,
            "void {}(struct oxy_state* st, const struct oxy_ops* ops);",
            symbol(*start)
        );
    }
    for (start, function) in functions {
        emit_function(&mut out, *start, function, &translated);
    }
    let mut entries: Vec<u32> = functions.iter().map(|(a, _)| *a).collect();
    entries.sort_unstable();
    ModuleSource {
        c_source: out,
        entries,
    }
}

fn emit_function(out: &mut String, start: u32, function: &Function, translated: &BTreeSet<u32>) {
    let _ = writeln!(
        out,
        "\nvoid {}(struct oxy_state* st, const struct oxy_ops* ops) {{",
        symbol(start)
    );
    for id in used_ids(function) {
        let _ = writeln!(out, "    uint32_t v{id} = 0u;");
    }
    // The entry block's parameters are the incoming machine state.
    for (slot, id) in function.blocks[0].params.iter().take(20).enumerate() {
        let _ = writeln!(out, "    v{} = {};", id.0, slot_field(slot));
    }
    for (index, block) in function.blocks.iter().enumerate() {
        emit_block(out, index, block, function, translated);
    }
    out.push_str("}\n");
}

fn emit_block(
    out: &mut String,
    index: usize,
    block: &Block,
    function: &Function,
    translated: &BTreeSet<u32>,
) {
    let _ = writeln!(out, "b{}:;", index);
    // A block containing an intrinsic is opaque: the model does not
    // cover the instruction, so the interpreter runs the whole block
    // from its leader and nothing re-executes.
    if block
        .insts
        .iter()
        .any(|inst| matches!(inst.op, Op::Intrinsic { .. }))
    {
        let _ = writeln!(
            out,
            "    st->pc = 0x{:x}u; st->exited = 1u; return;",
            block.addr
        );
        return;
    }
    for inst in &block.insts {
        emit_inst(out, inst);
    }
    emit_term(out, index, block, function, translated);
}

fn emit_inst(out: &mut String, inst: &Inst) {
    let body = inst_body(inst);
    match inst.guard {
        None => out.push_str(&body),
        Some(cond) => {
            let _ = writeln!(out, "    if ({}) {{", cond_expr(cond));
            out.push_str(&body);
            out.push_str("    }\n");
        }
    }
}

/// One IR instruction as C statements, indented two levels, wrapped
/// in a block so per-instruction temporaries stay scoped.
fn inst_body(inst: &Inst) -> String {
    let mut body = String::new();
    let d: Vec<String> = inst.dests.iter().map(|id| format!("v{}", id.0)).collect();
    let _ = writeln!(body, "    st->icount++; st->cycles += 1;");
    match &inst.op {
        Op::Add { set_flags, a, b } => {
            arith(&mut body, &d, *set_flags, expr(a), expr(b), "0u", "oxy_add");
        }
        Op::Adc {
            set_flags,
            a,
            b,
            carry_in,
        } => {
            arith(
                &mut body,
                &d,
                *set_flags,
                expr(a),
                expr(b),
                &expr(carry_in),
                "oxy_add",
            );
        }
        Op::Sub { set_flags, a, b } => {
            arith(&mut body, &d, *set_flags, expr(a), expr(b), "0u", "oxy_sub");
        }
        Op::Sbc {
            set_flags,
            a,
            b,
            carry_in,
        } => {
            // The borrow input is !carry.
            let _ = writeln!(
                body,
                "    {{ uint32_t co, vo; uint32_t r = oxy_sub({}, {}, !({}), &co, &vo);",
                expr(a),
                expr(b),
                expr(carry_in)
            );
            bind_arith(&mut body, &d, *set_flags, "r", "co", "vo");
        }
        Op::Rsb { set_flags, a, b } => {
            arith(&mut body, &d, *set_flags, expr(b), expr(a), "0u", "oxy_sub");
        }
        Op::Rsc {
            set_flags,
            a,
            b,
            carry_in,
        } => {
            let _ = writeln!(
                body,
                "    {{ uint32_t co, vo; uint32_t r = oxy_sub({}, {}, !({}), &co, &vo);",
                expr(b),
                expr(a),
                expr(carry_in)
            );
            bind_arith(&mut body, &d, *set_flags, "r", "co", "vo");
        }
        Op::And {
            set_flags,
            a,
            b,
            carry_in,
        } => {
            logic(
                &mut body,
                &d,
                *set_flags,
                format!("({} & {})", expr(a), expr(b)),
                expr(carry_in),
            );
        }
        Op::Orr {
            set_flags,
            a,
            b,
            carry_in,
        } => {
            logic(
                &mut body,
                &d,
                *set_flags,
                format!("({} | {})", expr(a), expr(b)),
                expr(carry_in),
            );
        }
        Op::Eor {
            set_flags,
            a,
            b,
            carry_in,
        } => {
            logic(
                &mut body,
                &d,
                *set_flags,
                format!("({} ^ {})", expr(a), expr(b)),
                expr(carry_in),
            );
        }
        Op::Bic {
            set_flags,
            a,
            b,
            carry_in,
        } => {
            logic(
                &mut body,
                &d,
                *set_flags,
                format!("({} & ~{})", expr(a), expr(b)),
                expr(carry_in),
            );
        }
        Op::Mov {
            set_flags,
            a,
            carry_in,
        } => {
            logic(&mut body, &d, *set_flags, expr(a), expr(carry_in));
        }
        Op::Mvn {
            set_flags,
            a,
            carry_in,
        } => {
            logic(
                &mut body,
                &d,
                *set_flags,
                format!("(~{})", expr(a)),
                expr(carry_in),
            );
        }
        Op::Mul { set_flags, a, b } => {
            let _ = writeln!(body, "    {{ {} = ({} * {});", d[0], expr(a), expr(b));
            if *set_flags {
                // C and V stay unchanged on ARMv4; contract [v, n, z].
                let _ = writeln!(
                    body,
                    "    {} = ({} >> 31) & 1u; {} = (uint32_t)({} == 0u);",
                    d[1], d[0], d[2], d[0]
                );
                let _ = writeln!(body, "    st->n = {}; st->z = {};", d[1], d[2]);
            }
            body.push_str("    }\n");
        }
        Op::MultiplyLong {
            unsigned,
            accumulate,
            set_flags,
            a,
            b,
            acc_lo,
            acc_hi,
        } => {
            let mul = if *unsigned {
                format!("((uint64_t){} * (uint64_t){})", expr(a), expr(b))
            } else {
                format!(
                    "((uint64_t)((int64_t)(int32_t){} * (int64_t)(int32_t){}))",
                    expr(a),
                    expr(b)
                )
            };
            let prod = if *accumulate {
                format!(
                    "((((uint64_t){} << 32) | {}) + {})",
                    expr(acc_hi),
                    expr(acc_lo),
                    mul
                )
            } else {
                mul
            };
            let _ = writeln!(body, "    {{ uint64_t p = {};", prod);
            let _ = writeln!(
                body,
                "    {} = (uint32_t)p; {} = (uint32_t)(p >> 32);",
                d[0], d[1]
            );
            if *set_flags {
                let _ = writeln!(
                    body,
                    "    {} = (uint32_t)(p >> 63); {} = (uint32_t)(p == 0u);",
                    d[2], d[3]
                );
                let _ = writeln!(body, "    st->n = {}; st->z = {};", d[2], d[3]);
            }
            body.push_str("    }\n");
        }
        Op::Cmp { a, b } => {
            let _ = writeln!(
                body,
                "    {{ uint32_t co, vo; uint32_t r = oxy_sub({}, {}, 0u, &co, &vo);",
                expr(a),
                expr(b)
            );
            cmp_flags(&mut body, &d, "r", "co", "vo");
        }
        Op::Cmn { a, b } => {
            let _ = writeln!(
                body,
                "    {{ uint32_t co, vo; uint32_t r = oxy_add({}, {}, 0u, &co, &vo);",
                expr(a),
                expr(b)
            );
            cmp_flags(&mut body, &d, "r", "co", "vo");
        }
        Op::Tst { a, b, carry_in } => {
            tst_body(
                &mut body,
                &d,
                format!("({} & {})", expr(a), expr(b)),
                expr(carry_in),
            );
        }
        Op::Teq { a, b, carry_in } => {
            tst_body(
                &mut body,
                &d,
                format!("({} ^ {})", expr(a), expr(b)),
                expr(carry_in),
            );
        }
        Op::Lsl {
            set_flags: _,
            a,
            amount,
            carry_in,
        } => {
            shift(
                &mut body,
                &d,
                expr(a),
                expr(amount),
                expr(carry_in),
                "oxy_shl",
            );
        }
        Op::Lsr {
            set_flags: _,
            a,
            amount,
            carry_in,
        } => {
            shift(
                &mut body,
                &d,
                expr(a),
                expr(amount),
                expr(carry_in),
                "oxy_shr",
            );
        }
        Op::Asr {
            set_flags: _,
            a,
            amount,
            carry_in,
        } => {
            shift(
                &mut body,
                &d,
                expr(a),
                expr(amount),
                expr(carry_in),
                "oxy_sar",
            );
        }
        Op::Ror {
            set_flags: _,
            a,
            amount,
            carry_in,
        } => {
            shift(
                &mut body,
                &d,
                expr(a),
                expr(amount),
                expr(carry_in),
                "oxy_ror",
            );
        }
        Op::Rrx {
            set_flags: _,
            a,
            carry_in,
        } => {
            // The evaluator binds d[0] and (when present) d[1] as
            // plain values without moving a slot representative.
            let _ = writeln!(
                body,
                "    {{ {} = ({} >> 1) | (({}) << 31);",
                d[0],
                expr(a),
                expr(carry_in)
            );
            if d.len() >= 2 {
                let _ = writeln!(body, "    {} = ({} & 1u);", d[1], expr(a));
            }
            body.push_str("    }\n");
        }
        Op::Load {
            addr,
            size,
            signed,
            mode,
        } => {
            let _ = writeln!(
                body,
                "    {} = ops->load(ops->ctx, st, {}, {}u, {}u, {}u);",
                d[0],
                expr(addr),
                size_const(size),
                u32::from(*signed),
                mode_const(mode)
            );
        }
        Op::Store { addr, value, size } => {
            let _ = writeln!(
                body,
                "    ops->store(ops->ctx, st, {}, {}, {}u);",
                expr(addr),
                expr(value),
                size_const(size)
            );
        }
        // Opaque blocks are filtered before instructions are emitted.
        Op::Intrinsic { .. } => {}
    }
    body
}

fn arith(
    body: &mut String,
    d: &[String],
    set_flags: bool,
    a: String,
    b: String,
    cin: &str,
    helper: &str,
) {
    let _ = writeln!(
        body,
        "    {{ uint32_t co, vo; uint32_t r = {helper}({a}, {b}, {cin}, &co, &vo);"
    );
    bind_arith(body, d, set_flags, "r", "co", "vo");
}

/// Binds an arithmetic op's dests `[v, n, z, c, v]`; dead-flag
/// elimination may truncate the flag tail (matching the evaluator's
/// length dispatch).
fn bind_arith(body: &mut String, d: &[String], set_flags: bool, r: &str, co: &str, vo: &str) {
    let _ = writeln!(body, "    {} = {r};", d[0]);
    if set_flags {
        let _ = writeln!(
            body,
            "    {} = ({r} >> 31) & 1u; {} = (uint32_t)({r} == 0u); st->n = {}; st->z = {};",
            d[1], d[2], d[1], d[2]
        );
        if d.len() >= 4 {
            let _ = writeln!(body, "    {} = {co}; st->c = {};", d[3], d[3]);
        }
        if d.len() >= 5 {
            let _ = writeln!(body, "    {} = {vo}; st->v = {};", d[4], d[4]);
        }
    }
    body.push_str("    }\n");
}

fn cmp_flags(body: &mut String, d: &[String], r: &str, co: &str, vo: &str) {
    let _ = writeln!(
        body,
        "    {} = ({r} >> 31) & 1u; {} = (uint32_t)({r} == 0u); {} = {co}; {} = {vo};",
        d[0], d[1], d[2], d[3]
    );
    let _ = writeln!(
        body,
        "    st->n = {}; st->z = {}; st->c = {}; st->v = {};",
        d[0], d[1], d[2], d[3]
    );
    body.push_str("    }\n");
}

fn tst_body(body: &mut String, d: &[String], value: String, carry_in: String) {
    let _ = writeln!(body, "    {{ {} = {value};", d[0]);
    let _ = writeln!(
        body,
        "    {} = ({} >> 31) & 1u; {} = (uint32_t)({} == 0u); {} = ({}); st->n = {}; st->z = {}; st->c = {};",
        d[1], d[0], d[2], d[0], d[3], carry_in, d[1], d[2], d[3]
    );
    body.push_str("    }\n");
}

/// Logic and move ops: `[v, n, z, c]` with the shifter carry passed
/// through, exactly as the evaluator's `bind_logic`.
fn logic(body: &mut String, d: &[String], set_flags: bool, value: String, carry_in: String) {
    let _ = writeln!(body, "    {{ {} = {value};", d[0]);
    if set_flags {
        let _ = writeln!(
            body,
            "    {} = ({} >> 31) & 1u; {} = (uint32_t)({} == 0u); {} = ({}); st->n = {}; st->z = {}; st->c = {};",
            d[1], d[0], d[2], d[0], d[3], carry_in, d[1], d[2], d[3]
        );
    }
    body.push_str("    }\n");
}

fn shift(body: &mut String, d: &[String], a: String, amount: String, cin: String, helper: &str) {
    let _ = writeln!(
        body,
        "    {{ uint32_t co; {} = {helper}({}, ({} & 0xFFu), ({}), &co);",
        d[0],
        a,
        amount,
        cin,
        helper = helper
    );
    match d.len() {
        4 => {
            let _ = writeln!(
                body,
                "    {} = ({} >> 31) & 1u; {} = (uint32_t)({} == 0u); {} = co; st->n = {}; st->z = {}; st->c = {};",
                d[1], d[0], d[2], d[0], d[3], d[1], d[2], d[3]
            );
        }
        3 => {
            let _ = writeln!(
                body,
                "    {} = ({} >> 31) & 1u; {} = (uint32_t)({} == 0u); st->n = {}; st->z = {};",
                d[1], d[0], d[2], d[0], d[1], d[2]
            );
        }
        2 => {
            let _ = writeln!(body, "    {} = co;", d[1]);
        }
        _ => {}
    }
    body.push_str("    }\n");
}

fn emit_term(
    out: &mut String,
    index: usize,
    block: &Block,
    function: &Function,
    translated: &BTreeSet<u32>,
) {
    match &block.term {
        Terminator::Branch { target, args } => {
            edge(out, args, &function.blocks[target.0 as usize]);
            budget_check(out, index, *target, function);
            let _ = writeln!(out, "    goto b{};", target.0);
        }
        Terminator::BranchCond {
            cond,
            then_target,
            then_args,
            else_target,
            else_args,
        } => {
            let _ = writeln!(out, "    if ({}) {{", cond_expr(*cond));
            edge(out, then_args, &function.blocks[then_target.0 as usize]);
            budget_check(out, index, *then_target, function);
            let _ = writeln!(out, "    goto b{};", then_target.0);
            out.push_str("    } else {\n");
            edge(out, else_args, &function.blocks[else_target.0 as usize]);
            budget_check(out, index, *else_target, function);
            let _ = writeln!(out, "    goto b{};", else_target.0);
            out.push_str("    }\n");
        }
        Terminator::Call {
            target,
            cont,
            state,
        } => match target {
            Value::Const(t) if translated.contains(t) => {
                // The callee (or, on a budget exit, the resumed
                // interpreter) observes the caller's pre-call state.
                flush_state(out, state);
                // Recursion and long call chains need the same
                // termination guarantee as loops.
                let _ = writeln!(
                    out,
                    "    if (st->icount >= ops->budget) {{ st->pc = 0x{t:x}u; st->exited = 1u; return; }}"
                );
                let _ = writeln!(out, "    {}(st, ops);", symbol(*t));
                match cont {
                    Some((b, _)) => {
                        // The callee clobbers the registers; the state
                        // as the callee left it is the continuation
                        // state, so the continuation's parameters
                        // bind from the struct.
                        let successor = &function.blocks[b.0 as usize];
                        for (slot, param) in successor.params.iter().take(STATE_ARITY).enumerate() {
                            let _ = writeln!(out, "    v{} = {};", param.0, slot_field(slot));
                        }
                        let _ = writeln!(out, "    goto b{};", b.0);
                    }
                    None => out.push_str("    return;\n"),
                }
            }
            Value::Const(t) => {
                flush_state(out, state);
                let _ = writeln!(out, "    st->pc = 0x{t:x}u; st->exited = 1u; return;");
            }
            Value::Id(id) => {
                flush_state(out, state);
                let _ = writeln!(out, "    st->pc = v{}; st->exited = 1u; return;", id.0);
            }
        },
        Terminator::Return { state } => {
            flush_state(out, state);
            // Control went back to the caller, so this is not an
            // exit; the resume PC is the returned r15.
            out.push_str("    st->exited = 0u;\n");
            out.push_str("    return;\n");
        }
        Terminator::IndirectExit { target, state } => {
            flush_state(out, state);
            let _ = writeln!(
                out,
                "    st->pc = {}; st->exited = 1u; return;",
                expr(target)
            );
        }
        Terminator::Unreachable => {
            out.push_str("    return; /* unreachable in the lifted region */\n");
        }
    }
}

/// The budget check for a backward edge: without it a native loop
/// could spin forever. The check charges the edge itself (the branch
/// instruction the interpreter would count) and exits with the target
/// block's leader address, a clean resume point nothing re-executes.
fn budget_check(out: &mut String, current: usize, target: BlockId, function: &Function) {
    if target.0 as usize <= current {
        let addr = function.blocks[target.0 as usize].addr;
        let _ = writeln!(
            out,
            "    if (st->icount++ >= ops->budget) {{ st->pc = 0x{addr:x}u; st->exited = 1u; return; }}"
        );
    }
}

/// Copies one edge's twenty state values into the target block's
/// parameter locals and the state struct.
fn edge(out: &mut String, args: &[Value], target: &Block) {
    for (slot, value) in args.iter().take(20).enumerate() {
        let Some(param) = target.params.get(slot) else {
            continue;
        };
        match value {
            Value::Const(c) => {
                let _ = writeln!(
                    out,
                    "    v{} = {c}u; {} = v{};",
                    param.0,
                    slot_field(slot),
                    param.0
                );
            }
            Value::Id(_) => {
                let e = expr(value);
                if e == slot_field(slot) {
                    let _ = writeln!(out, "    v{} = {e};", param.0);
                } else {
                    let _ = writeln!(
                        out,
                        "    v{} = {e}; {} = v{};",
                        param.0,
                        slot_field(slot),
                        param.0
                    );
                }
            }
        }
    }
}

fn cond_expr(cond: Condition) -> String {
    use Condition as C;
    match cond {
        C::Eq => "st->z".into(),
        C::Ne => "!st->z".into(),
        C::Cs => "st->c".into(),
        C::Cc => "!st->c".into(),
        C::Mi => "st->n".into(),
        C::Pl => "!st->n".into(),
        C::Vs => "st->v".into(),
        C::Vc => "!st->v".into(),
        C::Hi => "(st->c && !st->z)".into(),
        C::Ls => "(!st->c || st->z)".into(),
        C::Ge => "(st->n == st->v)".into(),
        C::Lt => "(st->n != st->v)".into(),
        C::Gt => "(!st->z && st->n == st->v)".into(),
        C::Le => "(st->z || st->n != st->v)".into(),
        C::Al => "1".into(),
    }
}

fn expr(value: &Value) -> String {
    match value {
        Value::Const(c) => format!("{c}u"),
        Value::Id(id) => format!("v{}", id.0),
    }
}

/// Copies the outgoing machine state into the state struct. Every
/// function-exiting terminator carries this list, so a native exit or
/// a direct callee observes the block's effects.
fn flush_state(out: &mut String, state: &[Value]) {
    for (slot, value) in state.iter().take(20).enumerate() {
        match value {
            Value::Const(c) => {
                let _ = writeln!(out, "    {} = {c}u;", slot_field(slot));
            }
            Value::Id(_) => {
                let e = expr(value);
                if e != slot_field(slot) {
                    let _ = writeln!(out, "    {} = {e};", slot_field(slot));
                }
            }
        }
    }
}

/// The state-struct expression of machine slot `slot`.
fn slot_field(slot: usize) -> String {
    if slot < 16 {
        format!("st->r[{slot}]")
    } else {
        format!("st->{}", ["n", "z", "c", "v"][slot - 16])
    }
}

fn size_const(size: &Size) -> u32 {
    match size {
        Size::Byte => crate::abi::SIZE_BYTE,
        Size::Half => crate::abi::SIZE_HALF,
        Size::Word => crate::abi::SIZE_WORD,
    }
}

fn mode_const(mode: &LoadMode) -> u32 {
    match mode {
        LoadMode::Ldr => crate::abi::MODE_LDR,
        LoadMode::Swap => crate::abi::MODE_SWAP,
        LoadMode::Block => crate::abi::MODE_BLOCK,
    }
}

fn used_ids(function: &Function) -> BTreeSet<u32> {
    fn push(used: &mut BTreeSet<u32>, v: &Value) {
        if let Value::Id(id) = v {
            used.insert(id.0);
        }
    }
    let mut used = BTreeSet::new();
    for block in &function.blocks {
        // Block params are written by the binding or edge code even
        // when the block body never reads them.
        for id in &block.params {
            used.insert(id.0);
        }
        for inst in &block.insts {
            for id in &inst.dests {
                used.insert(id.0);
            }
            for v in op_values(inst) {
                push(&mut used, v);
            }
        }
        match &block.term {
            Terminator::Branch { args, .. } | Terminator::Return { state: args } => {
                args.iter().for_each(|v| push(&mut used, v))
            }
            Terminator::BranchCond {
                then_args,
                else_args,
                ..
            } => then_args
                .iter()
                .chain(else_args.iter())
                .for_each(|v| push(&mut used, v)),
            Terminator::Call {
                target,
                cont,
                state,
            } => {
                push(&mut used, target);
                if let Some((_, args)) = cont {
                    args.iter().for_each(|v| push(&mut used, v));
                }
                state.iter().for_each(|v| push(&mut used, v));
            }
            Terminator::IndirectExit { target, state } => {
                push(&mut used, target);
                state.iter().for_each(|v| push(&mut used, v));
            }
            Terminator::Unreachable => {}
        }
    }
    used
}

fn op_values(inst: &Inst) -> Vec<&Value> {
    match &inst.op {
        Op::Add { set_flags: _, a, b }
        | Op::Sub { set_flags: _, a, b }
        | Op::Rsb { set_flags: _, a, b }
        | Op::Mul { set_flags: _, a, b }
        | Op::Cmp { a, b }
        | Op::Cmn { a, b } => vec![a, b],
        Op::Adc {
            set_flags: _,
            a,
            b,
            carry_in,
        }
        | Op::Sbc {
            set_flags: _,
            a,
            b,
            carry_in,
        }
        | Op::Rsc {
            set_flags: _,
            a,
            b,
            carry_in,
        }
        | Op::And {
            set_flags: _,
            a,
            b,
            carry_in,
        }
        | Op::Orr {
            set_flags: _,
            a,
            b,
            carry_in,
        }
        | Op::Eor {
            set_flags: _,
            a,
            b,
            carry_in,
        }
        | Op::Bic {
            set_flags: _,
            a,
            b,
            carry_in,
        }
        | Op::Tst { a, b, carry_in }
        | Op::Teq { a, b, carry_in }
        | Op::Lsl {
            set_flags: _,
            a,
            amount: b,
            carry_in,
        }
        | Op::Lsr {
            set_flags: _,
            a,
            amount: b,
            carry_in,
        }
        | Op::Asr {
            set_flags: _,
            a,
            amount: b,
            carry_in,
        }
        | Op::Ror {
            set_flags: _,
            a,
            amount: b,
            carry_in,
        } => vec![a, b, carry_in],
        Op::Mov {
            set_flags: _,
            a,
            carry_in,
        }
        | Op::Mvn {
            set_flags: _,
            a,
            carry_in,
        }
        | Op::Rrx {
            set_flags: _,
            a,
            carry_in,
        } => {
            vec![a, carry_in]
        }
        Op::MultiplyLong {
            a,
            b,
            acc_lo,
            acc_hi,
            ..
        } => vec![a, b, acc_lo, acc_hi],
        Op::Load { addr, .. } => vec![addr],
        Op::Store { addr, value, .. } => vec![addr, value],
        Op::Intrinsic { args, .. } => args.iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ir::{Block, ValueId};

    fn reg(index: u32) -> Value {
        Value::Id(ValueId(index))
    }

    /// `f { r0 = r0 + 4; if ne mem[r0] = r1; ret }` over the 20
    /// incoming state slots.
    fn sample() -> Function {
        Function {
            name: "sub_08000100".to_string(),
            blocks: vec![Block {
                addr: 0x0800_0100,
                params: (0..20).map(ValueId).collect(),
                insts: vec![
                    Inst {
                        guard: None,
                        dests: vec![ValueId(20)],
                        op: Op::Add {
                            set_flags: false,
                            a: reg(0),
                            b: Value::Const(4),
                        },
                    },
                    Inst {
                        guard: Some(Condition::Ne),
                        dests: vec![],
                        op: Op::Store {
                            addr: reg(20),
                            value: reg(1),
                            size: Size::Word,
                        },
                    },
                ],
                term: Terminator::Return {
                    state: (0..20)
                        .map(|slot| if slot == 0 { reg(20) } else { reg(slot) })
                        .collect(),
                },
            }],
        }
    }

    #[test]
    fn emits_symbol_locals_and_body() {
        let f = sample();
        let source = emit_module(&[(0x0800_0100, &f)]).c_source;
        assert!(
            source.contains("void oxy_fn_08000100(struct oxy_state* st"),
            "{source}"
        );
        assert!(source.contains("uint32_t v20 = 0u;"), "{source}");
        assert!(source.contains("v0 = st->r[0];"), "entry binding: {source}");
        assert!(
            source.contains("oxy_add(v0, 4u, 0u, &co, &vo);"),
            "{source}"
        );
        assert!(source.contains("if (!st->z) {"), "ne guard: {source}");
        assert!(
            source.contains("ops->store(ops->ctx, st, v20, v1, 2u);"),
            "{source}"
        );
        assert!(
            source.contains("st->r[0] = v20;"),
            "return binds r0: {source}"
        );
    }

    #[test]
    fn intrinsic_blocks_exit_at_the_leader() {
        let mut f = sample();
        f.blocks[0].insts.insert(
            0,
            Inst {
                guard: None,
                dests: vec![],
                op: Op::Intrinsic {
                    name: "swi",
                    args: vec![],
                },
            },
        );
        let source = emit_module(&[(0x0800_0100, &f)]).c_source;
        let body = source
            .split_once("void oxy_fn_08000100")
            .expect("function emitted")
            .1;
        assert!(
            body.contains("b0:;") && body.contains("st->pc = 0x8000100u; st->exited = 1u; return;"),
            "opaque block: {body}"
        );
        assert!(!body.contains("oxy_add"), "no effects emitted: {body}");
    }

    #[test]
    fn untranslated_calls_exit_and_translated_ones_call() {
        let mut caller = sample();
        caller.blocks[0].term = Terminator::Call {
            target: Value::Const(0x0800_0200),
            cont: None,
            state: (0..20).map(reg).collect(),
        };
        let source = emit_module(&[(0x0800_0100, &caller)]).c_source;
        assert!(
            source.contains("st->pc = 0x8000200u; st->exited = 1u; return;"),
            "{source}"
        );

        let callee = sample();
        let source = emit_module(&[(0x0800_0100, &caller), (0x0800_0200, &callee)]).c_source;
        assert!(source.contains("oxy_fn_08000200(st, ops);"), "{source}");
    }

    #[test]
    fn translated_call_binds_the_continuation_from_the_state() {
        // The caller calls a translated callee and continues in b1;
        // the continuation's parameters must bind from the post-call
        // state, not stay at their zero-initialized locals.
        let mut caller = sample();
        caller.blocks.push(Block {
            addr: 0x0800_0104,
            params: (40..60).map(ValueId).collect(),
            insts: vec![],
            term: Terminator::Return {
                state: (0..20)
                    .map(|slot| Value::Id(ValueId(40 + slot as u32)))
                    .collect(),
            },
        });
        caller.blocks[0].term = Terminator::Call {
            target: Value::Const(0x0800_0200),
            cont: Some((BlockId(1), (0..20).map(reg).collect())),
            state: (0..20).map(reg).collect(),
        };
        let callee = sample();
        let source = emit_module(&[(0x0800_0100, &caller), (0x0800_0200, &callee)]).c_source;
        assert!(source.contains("oxy_fn_08000200(st, ops);"), "{source}");
        assert!(
            source.contains("v40 = st->r[0];"),
            "continuation binding: {source}"
        );
        assert!(
            source.contains("v59 = st->v;"),
            "continuation flags: {source}"
        );
    }

    #[test]
    fn backward_edges_check_the_budget() {
        let mut f = sample();
        // b0 branches to itself: a one-block loop.
        f.blocks[0].term = Terminator::Branch {
            target: BlockId(0),
            args: (0..20).map(reg).collect(),
        };
        let source = emit_module(&[(0x0800_0100, &f)]).c_source;
        assert!(
            source.contains(
                "if (st->icount++ >= ops->budget) { st->pc = 0x8000100u; st->exited = 1u; return; }"
            ),
            "{source}"
        );
        assert!(source.contains("goto b0;"), "{source}");
    }
}
