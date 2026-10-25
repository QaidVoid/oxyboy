//! C-like emission over lifted functions.
//!
//! The emitter walks the CFG in control-flow order and structures the
//! patterns it recognizes: if/else diamonds, short-circuit condition
//! chains, early returns, and simple single-latch `while`/`do-while`
//! loops from the CFG loop forest. Everything else falls back to
//! labeled `goto`, so every function, however irreducible, emits
//! complete output where each reachable block appears exactly once.
//!
//! Rendering runs twice: pass one records the `goto` targets, pass
//! two emits labels only where control actually jumps, keeping the
//! structured output free of stray labels.

use std::collections::{BTreeMap, BTreeSet};

use analysis::config::{Config, sanitize_identifier};
use decode::{Condition, bios, mmio};

use crate::cfg::{Cfg, Loop};
use crate::{BlockId, Flag, Function, Inst, Op, SLOTS, Terminator, Value, ValueId};

/// How a recognized loop closes.
#[derive(Clone)]
enum Close {
    /// `while (cond) { ... }`: the latch closes with `}`.
    While,
    /// `do { ... } while (cond);`: the latch closes with its own
    /// trailing condition.
    DoWhile { cond: Condition, invert: bool },
}

/// A recognized simple loop.
#[derive(Clone)]
struct Shape {
    header: BlockId,
    body_enter: BlockId,
    exit: BlockId,
    latch: BlockId,
    close: Close,
}

/// What a conditional emission did with control.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    /// The walk closed (goto or structured return).
    Closed,
    /// Control falls through into the block.
    Fallthrough,
}

/// User-provided names for C output, keyed by function entry address.
/// Built from the analysis config: editing the config and
/// re-running `lift --c` renames functions and called subs.
#[derive(Debug, Default, Clone)]
pub struct Annotations {
    /// Entry address to C identifier.
    pub names: BTreeMap<u32, String>,
}

impl Annotations {
    /// Collects every named function spec from the config, sanitized
    /// into a valid C identifier.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let mut names = BTreeMap::new();
        for spec in &config.functions {
            if let Some(name) = &spec.name {
                let clean = sanitize_identifier(name);
                if !clean.is_empty() {
                    names.insert(spec.addr, clean);
                }
            }
        }
        Annotations { names }
    }
}

/// Extern declarations the unit form relies on: memory accessors,
/// flag helpers, and the intrinsics. Every referenced name must
/// appear here or `gcc -fsyntax-only` fails on an implicit
/// declaration.
const UNIT_PREAMBLE: &str = r#"typedef unsigned int u32;
typedef signed int i32;
typedef unsigned long long u64;
extern u32 load_u8(u32);
extern u32 load_u16(u32);
extern u32 load_u32(u32);
extern u32 load_i8(u32);
extern u32 load_i16(u32);
extern u32 load_i32(u32);
extern void store_u8(u32, u32);
extern void store_u16(u32, u32);
extern void store_u32(u32, u32);
extern void flags_cmp(u32, u32);
extern void flags_cmn(u32, u32);
extern void flags_tst(u32, u32);
extern void flags_teq(u32, u32);
extern void __discard(u32);
extern void __mov(u32);
extern void __mvn(u32);
extern u32 __asr(u32, u32);
extern u32 __ror(u32, u32);
extern u32 __rrx(u32);
extern void __indirect_call(u32);
extern void unhandled(void);
extern u32 mrs_cpsr(void);
extern u32 mrs_spsr(void);
extern void msr_cpsr(u32, u32);
extern void msr_spsr(u32, u32);
extern void swi(u32);
"#;

/// The structured emitter state.
struct Emitter<'a> {
    function: &'a Function,
    cfg: &'a Cfg,
    shapes: Vec<Shape>,
    /// Blocks reached by an emitted `goto`; they need labels. The
    /// sentinel `BlockId(u32::MAX)` is the indirect-exit epilogue.
    goto_targets: BTreeSet<BlockId>,
    consumed: BTreeSet<BlockId>,
    loop_stack: Vec<(BlockId, BlockId, Close)>,
    /// Compilable unit form: assignments instead of declarations,
    /// threaded call arguments, `load_*`/`store_*` helper names.
    unit: bool,
    /// Name source for called subs, keyed by entry address.
    callees: &'a BTreeMap<u32, String>,
    out: String,
    indent: usize,
}

/// Emits `function` as structured C-like text.
pub fn emit_function(function: &Function) -> String {
    let cfg = Cfg::build(function);
    let shapes = recognize_loops(&cfg, function);
    let callees = BTreeMap::new();
    let mut emitter = Emitter {
        function,
        cfg: &cfg,
        shapes,
        goto_targets: BTreeSet::new(),
        consumed: BTreeSet::new(),
        loop_stack: Vec::new(),
        unit: false,
        callees: &callees,
        out: String::new(),
        indent: 1,
    };
    // Pass 1 collects goto targets; pass 2 renders with labels.
    emitter.run();
    emitter.out.clear();
    emitter.consumed.clear();
    emitter.loop_stack.clear();
    emitter.indent = 1;
    let name = if function.name.is_empty() {
        "sub_0".to_string()
    } else {
        function.name.clone()
    };
    emitter.out.push_str(&format!("void {name}(void) {{\n"));
    emitter.run();
    if emitter.goto_targets.contains(&BlockId(u32::MAX)) {
        emitter.line("b_indirect:;");
        emitter.line("return;");
    }
    emitter.out.push_str("}\n");
    emitter.out
}

/// Emits the functions as one compilable C translation unit:
/// the same structured walk, wrapped in a preamble,
/// prototypes, and SSA-local declarations so `gcc -fsyntax-only`
/// accepts the output. `addrs[i]` is the entry address of
/// `functions[i]`; names come from each function's `name` field, so
/// apply config names before calling. Recovered signatures render as
/// comments above each definition.
#[must_use]
pub fn emit_unit(addrs: &[u32], functions: &[Function], annotations: &Annotations) -> String {
    let signatures = crate::sig::recover(functions);
    // Every callee the unit can name: internal functions by entry
    // address, then configured externals.
    let mut callees: BTreeMap<u32, String> = annotations.names.clone();
    for (addr, function) in addrs.iter().zip(functions) {
        if !function.name.is_empty() {
            callees.insert(*addr, function.name.clone());
        }
    }
    let mut out = String::from(UNIT_PREAMBLE);
    let mut rendered: BTreeSet<u32> = BTreeSet::new();
    rendered.extend(addrs.iter().copied());
    // Prototypes: internal functions first, then referenced external
    // calls, so forward references always resolve.
    for function in functions {
        let name = display_name(function);
        out.push_str(&format!("u32 {name}(u32, u32, u32, u32);\n"));
    }
    for function in functions {
        for addr in call_targets(function) {
            if !rendered.contains(&addr) {
                rendered.insert(addr);
                let name = callees
                    .get(&addr)
                    .cloned()
                    .unwrap_or_else(|| format!("sub_{addr:08x}"));
                out.push_str(&format!("u32 {name}(u32, u32, u32, u32);\n"));
            }
        }
    }
    for function in functions {
        let name = display_name(function);
        if let Some(sig) = signatures.get(&function.name) {
            let text = crate::sig::fmt_signature(&name, sig)
                .replace("/*", "")
                .replace("*/", "");
            out.push_str(&format!("/* sig: {text} */\n"));
        }
        let cfg = Cfg::build(function);
        let shapes = recognize_loops(&cfg, function);
        let mut emitter = Emitter {
            function,
            cfg: &cfg,
            shapes,
            goto_targets: BTreeSet::new(),
            consumed: BTreeSet::new(),
            loop_stack: Vec::new(),
            unit: true,
            callees: &callees,
            out: String::new(),
            indent: 1,
        };
        emitter.run();
        emitter.out.clear();
        emitter.consumed.clear();
        emitter.loop_stack.clear();
        emitter.indent = 1;
        emitter.run();
        out.push_str(&format!("u32 {name}({}) {{\n", emitter.unit_params()));
        out.push_str(&emitter.unit_declarations());
        out.push_str(&emitter.out);
        if emitter.goto_targets.contains(&BlockId(u32::MAX)) {
            out.push_str("    b_indirect:;\n    return 0;\n");
        }
        out.push_str("}\n\n");
    }
    out
}

