//! Intermediate representation with ARM and THUMB lifters.
//!
//! The IR is block-argument SSA over a fixed twenty-slot machine state
//! (sixteen registers plus individual N/Z/C/V flags). Values compute
//! once; memory is explicit loads and stores with computed addresses;
//! ARM conditions are per-instruction guards; unresolved indirect
//! control flow is an explicit terminator.

pub mod arm;
pub mod eval;
pub mod lift;
pub mod opt;
pub mod print;
pub mod thumb;

use decode::Condition;

/// A computed value: exactly one assignment somewhere in the function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValueId(pub u32);

/// An operand: an SSA value or a lift-time constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    /// A computed value named by its SSA id.
    Id(ValueId),
    /// A constant baked in at lift time (shifted immediates, PC bases
    /// lowered per the design's PC rule, literal pool addresses).
    Const(u32),
}

/// Memory access width of a load or store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    /// One byte.
    Byte,
    /// Two bytes, little-endian.
    Half,
    /// Four bytes, little-endian.
    Word,
}

/// Word-read flavor of a load, per the interpreter's paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadMode {
    /// LDR: read the aligned word, rotate the byte lanes by the
    /// address offset (ARM ARM A2.4.2, GBATEK "Mis-aligned").
    Ldr,
    /// SWP: read from the raw address, then rotate by the offset.
    Swap,
    /// Block transfers and THUMB pop: read raw with no rotation.
    Block,
}

/// Architectural registers in the state slot list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reg {
    /// r0 through r12, general purpose.
    R0,
    R1,
    R2,
    R3,
    R4,
    R5,
    R6,
    R7,
    R8,
    R9,
    R10,
    R11,
    R12,
    /// r13, stack pointer.
    Sp,
    /// r14, link register.
    Lr,
    /// r15, program counter.
    Pc,
}

/// Individual condition flags in the state slot list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flag {
    /// Negative.
    N,
    /// Zero.
    Z,
    /// Carry.
    C,
    /// Overflow.
    V,
}

/// One machine state slot: the entry block's parameters and every
/// `return`'s arguments follow [`SLOTS`] in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// A register.
    Reg(Reg),
    /// A flag.
    Flag(Flag),
}

/// The fixed machine state slot list: `r0..r12, sp, lr, pc, n, z, c, v`.
pub const SLOTS: [Slot; 20] = [
    Slot::Reg(Reg::R0),
    Slot::Reg(Reg::R1),
    Slot::Reg(Reg::R2),
    Slot::Reg(Reg::R3),
    Slot::Reg(Reg::R4),
    Slot::Reg(Reg::R5),
    Slot::Reg(Reg::R6),
    Slot::Reg(Reg::R7),
    Slot::Reg(Reg::R8),
    Slot::Reg(Reg::R9),
    Slot::Reg(Reg::R10),
    Slot::Reg(Reg::R11),
    Slot::Reg(Reg::R12),
    Slot::Reg(Reg::Sp),
    Slot::Reg(Reg::Lr),
    Slot::Reg(Reg::Pc),
    Slot::Flag(Flag::N),
    Slot::Flag(Flag::Z),
    Slot::Flag(Flag::C),
    Slot::Flag(Flag::V),
];

/// The number of state slots: the entry block's parameter count and
/// the argument count of every `return`.
pub const STATE_ARITY: usize = SLOTS.len();

