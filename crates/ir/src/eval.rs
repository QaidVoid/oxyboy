//! Mini IR evaluator: executes lifted instruction sequences over an
//! explicit machine state with ARMv4T semantics.
//!
//! This is the second, independent implementation the IR
//! differential harness compares against the interpreter: the flag
//! formulas and memory quirks here come from the same ARM ARM sections
//! the interpreter cites, and the harness asserts the two agree on
//! full state deltas.
//!
//! The evaluator tracks which SSA value currently represents each of
//! the twenty machine slots, mirroring the lifter: every op's dest
//! list is fixed by contract (the design doc's flag tables), so a
//! set-flags `Add` binds `[v, n, z, c, v]` and the C slot moves to
//! dest 3. Flag values are 0 or 1.

use decode::Condition;

use crate::{Inst, LoadMode, Op, Size, Value, ValueId};

/// Slot count of the machine state: 16 registers then N, Z, C, V.
pub const STATE_SLOTS: usize = 20;

/// Slot indexes into the flag tail.
const N_SLOT: usize = 16;
const Z_SLOT: usize = 17;
const C_SLOT: usize = 18;
const V_SLOT: usize = 19;

/// The memory the evaluator reads and writes: cart ROM as data, EWRAM,
/// and IWRAM, mirroring the interpreter's bus behavior for those
/// regions (everything else reads as zero and discards writes).
pub struct EvalMemory {
    rom: Vec<u8>,
    ewram: Vec<u8>,
    iwram: Vec<u8>,
    rom_mask: u32,
}

impl EvalMemory {
    /// Builds memory over the cart image bytes.
    pub fn new(rom: Vec<u8>) -> Self {
        let len = rom.len().next_power_of_two() as u32;
        EvalMemory {
            rom,
            ewram: vec![0; 0x4_0000],
            iwram: vec![0; 0x8000],
            rom_mask: len.wrapping_sub(1),
        }
    }

    fn read_u8(&self, addr: u32) -> u8 {
        let a = addr & 0x0FFF_FFFF;
        match a >> 24 {
            0x02 => self.ewram[(a & 0x0003_FFFF) as usize],
            0x03 => self.iwram[(a & 0x0000_7FFF) as usize],
            0x08..=0x0D => {
                let off = a.wrapping_sub(0x0800_0000) & self.rom_mask;
                self.rom.get(off as usize).copied().unwrap_or(0)
            }
            _ => 0,
        }
    }

    fn write_u8(&mut self, addr: u32, value: u8) {
        let a = addr & 0x0FFF_FFFF;
        match a >> 24 {
            0x02 => self.ewram[(a & 0x0003_FFFF) as usize] = value,
            0x03 => self.iwram[(a & 0x0000_7FFF) as usize] = value,
            _ => {}
        }
    }

    /// A word load the way the interpreter's bus assembles it.
    fn read_u32(&self, addr: u32) -> u32 {
        u32::from(self.read_u8(addr))
            | (u32::from(self.read_u8(addr.wrapping_add(1))) << 8)
            | (u32::from(self.read_u8(addr.wrapping_add(2))) << 16)
            | (u32::from(self.read_u8(addr.wrapping_add(3))) << 24)
    }

    fn read_u16(&self, addr: u32) -> u16 {
        u16::from(self.read_u8(addr)) | (u16::from(self.read_u8(addr.wrapping_add(1))) << 8)
    }

    fn write_u32(&mut self, addr: u32, value: u32) {
        for i in 0..4 {
            self.write_u8(addr.wrapping_add(i), (value >> (i * 8)) as u8);
        }
    }

