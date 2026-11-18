//! The ARM instruction lifter.
//!
//! Lifts decoded ARM instructions to IR with real register dataflow:
//! reads come from the current state array (PC reads lower to
//! `addr + 8` per the design's PC rule), writes allocate fresh SSA
//! values and update the array, and conditions become per-instruction
//! guards. The barrel shifter expands into explicit ops whose carry
//! feeds set-flags operations; LDM/STM expand into ordered per-register
//! accesses.

use decode::{
    ArmInstr, Condition, DataOp, HalfwordKind, HalfwordOffset, PsrSource, Register, RegisterShift,
    ShiftAmount, ShiftKind, ShifterOperand, TransferOffset,
};

use crate::{Inst, LoadMode, Op, STATE_ARITY, Size, Value, ValueId};

/// Slot index of the C flag within the flag tail.
pub(crate) const C_SLOT: usize = 2;

/// Per-instruction lifting state: the machine state array in play plus
/// the SSA allocator and the instruction's own address.
pub(crate) struct ArmLift {
    /// Register/flag slot values; registers index `0..16`, the flag
    /// tail `16..20` in N, Z, C, V order.
    pub(crate) current: [ValueId; STATE_ARITY],
    /// Next SSA id to allocate.
    pub(crate) next: ValueId,
    /// Instructions emitted for the current block, in address order.
    pub(crate) insts: Vec<Inst>,
    /// Canonical address of the instruction being lifted.
    pub addr: u32,
    /// The decoded condition guarding this instruction.
    pub(crate) guard: Option<Condition>,
    /// Slot writes under the current guard: dest id to the slot's
    /// previous representative. Consumed by the next [`ArmLift::emit`]
    /// into `Inst::fallbacks`.
    pub(crate) fallbacks: Vec<(ValueId, ValueId)>,
}

impl ArmLift {
    /// A fresh value id.
    pub(crate) fn fresh(&mut self) -> ValueId {
        let id = self.next;
        self.next = ValueId(id.0 + 1);
        id
    }

    /// Emits one instruction binding `dests`.
    pub(crate) fn emit(&mut self, op: Op, dests: Vec<ValueId>) {
        // Stores can abort mid-chunk (the store hits translated code
        // and the interpreter takes the instruction back), so they
        // carry the post-instruction machine state for the bail flush.
        let state = matches!(&op, Op::Store { .. })
            .then(|| self.current.iter().map(|id| Value::Id(*id)).collect())
            .unwrap_or_default();
        // Slot writes under a real condition carry the slot's previous
        // representative as the guard-fail fallback.
        let fallbacks = if self.guard.is_some_and(|cond| cond != Condition::Al) {
            dests
                .iter()
                .map(|dest| {
                    self.fallbacks
                        .iter()
                        .find(|(id, _)| id == dest)
                        .map(|(_, prev)| Value::Id(*prev))
                })
                .collect()
        } else {
            Vec::new()
        };
        self.insts.push(Inst {
            guard: self.guard,
            dests,
            fallbacks,
            op,
            pc: self.addr,
            state,
        });
    }

    /// The operand reading `reg`: PC reads lower to `addr + 8` (ARM
    /// ARM A2.3.1). THUMB lowering must use its own PC read (addr + 4).
    pub(crate) fn read(&self, reg: Register) -> Value {
        if reg == Register::Pc {
            Value::Const(self.addr + 8)
        } else {
            Value::Id(self.current[reg.bits() as usize])
        }
    }

    /// Writes `reg`: allocates a fresh id and updates the state.
    /// Under a real condition the previous representative is recorded
    /// as the write's guard-fail fallback.
    pub(crate) fn write(&mut self, reg: Register) -> ValueId {
        let id = self.fresh();
        self.record_fallback(reg.bits() as usize, id);
        self.current[reg.bits() as usize] = id;
        id
    }

