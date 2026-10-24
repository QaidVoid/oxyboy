//! Stable pretty-printer for IR functions.
//!
//! Output is deterministic for a given [`Function`]: value ids print as
//! `vN`, block ids as `bN`, and the machine state slots are named on
//! the entry block. The golden test pins the exact shape.

use crate::{
    BlockId, Flag, Function, Inst, LoadMode, Op, Reg, SLOTS, Size, Slot, Terminator, Value, ValueId,
};

/// Renders `function` in the stable text form.
#[must_use]
pub fn fmt_function(function: &Function) -> String {
    let mut out = String::new();
    out.push_str("fn ");
    out.push_str(&function.name);
    out.push('(');
    for (index, param) in function
        .blocks
        .first()
        .map_or(&[] as &[ValueId], |b| &b.params)
        .iter()
        .enumerate()
    {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("v{}: {}", param.0, slot_name(SLOTS[index])));
    }
    out.push_str(") {\n");
    for (index, block) in function.blocks.iter().enumerate() {
        out.push_str(&fmt_block(BlockId(index as u32), block));
    }
    out.push_str("}\n");
    out
}

/// Renders one block.
fn fmt_block(id: BlockId, block: &crate::Block) -> String {
    let mut out = String::new();
    out.push_str(&format!("b{}(", id.0));
    for (index, param) in block.params.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("v{}", param.0));
    }
    out.push_str("):\n");
    for inst in &block.insts {
        out.push_str(&fmt_inst(inst));
    }
    out.push_str(&fmt_term(&block.term));
    out
}

/// Renders one instruction.
fn fmt_inst(inst: &Inst) -> String {
    let mut out = String::new();
    if let Some(cond) = inst.guard {
        out.push_str(&format!("{}? ", cond.mnemonic()));
    }
    for (index, dest) in inst.dests.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("v{}", dest.0));
    }
    if !inst.dests.is_empty() {
        out.push_str(" = ");
    }
    out.push_str(&fmt_op(&inst.op));
    out.push('\n');
    out
}

/// Renders one operand.
fn fmt_value(value: &Value) -> String {
    match value {
        Value::Id(id) => format!("v{}", id.0),
        Value::Const(word) => format!("#{word:#x}"),
    }
}