    /// One ARM7 load: word reads come in the interpreter's three
    /// flavors (`LoadMode`); halfword and byte loads follow the GBATEK
    /// "Mis-aligned LDRH/LDRSH" quirks.
    pub fn load(&self, addr: u32, size: Size, signed: bool, mode: LoadMode) -> u32 {
        match (size, signed) {
            (Size::Byte, false) => u32::from(self.read_u8(addr)),
            (Size::Byte, true) => self.read_u8(addr) as i8 as i32 as u32,
            (Size::Half, false) => {
                if addr & 1 == 1 {
                    u32::from(self.read_u16(addr & !1)).rotate_right(8)
                } else {
                    u32::from(self.read_u16(addr))
                }
            }
            (Size::Half, true) => {
                if addr & 1 == 1 {
                    self.read_u8(addr) as i8 as i32 as u32
                } else {
                    self.read_u16(addr) as i16 as i32 as u32
                }
            }
            (Size::Word, _) => match mode {
                LoadMode::Ldr => self.read_u32(addr & !3).rotate_right((addr & 3) * 8),
                LoadMode::Swap => self.read_u32(addr).rotate_right((addr & 3) * 8),
                LoadMode::Block => self.read_u32(addr),
            },
        }
    }

    /// One store; writes are plain regardless of alignment.
    pub fn store(&mut self, addr: u32, value: u32, size: Size) {
        match size {
            Size::Byte => self.write_u8(addr, value as u8),
            Size::Half => {
                self.write_u8(addr, value as u8);
                self.write_u8(addr.wrapping_add(1), (value >> 8) as u8);
            }
            Size::Word => self.write_u32(addr, value),
        }
    }
}

/// The machine state the evaluator threads through lifted ops.
pub struct Machine {
    values: Vec<u32>,
    /// Whether each value was produced by an executed op; a guard-
    /// failed inst leaves its dests cold, and slots stay on their
    /// previous representative instead.
    written: Vec<bool>,
    slot_ids: [ValueId; STATE_SLOTS],
    /// Constant overrides from an optimized end state: a folded slot
    /// representative is a `Const` argument, not an id.
    consts: [Option<u32>; STATE_SLOTS],
    memory: EvalMemory,
}

impl Machine {
    /// A machine with room for `value_count` SSA ids. Block params
    /// (ids `0..STATE_SLOTS`) start as the initial state; seed it
    /// with [`Machine::set_slot`].
    pub fn new(value_count: usize, rom: Vec<u8>) -> Self {
        let mut slot_ids = [ValueId(0); STATE_SLOTS];
        for (i, id) in slot_ids.iter_mut().enumerate() {
            *id = ValueId(i as u32);
        }
        Machine {
            values: vec![0; value_count],
            written: vec![false; value_count],
            slot_ids,
            consts: [None; STATE_SLOTS],
            memory: EvalMemory::new(rom),
        }
    }

    /// Binds the entry block's parameter list as the initial state:
    /// each machine slot's representative moves to the matching id.
    pub fn bind_params(&mut self, ids: &[ValueId]) {
        for (slot, id) in ids.iter().take(STATE_SLOTS).enumerate() {
            self.slot_ids[slot] = *id;
            self.consts[slot] = None;
        }
    }

    /// Seeds one machine slot (registers raw, flags 0 or 1).
    pub fn set_slot(&mut self, slot: usize, value: u32) {
        let id = self.slot_ids[slot];
        self.set_id(id, value);
    }

    /// The current value of one machine slot.
    pub fn slot(&self, slot: usize) -> u32 {
        if let Some(c) = self.consts[slot] {
            return c;
        }
        self.values[self.slot_ids[slot].0 as usize]
    }

    /// EWRAM access for harness setup and inspection.
    pub fn ewram_mut(&mut self) -> &mut [u8] {
        &mut self.memory.ewram
    }

    /// EWRAM read-back.
    pub fn ewram(&self) -> &[u8] {
        &self.memory.ewram
    }

    /// IWRAM access for harness setup and inspection.
    pub fn iwram_mut(&mut self) -> &mut [u8] {
        &mut self.memory.iwram
    }

    /// IWRAM read-back.
    pub fn iwram(&self) -> &[u8] {
        &self.memory.iwram
    }