    /// Repoints the pc slot at its current value ANDed with `mask`:
    /// ARM loads into pc word-align without interworking (ARMv4), and
    /// THUMB pop pc clears bit 0 (GBATEK THUMB.14).
    pub(crate) fn mask_pc(&mut self, mask: u32) {
        let raw = Value::Id(self.current[Register::Pc.bits() as usize]);
        let masked = self.write(Register::Pc);
        self.emit(
            Op::And {
                set_flags: false,
                a: raw,
                b: Value::Const(mask),
                carry_in: Value::Const(0),
            },
            vec![masked],
        );
    }

    /// Binds one flag: a fresh id that becomes the flag's value.
    /// `slot` is 0..4 within N, Z, C, V. Under a real condition the
    /// previous representative is recorded as the write's guard-fail
    /// fallback.
    pub(crate) fn flag_dest(&mut self, slot: usize) -> ValueId {
        let id = self.fresh();
        self.record_fallback(16 + slot, id);
        self.current[16 + slot] = id;
        id
    }

    /// Records `slot`'s current representative as the fallback for a
    /// write binding `id`, when a real condition guards the write.
    fn record_fallback(&mut self, slot: usize, id: ValueId) {
        if self.guard.is_some_and(|cond| cond != Condition::Al) {
            self.fallbacks.push((id, self.current[slot]));
        }
    }

    /// The incoming carry value.
    pub(crate) fn carry_in(&self) -> Value {
        Value::Id(self.current[16 + C_SLOT])
    }

    /// Expands a shifter operand to `(value, carry)`; the carry is the
    /// value the set-flags C field takes (ARM ARM A4.1.3 carry-out
    /// rules): the shifter's carry when a real shift applies,
    /// otherwise the incoming C. Immediate carries are lift-time
    /// constants.
    pub(crate) fn shifter(&mut self, operand: ShifterOperand, with_carry: bool) -> (Value, Value) {
        match operand {
            ShifterOperand::Immediate { rotate, value } => {
                if rotate == 0 {
                    let c = if with_carry {
                        self.carry_in()
                    } else {
                        Value::Const(0)
                    };
                    (Value::Const(u32::from(value)), c)
                } else {
                    let word = u32::from(value).rotate_right(u32::from(rotate) * 2);
                    // Carry out = bit 31 of the rotated value.
                    (Value::Const(word), Value::Const(word >> 31))
                }
            }
            ShifterOperand::Register { rm, shift } => {
                let late = matches!(&shift.amount, ShiftAmount::Register(_));
                self.register_shift(rm, shift, late)
            }
        }
    }

    /// Expands one register shift to `(value, carry)`, emitting the
    /// explicit shift op when it has an effect. Shift ops always
    /// produce their carry-out; dead-flag elimination can drop it when
    /// no consumer needs it. `late` is the register-specified-shift
    /// pipeline rule: the extra register-file cycle reads R15 as
    /// instruction + 12 (ARM ARM A5.2; the external arm suite pins
    /// `mov r0, pc, lsl r0` and `add r0, pc, r0, lsl r0`).
    fn register_shift(&mut self, rm: Register, shift: RegisterShift, late: bool) -> (Value, Value) {
        let raw = if late && rm == Register::Pc {
            Value::Const(self.addr + 12)
        } else {
            self.read(rm)
        };
        let (kind, amount) = match shift.amount {
            ShiftAmount::Immediate(0) => match shift.kind {
                // A plain register: no shift op; carry passes through.
                ShiftKind::Lsl => return (raw, self.carry_in()),
                // ROR #0 encodes RRX (ARM ARM A4.1.3).
                ShiftKind::Ror => {
                    let out = self.fresh();
                    let c_in = self.carry_in();
                    // A plain value, not a flag dest: whether the
                    // shifter carry reaches C is the parent op's call.
                    let c = self.fresh();
                    self.emit(
                        Op::Rrx {
                            set_flags: false,
                            a: raw,
                            carry_in: c_in,
                        },
                        vec![out, c],
                    );
                    return (Value::Id(out), Value::Id(c));
                }
                // LSR/ASR #0 encode a 32-bit shift.
                kind => (kind, Value::Const(32)),
            },
            ShiftAmount::Immediate(n) => (shift.kind, Value::Const(u32::from(n))),
            ShiftAmount::Register(rs) => (shift.kind, self.read(rs)),
        };
        let out = self.fresh();
        let c_in = self.carry_in();
        // Plain value: the parent op decides whether the shifter carry
        // becomes the C flag.
        let c = self.fresh();
        let a = raw;
        let op = match kind {
            ShiftKind::Lsl => Op::Lsl {
                set_flags: false,
                a,
                amount,
                carry_in: c_in,
            },
            ShiftKind::Lsr => Op::Lsr {
                set_flags: false,
                a,
                amount,
                carry_in: c_in,
            },
            ShiftKind::Asr => Op::Asr {
                set_flags: false,
                a,
                amount,
                carry_in: c_in,
            },
            ShiftKind::Ror => Op::Ror {
                set_flags: false,
                a,
                amount,
                carry_in: c_in,
            },
            ShiftKind::Rrx => Op::Rrx {
                set_flags: false,
                a,
                carry_in: c_in,
            },
        };
        self.emit(op, vec![out, c]);
        (Value::Id(out), Value::Id(c))
    }