/// The function's display name, with a fallback for unnamed ones.
fn display_name(function: &Function) -> String {
    if function.name.is_empty() {
        "sub_0".to_string()
    } else {
        function.name.clone()
    }
}

/// Every direct-call target address in the function.
fn call_targets(function: &Function) -> Vec<u32> {
    let mut targets = Vec::new();
    for block in &function.blocks {
        if let Terminator::Call { target, .. } = &block.term
            && let Value::Const(addr) = target
        {
            targets.push(*addr);
        }
    }
    targets
}

/// Finds the simple loops worth structuring: exactly one back edge,
/// and the exit leaves from the header (`while`) or the latch
/// (`do-while`). Everything else stays goto-form.
fn recognize_loops(cfg: &Cfg, function: &Function) -> Vec<Shape> {
    let mut shapes = Vec::new();
    for loop_ in cfg.loops() {
        let header = loop_.header;
        let Some(latch) = single_back_edge(cfg, &loop_) else {
            continue;
        };
        let in_body = |b: BlockId| loop_.body.contains(&b);
        let header_term = &function.blocks[header.0 as usize].term;
        // while: the header condition picks body-in vs body-out and
        // the latch closes unconditionally.
        if let Terminator::BranchCond {
            then_target,
            else_target,
            ..
        } = header_term
        {
            let (body_enter, exit) = if in_body(*then_target) && !in_body(*else_target) {
                (*then_target, *else_target)
            } else if in_body(*else_target) && !in_body(*then_target) {
                (*else_target, *then_target)
            } else {
                continue;
            };
            let latch_closes = matches!(
                &function.blocks[latch.0 as usize].term,
                Terminator::Branch { target, .. } if *target == header
            );
            if latch_closes {
                shapes.push(Shape {
                    header,
                    body_enter,
                    exit,
                    latch,
                    close: Close::While,
                });
                continue;
            }
        }
        // do-while: unconditional entry, conditional latch whose other
        // arm leaves the loop.
        let entry_unconditional = matches!(
            header_term,
            Terminator::Branch { target, .. } if in_body(*target) && *target != header
        );
        if !entry_unconditional {
            continue;
        }
        let Terminator::Branch {
            target: body_enter, ..
        } = header_term
        else {
            continue;
        };
        let Terminator::BranchCond {
            cond,
            then_target,
            else_target,
            ..
        } = &function.blocks[latch.0 as usize].term
        else {
            continue;
        };
        let (close_cond, invert, exit) = if *then_target == header {
            (*cond, false, *else_target)
        } else if *else_target == header {
            (*cond, true, *then_target)
        } else {
            continue;
        };
        if in_body(exit) {
            continue;
        }
        shapes.push(Shape {
            header,
            body_enter: *body_enter,
            exit,
            latch,
            close: Close::DoWhile {
                cond: close_cond,
                invert,
            },
        });
    }
    shapes
}

/// The single back-edge source into a loop's header, if unique.
fn single_back_edge(cfg: &Cfg, loop_: &Loop) -> Option<BlockId> {
    let idoms = cfg.idoms();
    let dominates = |dominator: BlockId, mut walker: BlockId| {
        loop {
            if walker == dominator {
                return true;
            }
            match idoms[walker.0 as usize] {
                Some(up) => walker = up,
                None => return false,
            }
        }
    };
    let mut found = None;
    for pred in &cfg.preds[loop_.header.0 as usize] {
        if cfg.is_reachable(*pred) && dominates(loop_.header, *pred) {
            if found.is_some() {
                return None;
            }
            found = Some(*pred);
        }
    }
    found
}

/// The boolean negation of an ARM condition, when expressible.
fn negate(cond: Condition) -> Option<Condition> {
    Some(match cond {
        Condition::Eq => Condition::Ne,
        Condition::Ne => Condition::Eq,
        Condition::Cs => Condition::Cc,
        Condition::Cc => Condition::Cs,
        Condition::Mi => Condition::Pl,
        Condition::Pl => Condition::Mi,
        Condition::Vs => Condition::Vc,
        Condition::Vc => Condition::Vs,
        Condition::Hi => Condition::Ls,
        Condition::Ls => Condition::Hi,
        Condition::Ge => Condition::Lt,
        Condition::Lt => Condition::Ge,
        Condition::Gt => Condition::Le,
        Condition::Le => Condition::Gt,
        Condition::Al => return None,
    })
}