/// Renders the operation with its mnemonic and operands.
fn fmt_op(op: &Op) -> String {
    let suffix = |set_flags: bool| if set_flags { "s" } else { "" };
    match op {
        Op::Add { set_flags, a, b } => {
            format!(
                "add{} {}, {}",
                suffix(*set_flags),
                fmt_value(a),
                fmt_value(b)
            )
        }
        Op::Adc {
            set_flags,
            a,
            b,
            carry_in,
        } => format!(
            "adc{} {}, {}, c={}",
            suffix(*set_flags),
            fmt_value(a),
            fmt_value(b),
            fmt_value(carry_in)
        ),
        Op::Sub { set_flags, a, b } => {
            format!(
                "sub{} {}, {}",
                suffix(*set_flags),
                fmt_value(a),
                fmt_value(b)
            )
        }
        Op::Sbc {
            set_flags,
            a,
            b,
            carry_in,
        } => format!(
            "sbc{} {}, {}, c={}",
            suffix(*set_flags),
            fmt_value(a),
            fmt_value(b),
            fmt_value(carry_in)
        ),
        Op::Rsc {
            set_flags,
            a,
            b,
            carry_in,
        } => format!(
            "rsc{} {}, {}, c={}",
            suffix(*set_flags),
            fmt_value(a),
            fmt_value(b),
            fmt_value(carry_in)
        ),
        Op::MultiplyLong {
            unsigned,
            accumulate,
            set_flags,
            a,
            b,
            acc_lo,
            acc_hi,
        } => {
            let mnemonic = match (unsigned, accumulate) {
                (false, false) => "smull",
                (false, true) => "smlal",
                (true, false) => "umull",
                (true, true) => "umlal",
            };
            if *accumulate {
                format!(
                    "{}{} {}, {}, acc=({}, {})",
                    mnemonic,
                    suffix(*set_flags),
                    fmt_value(a),
                    fmt_value(b),
                    fmt_value(acc_lo),
                    fmt_value(acc_hi)
                )
            } else {
                format!(
                    "{}{} {}, {}",
                    mnemonic,
                    suffix(*set_flags),
                    fmt_value(a),
                    fmt_value(b)
                )
            }
        }
        Op::Rsb { set_flags, a, b } => {
            format!(
                "rsb{} {}, {}",
                suffix(*set_flags),
                fmt_value(a),
                fmt_value(b)
            )
        }
        Op::And {
            set_flags,
            a,
            b,
            carry_in,
        }
        | Op::Orr {
            set_flags,
            a,
            b,
            carry_in,
        }
        | Op::Eor {
            set_flags,
            a,
            b,
            carry_in,
        }
        | Op::Bic {
            set_flags,
            a,
            b,
            carry_in,
        } => {
            let mnemonic = match op {
                Op::And { .. } => "and",
                Op::Orr { .. } => "orr",
                Op::Eor { .. } => "eor",
                _ => "bic",
            };
            format!(
                "{}{} {}, {}{}",
                mnemonic,
                suffix(*set_flags),
                fmt_value(a),
                fmt_value(b),
                carry_suffix(*set_flags, carry_in)
            )
        }
        Op::Mov {
            set_flags,
            a,
            carry_in,
        }
        | Op::Mvn {
            set_flags,
            a,
            carry_in,
        } => {
            let mnemonic = match op {
                Op::Mov { .. } => "mov",
                _ => "mvn",
            };
            format!(
                "{}{} {}{}",
                mnemonic,
                suffix(*set_flags),
                fmt_value(a),
                carry_suffix(*set_flags, carry_in)
            )
        }
        Op::Mul { set_flags, a, b } => {
            format!(
                "mul{} {}, {}",
                suffix(*set_flags),
                fmt_value(a),
                fmt_value(b)
            )
        }
        Op::Cmp { a, b } => format!("cmp {}, {}", fmt_value(a), fmt_value(b)),
        Op::Cmn { a, b } => format!("cmn {}, {}", fmt_value(a), fmt_value(b)),
        Op::Tst { a, b, carry_in } | Op::Teq { a, b, carry_in } => {
            let mnemonic = match op {
                Op::Tst { .. } => "tst",
                _ => "teq",
            };
            format!(
                "{} {}, {}{}",
                mnemonic,
                fmt_value(a),
                fmt_value(b),
                carry_suffix(true, carry_in)
            )
        }
        Op::Lsl {
            set_flags,
            a,
            amount,
            carry_in,
        }
        | Op::Lsr {
            set_flags,
            a,
            amount,
            carry_in,
        }
        | Op::Asr {
            set_flags,
            a,
            amount,
            carry_in,
        }
        | Op::Ror {
            set_flags,
            a,
            amount,
            carry_in,
        } => {
            let mnemonic = match op {
                Op::Lsl { .. } => "lsl",
                Op::Lsr { .. } => "lsr",
                Op::Asr { .. } => "asr",
                _ => "ror",
            };
            format!(
                "{}{} {}, {}{}",
                mnemonic,
                suffix(*set_flags),
                fmt_value(a),
                fmt_value(amount),
                carry_suffix(true, carry_in)
            )
        }
        Op::Rrx {
            set_flags,
            a,
            carry_in,
        } => format!(
            "rrx{} {}, c={}",
            suffix(*set_flags),
            fmt_value(a),
            fmt_value(carry_in)
        ),
        Op::Load {
            addr,
            size,
            signed,
            mode,
        } => {
            let raw = match mode {
                LoadMode::Ldr => "",
                LoadMode::Swap => ".s",
                LoadMode::Block => ".b",
            };
            format!(
                "{}{} [{}]",
                load_mnemonic(*size, *signed),
                raw,
                fmt_value(addr)
            )
        }
        Op::Store { addr, value, size } => format!(
            "{} [{}], {}",
            store_mnemonic(*size),
            fmt_value(addr),
            fmt_value(value)
        ),
        Op::Intrinsic { name, args } => {
            let list = args.iter().map(fmt_value).collect::<Vec<_>>().join(", ");
            format!("intrinsic \"{name}\" {list}")
        }
    }
}

/// The ` c=...` suffix for ops whose carry is a transparent pass-through.
fn carry_suffix(set_flags: bool, carry_in: &Value) -> String {
    if set_flags {
        format!(", c={}", fmt_value(carry_in))
    } else {
        String::new()
    }
}

/// The load mnemonic for one access width.
fn load_mnemonic(size: Size, signed: bool) -> &'static str {
    match (size, signed) {
        (Size::Byte, false) => "ldrb",
        (Size::Byte, true) => "ldrsb",
        (Size::Half, false) => "ldrh",
        (Size::Half, true) => "ldrsh",
        (Size::Word, _) => "ldr",
    }
}