    /// Lifts one decoded instruction; control-flow classes are
    /// intercepted by the pipeline and never reach here.
    pub fn lift(&mut self, instr: ArmInstr) {
        match instr {
            ArmInstr::DataProcessing {
                opcode,
                set_flags,
                rd,
                rn,
                operand2,
            } => self.data_processing(opcode, set_flags, rd, rn, operand2),
            ArmInstr::Multiply {
                accumulate,
                set_flags,
                rd,
                rn,
                rs,
                rm,
            } => {
                // Reads come before the dest allocation: rd may alias
                // rm, rs, or rn (e.g. `mul r0, r1, r0`), and the CPU
                // consumes the old register values.
                let rm = self.read(rm);
                let rs = self.read(rs);
                let rn = self.read(rn);
                if accumulate {
                    // MLA's flags come from the accumulated result, so
                    // the product is a temp and the Add owns N/Z with
                    // the `[v, n, z]` contract (C, V unchanged).
                    let product = self.fresh();
                    self.emit(
                        Op::Mul {
                            set_flags: false,
                            a: rm,
                            b: rs,
                        },
                        vec![product],
                    );
                    let out = self.write(rd);
                    let mut dests = vec![out];
                    if set_flags {
                        dests.push(self.flag_dest(0));
                        dests.push(self.flag_dest(1));
                    }
                    self.emit(
                        Op::Add {
                            set_flags,
                            a: Value::Id(product),
                            b: rn,
                        },
                        dests,
                    );
                } else {
                    let out = self.write(rd);
                    let mut dests = vec![out];
                    if set_flags {
                        dests.push(self.flag_dest(0));
                        dests.push(self.flag_dest(1));
                    }
                    self.emit(
                        Op::Mul {
                            set_flags,
                            a: rm,
                            b: rs,
                        },
                        dests,
                    );
                }
            }
            ArmInstr::MultiplyLong {
                unsigned,
                accumulate,
                set_flags,
                rd_hi,
                rd_lo,
                rn,
                rm,
            } => {
                // rd_hi/rd_lo may alias rn or rm; read the sources and
                // the accumulate registers before allocating dests.
                let rm = self.read(rm);
                let rn = self.read(rn);
                let acc_lo = Value::Id(self.current[rd_lo.bits() as usize]);
                let acc_hi = Value::Id(self.current[rd_hi.bits() as usize]);
                let lo = self.write(rd_lo);
                let hi = self.write(rd_hi);
                let mut dests = vec![lo, hi];
                if set_flags {
                    dests.push(self.flag_dest(0));
                    dests.push(self.flag_dest(1));
                }
                self.emit(
                    Op::MultiplyLong {
                        unsigned,
                        accumulate,
                        set_flags,
                        a: rm,
                        b: rn,
                        acc_lo,
                        acc_hi,
                    },
                    dests,
                );
            }
            ArmInstr::Swap { byte, rd, rn, rm } => {
                // SWP reads the old memory value into rd and stores
                // rm; expanded as load-then-store (the single-core GBA
                // gives the pair its atomicity).
                let size = if byte { Size::Byte } else { Size::Word };
                let addr = self.read(rn);
                // rm may alias rd; the store consumes the old value.
                let value = self.read(rm);
                let out = self.write(rd);
                self.emit(
                    Op::Load {
                        addr,
                        size,
                        signed: false,
                        mode: LoadMode::Swap,
                    },
                    vec![out],
                );
                self.emit(Op::Store { addr, value, size }, vec![]);
            }
            ArmInstr::PsrRead { spsr, rd } => {
                let out = self.write(rd);
                self.emit(
                    Op::Intrinsic {
                        name: if spsr { "mrs-spsr" } else { "mrs-cpsr" },
                        args: vec![],
                    },
                    vec![out],
                );
            }
            ArmInstr::PsrWrite {
                spsr,
                fields,
                source,
            } => {
                let arg = match source {
                    PsrSource::Register(reg) => self.read(reg),
                    PsrSource::Immediate { rotate, value } => {
                        Value::Const(u32::from(value).rotate_right(u32::from(rotate) * 2))
                    }
                };
                self.emit(
                    Op::Intrinsic {
                        name: if spsr { "msr-spsr" } else { "msr-cpsr" },
                        args: vec![arg, Value::Const(u32::from(fields))],
                    },
                    vec![],
                );
            }
            ArmInstr::LoadStore {
                load,
                byte,
                writeback,
                up,
                pre_index,
                rd,
                rn,
                offset,
            } => {
                let size = if byte { Size::Byte } else { Size::Word };
                let offset = match offset {
                    TransferOffset::Immediate(imm) => Value::Const(imm),
                    TransferOffset::Register { rm, shift } => {
                        self.shifter(ShifterOperand::Register { rm, shift }, false)
                            .0
                    }
                };
                self.single_transfer(load, size, false, writeback, up, pre_index, rd, rn, offset);
            }
            ArmInstr::LoadStoreHalfwordSigned {
                kind,
                writeback,
                up,
                pre_index,
                rd,
                rn,
                offset,
            } => {
                let (size, signed, load) = match kind {
                    HalfwordKind::Strh => (Size::Half, false, false),
                    HalfwordKind::Ldrh => (Size::Half, false, true),
                    HalfwordKind::Ldrsb => (Size::Byte, true, true),
                    HalfwordKind::Ldrsh => (Size::Half, true, true),
                };
                let offset = match offset {
                    HalfwordOffset::Immediate(imm) => Value::Const(u32::from(imm)),
                    HalfwordOffset::Register(reg) => self.read(reg),
                };
                self.single_transfer(load, size, signed, writeback, up, pre_index, rd, rn, offset);
            }
            ArmInstr::BlockTransfer {
                load,
                psr_and_force_user: _,
                writeback,
                up,
                pre_index,
                rn,
                registers,
            } => self.block_transfer(load, writeback, up, pre_index, rn, registers),
            ArmInstr::SoftwareInterrupt { imm24 } => {
                self.emit(
                    Op::Intrinsic {
                        name: "swi",
                        args: vec![Value::Const(imm24)],
                    },
                    vec![],
                );
            }
            // Control classes are intercepted by the pipeline; their
            // arrival here would be a pipeline bug, so they demote to
            // an unhandled marker rather than vanishing.
            other => {
                let _ = other;
                self.emit(
                    Op::Intrinsic {
                        name: "unhandled",
                        args: vec![],
                    },
                    vec![],
                );
            }
        }
    }

