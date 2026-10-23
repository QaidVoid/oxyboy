//! THUMB instruction lowering onto the shared machine-state lifter.
//!
//! THUMB data ops always set flags, so every flag-binding dest list
//! here is fixed. PC reads use the THUMB delta (addr + 4), never the
//! ARM one. Control-flow formats never reach [`ArmLift::thumb`]: the
//! pipeline lifts them as terminators before instruction lifting
//! (conditional and unconditional branch, the BL pair, bx, pop pc,
//! undefined encodings).

use decode::{
    Register, RegisterList, ThumbAluOp, ThumbImmOp, ThumbInstr, ThumbMemOp, ThumbShift,
    ThumbSignedMemOp,
};

use crate::arm::{ArmLift, C_SLOT};
use crate::{Op, Size, Value, ValueId};

impl ArmLift {
    /// Lowers one THUMB instruction that is not a control-flow
    /// terminator.
    pub(crate) fn thumb(&mut self, instr: ThumbInstr) {
        match instr {
            ThumbInstr::ShiftedImmediate { kind, rd, rm, imm5 } => {
                self.thumb_shifted_immediate(kind, rd, rm, imm5);
            }
            ThumbInstr::AddSubtract {
                subtract,
                immediate,
                rn_or_imm3,
                rs,
                rd,
            } => {
                let a = self.thumb_read(rs);
                let b = if immediate {
                    Value::Const(u32::from(rn_or_imm3))
                } else {
                    self.thumb_read(Register::from_bits(rn_or_imm3))
                };
                let out = self.write(rd);
                let dests = self.arith_dests(out);
                let op = if subtract {
                    Op::Sub {
                        set_flags: true,
                        a,
                        b,
                    }
                } else {
                    Op::Add {
                        set_flags: true,
                        a,
                        b,
                    }
                };
                self.emit(op, dests);
            }
            ThumbInstr::ImmediateOperation { op, rd, imm8 } => {
                let imm = Value::Const(u32::from(imm8));
                match op {
                    ThumbImmOp::Mov => {
                        let out = self.movs_flags(imm);
                        self.current[rd.bits() as usize] = out;
                    }
                    ThumbImmOp::Cmp => {
                        let a = self.thumb_read(rd);
                        let dests = self.cmp_dests();
                        self.emit(Op::Cmp { a, b: imm }, dests);
                    }
                    ThumbImmOp::Add | ThumbImmOp::Sub => {
                        let a = self.thumb_read(rd);
                        let out = self.write(rd);
                        let dests = self.arith_dests(out);
                        let op = if op == ThumbImmOp::Add {
                            Op::Add {
                                set_flags: true,
                                a,
                                b: imm,
                            }
                        } else {
                            Op::Sub {
                                set_flags: true,
                                a,
                                b: imm,
                            }
                        };
                        self.emit(op, dests);
                    }
                }
            }
            ThumbInstr::AluOperation { op, rd, rs } => self.thumb_alu(op, rd, rs),
            ThumbInstr::HighRegisterOperation { op, rd, rs } => self.thumb_hi(op, rd, rs),
            ThumbInstr::PcRelativeLoad { rd, offset } => {
                let base = (self.addr + 4) & !0x3;
                let addr = Value::Const(base.wrapping_add(4 * u32::from(offset)));
                let out = self.write(rd);
                self.emit(
                    Op::Load {
                        addr,
                        size: Size::Word,
                        signed: false,
                    },
                    vec![out],
                );
            }
            ThumbInstr::LoadStoreRegisterOffset { kind, rd, rb, ro } => {
                let base = self.thumb_read(rb);
                let offset = self.thumb_read(ro);
                let addr = self.add(base, offset);
                self.thumb_access(kind, rd, addr);
            }
            ThumbInstr::LoadStoreSignedRegisterOffset { kind, rd, rb, ro } => {
                let base = self.thumb_read(rb);
                let offset = self.thumb_read(ro);
                let addr = self.add(base, offset);
                let (size, signed, load) = match kind {
                    ThumbSignedMemOp::Strh => (Size::Half, false, false),
                    ThumbSignedMemOp::Ldsb => (Size::Byte, true, true),
                    ThumbSignedMemOp::Ldrh => (Size::Half, false, true),
                    ThumbSignedMemOp::Ldsh => (Size::Half, true, true),
                };
                self.transfer(load, rd, addr, size, signed);
            }
            ThumbInstr::LoadStoreImmediateOffset {
                kind,
                rd,
                rb,
                offset,
            } => {
                let scale = match kind {
                    ThumbMemOp::Str | ThumbMemOp::Ldr => 4,
                    ThumbMemOp::Strb | ThumbMemOp::Ldrb => 1,
                };
                let base = self.thumb_read(rb);
                let addr = self.add(base, Value::Const(scale * u32::from(offset)));
                self.thumb_access(kind, rd, addr);
            }
            ThumbInstr::LoadStoreHalfwordImmediate {
                load,
                rd,
                rb,
                offset,
            } => {
                let base = self.thumb_read(rb);
                let addr = self.add(base, Value::Const(2 * u32::from(offset)));
                self.transfer(load, rd, addr, Size::Half, false);
            }
            ThumbInstr::LoadStoreStack { load, rd, offset } => {
                let base = self.thumb_read(Register::Sp);
                let addr = self.add(base, Value::Const(4 * u32::from(offset)));
                self.transfer(load, rd, addr, Size::Word, false);
            }
            ThumbInstr::LoadAddress { to_sp, rd, offset } => {
                let base = if to_sp {
                    self.thumb_read(Register::Sp)
                } else {
                    Value::Const((self.addr + 4) & !0x3)
                };
                let out = self.write(rd);
                self.emit(
                    Op::Add {
                        set_flags: false,
                        a: base,
                        b: Value::Const(4 * u32::from(offset)),
                    },
                    vec![out],
                );
            }
            ThumbInstr::AdjustStackPointer { subtract, offset } => {
                let delta = 4 * u32::from(offset);
                let base = self.thumb_read(Register::Sp);
                let out = self.write(Register::Sp);
                self.emit(
                    Op::Add {
                        set_flags: false,
                        a: base,
                        b: Value::Const(if subtract {
                            delta.wrapping_neg()
                        } else {
                            delta
                        }),
                    },
                    vec![out],
                );
            }
            ThumbInstr::PushPop {
                pop,
                include_pc_or_lr,
                registers,
            } => {
                if pop {
                    self.thumb_pop(u16::from(registers), include_pc_or_lr);
                } else {
                    self.thumb_push(u16::from(registers), include_pc_or_lr);
                }
            }
            ThumbInstr::LoadStoreMultiple {
                load,
                rb,
                registers,
            } => {
                self.block_transfer(
                    load,
                    true,
                    true,
                    rb,
                    RegisterList::from_bits(u16::from(registers)),
                );
            }
            ThumbInstr::SoftwareInterrupt { value } => {
                self.emit(
                    Op::Intrinsic {
                        name: "swi",
                        args: vec![Value::Const(u32::from(value))],
                    },
                    Vec::new(),
                );
            }
            // Control-flow formats are lifted as terminators by the
            // pipeline before this runs.
            ThumbInstr::ConditionalBranch { .. }
            | ThumbInstr::UnconditionalBranch { .. }
            | ThumbInstr::LongBranchWithLink { .. }
            | ThumbInstr::LongBranchWithLinkFromLr { .. }
            | ThumbInstr::Undefined { .. } => {}
        }
    }

