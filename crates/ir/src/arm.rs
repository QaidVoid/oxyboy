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

use crate::{Inst, Op, STATE_ARITY, Size, Value, ValueId};

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
        self.insts.push(Inst {
            guard: self.guard,
            dests,
            op,
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
    pub(crate) fn write(&mut self, reg: Register) -> ValueId {
        let id = self.fresh();
        self.current[reg.bits() as usize] = id;
        id
    }

    /// Binds one flag: a fresh id that becomes the flag's value.
    /// `slot` is 0..4 within N, Z, C, V.
    pub(crate) fn flag_dest(&mut self, slot: usize) -> ValueId {
        let id = self.fresh();
        self.current[16 + slot] = id;
        id
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
    fn shifter(&mut self, operand: ShifterOperand, with_carry: bool) -> (Value, Value) {
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
            ShifterOperand::Register { rm, shift } => self.register_shift(rm, shift),
        }
    }

    /// Expands one register shift to `(value, carry)`, emitting the
    /// explicit shift op when it has an effect. Shift ops always
    /// produce their carry-out; dead-flag elimination can drop it when
    /// no consumer needs it.
    fn register_shift(&mut self, rm: Register, shift: RegisterShift) -> (Value, Value) {
        let (kind, amount) = match shift.amount {
            ShiftAmount::Immediate(0) => match shift.kind {
                // A plain register: no shift op; carry passes through.
                ShiftKind::Lsl => return (self.read(rm), self.carry_in()),
                // ROR #0 encodes RRX (ARM ARM A4.1.3).
                ShiftKind::Ror => {
                    let out = self.fresh();
                    let c_in = self.carry_in();
                    let c = self.flag_dest(C_SLOT);
                    self.emit(
                        Op::Rrx {
                            set_flags: true,
                            a: self.read(rm),
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
        let c = self.flag_dest(C_SLOT);
        let op = match kind {
            ShiftKind::Lsl => Op::Lsl {
                set_flags: true,
                a: self.read(rm),
                amount,
            },
            ShiftKind::Lsr => Op::Lsr {
                set_flags: true,
                a: self.read(rm),
                amount,
            },
            ShiftKind::Asr => Op::Asr {
                set_flags: true,
                a: self.read(rm),
                amount,
            },
            ShiftKind::Ror => Op::Ror {
                set_flags: true,
                a: self.read(rm),
                amount,
            },
            ShiftKind::Rrx => {
                let c_in = self.carry_in();
                Op::Rrx {
                    set_flags: true,
                    a: self.read(rm),
                    carry_in: c_in,
                }
            }
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
                let product = self.write(rd);
                let mut dests = vec![product];
                if set_flags {
                    dests.push(self.flag_dest(0));
                    dests.push(self.flag_dest(1));
                }
                self.emit(
                    Op::Mul {
                        set_flags,
                        a: self.read(rm),
                        b: self.read(rs),
                    },
                    dests,
                );
                if accumulate {
                    let sum = self.write(rd);
                    self.emit(
                        Op::Add {
                            set_flags: false,
                            a: Value::Id(product),
                            b: self.read(rn),
                        },
                        vec![sum],
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
                        a: self.read(rm),
                        b: self.read(rn),
                    },
                    dests,
                );
            }
            ArmInstr::Swap { byte, rd, rn, rm } => {
                // SWP reads the old memory value into rd and stores
                // rm; expanded as load-then-store (the single-core GBA
                // gives the pair its atomicity).
                let size = if byte { Size::Byte } else { Size::Word };
                let out = self.write(rd);
                self.emit(
                    Op::Load {
                        addr: self.read(rn),
                        size,
                        signed: false,
                    },
                    vec![out],
                );
                self.emit(
                    Op::Store {
                        addr: self.read(rn),
                        value: self.read(rm),
                        size,
                    },
                    vec![],
                );
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
                pre_index: _,
                rn,
                registers,
            } => self.block_transfer(load, writeback, up, rn, registers),
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

    /// Data processing: ALU ops, comparisons, moves.
    fn data_processing(
        &mut self,
        opcode: DataOp,
        set_flags: bool,
        rd: Register,
        rn: Register,
        operand2: ShifterOperand,
    ) {
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
                let a = self.read(rn);
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
                let a = self.read(rn);
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
                let a = self.read(rn);
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
                let (b, _) = self.shifter(operand2, false);
                let a = self.read(rn);
                // Capture before dest allocation: flag_dest replaces the
                // carry slot, and Adc must consume the incoming carry.
                let carry = self.carry_in();
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
                let (b, _) = self.shifter(operand2, false);
                let a = self.read(rn);
                let carry = self.carry_in();
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
    fn single_transfer(
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
        let addr = if pre_index {
            let sum = self.fresh();
            self.emit(
                Op::Add {
                    set_flags: false,
                    a: self.read(rn),
                    b: offset,
                },
                vec![sum],
            );
            if writeback && rn != Register::Pc {
                self.current[rn.bits() as usize] = sum;
            }
            Value::Id(sum)
        } else {
            let addr = self.read(rn);
            if writeback && rn != Register::Pc {
                let sum = self.fresh();
                self.emit(
                    Op::Add {
                        set_flags: false,
                        a: self.read(rn),
                        b: offset,
                    },
                    vec![sum],
                );
                self.current[rn.bits() as usize] = sum;
            }
            addr
        };
        if load {
            let out = self.write(rd);
            self.emit(Op::Load { addr, size, signed }, vec![out]);
        } else {
            self.emit(
                Op::Store {
                    addr,
                    value: self.read(rd),
                    size,
                },
                vec![],
            );
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
        rn: Register,
        registers: decode::RegisterList,
    ) {
        let list: Vec<Register> = (0..16u8)
            .map(Register::from_bits)
            .filter(|reg| registers.contains(*reg))
            .collect();
        let count = list.len() as u32;
        for (index, reg) in list.iter().enumerate() {
            // Ascending transfers start at the base; descending ones
            // store the first register at base - 4 * count.
            let delta = if up {
                index as u32 * 4
            } else {
                ((count - index as u32) * 4).wrapping_neg()
            };
            let addr = if delta == 0 {
                self.read(rn)
            } else {
                let sum = self.fresh();
                self.emit(
                    Op::Add {
                        set_flags: false,
                        a: self.read(rn),
                        b: Value::Const(delta),
                    },
                    vec![sum],
                );
                Value::Id(sum)
            };
            if load {
                let out = self.write(*reg);
                self.emit(
                    Op::Load {
                        addr,
                        size: Size::Word,
                        signed: false,
                    },
                    vec![out],
                );
            } else {
                self.emit(
                    Op::Store {
                        addr,
                        value: self.read(*reg),
                        size: Size::Word,
                    },
                    vec![],
                );
            }
        }
        if writeback && rn != Register::Pc {
            let delta = if up {
                count * 4
            } else {
                (count * 4).wrapping_neg()
            };
            let sum = self.fresh();
            self.emit(
                Op::Add {
                    set_flags: false,
                    a: self.read(rn),
                    b: Value::Const(delta),
                },
                vec![sum],
            );
            self.current[rn.bits() as usize] = sum;
        }
    }
}