    /// Operand-1 read for data processing: under a register-specified
    /// shift, R15 reads instruction + 12 (see `register_shift`).
    fn dp_operand1(&self, late: bool, rn: Register) -> Value {
        if late && rn == Register::Pc {
            Value::Const(self.addr + 12)
        } else {
            self.read(rn)
        }
    }

    /// Data processing: ALU ops, comparisons, moves.
    fn data_processing(
        &mut self,
        opcode: DataOp,
        set_flags: bool,
        rd: Register,
        rn: Register,
        operand2: ShifterOperand,
    ) {
        let late = matches!(
            &operand2,
            ShifterOperand::Register {
                shift: RegisterShift {
                    amount: ShiftAmount::Register(_),
                    ..
                },
                ..
            }
        );
        match opcode {
            DataOp::Mov | DataOp::Mvn => {
                let (value, carry) = self.shifter(operand2, set_flags);
                let out = self.write(rd);
                let mut dests = vec![out];
                if set_flags {
                    dests.push(self.flag_dest(0));
                    dests.push(self.flag_dest(1));
                    dests.push(self.flag_dest(C_SLOT));
                }
                let op = match opcode {
                    DataOp::Mov => Op::Mov {
                        set_flags,
                        a: value,
                        carry_in: carry,
                    },
                    _ => Op::Mvn {
                        set_flags,
                        a: value,
                        carry_in: carry,
                    },
                };
                self.emit(op, dests);
            }
            DataOp::Cmp | DataOp::Cmn => {
                let (b, _) = self.shifter(operand2, false);
                let a = self.dp_operand1(late, rn);
                let n = self.flag_dest(0);
                let z = self.flag_dest(1);
                let c = self.flag_dest(C_SLOT);
                let v = self.flag_dest(3);
                let op = if opcode == DataOp::Cmp {
                    Op::Cmp { a, b }
                } else {
                    Op::Cmn { a, b }
                };
                self.emit(op, vec![n, z, c, v]);
            }
            DataOp::Tst | DataOp::Teq => {
                let (b, carry) = self.shifter(operand2, true);
                let a = self.dp_operand1(late, rn);
                let n = self.flag_dest(0);
                let z = self.flag_dest(1);
                let c = self.flag_dest(C_SLOT);
                let op = match opcode {
                    DataOp::Tst => Op::Tst {
                        a,
                        b,
                        carry_in: carry,
                    },
                    _ => Op::Teq {
                        a,
                        b,
                        carry_in: carry,
                    },
                };
                self.emit(op, vec![n, z, c]);
            }
            DataOp::And | DataOp::Eor | DataOp::Orr | DataOp::Bic => {
                let (b, carry) = self.shifter(operand2, set_flags);
                let a = self.dp_operand1(late, rn);
                let out = self.write(rd);
                let mut dests = vec![out];
                if set_flags {
                    dests.push(self.flag_dest(0));
                    dests.push(self.flag_dest(1));
                    dests.push(self.flag_dest(C_SLOT));
                }
                let op = match opcode {
                    DataOp::And => Op::And {
                        set_flags,
                        a,
                        b,
                        carry_in: carry,
                    },
                    DataOp::Eor => Op::Eor {
                        set_flags,
                        a,
                        b,
                        carry_in: carry,
                    },
                    DataOp::Orr => Op::Orr {
                        set_flags,
                        a,
                        b,
                        carry_in: carry,
                    },
                    _ => Op::Bic {
                        set_flags,
                        a,
                        b,
                        carry_in: carry,
                    },
                };
                self.emit(op, dests);
            }
            DataOp::Add | DataOp::Adc => {
                // Capture before the shifter runs: a register shift
                // re-points the carry slot, and Adc must consume the
                // instruction's incoming carry.
                let carry = self.carry_in();
                let (b, _) = self.shifter(operand2, false);
                let a = self.dp_operand1(late, rn);
                let out = self.write(rd);
                let mut dests = vec![out];
                if set_flags {
                    dests.push(self.flag_dest(0));
                    dests.push(self.flag_dest(1));
                    dests.push(self.flag_dest(C_SLOT));
                    dests.push(self.flag_dest(3));
                }
                let op = match opcode {
                    DataOp::Add => Op::Add { set_flags, a, b },
                    _ => Op::Adc {
                        set_flags,
                        a,
                        b,
                        carry_in: carry,
                    },
                };
                self.emit(op, dests);
            }
            DataOp::Sub | DataOp::Sbc | DataOp::Rsb | DataOp::Rsc => {
                let carry = self.carry_in();
                let (b, _) = self.shifter(operand2, false);
                let a = self.dp_operand1(late, rn);
                let out = self.write(rd);
                let mut dests = vec![out];
                if set_flags {
                    dests.push(self.flag_dest(0));
                    dests.push(self.flag_dest(1));
                    dests.push(self.flag_dest(C_SLOT));
                    dests.push(self.flag_dest(3));
                }
                let op = match opcode {
                    DataOp::Sub => Op::Sub { set_flags, a, b },
                    DataOp::Sbc => Op::Sbc {
                        set_flags,
                        a,
                        b,
                        carry_in: carry,
                    },
                    DataOp::Rsb => Op::Rsb { set_flags, a, b },
                    _ => Op::Rsc {
                        set_flags,
                        a,
                        b,
                        carry_in: carry,
                    },
                };
                self.emit(op, dests);
            }
        }
    }