/// The store mnemonic for one access width.
fn store_mnemonic(size: Size) -> &'static str {
    match size {
        Size::Byte => "strb",
        Size::Half => "strh",
        Size::Word => "str",
    }
}

/// Renders the terminator.
fn fmt_term(term: &Terminator) -> String {
    match term {
        Terminator::Branch { target, args } => {
            format!("br {}\n", fmt_block_ref(*target, args))
        }
        Terminator::BranchCond {
            cond,
            then_target,
            then_args,
            else_target,
            else_args,
        } => format!(
            "br.{} {}, else {}\n",
            cond.mnemonic(),
            fmt_block_ref(*then_target, then_args),
            fmt_block_ref(*else_target, else_args)
        ),
        Terminator::Call {
            target,
            cont,
            state,
        } => match cont {
            Some((block, args)) => format!(
                "bl {}, cont {}, state {}\n",
                fmt_value(target),
                fmt_block_ref(*block, args),
                fmt_state(state)
            ),
            None => format!("bl {}, state {}\n", fmt_value(target), fmt_state(state)),
        },
        Terminator::Return {
            state,
            switches_state: _,
        } => format!("ret {}\n", fmt_state(state)),
        Terminator::IndirectExit {
            target,
            state,
            switches_state: _,
            pc_mask: _,
        } => format!("bx {}, state {}\n", fmt_value(target), fmt_state(state)),
        Terminator::Unreachable => "unreachable\n".to_string(),
    }
}

/// Renders `bN(args)`.
fn fmt_block_ref(target: BlockId, args: &[Value]) -> String {
    let list = args.iter().map(fmt_value).collect::<Vec<_>>().join(", ");
    format!("b{}({list})", target.0)
}

/// Renders an outgoing state list as `r0=v0, r1=v1, ...`.
fn fmt_state(state: &[Value]) -> String {
    state
        .iter()
        .zip(SLOTS)
        .map(|(value, slot)| format!("{}={}", slot_name(slot), fmt_value(value)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The stable name of one state slot.
fn slot_name(slot: Slot) -> &'static str {
    match slot {
        Slot::Reg(reg) => reg_name(&reg),
        Slot::Flag(flag) => flag_name(&flag),
    }
}

/// The stable name of one register.
fn reg_name(reg: &Reg) -> &'static str {
    match reg {
        Reg::R0 => "r0",
        Reg::R1 => "r1",
        Reg::R2 => "r2",
        Reg::R3 => "r3",
        Reg::R4 => "r4",
        Reg::R5 => "r5",
        Reg::R6 => "r6",
        Reg::R7 => "r7",
        Reg::R8 => "r8",
        Reg::R9 => "r9",
        Reg::R10 => "r10",
        Reg::R11 => "r11",
        Reg::R12 => "r12",
        Reg::Sp => "sp",
        Reg::Lr => "lr",
        Reg::Pc => "pc",
    }
}

/// The stable name of one flag.
fn flag_name(flag: &Flag) -> &'static str {
    match flag {
        Flag::N => "n",
        Flag::Z => "z",
        Flag::C => "c",
        Flag::V => "v",
    }
}