    /// Evaluates one lifted instruction sequence in order. Guards
    /// gate each op on the current flags, exactly like the
    /// interpreter's condition check. `end_state` is the Return
    /// terminator's argument list: after the ops run, each machine
    /// slot's representative moves to the matching id.
    pub fn eval(&mut self, insts: &[Inst], end_state: &[Value]) {
        for inst in insts {
            if let Some(cond) = inst.guard
                && !self.condition_holds(cond)
            {
                continue;
            }
            self.eval_inst(inst);
        }
        for (slot, value) in end_state.iter().take(STATE_SLOTS).enumerate() {
            match value {
                Value::Id(id) => {
                    if self.written[id.0 as usize] {
                        self.slot_ids[slot] = *id;
                        self.consts[slot] = None;
                    }
                }
                // A folded representative: the slot holds a constant.
                Value::Const(c) => self.consts[slot] = Some(*c),
            }
        }
    }

    /// Whether `cond` holds, mirroring the interpreter's CPSR rules.
    pub fn condition_holds(&self, cond: Condition) -> bool {
        let n = self.slot(N_SLOT) != 0;
        let z = self.slot(Z_SLOT) != 0;
        let c = self.slot(C_SLOT) != 0;
        let v = self.slot(V_SLOT) != 0;
        match cond {
            Condition::Eq => z,
            Condition::Ne => !z,
            Condition::Cs => c,
            Condition::Cc => !c,
            Condition::Mi => n,
            Condition::Pl => !n,
            Condition::Vs => v,
            Condition::Vc => !v,
            Condition::Hi => c && !z,
            Condition::Ls => !c || z,
            Condition::Ge => n == v,
            Condition::Lt => n != v,
            Condition::Gt => !z && n == v,
            Condition::Le => z || n != v,
            Condition::Al => true,
        }
    }