/// One operation. The `dests` list length is fixed per variant and
/// documented here; the differential evaluator relies on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// `v = a + b`, or with flags `[v, n, z, c, v]`.
    Add { set_flags: bool, a: Value, b: Value },
    /// `v = a + b + carry_in`, flags as `Add`.
    Adc {
        set_flags: bool,
        a: Value,
        b: Value,
        carry_in: Value,
    },
    /// `v = a - b`, or with flags `[v, n, z, c, v]`.
    Sub { set_flags: bool, a: Value, b: Value },
    /// `v = a - b - (1 - carry_in)`, flags as `Sub`.
    Sbc {
        set_flags: bool,
        a: Value,
        b: Value,
        carry_in: Value,
    },
    /// `v = b - a - (1 - carry_in)` (reverse subtract with carry),
    /// flags as `Sub`.
    Rsc {
        set_flags: bool,
        a: Value,
        b: Value,
        carry_in: Value,
    },
    /// `v = b - a` (reverse subtract), flags as `Sub`.
    Rsb { set_flags: bool, a: Value, b: Value },
    /// `v = a & b`, or with flags `[v, n, z, c]` where `c` is the
    /// shifter carry, passed in explicitly as `carry_in`.
    And {
        set_flags: bool,
        a: Value,
        b: Value,
        carry_in: Value,
    },
    /// `v = a | b`, flags as `And`.
    Orr {
        set_flags: bool,
        a: Value,
        b: Value,
        carry_in: Value,
    },
    /// `v = a ^ b`, flags as `And`.
    Eor {
        set_flags: bool,
        a: Value,
        b: Value,
        carry_in: Value,
    },
    /// `v = a & !b`, flags as `And`.
    Bic {
        set_flags: bool,
        a: Value,
        b: Value,
        carry_in: Value,
    },
    /// `v = a`, or with flags `[v, n, z, c]` (`c = carry_in`).
    Mov {
        set_flags: bool,
        a: Value,
        carry_in: Value,
    },
    /// `v = !a`, flags as `Mov`.
    Mvn {
        set_flags: bool,
        a: Value,
        carry_in: Value,
    },
    /// `v = (a * b) & 0xffffffff`, or with flags `[v, n, z]` (C is
    /// architecturally unpredictable on ARM7TDMI and not produced).
    Mul { set_flags: bool, a: Value, b: Value },
    /// 64-bit multiply: `[lo, hi] = a * b` (sign per `unsigned`),
    /// `acc_hi:acc_lo` accumulated into the 64-bit result when
    /// `accumulate` (the pre-instruction RdLo/RdHi). With flags:
    /// `[lo, hi, n, z]` (C, V unpredictable and not produced).
    MultiplyLong {
        unsigned: bool,
        accumulate: bool,
        set_flags: bool,
        a: Value,
        b: Value,
        acc_lo: Value,
        acc_hi: Value,
    },
    /// Comparison `a - b`, discarding the result: `[n, z, c, v]`.
    Cmp { a: Value, b: Value },
    /// Comparison `a + b`, discarding the result: `[n, z, c, v]`.
    Cmn { a: Value, b: Value },
    /// Test `a & b`, discarding the result: `[n, z, c]` with `c` the
    /// shifter carry (`carry_in`).
    Tst { a: Value, b: Value, carry_in: Value },
    /// Test `a ^ b`, discarding the result: `[n, z, c]` as `Tst`.
    Teq { a: Value, b: Value, carry_in: Value },
    /// `v = a << amount`, or with flags `[v, c]` where `c` follows
    /// ARM ARM A2.4: amount 0 keeps the carry, passed as `carry_in`.
    Lsl {
        set_flags: bool,
        a: Value,
        amount: Value,
        carry_in: Value,
    },
    /// `v = a >> amount` (logical), flags as `Lsl`.
    Lsr {
        set_flags: bool,
        a: Value,
        amount: Value,
        carry_in: Value,
    },
    /// `v = a >> amount` (arithmetic), flags as `Lsl`.
    Asr {
        set_flags: bool,
        a: Value,
        amount: Value,
        carry_in: Value,
    },
    /// `v = a rotate_right amount`, flags as `Lsl`.
    Ror {
        set_flags: bool,
        a: Value,
        amount: Value,
        carry_in: Value,
    },
    /// `v = {carry_in, a} rotate_right 1`, flags as `Lsl`.
    Rrx {
        set_flags: bool,
        a: Value,
        carry_in: Value,
    },
    /// `v = memory[addr]` at the access width, zero- or sign-extended
    /// to 32 bits per `signed`. Word reads differ by interpreter path:
    /// `Ldr` reads the aligned word and rotates the byte lanes, `Swap`
    /// reads from the raw address and rotates, `Block` reads raw with
    /// no rotation (LDM and THUMB pop).
    Load {
        addr: Value,
        size: Size,
        signed: bool,
        mode: LoadMode,
    },
    /// `memory[addr] = value` at the access width. No dests.
    Store {
        addr: Value,
        value: Value,
        size: Size,
    },
    /// An operation the IR does not model structurally (CPU-mode
    /// switches, banked register access, MRS/MSR). `dests` count is
    /// agreed per intrinsic name at the lifting site.
    Intrinsic {
        name: &'static str,
        args: Vec<Value>,
    },
}