    /// Format 1: shift by immediate. LSL #0 passes the value through
    /// with C unchanged; LSR/ASR #0 mean an amount of 32.
    fn thumb_shifted_immediate(&mut self, kind: ThumbShift, rd: Register, rm: Register, imm5: u8) {
        let a = self.thumb_read(rm);
        if kind == ThumbShift::Lsl && imm5 == 0 {
            let out = self.movs_flags(a);
            self.current[rd.bits() as usize] = out;
            return;
        }
        let amount = match kind {
            ThumbShift::Lsl => u32::from(imm5),
            ThumbShift::Lsr | ThumbShift::Asr if imm5 == 0 => 32,
            ThumbShift::Lsr | ThumbShift::Asr => u32::from(imm5),
        };
        let out = self.write(rd);
        let c = self.flag_dest(C_SLOT);
        let op = match kind {
            ThumbShift::Lsl => Op::Lsl {
                set_flags: true,
                a,
                amount: Value::Const(amount),
            },
            ThumbShift::Lsr => Op::Lsr {
                set_flags: true,
                a,
                amount: Value::Const(amount),
            },
            ThumbShift::Asr => Op::Asr {
                set_flags: true,
                a,
                amount: Value::Const(amount),
            },
        };
        self.emit(op, vec![out, c]);
    }