    fn eval_inst(&mut self, inst: &Inst) {
        match &inst.op {
            Op::Add { set_flags, a, b } => {
                let (v, c, o) = add_with_carry(self.value(a), self.value(b), false);
                self.bind_arith(inst, v, *set_flags, c, o);
            }
            Op::Adc {
                set_flags,
                a,
                b,
                carry_in,
            } => {
                let cin = self.flag_value(carry_in);
                let (v, c, o) = add_with_carry(self.value(a), self.value(b), cin);
                self.bind_arith(inst, v, *set_flags, c, o);
            }
            Op::Sub { set_flags, a, b } => {
                let (v, c, o) = sub_with_borrow(self.value(a), self.value(b), false);
                self.bind_arith(inst, v, *set_flags, c, o);
            }
            Op::Sbc {
                set_flags,
                a,
                b,
                carry_in,
            } => {
                let cin = self.flag_value(carry_in);
                let (v, c, o) = sub_with_borrow(self.value(a), self.value(b), !cin);
                self.bind_arith(inst, v, *set_flags, c, o);
            }
            Op::Rsb { set_flags, a, b } => {
                let (v, c, o) = sub_with_borrow(self.value(b), self.value(a), false);
                self.bind_arith(inst, v, *set_flags, c, o);
            }
            Op::Rsc {
                set_flags,
                a,
                b,
                carry_in,
            } => {
                let cin = self.flag_value(carry_in);
                let (v, c, o) = sub_with_borrow(self.value(b), self.value(a), !cin);
                self.bind_arith(inst, v, *set_flags, c, o);
            }
            Op::And {
                set_flags,
                a,
                b,
                carry_in,
            } => {
                let v = self.value(a) & self.value(b);
                self.bind_logic(inst, v, *set_flags, carry_in);
            }
            Op::Orr {
                set_flags,
                a,
                b,
                carry_in,
            } => {
                let v = self.value(a) | self.value(b);
                self.bind_logic(inst, v, *set_flags, carry_in);
            }
            Op::Eor {
                set_flags,
                a,
                b,
                carry_in,
            } => {
                let v = self.value(a) ^ self.value(b);
                self.bind_logic(inst, v, *set_flags, carry_in);
            }
            Op::Bic {
                set_flags,
                a,
                b,
                carry_in,
            } => {
                let v = self.value(a) & !self.value(b);
                self.bind_logic(inst, v, *set_flags, carry_in);
            }
            Op::Mov {
                set_flags,
                a,
                carry_in,
            } => {
                let v = self.value(a);
                self.bind_logic(inst, v, *set_flags, carry_in);
            }
            Op::Mvn {
                set_flags,
                a,
                carry_in,
            } => {
                let v = !self.value(a);
                self.bind_logic(inst, v, *set_flags, carry_in);
            }
            Op::Mul { set_flags, a, b } => {
                let v = self.value(a).wrapping_mul(self.value(b));
                self.set_id(inst.dests[0], v);
                if *set_flags {
                    // C and V stay unchanged on ARMv4 (destroyed), as
                    // the interpreter models it. Contract `[v, n, z]`.
                    let d = &inst.dests;
                    self.set_slot_value(N_SLOT, d[1], u32::from(v & 0x8000_0000 != 0));
                    self.set_slot_value(Z_SLOT, d[2], u32::from(v == 0));
                }
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
                let (a, b) = (self.value(a), self.value(b));
                let mut prod: u64 = if *unsigned {
                    u64::from(a).wrapping_mul(u64::from(b))
                } else {
                    ((a as i32) as i64).wrapping_mul((b as i32) as i64) as u64
                };
                if *accumulate {
                    let acc = (u64::from(self.value(acc_hi)) << 32) | u64::from(self.value(acc_lo));
                    prod = acc.wrapping_add(prod);
                }
                self.set_id(inst.dests[0], prod as u32);
                self.set_id(inst.dests[1], (prod >> 32) as u32);
                if *set_flags {
                    let d = &inst.dests;
                    self.set_slot_value(N_SLOT, d[2], u32::from(prod >> 63 == 1));
                    self.set_slot_value(Z_SLOT, d[3], u32::from(prod == 0));
                }
            }
            Op::Cmp { a, b } => {
                let (v, c, o) = sub_with_borrow(self.value(a), self.value(b), false);
                let d = &inst.dests;
                self.set_slot_value(N_SLOT, d[0], u32::from(v & 0x8000_0000 != 0));
                self.set_slot_value(Z_SLOT, d[1], u32::from(v == 0));
                self.set_slot_value(C_SLOT, d[2], u32::from(c));
                self.set_slot_value(V_SLOT, d[3], u32::from(o));
            }
            Op::Cmn { a, b } => {
                let (v, c, o) = add_with_carry(self.value(a), self.value(b), false);
                let d = &inst.dests;
                self.set_slot_value(N_SLOT, d[0], u32::from(v & 0x8000_0000 != 0));
                self.set_slot_value(Z_SLOT, d[1], u32::from(v == 0));
                self.set_slot_value(C_SLOT, d[2], u32::from(c));
                self.set_slot_value(V_SLOT, d[3], u32::from(o));
            }
            Op::Tst { a, b, carry_in } | Op::Teq { a, b, carry_in } => {
                let v = match &inst.op {
                    Op::Tst { .. } => self.value(a) & self.value(b),
                    _ => self.value(a) ^ self.value(b),
                };
                let c = self.flag_value(carry_in);
                let d = &inst.dests;
                self.set_slot_value(N_SLOT, d[0], u32::from(v & 0x8000_0000 != 0));
                self.set_slot_value(Z_SLOT, d[1], u32::from(v == 0));
                self.set_slot_value(C_SLOT, d[2], u32::from(c));
            }
            Op::Lsl {
                set_flags: _,
                a,
                amount,
                carry_in,
            }
            | Op::Lsr {
                set_flags: _,
                a,
                amount,
                carry_in,
            }
            | Op::Asr {
                set_flags: _,
                a,
                amount,
                carry_in,
            }
            | Op::Ror {
                set_flags: _,
                a,
                amount,
                carry_in,
            } => {
                let kind = match &inst.op {
                    Op::Lsl { .. } => ShiftKind::Lsl,
                    Op::Lsr { .. } => ShiftKind::Lsr,
                    Op::Asr { .. } => ShiftKind::Asr,
                    _ => ShiftKind::Ror,
                };
                let (v, c) = self.apply_shift(
                    self.value(a),
                    // Register-controlled amounts are taken mod 256;
                    // constant amounts are already small.
                    self.value(amount) & 0xFF,
                    self.flag_value(carry_in),
                    kind,
                );
                // Length dispatch: 4 is the THUMB format-1 contract
                // `[v, n, z, c]`, 3 its carry-dead truncation, 2 the
                // operand-2 expansion `[v, c]`, 1 a value-only list
                // after dead-flag elimination.
                let d = &inst.dests;
                self.set_id(d[0], v);
                match d.len() {
                    4 => {
                        self.set_slot_value(N_SLOT, d[1], u32::from(v & 0x8000_0000 != 0));
                        self.set_slot_value(Z_SLOT, d[2], u32::from(v == 0));
                        self.set_slot_value(C_SLOT, d[3], u32::from(c));
                    }
                    3 => {
                        self.set_slot_value(N_SLOT, d[1], u32::from(v & 0x8000_0000 != 0));
                        self.set_slot_value(Z_SLOT, d[2], u32::from(v == 0));
                    }
                    2 => {
                        self.set_id(d[1], u32::from(c));
                    }
                    _ => {}
                }
            }
            Op::Rrx {
                set_flags: _,
                a,
                carry_in,
            } => {
                let a = self.value(a);
                let cin = self.flag_value(carry_in);
                let v = (a >> 1) | (u32::from(cin) << 31);
                self.set_id(inst.dests[0], v);
                if inst.dests.len() >= 2 {
                    self.set_id(inst.dests[1], u32::from(a & 1 != 0));
                }
            }
            Op::Load {
                addr,
                size,
                signed,
                mode,
            } => {
                let v = self.memory.load(self.value(addr), *size, *signed, *mode);
                self.set_id(inst.dests[0], v);
            }
            Op::Store { addr, value, size } => {
                let (addr, value) = (self.value(addr), self.value(value));
                self.memory.store(addr, value, *size);
            }
            Op::Intrinsic { name, args } => self.eval_intrinsic(name, args, inst),
        }
    }