/// A sample function exercising guarded effects, flag defs, memory ops
/// with computed addresses, a call, and an indirect exit; the golden
/// test pins its printed form.
#[cfg(test)]
pub(crate) fn sample_function() -> Function {
    use crate::{Block, Inst, Op, Terminator, Value, ValueId};
    use decode::Condition;
    let entry: Vec<ValueId> = (0..20).map(ValueId).collect();
    let reg = |index: u32| Value::Id(ValueId(index));
    Function {
        name: "sample".to_string(),
        blocks: vec![
            Block {
                addr: 0x100,
                params: entry.clone(),
                insts: vec![
                    Inst {
                        guard: None,
                        dests: vec![ValueId(20)],
                        op: Op::Add {
                            set_flags: false,
                            a: reg(0),
                            b: Value::Const(4),
                        },
                        pc: 0x100,
                        state: Vec::new(),
                    },
                    Inst {
                        guard: None,
                        dests: vec![
                            ValueId(21),
                            ValueId(22),
                            ValueId(23),
                            ValueId(24),
                            ValueId(25),
                        ],
                        op: Op::Add {
                            set_flags: true,
                            a: reg(20),
                            b: reg(1),
                        },
                        pc: 0x104,
                        state: Vec::new(),
                    },
                    Inst {
                        guard: Some(Condition::Ne),
                        dests: vec![],
                        op: Op::Store {
                            addr: reg(20),
                            value: reg(2),
                            size: crate::Size::Word,
                        },
                        pc: 0x108,
                        state: Vec::new(),
                    },
                    Inst {
                        guard: None,
                        dests: vec![ValueId(26)],
                        op: Op::Load {
                            addr: reg(20),
                            size: crate::Size::Half,
                            signed: true,
                            mode: crate::LoadMode::Ldr,
                        },
                        pc: 0x10C,
                        state: Vec::new(),
                    },
                ],
                term: Terminator::BranchCond {
                    cond: Condition::Cs,
                    then_target: BlockId(1),
                    then_args: vec![reg(21), reg(26)],
                    else_target: BlockId(3),
                    else_args: vec![reg(21)],
                },
            },
            Block {
                addr: 0x120,
                params: vec![ValueId(27), ValueId(28)],
                insts: vec![],
                term: Terminator::Call {
                    target: reg(3),
                    cont: Some((BlockId(2), vec![reg(27)])),
                    state: (0..16)
                        .map(|slot| if slot == 14 { reg(28) } else { reg(slot) })
                        .chain([reg(22), reg(23), reg(24), reg(25)])
                        .collect(),
                },
            },
            Block {
                addr: 0x130,
                params: vec![ValueId(29)],
                insts: vec![Inst {
                    guard: None,
                    dests: vec![ValueId(30), ValueId(31)],
                    op: Op::Rrx {
                        set_flags: true,
                        a: reg(29),
                        carry_in: reg(24),
                    },
                    pc: 0x130,
                    state: Vec::new(),
                }],
                term: Terminator::IndirectExit {
                    target: reg(15),
                    state: (0..16)
                        .map(reg)
                        .chain([reg(22), reg(23), reg(24), reg(25)])
                        .collect(),
                    switches_state: true,
                    pc_mask: !1,
                },
            },
            Block {
                addr: 0x140,
                params: vec![ValueId(32)],
                insts: vec![],
                term: Terminator::Return {
                    state: (0..16)
                        .map(|slot| if slot == 0 { reg(32) } else { reg(slot) })
                        .chain([reg(22), reg(23), reg(24), reg(25)])
                        .collect(),
                    switches_state: false,
                },
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_prints_stably() {
        let expected = "fn sample(v0: r0, v1: r1, v2: r2, v3: r3, v4: r4, v5: r5, v6: r6, \
v7: r7, v8: r8, v9: r9, v10: r10, v11: r11, v12: r12, v13: sp, v14: lr, v15: pc, \
v16: n, v17: z, v18: c, v19: v) {\n\
b0(v0, v1, v2, v3, v4, v5, v6, v7, v8, v9, v10, v11, v12, v13, v14, v15, v16, v17, v18, v19):\n\
  v20 = add v0, #0x4\n\
  v21, v22, v23, v24, v25 = adds v20, v1\n\
  ne? str [v20], v2\n\
  v26 = ldrsh [v20]\n\
br.cs b1(v21, v26), else b3(v21)\n\
b1(v27, v28):\n\
  bl v3, cont b2(v27), state r0=v0, r1=v1, r2=v2, r3=v3, r4=v4, r5=v5, r6=v6, r7=v7, r8=v8, r9=v9, r10=v10, r11=v11, r12=v12, sp=v13, lr=v28, pc=v15, n=v22, z=v23, c=v24, v=v25\n\
b2(v29):\n\
  v30, v31 = rrxs v29, c=v24\n\
  bx v15, state r0=v0, r1=v1, r2=v2, r3=v3, r4=v4, r5=v5, r6=v6, r7=v7, r8=v8, r9=v9, r10=v10, r11=v11, r12=v12, sp=v13, lr=v14, pc=v15, n=v22, z=v23, c=v24, v=v25\n\
b3(v32):\n\
  ret r0=v32, r1=v1, r2=v2, r3=v3, r4=v4, r5=v5, r6=v6, r7=v7, r8=v8, r9=v9, \
r10=v10, r11=v11, r12=v12, sp=v13, lr=v14, pc=v15, n=v22, z=v23, c=v24, v=v25\n\
}\n";
        assert_eq!(fmt_function(&sample_function()), expected);
    }

    #[test]
    fn printing_twice_is_identical() {
        let once = fmt_function(&sample_function());
        let twice = fmt_function(&sample_function());
        assert_eq!(once, twice);
    }
}