    /// Format 4: ALU operation on low registers, always flag-setting.
    fn thumb_alu(&mut self, op: ThumbAluOp, rd: Register, rs: Register) {
        match op {
            ThumbAluOp::And | ThumbAluOp::Eor | ThumbAluOp::Orr | ThumbAluOp::Bic => {
                let carry = self.carry_in();
                let a = self.thumb_read(rd);
                let b = self.thumb_read(rs);
                let out = self.write(rd);
                let op = match op {
                    ThumbAluOp::And => Op::And {
                        set_flags: true,
                        a,
                        b,
                        carry_in: carry,
                    },
                    ThumbAluOp::Eor => Op::Eor {
                        set_flags: true,
                        a,
                        b,
                        carry_in: carry,
                    },
                    ThumbAluOp::Orr => Op::Orr {
                        set_flags: true,
                        a,
                        b,
                        carry_in: carry,
                    },
                    _ => Op::Bic {
                        set_flags: true,
                        a,
                        b,
                        carry_in: carry,
                    },
                };
                let dests = self.logic_dests(out);
                self.emit(op, dests);
            }
            ThumbAluOp::Lsl | ThumbAluOp::Lsr | ThumbAluOp::Asr | ThumbAluOp::Ror => {
                let a = self.thumb_read(rd);
                let amount = self.thumb_read(rs);
                let shifted = self.fresh();
                let c = self.flag_dest(C_SLOT);
                let op = match op {
                    ThumbAluOp::Lsl => Op::Lsl {
                        set_flags: true,
                        a,
                        amount,
                    },
                    ThumbAluOp::Lsr => Op::Lsr {
                        set_flags: true,
                        a,
                        amount,
                    },
                    ThumbAluOp::Asr => Op::Asr {
                        set_flags: true,
                        a,
                        amount,
                    },
                    _ => Op::Ror {
                        set_flags: true,
                        a,
                        amount,
                    },
                };
                self.emit(op, vec![shifted, c]);
                // N and Z come from the shifted value; the movs carries
                // the shift's C through unchanged.
                let out = self.movs_flags(Value::Id(shifted));
                self.current[rd.bits() as usize] = out;
            }
            ThumbAluOp::Adc | ThumbAluOp::Sbc => {
                let carry = self.carry_in();
                let a = self.thumb_read(rd);
                let b = self.thumb_read(rs);
                let out = self.write(rd);
                let dests = self.arith_dests(out);
                let op = if op == ThumbAluOp::Adc {
                    Op::Adc {
                        set_flags: true,
                        a,
                        b,
                        carry_in: carry,
                    }
                } else {
                    Op::Sbc {
                        set_flags: true,
                        a,
                        b,
                        carry_in: carry,
                    }
                };
                self.emit(op, dests);
            }
            ThumbAluOp::Tst => {
                let carry = self.carry_in();
                let a = self.thumb_read(rd);
                let b = self.thumb_read(rs);
                let n = self.flag_dest(0);
                let z = self.flag_dest(1);
                let c = self.flag_dest(C_SLOT);
                self.emit(
                    Op::Tst {
                        a,
                        b,
                        carry_in: carry,
                    },
                    vec![n, z, c],
                );
            }
            ThumbAluOp::Neg => {
                let a = self.thumb_read(rs);
                let out = self.write(rd);
                let dests = self.arith_dests(out);
                // Op::Rsb computes `b - a`; NEG is 0 - rs.
                self.emit(
                    Op::Rsb {
                        set_flags: true,
                        a,
                        b: Value::Const(0),
                    },
                    dests,
                );
            }
            ThumbAluOp::Cmp => {
                let a = self.thumb_read(rd);
                let b = self.thumb_read(rs);
                let dests = self.cmp_dests();
                self.emit(Op::Cmp { a, b }, dests);
            }
            ThumbAluOp::Cmn => {
                let a = self.thumb_read(rd);
                let b = self.thumb_read(rs);
                let dests = self.cmp_dests();
                self.emit(Op::Cmn { a, b }, dests);
            }
            ThumbAluOp::Mul => {
                let a = self.thumb_read(rd);
                let b = self.thumb_read(rs);
                let out = self.write(rd);
                let n = self.flag_dest(0);
                let z = self.flag_dest(1);
                self.emit(
                    Op::Mul {
                        set_flags: true,
                        a,
                        b,
                    },
                    vec![out, n, z],
                );
            }
            ThumbAluOp::Mvn => {
                let carry = self.carry_in();
                let a = self.thumb_read(rs);
                let out = self.write(rd);
                let op = Op::Mvn {
                    set_flags: true,
                    a,
                    carry_in: carry,
                };
                let dests = self.logic_dests(out);
                self.emit(op, dests);
            }
        }
    }