    fn eval_intrinsic(&mut self, name: &str, args: &[Value], inst: &Inst) {
        match name {
            // CPSR read: only the flag tail exists in this model.
            "mrs-cpsr" => {
                let word = self.flags_word();
                self.set_id(inst.dests[0], word);
            }
            // Field mask bit 3 is the flag field (bits 31..28).
            "msr-cpsr" => {
                let value = self.value(&args[0]);
                let mask = self.value(&args[1]);
                if mask & 0b1000 != 0 {
                    self.set_flag(N_SLOT, value & 0x8000_0000 != 0);
                    self.set_flag(Z_SLOT, value & 0x4000_0000 != 0);
                    self.set_flag(C_SLOT, value & 0x2000_0000 != 0);
                    self.set_flag(V_SLOT, value & 0x1000_0000 != 0);
                }
            }
            // SWI and the SPSR forms have no data effect in this model.
            _ => {}
        }
    }

    fn flags_word(&self) -> u32 {
        (u32::from(self.slot(N_SLOT) != 0) << 31)
            | (u32::from(self.slot(Z_SLOT) != 0) << 30)
            | (u32::from(self.slot(C_SLOT) != 0) << 29)
            | (u32::from(self.slot(V_SLOT) != 0) << 28)
    }

    /// Arithmetic dest contract `[v, n, z, c, v]`, or the MUL-style
    /// `[v, n, z]` prefix when the op leaves C and V unchanged.
    fn bind_arith(&mut self, inst: &Inst, v: u32, set_flags: bool, c: bool, o: bool) {
        self.set_id(inst.dests[0], v);
        if set_flags {
            let d = &inst.dests;
            self.set_slot_value(N_SLOT, d[1], u32::from(v & 0x8000_0000 != 0));
            self.set_slot_value(Z_SLOT, d[2], u32::from(v == 0));
            if d.len() >= 4 {
                self.set_slot_value(C_SLOT, d[3], u32::from(c));
            }
            if d.len() >= 5 {
                self.set_slot_value(V_SLOT, d[4], u32::from(o));
            }
        }
    }

    /// Logic and move dest contract `[v, n, z, c]` with the shifter
    /// carry passed through explicitly.
    fn bind_logic(&mut self, inst: &Inst, v: u32, set_flags: bool, carry_in: &Value) {
        self.set_id(inst.dests[0], v);
        if set_flags {
            let c = self.flag_value(carry_in);
            let d = &inst.dests;
            self.set_slot_value(N_SLOT, d[1], u32::from(v & 0x8000_0000 != 0));
            self.set_slot_value(Z_SLOT, d[2], u32::from(v == 0));
            self.set_slot_value(C_SLOT, d[3], u32::from(c));
        }
    }