impl Emitter<'_> {
    fn run(&mut self) {
        self.emit_from(BlockId(0), None);
        for index in 0..self.function.blocks.len() {
            let block = BlockId(index as u32);
            if self.cfg.is_reachable(block) && !self.consumed.contains(&block) {
                self.emit_from(block, None);
            }
        }
    }

    fn line(&mut self, text: &str) {
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
        self.out.push_str(text);
        self.out.push('\n');
    }

    /// The rendered name of a block: the sentinel is the shared
    /// indirect-exit epilogue.
    fn block_name(block: BlockId) -> String {
        if block == BlockId(u32::MAX) {
            "b_indirect".to_string()
        } else {
            format!("b{}", block.0)
        }
    }

    fn label(&mut self, block: BlockId) {
        self.line(&format!("{}:;", Self::block_name(block)));
    }

    fn goto(&mut self, target: BlockId) {
        self.goto_targets.insert(target);
        self.line(&format!("goto {};", Self::block_name(target)));
    }

    fn shape_at(&self, block: BlockId) -> Option<&Shape> {
        self.shapes
            .iter()
            .find(|s| s.header == block && !self.consumed.contains(&block))
    }

    fn is_open_latch(&self, block: BlockId) -> bool {
        matches!(self.loop_stack.last(), Some(&(_, latch, _)) if latch == block)
    }

    /// True when the block's predecessors are exactly `expected`.
    fn preds_are(&self, block: BlockId, expected: &[BlockId]) -> bool {
        if block.0 as usize >= self.cfg.preds.len() {
            return false;
        }
        let mut preds: Vec<BlockId> = self.cfg.preds[block.0 as usize].to_vec();
        preds.sort();
        preds.dedup();
        let mut expected = expected.to_vec();
        expected.sort();
        expected.dedup();
        preds == expected
    }

    /// The core walk: emits the chain starting at `start`, closing
    /// loops and returning when control closes. `stop` names the
    /// latch this walk must close as `(latch, exit)`; the return
    /// value reports whether that close happened.
    fn emit_from(&mut self, start: BlockId, stop: Option<(BlockId, BlockId)>) -> bool {
        let mut block = start;
        loop {
            if self.consumed.contains(&block) {
                return false;
            }
            if self.goto_targets.contains(&block) {
                self.label(block);
            }
            if let Some((latch, _)) = stop
                && block == latch
            {
                self.consumed.insert(block);
                let skip = self.latch_skip(block);
                self.emit_insts(block, skip);
                self.emit_close(block);
                self.loop_stack.pop();
                return true;
            }
            if let Some(shape) = self.shape_at(block).cloned() {
                self.open_loop(shape, stop);
                return false;
            }
            self.consumed.insert(block);
            self.emit_insts(block, false);
            let term = self.function.blocks[block.0 as usize].term.clone();
            match term {
                Terminator::Unreachable => {
                    let empty = if self.unit { "return 0;" } else { "return;" };
                    self.line(&format!("/* unreachable */ {empty}"));
                    return false;
                }
                Terminator::Return { state, .. } => {
                    self.emit_return(&state);
                    return false;
                }
                Terminator::IndirectExit { target, .. } => {
                    let target = self.value(&target);
                    self.line(&format!("/* unresolved indirect exit: pc = {target} */"));
                    self.goto(BlockId(u32::MAX));
                    return false;
                }
                Terminator::Call {
                    target,
                    cont,
                    state,
                } => {
                    self.emit_call(&target, &state);
                    match cont {
                        Some((cont, _)) => {
                            if self.consumed.contains(&cont) || self.goto_targets.contains(&cont) {
                                self.goto(cont);
                                return false;
                            }
                            block = cont;
                        }
                        None => {
                            let empty = if self.unit { "return 0;" } else { "return;" };
                            self.line(empty);
                            return false;
                        }
                    }
                }
                Terminator::Branch { target, args: _ } => {
                    if self.branch_continues(target) {
                        return false;
                    }
                    if self.consumed.contains(&target) || self.goto_targets.contains(&target) {
                        self.goto(target);
                        return false;
                    }
                    block = target;
                }
                Terminator::BranchCond {
                    cond,
                    then_target,
                    else_target,
                    ..
                } => {
                    let (action, next) =
                        self.emit_conditional(block, cond, then_target, else_target);
                    match action {
                        Action::Closed => return false,
                        Action::Fallthrough => block = next,
                    }
                }
            }
        }
    }

    /// A branch to the surrounding loop's header re-evaluates the
    /// loop condition. Shaped loops have exactly one back edge (the
    /// latch, closed by the walk), so this is a safety net.
    fn branch_continues(&mut self, target: BlockId) -> bool {
        match self.loop_stack.last() {
            Some(&(header, _, _)) if target == header => {
                self.line("continue;");
                true
            }
            _ => false,
        }
    }

    /// Whether the latch's trailing `cmp` is its close condition.
    fn latch_skip(&self, latch: BlockId) -> bool {
        let do_while = matches!(self.loop_stack.last(), Some((_, _, Close::DoWhile { .. })));
        do_while && self.last_inst_is_cmp(latch)
    }

    /// Emits the closing line of the innermost open loop.
    fn emit_close(&mut self, latch: BlockId) {
        match self.loop_stack.last().map(|(_, _, close)| close.clone()) {
            Some(Close::While) => self.line("}"),
            Some(Close::DoWhile { cond, invert }) => {
                let text = self.condition_text(cond, latch, invert);
                self.line(&format!("}} while ({text});"));
            }
            None => self.line("}"),
        }
    }

    /// Emits one recognized loop and continues at its exit. `stop`
    /// is the enclosing walk's latch, used for the exit continuation.
    fn open_loop(&mut self, shape: Shape, stop: Option<(BlockId, BlockId)>) {
        let skip = matches!(shape.close, Close::While) && self.last_inst_is_cmp(shape.header);
        self.consumed.insert(shape.header);
        self.emit_insts(shape.header, skip);
        let (opening, entry) = if matches!(shape.close, Close::While) {
            let (cond, entry) = self.loop_condition(&shape);
            (format!("while ({cond}) {{"), entry)
        } else {
            ("do {".to_string(), shape.body_enter)
        };
        self.line(&opening);
        self.loop_stack
            .push((shape.header, shape.latch, shape.close.clone()));
        self.indent += 1;
        let closed = self.emit_from(entry, Some((shape.latch, shape.exit)));
        self.indent -= 1;
        if !closed {
            self.loop_stack.pop();
            self.frame_close(&shape);
        }
        let (action, next) = self.fallthrough_or_goto(shape.exit);
        if action == Action::Fallthrough {
            self.emit_from(next, stop);
        }
    }

    /// The loop close emitted by the open_loop frame itself, when the
    /// body walk ended without reaching the latch.
    fn frame_close(&mut self, shape: &Shape) {
        match &shape.close {
            Close::While => self.line("}"),
            Close::DoWhile { cond, invert } => {
                let text = self.condition_text(*cond, shape.latch, *invert);
                self.line(&format!("}} while ({text});"));
            }
        }
    }

    /// The while condition and the effective body entry: the
    /// header's branch condition, negated when the else-arm enters
    /// the body, folded across the short-circuit chain. Each chain
    /// link's else-arm must be the loop exit, so `a && b` folds while
    /// other shapes stay untouched. Chain blocks are consumed.
    fn loop_condition(&mut self, shape: &Shape) -> (String, BlockId) {
        let header = &self.function.blocks[shape.header.0 as usize].term;
        let Some((cond, first, invert)) = (match header {
            Terminator::BranchCond {
                cond,
                then_target,
                else_target,
                ..
            } => {
                if *then_target == shape.body_enter {
                    Some((*cond, *then_target, false))
                } else if *else_target == shape.body_enter {
                    Some((*cond, *else_target, true))
                } else {
                    None
                }
            }
            _ => None,
        }) else {
            return ("1".to_string(), shape.body_enter);
        };
        let mut text = self.condition_text(cond, shape.header, invert);
        let mut next = first;
        let mut current = shape.header;
        let mut folded: Vec<BlockId> = Vec::new();
        for _ in 0..8 {
            if next == shape.exit {
                break;
            }
            let Some((inner_cond, inner_then, inner_else)) = self.fold_candidate(next, &[current])
            else {
                break;
            };
            let (fold_cond, follow, negated) = if inner_else == shape.exit {
                (inner_cond, inner_then, false)
            } else if inner_then == shape.exit {
                (inner_cond, inner_else, true)
            } else {
                break;
            };
            if follow == next {
                break;
            }
            let inner = self.condition_text(fold_cond, next, negated);
            text = format!("({text}) && ({inner})");
            folded.push(next);
            current = next;
            next = follow;
        }
        for block in folded {
            self.consumed.insert(block);
        }
        (text, next)
    }

    fn emit_return(&mut self, state: &[Value]) {
        match state.first() {
            Some(value) => {
                let value = self.value(value);
                self.line(&format!("return {value};"));
            }
            None => {
                let empty = if self.unit { "return 0;" } else { "return;" };
                self.line(empty);
            }
        }
    }

    fn emit_insts(&mut self, block: BlockId, skip_trailing_cmp: bool) {
        let insts = self.function.blocks[block.0 as usize].insts.clone();
        let count = insts.len();
        for (index, inst) in insts.iter().enumerate() {
            if skip_trailing_cmp && index + 1 == count && matches!(inst.op, Op::Cmp { .. }) {
                continue;
            }
            self.emit_inst(inst);
        }
    }

    fn last_inst_is_cmp(&self, block: BlockId) -> bool {
        self.function.blocks[block.0 as usize]
            .insts
            .last()
            .is_some_and(|inst| matches!(inst.op, Op::Cmp { .. }))
    }

    fn emit_call(&mut self, target: &Value, state: &[Value]) {
        match target {
            Value::Const(addr) => {
                let name = self.callee_name(*addr);
                if self.unit {
                    let mut args: Vec<String> = state
                        .iter()
                        .take(4)
                        .map(|value| self.value(value))
                        .collect();
                    args.resize(4, "0u".to_string());
                    let stack = if state.len() > 4 {
                        format!(" /* + {} stack args */", state.len() - 4)
                    } else {
                        String::new()
                    };
                    self.line(&format!("{}({});{stack}", name, args.join(", ")));
                } else {
                    self.line(&format!("{name}();"));
                }
            }
            value => {
                let value = self.value(value);
                self.line(&format!("__indirect_call({value});"));
            }
        }
    }

    /// Emits a conditional, structuring early returns, short-circuit
    /// chains, and diamonds. Returns the next block when control
    /// falls through; the fallback emits plain gotos from the
    /// original arms so every edge survives.
    fn emit_conditional(
        &mut self,
        block: BlockId,
        cond: Condition,
        then_target: BlockId,
        else_target: BlockId,
    ) -> (Action, BlockId) {
        if then_target == else_target {
            return self.fallthrough_or_goto(then_target);
        }
        let mut cond_text = self.condition_text(cond, block, false);
        let mut taken = then_target;
        let mut not_taken = else_target;
        let mut taken_preds = vec![block];
        let mut else_preds = vec![block];
        let mut folded: Vec<BlockId> = Vec::new();
        for _ in 0..8 {
            if let Some((inner_cond, inner_then, inner_else)) =
                self.fold_candidate(taken, &taken_preds)
                && inner_else == not_taken
                && inner_then != taken
                && inner_then != block
            {
                let inner = self.condition_text(inner_cond, taken, false);
                cond_text = format!("({cond_text}) && ({inner})");
                folded.push(taken);
                else_preds.push(taken);
                taken_preds = vec![taken];
                taken = inner_then;
                continue;
            }
            if let Some((inner_cond, inner_then, inner_else)) =
                self.fold_candidate(not_taken, &else_preds)
                && inner_then == taken
                && inner_else != not_taken
                && inner_else != block
            {
                let inner = self.condition_text(inner_cond, not_taken, true);
                cond_text = format!("({cond_text}) || ({inner})");
                folded.push(not_taken);
                taken_preds.push(not_taken);
                else_preds = vec![not_taken];
                not_taken = inner_else;
                continue;
            }
            break;
        }
        // Early return in exactly one arm.
        let taken_returns = self.return_arm(taken, &taken_preds, block);
        let else_returns = self.return_arm(not_taken, &else_preds, block);
        if taken_returns != else_returns {
            let (arm, holds) = if taken_returns {
                (taken, true)
            } else {
                (not_taken, false)
            };
            let text = if holds {
                format!("if ({cond_text}) {{")
            } else {
                let inverted = self.condition_text(cond, block, true);
                format!("if ({inverted}) {{")
            };
            self.line(&text);
            self.indent += 1;
            self.emit_insts(arm, false);
            let state = match self.function.blocks[arm.0 as usize].term.clone() {
                Terminator::Return { state, .. } => state,
                _ => Vec::new(),
            };
            self.emit_return(&state);
            self.indent -= 1;
            self.line("}");
            self.consumed.insert(arm);
            for f in folded {
                self.consumed.insert(f);
            }
            let next = if holds { not_taken } else { taken };
            return self.fallthrough_or_goto(next);
        }
        // Diamond: both arms are plain chains ending at the same
        // join, or both return.
        let then_end = self.chain_end(taken, &taken_preds);
        let else_end = self.chain_end(not_taken, &else_preds);
        if let (Some(then_join), Some(else_join)) = (then_end, else_end)
            && then_join == else_join
        {
            self.line(&format!("if ({cond_text}) {{"));
            self.indent += 1;
            self.emit_arm(taken, then_join);
            self.indent -= 1;
            self.line("} else {");
            self.indent += 1;
            self.emit_arm(not_taken, else_join);
            self.indent -= 1;
            self.line("}");
            for f in folded {
                self.consumed.insert(f);
            }
            return match then_join {
                Some(join) => self.fallthrough_or_goto(join),
                None => (Action::Closed, block),
            };
        }
        // Nothing matched: plain gotos from the original arms.
        let text = self.condition_text(cond, block, false);
        self.goto_targets.insert(then_target);
        let name = Self::block_name(then_target);
        self.line(&format!("if ({text}) goto {name};"));
        if self.consumed.contains(&else_target) || self.goto_targets.contains(&else_target) {
            self.goto(else_target);
            return (Action::Closed, block);
        }
        (Action::Fallthrough, else_target)
    }

    /// Whether the block is an unvisited single-entry return block.
    fn return_arm(&self, arm: BlockId, preds: &[BlockId], from: BlockId) -> bool {
        arm != from
            && arm != BlockId(u32::MAX)
            && !self.consumed.contains(&arm)
            && self.preds_are(arm, preds)
            && matches!(
                self.function.blocks[arm.0 as usize].term,
                Terminator::Return { .. }
            )
    }

    /// After a structured construct: `goto` when the join was already
    /// reached or needs its label, fallthrough otherwise.
    fn fallthrough_or_goto(&mut self, next: BlockId) -> (Action, BlockId) {
        if next == BlockId(u32::MAX)
            || self.consumed.contains(&next)
            || self.goto_targets.contains(&next)
        {
            self.goto(next);
            return (Action::Closed, next);
        }
        (Action::Fallthrough, next)
    }

    /// The short-circuit continuation of a conditional chain, when
    /// `block` is an unvisited branch-only block ending in a `cmp`
    /// whose predecessors are exactly `preds`.
    fn fold_candidate(
        &self,
        block: BlockId,
        preds: &[BlockId],
    ) -> Option<(Condition, BlockId, BlockId)> {
        if block == BlockId(u32::MAX)
            || self.consumed.contains(&block)
            || self.is_open_latch(block)
            || self.shape_at(block).is_some()
            || self.function.blocks[block.0 as usize].insts.len() > 1
            || !self.last_inst_is_cmp(block)
            || !self.preds_are(block, preds)
        {
            return None;
        }
        match &self.function.blocks[block.0 as usize].term {
            Terminator::BranchCond {
                cond,
                then_target,
                else_target,
                ..
            } => Some((*cond, *then_target, *else_target)),
            _ => None,
        }
    }

    /// The diamond arm's end: `Some(Some(join))` when the arm block
    /// branches to a join, `Some(None)` when it returns, `None` when
    /// it cannot arm a diamond (nested conditional, shared entry).
    /// Straight-line instructions live inside one block, so an arm is
    /// a single block; deeper nesting comes from the walk continuing
    /// at the join.
    fn chain_end(&self, start: BlockId, head_preds: &[BlockId]) -> Option<Option<BlockId>> {
        if start == BlockId(u32::MAX) {
            return None;
        }
        if self.is_open_latch(start) {
            return Some(Some(start));
        }
        if self.consumed.contains(&start) || self.shape_at(start).is_some() {
            return None;
        }
        if !self.preds_are(start, head_preds) {
            return None;
        }
        match self.function.blocks[start.0 as usize].term.clone() {
            Terminator::Branch { target, args: _ } if target != start => Some(Some(target)),
            Terminator::Return { .. } => Some(None),
            _ => None,
        }
    }

    /// Emits one diamond arm: a plain chain from `start` whose final
    /// `Branch { join }` is consumed as the join edge. `None` joins
    /// on a returning arm. The enclosing loop's latch is never
    /// consumed here; the body walk closes it.
    fn emit_arm(&mut self, start: BlockId, join: Option<BlockId>) {
        let mut current = start;
        loop {
            if current == BlockId(u32::MAX) || self.is_open_latch(current) {
                return;
            }
            self.consumed.insert(current);
            self.emit_insts(current, false);
            match self.function.blocks[current.0 as usize].term.clone() {
                Terminator::Branch { target, args: _ } => {
                    if Some(target) == join {
                        return;
                    }
                    current = target;
                }
                Terminator::Return { state, .. } => {
                    self.emit_return(&state);
                    return;
                }
                _ => return,
            }
        }
    }

    fn emit_inst(&mut self, inst: &Inst) {
        let dest = inst.dests.first().map(|id| format!("v{}", id.0));
        let text = match &inst.op {
            Op::Add { set_flags, a, b } => self.alu(dest.as_deref(), "+", a, b, *set_flags),
            Op::Adc {
                set_flags,
                a,
                b,
                carry_in,
            } => self.alu3(dest.as_deref(), "+", a, b, carry_in, *set_flags),
            Op::Sub { set_flags, a, b } => self.alu(dest.as_deref(), "-", a, b, *set_flags),
            Op::Sbc {
                set_flags,
                a,
                b,
                carry_in,
            } => self.alu3(dest.as_deref(), "-", a, b, carry_in, *set_flags),
            Op::Rsc {
                set_flags,
                a,
                b,
                carry_in,
            } => self.alu3(dest.as_deref(), "-", b, a, carry_in, *set_flags),
            Op::Rsb { set_flags, a, b } => self.alu(dest.as_deref(), "-", b, a, *set_flags),
            Op::And {
                set_flags,
                a,
                b,
                carry_in: _,
            } => self.alu(dest.as_deref(), "&", a, b, *set_flags),
            Op::Orr {
                set_flags,
                a,
                b,
                carry_in: _,
            } => self.alu(dest.as_deref(), "|", a, b, *set_flags),
            Op::Eor {
                set_flags,
                a,
                b,
                carry_in: _,
            } => self.alu(dest.as_deref(), "^", a, b, *set_flags),
            Op::Bic {
                set_flags,
                a,
                b,
                carry_in: _,
            } => self.alu(dest.as_deref(), "& ~", a, b, *set_flags),
            Op::Mov { a, .. } => match dest.as_deref() {
                Some(dest) => {
                    let value = self.value(a);
                    format!("{}{value};", self.def_prefix(dest))
                }
                None => {
                    let value = self.value(a);
                    format!("__mov({value});")
                }
            },
            Op::Mvn { a, .. } => match dest.as_deref() {
                Some(dest) => {
                    let value = self.value(a);
                    format!("{}~{value};", self.def_prefix(dest))
                }
                None => {
                    let value = self.value(a);
                    format!("__mvn({value});")
                }
            },
            Op::Mul { set_flags, a, b } => self.alu(dest.as_deref(), "*", a, b, *set_flags),
            Op::MultiplyLong {
                unsigned,
                accumulate,
                a,
                b,
                acc_lo,
                acc_hi,
                set_flags: _,
            } => {
                let sign = if *unsigned { "u" } else { "i" };
                let acc = if *accumulate {
                    let hi = self.value(acc_hi);
                    let lo = self.value(acc_lo);
                    format!(" + ({hi} << 32) + {lo}")
                } else {
                    String::new()
                };
                let (lo, hi) = match (inst.dests.first(), inst.dests.get(1)) {
                    (Some(lo), Some(hi)) => (format!("v{}", lo.0), format!("v{}", hi.0)),
                    _ => ("__lo".to_string(), "__hi".to_string()),
                };
                let a = self.value(a);
                let b = self.value(b);
                if self.unit {
                    if inst.dests.is_empty() {
                        format!(
                            "{{ u64 __pair = ({sign}64){a} * ({sign}64){b}{acc}; (void)__pair; }}"
                        )
                    } else {
                        format!(
                            "{{ u64 __pair = ({sign}64){a} * ({sign}64){b}{acc}; {lo} = (u32)__pair; {hi} = (u32)(__pair >> 32); }}"
                        )
                    }
                } else {
                    format!("u64 {lo}:{hi} = ({sign}64){a} * ({sign}64){b}{acc};")
                }
            }
            Op::Cmp { a, b } => {
                let (a, b) = (self.value(a), self.value(b));
                format!("flags_cmp({a}, {b});")
            }
            Op::Cmn { a, b } => {
                let (a, b) = (self.value(a), self.value(b));
                format!("flags_cmn({a}, {b});")
            }
            Op::Tst { a, b, .. } => {
                let (a, b) = (self.value(a), self.value(b));
                format!("flags_tst({a}, {b});")
            }
            Op::Teq { a, b, .. } => {
                let (a, b) = (self.value(a), self.value(b));
                format!("flags_teq({a}, {b});")
            }
            Op::Lsl {
                set_flags,
                a,
                amount,
                ..
            } => self.alu(dest.as_deref(), "<<", a, amount, *set_flags),
            Op::Lsr {
                set_flags,
                a,
                amount,
                ..
            } => self.alu(dest.as_deref(), ">>", a, amount, *set_flags),
            Op::Asr {
                set_flags,
                a,
                amount,
                ..
            } => {
                let (a, amount) = (self.value(a), self.value(amount));
                match dest.as_deref() {
                    Some(dest) => {
                        let flags = if *set_flags { " /* sets flags */" } else { "" };
                        format!(
                            "{}(u32)((i32){a} >> {amount});{flags}",
                            self.def_prefix(dest)
                        )
                    }
                    None => format!("__asr({a}, {amount});"),
                }
            }
            Op::Ror {
                set_flags,
                a,
                amount,
                ..
            } => {
                let (a, amount) = (self.value(a), self.value(amount));
                match dest.as_deref() {
                    Some(dest) => {
                        let flags = if *set_flags { " /* sets flags */" } else { "" };
                        format!("{}__ror({a}, {amount});{flags}", self.def_prefix(dest))
                    }
                    None => format!("__ror({a}, {amount});"),
                }
            }
            Op::Rrx { set_flags, a, .. } => {
                let a = self.value(a);
                match dest.as_deref() {
                    Some(dest) => {
                        let flags = if *set_flags { " /* sets flags */" } else { "" };
                        format!("{}__rrx({a});{flags}", self.def_prefix(dest))
                    }
                    None => format!("__rrx({a});"),
                }
            }
            Op::Load {
                addr,
                size,
                signed,
                mode,
            } => {
                let width = match size {
                    crate::Size::Byte => "8",
                    crate::Size::Half => "16",
                    crate::Size::Word => "32",
                };
                let sign = if *signed { "i" } else { "u" };
                let helper = if self.unit { "load" } else { "mem" };
                let comment = Self::io_comment(addr).unwrap_or_default();
                let addr = self.value(addr);
                let line = match dest.as_deref() {
                    Some(dest) => {
                        format!(
                            "{}{helper}_{sign}{width}({addr});{comment}",
                            self.def_prefix(dest)
                        )
                    }
                    None => format!("{helper}_{sign}{width}({addr});{comment}"),
                };
                if matches!(mode, crate::LoadMode::Swap | crate::LoadMode::Block) {
                    line.trim_end_matches(';').to_string() + "; /* swap or block mode */"
                } else {
                    line
                }
            }
            Op::Store { addr, value, size } => {
                let width = match size {
                    crate::Size::Byte => "8",
                    crate::Size::Half => "16",
                    crate::Size::Word => "32",
                };
                let helper = if self.unit { "store_u" } else { "mem_u" };
                let comment = Self::io_comment(addr).unwrap_or_default();
                let (addr, value) = (self.value(addr), self.value(value));
                format!("{helper}{width}({addr}, {value});{comment}")
            }
            Op::Intrinsic { name, args } => {
                let comment = if name == &"swi" {
                    Self::swi_comment(args).unwrap_or_default()
                } else {
                    String::new()
                };
                let name = if self.unit {
                    name.replace('-', "_")
                } else {
                    (*name).to_string()
                };
                let args = args
                    .iter()
                    .map(|a| self.value(a))
                    .collect::<Vec<_>>()
                    .join(", ");
                match dest.as_deref() {
                    Some(dest) => {
                        format!("{}{name}({args});{comment}", self.def_prefix(dest))
                    }
                    None => format!("{name}({args});{comment}"),
                }
            }
        };
        self.line(&text);
    }

    fn alu(&self, dest: Option<&str>, op: &str, a: &Value, b: &Value, set_flags: bool) -> String {
        let (a, b) = (self.value(a), self.value(b));
        let flags = if set_flags { " /* sets flags */" } else { "" };
        match dest {
            Some(dest) => format!("{}{a} {op} {b};{flags}", self.def_prefix(dest)),
            None => format!("__discard({a} {op} {b});"),
        }
    }

    fn alu3(
        &self,
        dest: Option<&str>,
        op: &str,
        a: &Value,
        b: &Value,
        carry: &Value,
        set_flags: bool,
    ) -> String {
        let (a, b, carry) = (self.value(a), self.value(b), self.value(carry));
        let flags = if set_flags { " /* sets flags */" } else { "" };
        match dest {
            Some(dest) => format!("{}{a} {op} {b} + {carry};{flags}", self.def_prefix(dest)),
            None => format!("__discard({a} {op} {b} + {carry});"),
        }
    }

    fn value(&self, value: &Value) -> String {
        match value {
            Value::Const(c) => format!("0x{c:x}u"),
            Value::Id(id) => format!("v{}", id.0),
        }
    }

    /// A dest binding: a declaration in the readable form, a plain
    /// assignment in the unit form (declared once at the top).
    fn def_prefix(&self, dest: &str) -> String {
        if self.unit {
            format!("{dest} = ")
        } else {
            format!("u32 {dest} = ")
        }
    }

    /// The C name of a called sub: unit-form names resolve through
    /// the unit's callee table; the readable form keeps `sub_ADDR`.
    fn callee_name(&self, addr: u32) -> String {
        if self.unit {
            self.callees
                .get(&addr)
                .cloned()
                .unwrap_or_else(|| format!("sub_{addr:08x}"))
        } else {
            format!("sub_{addr:x}")
        }
    }

    /// The unit-form parameter list: the entry block's first four
    /// value slots become the C parameters r0..r3.
    fn unit_params(&self) -> String {
        let entry = &self.function.blocks[0].params;
        (0..4)
            .map(|slot| {
                entry
                    .get(slot)
                    .map(|id| format!("u32 v{}", id.0))
                    .unwrap_or_else(|| format!("u32 a{slot}"))
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The unit-form declaration block: every referenced SSA id that
    /// is not an entry parameter, one local per line.
    fn unit_declarations(&self) -> String {
        let entry: Vec<ValueId> = self.function.blocks[0]
            .params
            .iter()
            .take(4)
            .copied()
            .collect();
        let mut text = String::new();
        for id in self.referenced_ids() {
            if entry.contains(&id) {
                continue;
            }
            text.push_str(&format!("    u32 v{};\n", id.0));
        }
        text
    }

    /// Every SSA id the rendered text can mention: instruction dests
    /// and operands, plus call, return, and indirect-exit state.
    fn referenced_ids(&self) -> BTreeSet<ValueId> {
        let mut ids = BTreeSet::new();
        for block in &self.function.blocks {
            for param in &block.params {
                ids.insert(*param);
            }
            for inst in &block.insts {
                for dest in &inst.dests {
                    ids.insert(*dest);
                }
                for value in crate::opt::operands(&inst.op) {
                    if let Value::Id(id) = value {
                        ids.insert(id);
                    }
                }
            }
            let mut state: Vec<Value> = Vec::new();
            match &block.term {
                Terminator::Return { state: out, .. }
                | Terminator::IndirectExit { state: out, .. } => {
                    state = out.clone();
                }
                Terminator::Call {
                    target, state: out, ..
                } => {
                    state = out.clone();
                    if let Value::Id(id) = target {
                        ids.insert(*id);
                    }
                }
                _ => {}
            }
            for value in state {
                if let Value::Id(id) = value {
                    ids.insert(id);
                }
            }
        }
        ids
    }

    /// The MMIO register name when `addr` is a constant in the IO
    /// block, for the inline comment.
    fn io_comment(addr: &Value) -> Option<String> {
        match addr {
            Value::Const(c) => mmio::io_name(*c).map(|name| format!(" /* {name} */")),
            _ => None,
        }
    }

    /// The SWI service comment for a `swi` intrinsic: the number sits
    /// in bits 16-23 of the ARM immediate, plain in THUMB.
    fn swi_comment(args: &[Value]) -> Option<String> {
        let Value::Const(raw) = args.first()? else {
            return None;
        };
        let number = if *raw > 0xFF {
            (raw >> 16) & 0xFF
        } else {
            *raw
        };
        let name = bios::swi_name(u8::try_from(number).ok()?)
            .map(|name| format!(": {name}"))
            .unwrap_or_default();
        Some(format!(" /* swi 0x{number:02x}{name} */"))
    }

    /// Renders `cond` against the block's trailing `cmp` when
    /// present, else as a flag expression. `invert` negates.
    fn condition_text(&self, cond: Condition, block: BlockId, invert: bool) -> String {
        let (cond, wrapped) = if invert {
            (negate(cond), cond == Condition::Al)
        } else {
            (Some(cond), false)
        };
        let Some(cond) = cond else {
            let text = self.flag_expr(Condition::Al, block);
            return format!("!({text})");
        };
        let text = match self.block_cmp(block) {
            Some((a, b)) => self.comparison_text(cond, a, b, block),
            None => self.flag_expr(cond, block),
        };
        if wrapped { format!("!({text})") } else { text }
    }

    /// The operands of the block's last instruction, when it is a
    /// `cmp`.
    fn block_cmp(&self, block: BlockId) -> Option<(Value, Value)> {
        let block = self.function.blocks.get(block.0 as usize)?;
        match &block.insts.last()?.op {
            Op::Cmp { a, b } => Some((*a, *b)),
            _ => None,
        }
    }

    fn comparison_text(&self, cond: Condition, a: Value, b: Value, block: BlockId) -> String {
        let (a, b) = (self.value(&a), self.value(&b));
        match cond {
            Condition::Eq => format!("({a} == {b})"),
            Condition::Ne => format!("({a} != {b})"),
            Condition::Hi => format!("((u32){a} > (u32){b})"),
            Condition::Ls => format!("((u32){a} <= (u32){b})"),
            Condition::Cs => format!("((u32){a} >= (u32){b})"),
            Condition::Cc => format!("((u32){a} < (u32){b})"),
            Condition::Ge => format!("((i32){a} >= (i32){b})"),
            Condition::Lt => format!("((i32){a} < (i32){b})"),
            Condition::Gt => format!("((i32){a} > (i32){b})"),
            Condition::Le => format!("((i32){a} <= (i32){b})"),
            _ => format!("{} /* {cond:?} */", self.flag_expr(cond, block)),
        }
    }

    fn flag_expr(&self, cond: Condition, block: BlockId) -> String {
        let n = self.flag_slot(block, Flag::N);
        let z = self.flag_slot(block, Flag::Z);
        let c = self.flag_slot(block, Flag::C);
        let v = self.flag_slot(block, Flag::V);
        match cond {
            Condition::Eq => z,
            Condition::Ne => format!("!{z}"),
            Condition::Cs => c,
            Condition::Cc => format!("!{c}"),
            Condition::Mi => n,
            Condition::Pl => format!("!{n}"),
            Condition::Vs => v,
            Condition::Vc => format!("!{v}"),
            Condition::Hi => format!("({c} && !{z})"),
            Condition::Ls => format!("(!{c} || {z})"),
            Condition::Ge => format!("({n} == {v})"),
            Condition::Lt => format!("({n} != {v})"),
            Condition::Gt => format!("(!{z} && {n} == {v})"),
            Condition::Le => format!("({z} || {n} != {v})"),
            Condition::Al => "1".to_string(),
        }
    }

    /// The SSA name of one flag slot in `block`'s parameters, or a
    /// placeholder when the block does not carry it.
    fn flag_slot(&self, block: BlockId, flag: Flag) -> String {
        let index = SLOTS
            .iter()
            .position(|slot| matches!(slot, crate::Slot::Flag(f) if *f == flag));
        let Some(index) = index else {
            return format!("0u /* {flag:?} */");
        };
        match self
            .function
            .blocks
            .get(block.0 as usize)
            .and_then(|b| b.params.get(index))
        {
            Some(param) => format!("v{}", param.0),
            None => format!("0u /* {flag:?} */"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Block, ValueId};

    fn inst(dests: Vec<ValueId>, op: Op) -> Inst {
        Inst {
            guard: None,
            dests,
            fallbacks: Vec::new(),
            op,
            pc: 0,
            state: Vec::new(),
        }
    }

    fn cmp(a: u32, b: u32) -> Inst {
        inst(
            Vec::new(),
            Op::Cmp {
                a: Value::Const(a),
                b: Value::Const(b),
            },
        )
    }

    fn block(insts: Vec<Inst>, term: Terminator) -> Block {
        Block {
            addr: 0,
            params: Vec::new(),
            insts,
            cycles: Vec::new(),
            term,
        }
    }

    fn branch(target: BlockId) -> Terminator {
        Terminator::Branch {
            target,
            args: Vec::new(),
        }
    }

    fn branch_cond(cond: Condition, then_target: BlockId, else_target: BlockId) -> Terminator {
        Terminator::BranchCond {
            cond,
            then_target,
            then_args: Vec::new(),
            else_target,
            else_args: Vec::new(),
        }
    }

    fn ret() -> Terminator {
        Terminator::Return {
            state: Vec::new(),
            switches_state: false,
        }
    }

    fn value_return(value: u32) -> Terminator {
        Terminator::Return {
            state: vec![Value::Const(value)],
            switches_state: false,
        }
    }

    fn emitted(blocks: Vec<Block>) -> String {
        emit_function(&Function {
            name: "test_fn".to_string(),
            thumb: false,
            blocks,
        })
    }

    /// Every `goto bN` has a matching label and braces balance.
    fn assert_wellformed(text: &str) {
        for chunk in text.split("goto ").skip(1) {
            let target = chunk.split(';').next().unwrap().trim();
            assert!(
                text.contains(&format!("{target}:;")),
                "dangling goto {target} in:\n{text}"
            );
        }
        assert_eq!(
            text.matches('{').count(),
            text.matches('}').count(),
            "unbalanced braces in:\n{text}"
        );
    }

    #[test]
    fn if_else_diamond() {
        let text = emitted(vec![
            block(
                vec![cmp(0x5, 0x1)],
                branch_cond(Condition::Ne, BlockId(1), BlockId(2)),
            ),
            block(vec![], branch(BlockId(3))),
            block(vec![], branch(BlockId(3))),
            block(vec![], ret()),
        ]);
        assert_wellformed(&text);
        assert!(text.contains("if ("), "{text}");
        assert!(text.contains("} else {"), "{text}");
        assert!(!text.contains("goto"), "{text}");
    }

    #[test]
    fn else_if_chain() {
        let text = emitted(vec![
            // b0: cond ? b1 : b2
            block(
                vec![cmp(0x1, 0x2)],
                branch_cond(Condition::Eq, BlockId(1), BlockId(2)),
            ),
            // b1 (then): return
            block(vec![], value_return(0x11)),
            // b2 (else): nested cond ? b3 : b4
            block(
                vec![cmp(0x3, 0x4)],
                branch_cond(Condition::Lt, BlockId(3), BlockId(4)),
            ),
            block(vec![], value_return(0x22)),
            block(vec![], value_return(0x33)),
        ]);
        assert_wellformed(&text);
        assert!(text.contains("} else {"), "{text}");
        // The second decision is nested after the first arm's return.
        let first_return = text.find("return 0x11u;").unwrap();
        let nested = text
            .find("if (((i32)0x3u < (i32)0x4u))")
            .expect("nested if");
        assert!(first_return < nested, "{text}");
    }

    #[test]
    fn short_circuit_conjunction() {
        let text = emitted(vec![
            // b0: a ? b1 : b3; b1: b ? b2 : b3 (folded into a && b)
            block(
                vec![cmp(0x1, 0x0)],
                branch_cond(Condition::Ne, BlockId(1), BlockId(3)),
            ),
            block(
                vec![cmp(0x2, 0x0)],
                branch_cond(Condition::Ne, BlockId(2), BlockId(3)),
            ),
            block(vec![], value_return(0x1)),
            block(vec![], value_return(0x0)),
        ]);
        assert_wellformed(&text);
        assert!(text.contains("&&"), "{text}");
        assert!(!text.contains("goto"), "{text}");
    }

    #[test]
    fn short_circuit_disjunction() {
        let text = emitted(vec![
            // b0: a ? b2 : b1; b1: b ? b2 : b3 (folded into a || b)
            block(
                vec![cmp(0x1, 0x0)],
                branch_cond(Condition::Ne, BlockId(2), BlockId(1)),
            ),
            block(
                vec![cmp(0x2, 0x0)],
                branch_cond(Condition::Ne, BlockId(2), BlockId(3)),
            ),
            block(vec![], value_return(0x1)),
            block(vec![], value_return(0x0)),
        ]);
        assert_wellformed(&text);
        assert!(text.contains("||"), "{text}");
        assert!(!text.contains("goto"), "{text}");
    }

    #[test]
    fn while_loop() {
        let text = emitted(vec![
            // b0 header: while (r != 0); b1 body; b2 latch; b3 exit
            block(
                vec![cmp(0x5, 0x0)],
                branch_cond(Condition::Ne, BlockId(1), BlockId(3)),
            ),
            block(vec![], branch(BlockId(2))),
            block(vec![], branch(BlockId(0))),
            block(vec![], ret()),
        ]);
        assert_wellformed(&text);
        assert!(text.contains("while ((0x5u != 0x0u)) {"), "{text}");
        assert!(!text.contains("goto"), "{text}");
        let close = text.find("while (").unwrap();
        assert!(text[close..].contains("}\n"), "{text}");
    }

    #[test]
    fn while_loop_with_compound_condition() {
        let text = emitted(vec![
            // b0: a ? b1 : b4; b1: b ? b2 : b4 (folded into a && b);
            // b2 body; b3 latch; b4 exit
            block(
                vec![cmp(0x1, 0x0)],
                branch_cond(Condition::Ne, BlockId(1), BlockId(4)),
            ),
            block(
                vec![cmp(0x2, 0x0)],
                branch_cond(Condition::Ne, BlockId(2), BlockId(4)),
            ),
            block(vec![], branch(BlockId(3))),
            block(vec![], branch(BlockId(0))),
            block(vec![], ret()),
        ]);
        assert_wellformed(&text);
        assert!(
            text.contains("while (((0x1u != 0x0u)) && ((0x2u != 0x0u))) {"),
            "{text}"
        );
        assert!(!text.contains("goto"), "{text}");
    }

    #[test]
    fn do_while_loop() {
        let text = emitted(vec![
            // b0 entry; b1 body; b2 latch: loop while equal; b3 exit
            block(vec![], branch(BlockId(1))),
            block(vec![], branch(BlockId(2))),
            block(
                vec![cmp(0x1, 0x1)],
                branch_cond(Condition::Eq, BlockId(1), BlockId(3)),
            ),
            block(vec![], ret()),
        ]);
        assert_wellformed(&text);
        assert!(text.contains("do {"), "{text}");
        assert!(text.contains("} while ((0x1u == 0x1u));"), "{text}");
        assert!(!text.contains("goto"), "{text}");
    }

    #[test]
    fn early_return() {
        let text = emitted(vec![
            // b0: cond ? b1 : b2; b1 returns; b2 falls to b3
            block(
                vec![cmp(0x7, 0x7)],
                branch_cond(Condition::Eq, BlockId(1), BlockId(2)),
            ),
            block(vec![], value_return(0x2a)),
            block(vec![], branch(BlockId(3))),
            block(vec![], value_return(0x0)),
        ]);
        assert_wellformed(&text);
        assert!(text.contains("if ((0x7u == 0x7u)) {"), "{text}");
        assert!(text.contains("return 0x2au;"), "{text}");
        assert!(!text.contains("goto"), "{text}");
    }

    #[test]
    fn switch_site_falls_back_to_marked_goto() {
        let text = emitted(vec![block(
            vec![],
            Terminator::IndirectExit {
                target: Value::Const(0x0800_0100),
                state: Vec::new(),
                switches_state: false,
                pc_mask: !3,
            },
        )]);
        assert_wellformed(&text);
        assert!(text.contains("/* unresolved indirect exit"), "{text}");
        assert!(text.contains("goto b_indirect;"), "{text}");
        assert!(text.contains("b_indirect:;"), "{text}");
    }

    #[test]
    fn irreducible_loop_stays_goto_form_and_is_complete() {
        // Two back edges into one header: not a shaped loop.
        let text = emitted(vec![
            block(
                vec![cmp(0x5, 0x0)],
                branch_cond(Condition::Ne, BlockId(1), BlockId(4)),
            ),
            block(
                vec![cmp(0x1, 0x1)],
                branch_cond(Condition::Eq, BlockId(2), BlockId(3)),
            ),
            block(vec![], branch(BlockId(0))),
            block(vec![], branch(BlockId(0))),
            block(vec![], ret()),
        ]);
        assert_wellformed(&text);
        assert!(!text.contains("while ("), "{text}");
        assert!(text.contains("goto"), "{text}");
        // Every block is emitted exactly once.
        assert_eq!(text.matches("flags_cmp(0x5u, 0x0u);").count(), 1, "{text}");
        assert_eq!(text.matches("flags_cmp(0x1u, 0x1u);").count(), 1, "{text}");
        assert_eq!(text.matches("return;").count(), 1, "{text}");
    }

    #[test]
    fn goto_fallback_keeps_every_edge() {
        // Shared then-target with a second predecessor into the fold
        // candidate: folds must not fire, gotos must carry the edges.
        let text = emitted(vec![
            // b0: ? b1 : b2; b3 also enters b1.
            block(
                vec![cmp(0x1, 0x0)],
                branch_cond(Condition::Ne, BlockId(1), BlockId(2)),
            ),
            block(vec![], branch(BlockId(4))),
            block(vec![], branch(BlockId(4))),
            block(vec![], branch(BlockId(1))),
            block(vec![], ret()),
        ]);
        assert_wellformed(&text);
        // b1 has two preds, so no fold or diamond may claim it: the
        // text must branch to it explicitly and label it.
        assert!(text.contains("goto b1;"), "{text}");
        assert!(text.contains("b1:;"), "{text}");
        assert_eq!(text.matches("b1:;").count(), 1, "{text}");
    }

    /// Lifts real encoded ARM words through the phase-1 lifter.
    fn lift_arm(words: &[u32], count: usize) -> Function {
        let mut bytes = vec![0u8; 256];
        for (index, word) in words.iter().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        let image = rom::Image::from_bytes(bytes).expect("parses");
        let mut map = analysis::FlagMap::from_image(&image);
        let base = map.canonical().base();
        map.mark_range(base, base + count as u32 * 4, analysis::Flags::EXEC);
        map.set(base, analysis::Flags::EXEC | analysis::Flags::FUNC_START);
        crate::lift::lift_function(
            &image,
            &map,
            crate::lift::FunctionRegion {
                start: base,
                end: base + count as u32 * 4,
                state: analysis::State::Arm,
            },
            &[],
        )
    }

    #[test]
    fn emitted_real_lifted_loop_is_complete() {
        // mov r0, #5; loop: cmp r0, #0; bne loop; bx lr
        let function = lift_arm(&[0xE3A0_0005, 0xE350_0000, 0x1AFF_FFFE, 0xE12F_FF1E], 4);
        let text = emit_function(&function);
        assert_wellformed(&text);
        assert!(text.contains("flags_cmp"), "{text}");
        assert!(text.contains("return"), "{text}");
        // The comparison is emitted exactly once (no duplication).
        assert_eq!(text.matches("flags_cmp").count(), 1, "{text}");
    }

    #[test]
    fn emitted_real_lifted_thumb_return_is_complete() {
        // mov r0, #5; bx lr
        let mut bytes = vec![0u8; 256];
        for (index, half) in [0x2005u16, 0x4770u16].iter().enumerate() {
            bytes[index * 2..index * 2 + 2].copy_from_slice(&half.to_le_bytes());
        }
        let image = rom::Image::from_bytes(bytes).expect("parses");
        let mut map = analysis::FlagMap::from_image(&image);
        let base = map.canonical().base();
        map.mark_range(base, base + 4, analysis::Flags::EXEC);
        map.set(
            base,
            analysis::Flags::EXEC | analysis::Flags::FUNC_START | analysis::Flags::THUMB,
        );
        let function = crate::lift::lift_function(
            &image,
            &map,
            crate::lift::FunctionRegion {
                start: base,
                end: base + 4,
                state: analysis::State::Thumb,
            },
            &[],
        );
        let text = emit_function(&function);
        assert_wellformed(&text);
        assert!(text.contains("return v40;"), "{text}");
    }

    #[test]
    fn annotation_comments_render_io_and_swi_names() {
        let store_dispcnt = inst(
            Vec::new(),
            Op::Store {
                addr: Value::Const(0x0400_0000),
                value: Value::Const(0x0400),
                size: crate::Size::Word,
            },
        );
        let read_keys = inst(
            vec![ValueId(0)],
            Op::Load {
                addr: Value::Const(0x0400_0130),
                size: crate::Size::Half,
                signed: false,
                mode: crate::LoadMode::Ldr,
            },
        );
        let software_interrupt = inst(
            Vec::new(),
            Op::Intrinsic {
                name: "swi",
                args: vec![Value::Const(0x0006_0000)],
            },
        );
        let text = emitted(vec![
            block(
                vec![store_dispcnt, read_keys, software_interrupt],
                branch(BlockId(1)),
            ),
            block(Vec::new(), ret()),
        ]);
        assert!(
            text.contains("mem_u32(0x4000000u, 0x400u); /* DISPCNT */"),
            "{text}"
        );
        assert!(text.contains("/* KEYINPUT */"), "{text}");
        assert!(
            text.contains("swi(0x60000u); /* swi 0x06: Div */"),
            "{text}"
        );
    }

    #[test]
    fn unit_form_names_functions_and_renders_signatures() {
        // mov r0, #5; loop: cmp r0, #0; bne loop; bx lr
        let mut callee = lift_arm(&[0xE3A0_0005, 0xE350_0000, 0x1AFF_FFFE, 0xE12F_FF1E], 4);
        let base = callee.blocks[0].addr;
        callee.name = "count_down".to_string();
        let caller = Function {
            name: "caller_fn".to_string(),
            thumb: false,
            blocks: vec![block(
                Vec::new(),
                Terminator::Call {
                    target: Value::Const(base),
                    cont: None,
                    state: vec![Value::Const(0); crate::STATE_ARITY],
                },
            )],
        };
        let mut annotations = Annotations::default();
        annotations.names.insert(base, "count_down".to_string());
        let text = emit_unit(&[base], &[caller, callee], &annotations);
        assert!(
            text.contains("u32 count_down(u32, u32, u32, u32);"),
            "{text}"
        );
        assert!(
            text.contains("u32 caller_fn(u32, u32, u32, u32);"),
            "{text}"
        );
        assert!(text.contains("count_down("), "{text}");
        assert!(text.contains("/* sig:"), "{text}");
        assert!(!text.contains("/* /*"), "{text}");
        assert_wellformed(&text);
    }

    #[test]
    fn unit_form_passes_gcc_syntax_when_available() {
        // mov r0, #5; loop: cmp r0, #0; bne loop; bx lr
        let mut callee = lift_arm(&[0xE3A0_0005, 0xE350_0000, 0x1AFF_FFFE, 0xE12F_FF1E], 4);
        let base = callee.blocks[0].addr;
        callee.name = "count_down".to_string();
        let caller = Function {
            name: "caller_fn".to_string(),
            thumb: false,
            blocks: vec![block(
                Vec::new(),
                Terminator::Call {
                    target: Value::Const(base),
                    cont: None,
                    state: vec![Value::Const(0); crate::STATE_ARITY],
                },
            )],
        };
        let text = emit_unit(&[base], &[caller, callee], &Annotations::default());
        let Ok(check) = std::process::Command::new("gcc").arg("--version").output() else {
            eprintln!("skipping: gcc not runnable");
            return;
        };
        assert!(check.status.success(), "gcc --version failed");
        let path = std::env::temp_dir().join(format!("oxyboy_unit_{}.c", std::process::id()));
        std::fs::write(&path, &text).expect("writes temp unit");
        let result = std::process::Command::new("gcc")
            .arg("-fsyntax-only")
            .arg(&path)
            .output();
        let _ = std::fs::remove_file(&path);
        match result {
            Ok(output) => {
                assert!(
                    output.status.success(),
                    "gcc rejected the unit: {}\n{text}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(e) => eprintln!("skipping: gcc not runnable: {e}"),
        }
    }
}