    /// Format 5 without BX and without a PC destination: ADD/MOV copy
    /// without flags, CMP compares. PC reads use the THUMB delta.
    pub(crate) fn thumb_hi(&mut self, op: decode::ThumbHiOp, rd: Register, rs: Register) {
        match op {
            decode::ThumbHiOp::Add | decode::ThumbHiOp::Mov => {
                // Read the dest operand before write replaces its slot.
                let a = self.thumb_read(rd);
                let b = self.thumb_read(rs);
                let out = self.write(rd);
                let op = if op == decode::ThumbHiOp::Add {
                    Op::Add {
                        set_flags: false,
                        a,
                        b,
                    }
                } else {
                    Op::Mov {
                        set_flags: false,
                        a: b,
                        carry_in: Value::Const(0),
                    }
                };
                self.emit(op, vec![out]);
            }
            decode::ThumbHiOp::Cmp => {
                let a = self.thumb_read(rd);
                let b = self.thumb_read(rs);
                let dests = self.cmp_dests();
                self.emit(Op::Cmp { a, b }, dests);
            }
            // BX is a terminator and never reaches here.
            decode::ThumbHiOp::Bx => {}
        }
    }

    /// Format 14 push: SP drops by the total size, stores run from the
    /// new SP upward with LR at the highest slot.
    fn thumb_push(&mut self, registers: u16, include_lr: bool) {
        let mut list: Vec<Register> = (0..8u8)
            .map(Register::from_bits)
            .filter(|reg| registers & (1 << reg.bits()) != 0)
            .collect();
        if include_lr {
            list.push(Register::Lr);
        }
        let total = 4 * list.len() as u32;
        let sp = self.thumb_read(Register::Sp);
        let new_sp = self.add(sp, Value::Const(total.wrapping_neg()));
        for (index, reg) in list.iter().enumerate() {
            let addr = if index == 0 {
                new_sp
            } else {
                self.add(Value::Id(new_sp), Value::Const(4 * index as u32))
            };
            self.emit(
                Op::Store {
                    addr: Value::Id(addr),
                    value: self.thumb_read(*reg),
                    size: Size::Word,
                },
                Vec::new(),
            );
        }
        self.current[Register::Sp.bits() as usize] = new_sp;
    }