/// One instruction: a guarded operation binding its dests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inst {
    /// ARM condition guarding the effect; `None` means always.
    pub guard: Option<Condition>,
    /// Values bound by the op, in the order the op documents.
    pub dests: Vec<ValueId>,
    /// The operation and its operands.
    pub op: Op,
    /// Canonical guest address of the source instruction. Effects that
    /// need a resume point when they abort mid-chunk (stores that hit
    /// translated code) read it at emit time.
    pub pc: u32,
    /// Full machine state right after this instruction, for effects
    /// that hand control back to the interpreter mid-chunk: the bail
    /// flushes it so the resumed interpreter sees the chunk's effects.
    /// Empty on instructions that never abort.
    pub state: Vec<Value>,
}

/// A block index into [`Function::blocks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u32);

/// How control leaves a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Terminator {
    /// Jump to `target`, passing `args` to its parameters.
    Branch { target: BlockId, args: Vec<Value> },
    /// ARM-style conditional branch to two targets; the guard is the
    /// terminator itself, so both argument lists are explicit.
    BranchCond {
        cond: Condition,
        then_target: BlockId,
        then_args: Vec<Value>,
        else_target: BlockId,
        else_args: Vec<Value>,
    },
    /// Call `target` and, when known, fall through into `cont` with
    /// the caller state.
    /// `state` is the pre-call machine state in [`SLOTS`] order: it
    /// becomes visible when the budget expires before the callee runs
    /// or when the call is a tail call (`cont` is [`None`]).
    Call {
        target: Value,
        cont: Option<(BlockId, Vec<Value>)>,
        state: Vec<Value>,
    },
    /// Leave the function; the arguments are the outgoing machine
    /// state in [`SLOTS`] order.
    Return { state: Vec<Value> },
    /// Control leaves to an unresolved value (`bx` on an unknown
    /// register, an unresolved switch arm). `state` is the outgoing
    /// machine state in [`SLOTS`] order, observable across direct
    /// calls between translated functions.
    IndirectExit { target: Value, state: Vec<Value> },
    /// Control never reaches here.
    Unreachable,
}

/// One basic block: parameters, ordered instructions, one terminator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// The block leader's guest address: the first instruction's
    /// address. Exit helpers carry the address control leaves to.
    pub addr: u32,
    /// Values the block receives from its predecessors' arguments.
    pub params: Vec<ValueId>,
    /// Instructions in address order within the block.
    pub insts: Vec<Inst>,
    /// How control leaves the block.
    pub term: Terminator,
}

/// A lifted function: the entry block's parameters are the incoming
/// machine state ([`SLOTS`] order) and every [`Terminator::Return`],
/// [`Terminator::IndirectExit`], and [`Terminator::Call`] carries the
/// outgoing (or pre-call) machine state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    /// Symbol name, matching the emitter's naming.
    pub name: String,
    /// Basic blocks; the entry block is index 0 and its parameter list
    /// is the incoming machine state.
    pub blocks: Vec<Block>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_wiring() {
        assert_eq!(env!("CARGO_PKG_NAME"), "ir");
    }

    #[test]
    fn state_slots_are_registers_then_flags() {
        assert_eq!(SLOTS[0], Slot::Reg(Reg::R0));
        assert_eq!(SLOTS[15], Slot::Reg(Reg::Pc));
        assert_eq!(SLOTS[16], Slot::Flag(Flag::N));
        assert_eq!(SLOTS[19], Slot::Flag(Flag::V));
        assert_eq!(STATE_ARITY, 20);
    }
}