    /// Shift with ARM7 carry rules (the interpreter's `apply_shift`).
    fn apply_shift(&self, value: u32, amount: u32, cin: bool, kind: ShiftKind) -> (u32, bool) {
        apply_shift(value, amount, cin, kind)
    }
}

/// The ARM7 barrel-shifter result, mirroring the interpreter's
/// `apply_shift` (cpu.rs `apply_shift`).
pub(crate) fn apply_shift(value: u32, amount: u32, cin: bool, kind: ShiftKind) -> (u32, bool) {
    match kind {
        ShiftKind::Lsl => match amount {
            0 => (value, cin),
            1..=31 => (value << amount, value & (1 << (32 - amount)) != 0),
            32 => (0, value & 1 != 0),
            _ => (0, false),
        },
        ShiftKind::Lsr => match amount {
            0 => (value, cin),
            1..=31 => (value >> amount, value & (1 << (amount - 1)) != 0),
            32 => (0, value & 0x8000_0000 != 0),
            // Beyond 32 no bit is shifted out: carry clear (A2.4).
            _ => (0, false),
        },
        ShiftKind::Asr => {
            if amount == 0 {
                (value, cin)
            } else if amount >= 32 {
                let fill = u32::from(value & 0x8000_0000 != 0);
                (fill.wrapping_neg(), fill != 0)
            } else {
                (
                    ((value as i32) >> amount) as u32,
                    value & (1 << (amount - 1)) != 0,
                )
            }
        }
        ShiftKind::Ror => {
            if amount == 0 {
                (value, cin)
            } else {
                let n = amount % 32;
                if n == 0 {
                    (value, value & 0x8000_0000 != 0)
                } else {
                    (value.rotate_right(n), value & (1 << (n - 1)) != 0)
                }
            }
        }
    }
}

impl Machine {
    fn value(&self, v: &Value) -> u32 {
        match v {
            Value::Const(w) => *w,
            Value::Id(id) => self.values[id.0 as usize],
        }
    }

    fn flag_value(&self, v: &Value) -> bool {
        self.value(v) != 0
    }

    fn set_id(&mut self, id: ValueId, value: u32) {
        self.values[id.0 as usize] = value;
        self.written[id.0 as usize] = true;
    }

    /// Binds a flag dest and moves the slot's representative to it.
    fn set_slot_value(&mut self, slot: usize, id: ValueId, value: u32) {
        self.values[id.0 as usize] = value;
        self.slot_ids[slot] = id;
        self.consts[slot] = None;
    }

    fn set_flag(&mut self, slot: usize, value: bool) {
        let id = self.slot_ids[slot];
        self.values[id.0 as usize] = u32::from(value);
        self.consts[slot] = None;
    }
}

pub(crate) enum ShiftKind {
    Lsl,
    Lsr,
    Asr,
    Ror,
}

/// 64-bit add with carry-in, returning result, carry-out, and signed
/// overflow (ARM ARM A2.3.1 formulas, as the interpreter implements).
pub(crate) fn add_with_carry(a: u32, b: u32, carry_in: bool) -> (u32, bool, bool) {
    let r = u64::from(a) + u64::from(b) + u64::from(carry_in);
    let res = r as u32;
    let c = r >> 32 == 1;
    let v = ((!(a ^ b)) & (a ^ res)) & 0x8000_0000 != 0;
    (res, c, v)
}

/// 64-bit subtract with borrow-in (`borrow = !carry`), returning
/// result, carry-out (true when no borrow), and signed overflow.
pub(crate) fn sub_with_borrow(a: u32, b: u32, borrow: bool) -> (u32, bool, bool) {
    let r = u64::from(a)
        .wrapping_sub(u64::from(b))
        .wrapping_sub(u64::from(borrow));
    let res = r as u32;
    let c = r >> 63 == 0;
    let v = ((a ^ b) & (a ^ res)) & 0x8000_0000 != 0;
    (res, c, v)
}