    /// Format 14 pop: loads run from the old SP upward with PC at the
    /// highest slot; SP then grows by the total size. A loaded PC
    /// lands in the pc state slot for the pipeline's Return terminator.
    pub(crate) fn thumb_pop(&mut self, registers: u16, include_pc: bool) {
        let mut list: Vec<Register> = (0..8u8)
            .map(Register::from_bits)
            .filter(|reg| registers & (1 << reg.bits()) != 0)
            .collect();
        if include_pc {
            list.push(Register::Pc);
        }
        let total = 4 * list.len() as u32;
        let sp = self.thumb_read(Register::Sp);
        for (index, reg) in list.iter().enumerate() {
            let addr = if index == 0 {
                sp
            } else {
                Value::Id(self.add(sp, Value::Const(4 * index as u32)))
            };
            let out = self.write(*reg);
            self.emit(
                Op::Load {
                    addr,
                    size: Size::Word,
                    signed: false,
                },
                vec![out],
            );
        }
        let new_sp = self.add(sp, Value::Const(total));
        self.current[Register::Sp.bits() as usize] = new_sp;
    }

    /// The operand reading `reg` with the THUMB PC delta (addr + 4).
    fn thumb_read(&self, reg: Register) -> Value {
        if reg == Register::Pc {
            Value::Const(self.addr + 4)
        } else {
            Value::Id(self.current[reg.bits() as usize])
        }
    }

    /// Emits `a + b` without flags and returns its value.
    fn add(&mut self, a: Value, b: Value) -> ValueId {
        let out = self.fresh();
        self.emit(
            Op::Add {
                set_flags: false,
                a,
                b,
            },
            vec![out],
        );
        out
    }

    /// Emits a load or a store of the low register `rd` at `addr`.
    fn transfer(&mut self, load: bool, rd: Register, addr: ValueId, size: Size, signed: bool) {
        let addr = Value::Id(addr);
        if load {
            let out = self.write(rd);
            self.emit(Op::Load { addr, size, signed }, vec![out]);
        } else {
            self.emit(
                Op::Store {
                    addr,
                    value: self.thumb_read(rd),
                    size,
                },
                Vec::new(),
            );
        }
    }

    /// Word or byte transfer per the format 7/9 kind.
    fn thumb_access(&mut self, kind: ThumbMemOp, rd: Register, addr: ValueId) {
        let (size, load) = match kind {
            ThumbMemOp::Str => (Size::Word, false),
            ThumbMemOp::Ldr => (Size::Word, true),
            ThumbMemOp::Strb => (Size::Byte, false),
            ThumbMemOp::Ldrb => (Size::Byte, true),
        };
        self.transfer(load, rd, addr, size, false);
    }

    /// `movs` passthrough: binds the value plus N, Z, and the carried
    /// C (unchanged for THUMB ops with no shifter).
    fn movs_flags(&mut self, a: Value) -> ValueId {
        let carry = self.carry_in();
        let out = self.fresh();
        let dests = self.logic_dests(out);
        self.emit(
            Op::Mov {
                set_flags: true,
                a,
                carry_in: carry,
            },
            dests,
        );
        out
    }

    /// The arithmetic flag tail after `out`: dests [v, n, z, c, v].
    fn arith_dests(&mut self, out: ValueId) -> Vec<ValueId> {
        let n = self.flag_dest(0);
        let z = self.flag_dest(1);
        let c = self.flag_dest(C_SLOT);
        let v = self.flag_dest(3);
        vec![out, n, z, c, v]
    }

    /// The logic flag tail after `out`: dests [v, n, z, c].
    fn logic_dests(&mut self, out: ValueId) -> Vec<ValueId> {
        let n = self.flag_dest(0);
        let z = self.flag_dest(1);
        let c = self.flag_dest(C_SLOT);
        vec![out, n, z, c]
    }

    /// The compare dest list [n, z, c, v].
    fn cmp_dests(&mut self) -> Vec<ValueId> {
        let n = self.flag_dest(0);
        let z = self.flag_dest(1);
        let c = self.flag_dest(C_SLOT);
        let v = self.flag_dest(3);
        vec![n, z, c, v]
    }
}