    /// LDR/STR/LDRH/STRH/LDRSB/LDRSH with full addressing: address
    /// computed first, write-back after, PC bases lowered to the
    /// literal address at lift time.
    /// Mirrors the decoded transfer fields one to one; the field
    /// count is the instruction set's, not a design choice.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn single_transfer(
        &mut self,
        load: bool,
        size: Size,
        signed: bool,
        writeback: bool,
        up: bool,
        pre_index: bool,
        rd: Register,
        rn: Register,
        offset: Value,
    ) {
        // Post-indexed addressing writes the base back regardless of
        // the W bit (ARM ARM A5.2.8: W is 0 there, yet Rn is updated).
        let writeback = writeback || !pre_index;
        let offset = if up {
            offset
        } else {
            match offset {
                Value::Const(word) => Value::Const(word.wrapping_neg()),
                Value::Id(id) => {
                    let neg = self.fresh();
                    self.emit(
                        Op::Rsb {
                            set_flags: false,
                            a: Value::Id(id),
                            b: Value::Const(0),
                        },
                        vec![neg],
                    );
                    Value::Id(neg)
                }
            }
        };
        // The stored value is the pre-instruction register: a base that
        // aliases rd must not leak the writeback into the store. ARM7
        // stores R15 as the instruction address plus 12 (GBATEK).
        let stored = if load {
            Value::Const(0)
        } else if rd == Register::Pc {
            Value::Const(self.addr + 12)
        } else {
            self.read(rd)
        };
        let base = self.read(rn);
        let mut pre_sum = None;
        let addr = if pre_index {
            let sum = self.fresh();
            self.emit(
                Op::Add {
                    set_flags: false,
                    a: base,
                    b: offset,
                },
                vec![sum],
            );
            pre_sum = Some(sum);
            Value::Id(sum)
        } else {
            base
        };
        if load {
            let out = self.write(rd);
            self.emit(
                Op::Load {
                    addr,
                    size,
                    signed,
                    mode: LoadMode::Ldr,
                },
                vec![out],
            );
            // Any load into pc branches: word-align, no interworking
            // (ARMv4). The walk turns word loads into returns; the
            // halfword and byte forms keep executing in-block.
            if rd == Register::Pc {
                self.mask_pc(!3);
            }
        } else {
            self.emit(
                Op::Store {
                    addr,
                    value: stored,
                    size,
                },
                vec![],
            );
        }
        // The writeback runs after the access, and a load into the
        // base register wins over it (ARM ARM A4.1.6).
        if writeback && rn != Register::Pc && !(load && rd == rn) {
            match pre_sum {
                // The pre-indexed address is already computed; a
                // guarded writeback still needs a slot write so the
                // guard-fail path keeps the old base.
                Some(sum) if self.guard.is_some_and(|cond| cond != Condition::Al) => {
                    let id = self.write(rn);
                    self.emit(
                        Op::Mov {
                            set_flags: false,
                            a: Value::Id(sum),
                            carry_in: Value::Const(0),
                        },
                        vec![id],
                    );
                }
                Some(sum) => self.current[rn.bits() as usize] = sum,
                None => {
                    let sum = self.write(rn);
                    self.emit(
                        Op::Add {
                            set_flags: false,
                            a: base,
                            b: offset,
                        },
                        vec![sum],
                    );
                }
            }
        }
    }

    /// LDM/STM expanded into ordered per-register accesses per the
    /// design's base rules: STM stores pre-instruction values, LDM
    /// with the base in the list lets the loaded value win (ARM ARM
    /// A4.1.10), and write-back updates the base after the accesses.
    pub(crate) fn block_transfer(
        &mut self,
        load: bool,
        writeback: bool,
        up: bool,
        pre_index: bool,
        rn: Register,
        registers: decode::RegisterList,
    ) {
        let list: Vec<Register> = (0..16u8)
            .map(Register::from_bits)
            .filter(|reg| registers.contains(*reg))
            .collect();
        let count = list.len() as u32;
        // The transfer addresses derive from the original base; a list
        // entry that loads into rn must not skew later entries.
        let base = self.read(rn);
        for (index, reg) in list.iter().enumerate() {
            // Offsets follow the four addressing modes: IA, IB, DB, DA
            // (ARM ARM A4.1.10), derived from the P and U bits.
            let delta: u32 = match (up, pre_index) {
                (true, false) => index as u32 * 4,
                (true, true) => (index as u32 + 1) * 4,
                (false, true) => (index as u32 * 4).wrapping_sub(count * 4),
                (false, false) => (index as u32 * 4 + 4).wrapping_sub(count * 4),
            };
            let addr = if delta == 0 {
                base
            } else {
                let sum = self.fresh();
                self.emit(
                    Op::Add {
                        set_flags: false,
                        a: base,
                        b: Value::Const(delta),
                    },
                    vec![sum],
                );
                Value::Id(sum)
            };
            if load {
                let out = self.write(*reg);
                // Block loads take the raw addressed word: no LDR
                // byte-lane rotation (the interpreter's LDM path).
                self.emit(
                    Op::Load {
                        addr,
                        size: Size::Word,
                        signed: false,
                        mode: LoadMode::Block,
                    },
                    vec![out],
                );
            } else {
                // For STM, a base register that is not the first list
                // entry stores the final writeback value (the old base
                // stays current until the writeback after the loop).
                // ARM7 stores R15 as the instruction address plus 12.
                let value = if *reg == Register::Pc {
                    Value::Const(self.addr + 12)
                } else if *reg == rn && Some(*reg) != list.first().copied() {
                    let total = count * 4;
                    let delta = if up { total } else { total.wrapping_neg() };
                    let sum = self.fresh();
                    self.emit(
                        Op::Add {
                            set_flags: false,
                            a: base,
                            b: Value::Const(delta),
                        },
                        vec![sum],
                    );
                    Value::Id(sum)
                } else {
                    self.read(*reg)
                };
                self.emit(
                    Op::Store {
                        addr,
                        value,
                        size: Size::Word,
                    },
                    vec![],
                );
            }
        }
        // A block load into pc branches: word-align the loaded value,
        // no interworking (ARMv4). The pc is the highest list bit, so
        // its load is the loop's last write and the mask lands after
        // every access.
        if load && registers.contains(Register::Pc) {
            self.mask_pc(!3);
        }
        // ARMv4: an LDM whose list contains the base leaves the loaded
        // value in place; no writeback runs.
        let base_in_list = registers.contains(rn);
        if writeback && rn != Register::Pc && !(load && base_in_list) {
            let delta = if up {
                count * 4
            } else {
                (count * 4).wrapping_neg()
            };
            let sum = self.write(rn);
            self.emit(
                Op::Add {
                    set_flags: false,
                    a: base,
                    b: Value::Const(delta),
                },
                vec![sum],
            );
        }
    }
}
