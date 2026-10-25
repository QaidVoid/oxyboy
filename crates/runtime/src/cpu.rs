//! The ARM7TDMI CPU interpreter.
//!
//! Implements ARM-mode execution for every instruction class the
//! decoder produces, with CPU modes, banked registers, CPSR/SPSR
//! handling, and exception entry. Cycle counts are coarse region-default
//! approximations (one N-cycle per bus access, region cost from
//! [`crate::bus`]); real WAITSTATE timing is not modeled yet.

use crate::bus::Bus;
use decode::{
    ArmInstr, Condition, DataOp, HalfwordKind, Register, RegisterList, RegisterShift, ShiftAmount,
    ShiftKind, ShifterOperand, ThumbAluOp, ThumbHiOp, ThumbImmOp, ThumbInstr, ThumbMemOp,
    ThumbShift, ThumbSignedMemOp, decode_arm, decode_thumb,
};

/// A CPU operating mode (ARM ARM A2.2, GBATEK "CPU Modes").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 0b10000: user, unprivileged.
    User,
    /// 0b10001: FIQ.
    Fiq,
    /// 0b10010: IRQ.
    Irq,
    /// 0b10011: supervisor (reset, SWI).
    Supervisor,
    /// 0b10111: abort.
    Abort,
    /// 0b11011: undefined instruction.
    Undefined,
    /// 0b11111: system (user registers, privileged).
    System,
}

impl Mode {
    /// Decodes the CPSR mode field, bits 4..0.
    #[must_use]
    pub fn from_bits(bits: u8) -> Option<Self> {
        Some(match bits {
            0x10 => Self::User,
            0x11 => Self::Fiq,
            0x12 => Self::Irq,
            0x13 => Self::Supervisor,
            0x17 => Self::Abort,
            0x1B => Self::Undefined,
            0x1F => Self::System,
            _ => return None,
        })
    }

    /// The CPSR mode field value.
    #[must_use]
    pub fn bits(self) -> u8 {
        match self {
            Self::User => 0x10,
            Self::Fiq => 0x11,
            Self::Irq => 0x12,
            Self::Supervisor => 0x13,
            Self::Abort => 0x17,
            Self::Undefined => 0x1B,
            Self::System => 0x1F,
        }
    }

    /// The register-bank slot for this mode.
    ///
    /// User and System share one r13/r14 bank; only FIQ banks r8..r12.
    fn bank(self) -> Bank {
        match self {
            Self::User | Self::System => Bank::Usr,
            Self::Fiq => Bank::Fiq,
            Self::Irq => Bank::Irq,
            Self::Supervisor => Bank::Svc,
            Self::Abort => Bank::Abt,
            Self::Undefined => Bank::Und,
        }
    }
}

/// Register-bank slot identifiers, one per distinct register set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bank {
    Usr,
    Fiq,
    Irq,
    Svc,
    Abt,
    Und,
}

/// One slot per [`Bank`]; the `Usr` SPSR slot is unused (User and System
/// modes have no SPSR).
const BANK_COUNT: usize = 6;

fn bank_index(bank: Bank) -> usize {
    match bank {
        Bank::Usr => 0,
        Bank::Fiq => 1,
        Bank::Irq => 2,
        Bank::Svc => 3,
        Bank::Abt => 4,
        Bank::Und => 5,
    }
}

/// Where execution continues after the current instruction.
enum Flow {
    /// No control transfer: advance by one instruction.
    Sequential,
    /// `self.pc` already holds the next fetch address.
    Handled,
}

/// The CPU low-power state (GBATEK HALTCNT / BIOS Halt and IntrWait).
/// `Any` wakes when any enabled interrupt is requested; `Wait(mask)`
/// wakes when (IE & IF & mask) is nonzero, the IntrWait shape. IME and
/// the CPSR I bit are don't care for the wake, per GBATEK SWI Halt.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Halt {
    None,
    Any,
    Wait(u16),
}

/// The ARM7TDMI processor state.
///
/// The visible register file is `regs`; r8..r12 have a second copy that
/// only FIQ swaps, and r13/r14 have one bank per mode (User and System
/// share theirs). Every privileged mode also owns an SPSR.
pub struct Cpu {
    regs: [u32; 16],
    /// The r8..r12 copy for the non-FIQ view (user, system, IRQ, SVC,
    /// ABT, UND all share it).
    r8_12_normal: [u32; 5],
    /// The r8..r12 copy visible only in FIQ mode.
    r8_12_fiq: [u32; 5],
    /// r13/r14 per bank slot.
    sp: [[u32; 2]; BANK_COUNT],
    /// SPSR per bank slot; slot 0 is unused (no SPSR in User/System).
    spsr: [u32; BANK_COUNT],
    cpsr: u32,
    /// Address of the instruction currently being executed.
    pc: u32,
    /// Accumulated cycle count (coarse region-default approximation).
    cycles: u64,
    /// The low-power state entered by BIOS Halt/IntrWait HLE.
    halt: Halt,
    /// True while BIOS calls are emulated instead of vectoring into
    /// the (zero-filled) BIOS array; cleared by `System::with_bios`.
    bios_hle: bool,
    /// The IRQ input line, sampled at the start of every step.
    irq_line: bool,
    /// Saved interrupted-context registers of in-flight BIOS-HLE IRQ
    /// dispatches, innermost last: r0, r1, r2, r3, r12, lr. The real
    /// BIOS preamble parks these on the IRQ-mode stack; the HLE
    /// dispatcher keeps them CPU-side so ROMs see the documented
    /// preservation without needing r13_irq to be valid. Depth bounds
    /// nested dispatches (a handler that clears the I bit).
    irq_hle_saved: [[u32; SAVED_IRQ_REGS]; IRQ_HLE_DEPTH],
    /// How many dispatches are in flight (0 = none).
    irq_hle_depth: usize,
}

/// Registers preserved across a BIOS-HLE IRQ dispatch: r0..r3, r12,
/// lr (the BIOS preamble pushes r0-r3, r12, lr).
const SAVED_IRQ_REGS: usize = 6;

/// How many nested BIOS-HLE dispatches can be in flight.
const IRQ_HLE_DEPTH: usize = 8;

/// Where the BIOS-HLE dispatcher parks `lr` for the user IRQ handler;
/// fetching this address performs the interrupt return (the real BIOS
/// postamble inlined). Lives in BIOS space, which stays unmapped.
pub const IRQ_HLE_RETURN: u32 = 0x0000_00A0;

/// The IWRAM slot holding the user IRQ handler address (GBATEK "BIOS
/// IRQ dispatcher": the dispatch at 0x18 jumps through `[0x03007FFC]`).
const IRQ_HANDLER_PTR: u32 = 0x0300_7FFC;

impl Cpu {
    /// Builds a CPU in supervisor mode with interrupts masked, as after
    /// reset; the walking skeleton boots straight into the cart, so the
    /// caller picks the entry point (normally `0x08000000`).
    #[must_use]
    pub fn new(pc: u32) -> Self {
        let mut cpu = Cpu {
            regs: [0; 16],
            r8_12_normal: [0; 5],
            r8_12_fiq: [0; 5],
            sp: [[0; 2]; BANK_COUNT],
            spsr: [0; BANK_COUNT],
            cpsr: (u32::from(Mode::Supervisor.bits())) | 0x0000_00C0,
            pc,
            cycles: 0,
            halt: Halt::None,
            bios_hle: true,
            irq_line: false,
            irq_hle_saved: [[0; SAVED_IRQ_REGS]; IRQ_HLE_DEPTH],
            irq_hle_depth: 0,
        };
        cpu.init_stacks();
        // The boot CPSR names a mode whose live r13/r14 sit in
        // regs[13..15]; switch_mode only swaps them on a bank change,
        // so the boot bank must be loaded here once.
        let bank = bank_index(cpu.mode().bank());
        cpu.regs[13..15].copy_from_slice(&cpu.sp[bank]);
        cpu
    }

    /// Installs the stack pointers the BIOS sets up during boot
    /// (GBATEK "GBA BIOS RAM Usage"): sp_usr/sys = 3007F00h,
    /// sp_irq = 3007FA0h, sp_svc = 3007FE0h. The no-BIOS entry paths
    /// (cart and multiboot boots) must reproduce the post-boot state;
    /// ROMs without their own CRT0 read these directly.
    fn init_stacks(&mut self) {
        self.sp[bank_index(Bank::Irq)][0] = 0x0300_7FA0;
        self.sp[bank_index(Bank::Svc)][0] = 0x0300_7FE0;
        self.sp[bank_index(Bank::Usr)][0] = 0x0300_7F00;
    }

    /// The program counter: address of the instruction being executed.
    #[must_use]
    pub fn pc(&self) -> u32 {
        self.pc
    }

    /// Overrides the program counter; used to boot the CPU at an entry
    /// point and by tests that park execution at a synthetic handler.
    pub fn set_pc(&mut self, pc: u32) {
        self.pc = pc;
    }

    /// Total coarse cycle count accumulated so far.
    #[must_use]
    pub fn cycles(&self) -> u64 {
        self.cycles
    }

    /// Raw register value; R15 reads as the instruction address plus 8
    /// (ARM pipeline behavior, ARM ARM A2.3).
    #[must_use]
    pub fn reg(&self, r: Register) -> u32 {
        match r {
            Register::Pc => self.pc.wrapping_add(8),
            _ => self.regs[r.bits() as usize],
        }
    }

    /// Writes a general register in the current bank.
    pub fn set_reg(&mut self, r: Register, value: u32) {
        if r != Register::Pc {
            self.regs[r.bits() as usize] = value;
        }
    }

    /// Reads a general register from the user bank (the force-user T
    /// form and the block-transfer S bit), ARM ARM A4.1.9.
    fn reg_user(&self, r: Register) -> u32 {
        match r {
            Register::Pc => self.pc.wrapping_add(8),
            Register::Sp | Register::Lr => self.sp[bank_index(Bank::Usr)][(r.bits() - 13) as usize],
            _ if self.mode() == Mode::Fiq => self.r8_12_normal[r.bits() as usize - 8],
            _ => self.regs[r.bits() as usize],
        }
    }

    /// Writes a general register in the user bank.
    fn set_reg_user(&mut self, r: Register, value: u32) {
        match r {
            Register::Pc => {}
            Register::Sp | Register::Lr => {
                self.sp[bank_index(Bank::Usr)][(r.bits() - 13) as usize] = value;
            }
            _ if self.mode() == Mode::Fiq => {
                self.r8_12_normal[r.bits() as usize - 8] = value;
            }
            _ => self.regs[r.bits() as usize] = value,
        }
    }

    /// The current operating mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        Mode::from_bits((self.cpsr & 0x1F) as u8).unwrap_or(Mode::Supervisor)
    }

    /// The raw CPSR value.
    #[must_use]
    pub fn cpsr(&self) -> u32 {
        self.cpsr
    }

    /// Overrides the CPSR wholesale (test hook).
    pub fn set_cpsr_raw(&mut self, value: u32) {
        self.cpsr = value;
    }

    /// Reads a general register for tests.
    #[must_use]
    pub fn reg_for_test(&self, index: u8) -> u32 {
        self.regs[(index & 0xF) as usize]
    }

    /// Writes a general register for tests.
    pub fn set_reg_for_test(&mut self, index: u8, value: u32) {
        self.regs[(index & 0xF) as usize] = value;
    }

    /// Raw GPR read for the BIOS-call HLE (arguments in r0-r3).
    pub(crate) fn gpr(&self, index: u8) -> u32 {
        self.reg_for_test(index)
    }

    /// Raw GPR write for the BIOS-call HLE (results in r0-r3).
    pub(crate) fn set_gpr(&mut self, index: u8, value: u32) {
        self.set_reg_for_test(index, value);
    }

    /// Charges HLE cycle cost onto the coarse cycle counter.
    pub(crate) fn charge(&mut self, cycles: u32) {
        self.cycles += u64::from(cycles);
    }

    /// Enters the halt state (BIOS HLE: SWI 02h Halt passes `None`,
    /// SWI 04h IntrWait passes the waited flag mask).
    pub(crate) fn halt(&mut self, wait: Option<u16>) {
        self.halt = match wait {
            Some(m) => Halt::Wait(m),
            None => Halt::Any,
        };
    }

    /// True while the CPU is halted (halt or wait-for-interrupt);
    /// halted steps charge one cycle and change nothing.
    #[must_use]
    pub fn is_halted(&self) -> bool {
        self.halt != Halt::None
    }

    /// Executes a BIOS SoftReset (GBATEK SWI 00h): zeroes r0-r12, the
    /// banked LR/SPSR of every mode, and the 200h-byte IWRAM tail was
    /// cleared by the caller; installs the documented stack pointers,
    /// enters System mode with IRQs masked (the real reset path runs
    /// with IRQs disabled), and redirects to the reset entry: ROM when
    /// the IWRAM flag byte is zero, else EWRAM, always in ARM state.
    pub(crate) fn soft_reset(&mut self, flag: u8) {
        self.regs = [0; 16];
        self.r8_12_normal = [0; 5];
        self.r8_12_fiq = [0; 5];
        self.sp = [[0; 2]; BANK_COUNT];
        self.spsr = [0; BANK_COUNT];
        self.init_stacks();
        self.switch_mode(Mode::System);
        self.cpsr |= 0x80;
        self.pc = if flag == 0 { 0x0800_0000 } else { 0x0200_0000 };
    }

    /// Enables or disables BIOS-call HLE. On by default: SWI executes
    /// the HLE module instead of vectoring into the (zero-filled) BIOS
    /// array. `System::with_bios` disables it; tests that exercise the
    /// exception-entry path itself disable it too.
    pub fn set_bios_hle(&mut self, hle: bool) {
        self.bios_hle = hle;
    }

    /// The raw SPSR of the current mode; `None` in User/System.
    #[must_use]
    pub fn spsr_for_test(&self) -> Option<u32> {
        let i = bank_index(self.mode().bank());
        (i != bank_index(Bank::Usr)).then_some(self.spsr[i])
    }

    /// Raises the IRQ input line.
    pub fn set_irq_line(&mut self, high: bool) {
        self.irq_line = high;
    }

    /// Switches operating mode, swapping the banked registers.
    ///
    /// r8..r12 swap between the normal view and the FIQ view only when
    /// crossing the FIQ boundary; r13/r14 swap per bank slot.
    fn switch_mode(&mut self, new: Mode) {
        let old_bank = self.mode().bank();
        let new_bank = new.bank();
        if old_bank == new_bank {
            self.cpsr = (self.cpsr & !0x1F) | u32::from(new.bits());
            return;
        }
        if old_bank == Bank::Fiq {
            self.r8_12_fiq.copy_from_slice(&self.regs[8..13]);
            self.regs[8..13].copy_from_slice(&self.r8_12_normal);
        } else if new_bank == Bank::Fiq {
            self.r8_12_normal.copy_from_slice(&self.regs[8..13]);
            self.regs[8..13].copy_from_slice(&self.r8_12_fiq);
        }
        self.sp[bank_index(old_bank)].copy_from_slice(&self.regs[13..15]);
        self.regs[13..15].copy_from_slice(&self.sp[bank_index(new_bank)]);
        self.cpsr = (self.cpsr & !0x1F) | u32::from(new.bits());
    }

    /// The SPSR of the current mode; User/System have none (reads 0,
    /// writes ignored: ARM7TDMI behavior, documented as UNPREDICTABLE in
    /// the ARM ARM A2.6 note on SPSR access).
    fn spsr_of_current(&self) -> u32 {
        let i = bank_index(self.mode().bank());
        if i == bank_index(Bank::Usr) {
            0
        } else {
            self.spsr[i]
        }
    }

    /// Architectural exception entry (ARM ARM A2.6).
    ///
    /// Saves the CPSR into the new mode's SPSR, banks the registers,
    /// forces ARM state, masks IRQ (and FIQ when requested), and jumps
    /// to the vector.
    fn exception(&mut self, new_mode: Mode, lr: u32, vector: u32, mask_fiq: bool) {
        self.spsr[bank_index(new_mode.bank())] = self.cpsr;
        self.switch_mode(new_mode);
        self.cpsr &= !0x20; // T = 0: vectors entered in ARM state
        self.cpsr |= 0x80; // I = 1
        if mask_fiq {
            self.cpsr |= 0x40; // F = 1
        }
        self.regs[Register::Lr.bits() as usize] = lr;
        self.pc = vector;
    }

    fn flag_c(&self) -> bool {
        self.cpsr & 0x2000_0000 != 0
    }

    /// Updates the NZCV flags from parts; `None` leaves that flag
    /// unchanged (logical operations do not touch C or V).
    fn set_flags(&mut self, n: bool, z: bool, c: Option<bool>, v: Option<bool>) {
        let mut cpsr = self.cpsr & !(0xF000_0000);
        if n {
            cpsr |= 0x8000_0000;
        }
        if z {
            cpsr |= 0x4000_0000;
        }
        if c.unwrap_or(self.flag_c()) {
            cpsr |= 0x2000_0000;
        }
        if v.unwrap_or(self.cpsr & 0x1000_0000 != 0) {
            cpsr |= 0x1000_0000;
        }
        self.cpsr = cpsr;
    }

    fn set_flag_bits(&mut self, mask: u32, value: u32) {
        self.cpsr = (self.cpsr & !mask) | (value & mask);
    }
}

/// Evaluates an instruction condition against CPSR flag bits.
fn condition_holds(cond: Condition, cpsr: u32) -> bool {
    let n = cpsr & 0x8000_0000 != 0;
    let z = cpsr & 0x4000_0000 != 0;
    let c = cpsr & 0x2000_0000 != 0;
    let v = cpsr & 0x1000_0000 != 0;
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

/// A shifted operand: its value plus the carry the shift produced.
struct ShiftOut {
    value: u32,
    carry: bool,
}

/// Applies a barrel shifter operation with ARM7 carry semantics.
///
/// `amount` is the resolved shift amount; the immediate 0 forms that
/// mean "32" (LSR/ASR) or RRX are normalized by the caller. Register
/// amounts of exactly 32 and above follow ARM ARM A2.4 (shift results
/// and carry for amounts >= 32).
fn apply_shift(value: u32, kind: ShiftKind, amount: u32, carry_in: bool) -> ShiftOut {
    match kind {
        ShiftKind::Lsl => match amount {
            0 => ShiftOut {
                value,
                carry: carry_in,
            },
            1..=31 => ShiftOut {
                value: value << amount,
                carry: value & (1 << (32 - amount)) != 0,
            },
            32 => ShiftOut {
                value: 0,
                carry: value & 1 != 0,
            },
            _ => ShiftOut {
                value: 0,
                carry: false,
            },
        },
        ShiftKind::Lsr => match amount {
            0 => ShiftOut {
                value,
                carry: carry_in,
            },
            1..=31 => ShiftOut {
                value: value >> amount,
                carry: value & (1 << (amount - 1)) != 0,
            },
            32 => ShiftOut {
                value: 0,
                carry: value & 0x8000_0000 != 0,
            },
            // Beyond 32 no bit of the value is ever shifted out, so
            // the carry is clear (ARM ARM A2.4 LSR, amount > 32).
            _ => ShiftOut {
                value: 0,
                carry: false,
            },
        },
        ShiftKind::Asr => {
            if amount == 0 {
                ShiftOut {
                    value,
                    carry: carry_in,
                }
            } else if amount >= 32 {
                let fill = u32::from(value & 0x8000_0000 != 0);
                ShiftOut {
                    value: fill.wrapping_neg(),
                    carry: fill != 0,
                }
            } else {
                ShiftOut {
                    value: ((value as i32) >> amount) as u32,
                    carry: value & (1 << (amount - 1)) != 0,
                }
            }
        }
        ShiftKind::Ror => {
            if amount == 0 {
                ShiftOut {
                    value,
                    carry: carry_in,
                }
            } else {
                let n = amount % 32;
                if n == 0 {
                    ShiftOut {
                        value,
                        carry: value & 0x8000_0000 != 0,
                    }
                } else {
                    ShiftOut {
                        value: value.rotate_right(n),
                        carry: value & (1 << (n - 1)) != 0,
                    }
                }
            }
        }
        // RRX: rotate right one place through the carry.
        ShiftKind::Rrx => ShiftOut {
            value: (value >> 1) | (u32::from(carry_in) << 31),
            carry: value & 1 != 0,
        },
    }
}

/// 64-bit add with carry-in, returning result, carry-out, and signed
/// overflow.
fn add_with_carry(a: u32, b: u32, carry_in: bool) -> (u32, bool, bool) {
    let r = u64::from(a) + u64::from(b) + u64::from(carry_in);
    let res = r as u32;
    let c = r >> 32 == 1;
    let v = ((!(a ^ b)) & (a ^ res)) & 0x8000_0000 != 0;
    (res, c, v)
}

/// 64-bit subtract with borrow-in (`borrow = !carry`), returning
/// result, carry-out (true when no borrow), and signed overflow.
fn sub_with_borrow(a: u32, b: u32, borrow: bool) -> (u32, bool, bool) {
    let r = (u64::from(a))
        .wrapping_sub(u64::from(b))
        .wrapping_sub(u64::from(borrow));
    let res = r as u32;
    let c = r >> 63 == 0;
    let v = ((a ^ b) & (a ^ res)) & 0x8000_0000 != 0;
    (res, c, v)
}

impl Cpu {
    /// Executes one instruction at `self.pc` in the state selected by
    /// the T bit, accumulating coarse cycles. Undefined encodings take
    /// the undefined-instruction exception; nothing panics.
    pub fn step(&mut self, bus: &mut Bus) -> u32 {
        let before = self.cycles;
        if self.pc == IRQ_HLE_RETURN && self.irq_hle_depth > 0 {
            // Fetching the HLE sentinel performs the interrupt return
            // (the BIOS postamble inlined): restore the interrupted
            // registers, CPSR, and resume at r14_irq - 4.
            self.cycles += 3;
            self.irq_hle_return();
            return (self.cycles - before) as u32;
        }
        if self.halt != Halt::None {
            let mask = match self.halt {
                Halt::Wait(m) => m,
                _ => 0xFFFF,
            };
            if bus.int_enable() & bus.int_flags() & mask != 0 {
                let was_wait = matches!(self.halt, Halt::Wait(_));
                self.halt = Halt::None;
                if was_wait {
                    // IntrWait clears the waited flags in the BIOS
                    // interrupt-flag word on return (GBATEK SWI 04h).
                    let flags = bus.read_u16(crate::swi::BIOS_INTR_FLAGS);
                    bus.write_u16(crate::swi::BIOS_INTR_FLAGS, flags & !mask);
                }
            } else {
                // The halted CPU draws no instructions; the system
                // driver still advances hardware by the charged cycle.
                self.cycles += 1;
                return (self.cycles - before) as u32;
            }
        }
        if self.irq_line && self.cpsr & 0x80 == 0 {
            // IRQ entry: the not-yet-executed instruction at `self.pc`
            // is not executed. The return address is saved in ARM-style
            // form in both states (ARM ARM A2.6, GBATEK "Actions
            // performed by CPU when entering an exception") so
            // `subs pc, lr, #4` re-executes it.
            let width = if self.cpsr & 0x20 != 0 { 2 } else { 4 };
            self.cycles += u64::from(bus.access_cost(self.pc, width));
            self.take_irq(bus);
            return (self.cycles - before) as u32;
        }
        if self.cpsr & 0x20 != 0 {
            self.step_thumb(bus);
        } else {
            self.step_arm(bus);
        }
        (self.cycles - before) as u32
    }

    /// Takes a pending IRQ through the BIOS-HLE dispatch path.
    ///
    /// With no BIOS ROM mapped, the vector at 0x18 is emulated at the
    /// documented dispatch level instead: the interrupted caller-saved
    /// registers (r0-r3, r12, lr) are preserved CPU-side, the CPU
    /// enters ARM System state with IRQs disabled, `r0` carries the
    /// `IE & IF` mask, `lr` parks on the [`IRQ_HLE_RETURN`] sentinel,
    /// and execution resumes at `[0x03007FFC]`. The handler
    /// acknowledges IF itself (write-1-to-clear) and returns with
    /// `bx lr`; a handler address of 0 leaves the request pending
    /// (nothing installed it, so there is nothing to dispatch to).
    fn take_irq(&mut self, bus: &mut Bus) {
        let handler = bus.read_u32(IRQ_HANDLER_PTR);
        if handler == 0 {
            return;
        }
        // Preserve the interrupted context before the mode switch
        // banks r14 away (r0-r3 and r12 are unbanked).
        let saved = [
            self.regs[0],
            self.regs[1],
            self.regs[2],
            self.regs[3],
            self.regs[12],
            self.regs[14],
        ];
        if self.irq_hle_depth < IRQ_HLE_DEPTH {
            self.irq_hle_saved[self.irq_hle_depth] = saved;
            self.irq_hle_depth += 1;
        }
        self.exception(Mode::Irq, self.pc.wrapping_add(4), 0x18, false);
        // The BIOS preamble runs the handler in ARM System state with
        // IRQs still disabled, r0 = IE & IF (GBATEK "Interrupt
        // Dispatch").
        self.switch_mode(Mode::System);
        self.regs[Register::Lr.bits() as usize] = IRQ_HLE_RETURN;
        self.regs[0] = u32::from(bus.int_enable() & bus.int_flags());
        self.pc = handler;
    }

    /// Performs the interrupt return for one in-flight HLE dispatch:
    /// restores the interrupted registers and CPSR and resumes at
    /// `r14_irq - 4` (the not-yet-executed instruction).
    fn irq_hle_return(&mut self) {
        self.irq_hle_depth -= 1;
        let saved = self.irq_hle_saved[self.irq_hle_depth];
        let spsr = self.spsr[bank_index(Bank::Irq)];
        let resume = self.sp[bank_index(Bank::Irq)][1].wrapping_sub(4);
        self.restore_cpsr(spsr);
        self.regs[0] = saved[0];
        self.regs[1] = saved[1];
        self.regs[2] = saved[2];
        self.regs[3] = saved[3];
        self.regs[12] = saved[4];
        self.regs[Register::Lr.bits() as usize] = saved[5];
        self.pc = resume;
    }

    /// One ARM-state instruction.
    fn step_arm(&mut self, bus: &mut Bus) {
        self.cycles += u64::from(bus.fetch_cost(self.pc, 4));
        let word = bus.read_u32(self.pc);
        let start_pc = self.pc;
        let cond = Condition::from_bits((word >> 28) as u8);
        let instr = decode_arm(word);
        let flow = match cond {
            Some(c) if condition_holds(c, self.cpsr) => self.execute(instr, bus),
            Some(_) => {
                // Condition failed: one internal cycle, no effect.
                self.cycles += 1;
                Flow::Sequential
            }
            None => {
                // Reserved condition 0b1111: treated as an undefined
                // instruction (ARM7TDMI marks it UNPREDICTABLE).
                self.undefined_exception();
                Flow::Handled
            }
        };
        if matches!(flow, Flow::Sequential) {
            self.pc = start_pc.wrapping_add(4);
        }
    }

    /// One THUMB-state instruction: a 16-bit fetch (plus the following
    /// halfword, which only BL consumes) decoded by [`decode_thumb`].
    fn step_thumb(&mut self, bus: &mut Bus) {
        self.cycles += u64::from(bus.fetch_cost(self.pc, 2));
        let first = bus.read_u16(self.pc);
        let next = bus.read_u16(self.pc.wrapping_add(2));
        let start_pc = self.pc;
        let instr = decode_thumb(first, next);
        let flow = self.execute_thumb(instr, bus);
        if matches!(flow, Flow::Sequential) {
            self.pc = start_pc.wrapping_add(2);
        }
    }

    /// Reads a register with THUMB pipeline semantics: R15 reads as the
    /// instruction address plus 4 (GBATEK THUMB.5 note), unlike the
    /// ARM-state plus-8.
    fn reg_thumb(&self, r: Register) -> u32 {
        match r {
            Register::Pc => self.pc.wrapping_add(4),
            _ => self.regs[r.bits() as usize],
        }
    }

    fn undefined_exception(&mut self) {
        let lr = self.pc.wrapping_add(4);
        self.exception(Mode::Undefined, lr, 0x04, false);
    }

    fn execute(&mut self, instr: ArmInstr, bus: &mut Bus) -> Flow {
        match instr {
            ArmInstr::DataProcessing {
                opcode,
                set_flags,
                rd,
                rn,
                operand2,
            } => self.exec_data_processing(opcode, set_flags, rd, rn, operand2),
            ArmInstr::Multiply {
                accumulate,
                set_flags,
                rd,
                rn,
                rs,
                rm,
            } => {
                let mut res = self.reg(rm).wrapping_mul(self.reg(rs));
                if accumulate {
                    res = res.wrapping_add(self.reg(rn));
                }
                self.set_reg(rd, res);
                if set_flags {
                    // ARMv4 leaves C and V "destroyed" (GBATEK MUL/MLA
                    // flags note); modeled as unchanged.
                    self.set_flags(res & 0x8000_0000 != 0, res == 0, None, None);
                }
                self.cycles += 3;
                Flow::Sequential
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
                let m_val = self.reg(rm);
                let n_val = self.reg(rn);
                let m = if unsigned {
                    u64::from(m_val)
                } else {
                    (m_val as i32) as u64
                };
                let n = if unsigned {
                    u64::from(n_val)
                } else {
                    (n_val as i32) as u64
                };
                let mut prod = m.wrapping_mul(n);
                if accumulate {
                    let hi = u64::from(self.reg(rd_hi));
                    let lo = u64::from(self.reg(rd_lo));
                    prod = prod.wrapping_add((hi << 32) | lo);
                }
                self.set_reg(rd_hi, (prod >> 32) as u32);
                self.set_reg(rd_lo, prod as u32);
                if set_flags {
                    // N from bit 63, Z from the 64-bit result; C and V
                    // destroyed on ARMv4 (GBATEK MULL/MLAL flags note).
                    self.set_flags(prod >> 63 == 1, prod == 0, None, None);
                }
                self.cycles += 5;
                Flow::Sequential
            }
            ArmInstr::Swap { byte, rd, rn, rm } => {
                let addr = self.reg(rn);
                self.cycles += u64::from(bus.access_cost(addr, if byte { 1 } else { 4 }));
                if byte {
                    let tmp = bus.read_u8(addr);
                    bus.write_u8(addr, self.reg(rm) as u8);
                    self.set_reg(rd, u32::from(tmp));
                } else {
                    let raw = bus.read_u32(addr);
                    let tmp = raw.rotate_right((addr & 3) * 8);
                    bus.write_u32(addr, self.reg(rm));
                    self.set_reg(rd, tmp);
                }
                self.cycles += 1;
                Flow::Sequential
            }
            ArmInstr::PsrRead { spsr, rd } => {
                let value = if spsr {
                    self.spsr_of_current()
                } else {
                    self.cpsr
                };
                self.set_reg(rd, value);
                Flow::Sequential
            }
            ArmInstr::PsrWrite {
                spsr,
                fields,
                source,
            } => {
                let value = match source {
                    decode::PsrSource::Register(r) => self.reg(r),
                    decode::PsrSource::Immediate { rotate, value } => {
                        u32::from(value).rotate_right(u32::from(rotate) * 2)
                    }
                };
                self.msr(spsr, fields, value);
                Flow::Sequential
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
            } => self.exec_load_store(load, byte, writeback, up, pre_index, rd, rn, offset, bus),
            ArmInstr::LoadStoreHalfwordSigned {
                kind,
                writeback,
                up,
                pre_index,
                rd,
                rn,
                offset,
            } => self.exec_halfword_transfer(kind, writeback, up, pre_index, rd, rn, offset, bus),
            ArmInstr::BlockTransfer {
                load,
                psr_and_force_user,
                writeback,
                up,
                pre_index,
                rn,
                registers,
            } => self.exec_block_transfer(
                load,
                psr_and_force_user,
                writeback,
                up,
                pre_index,
                rn,
                registers,
                bus,
            ),
            ArmInstr::Branch { link, offset } => {
                if link {
                    // Return convention: the branch address plus 4; the
                    // callee returns with `sub pc, lr, #4`.
                    self.regs[Register::Lr.bits() as usize] = self.pc.wrapping_add(4);
                }
                self.pc = self.pc.wrapping_add(8).wrapping_add(offset as u32);
                Flow::Handled
            }
            ArmInstr::BranchExchange { rn } => {
                let target = self.reg(rn);
                if target & 1 == 1 {
                    self.set_flag_bits(0x20, 0x20); // T = 1, THUMB
                    self.pc = target & !1;
                } else {
                    self.set_flag_bits(0x20, 0);
                    self.pc = target & !3;
                }
                Flow::Handled
            }
            ArmInstr::SoftwareInterrupt { imm24 } => {
                if self.bios_hle {
                    // ARM state carries the call number in the comment
                    // field, bits 16-23 (GBATEK: the BIOS reads the
                    // instruction at r14_svc - 4); THUMB carries it in
                    // bits 0-7 and pre-shifts at its dispatch site.
                    if crate::swi::execute(self, bus, imm24 >> 16) {
                        self.pc = self.pc.wrapping_add(4);
                    }
                } else {
                    let lr = self.pc.wrapping_add(4);
                    self.exception(Mode::Supervisor, lr, 0x08, false);
                }
                Flow::Handled
            }
            ArmInstr::Undefined { .. } => {
                self.undefined_exception();
                Flow::Handled
            }
        }
    }

    /// Data processing with full shifter and ALU flag semantics.
    fn exec_data_processing(
        &mut self,
        opcode: DataOp,
        set_flags: bool,
        rd: Register,
        rn: Register,
        operand2: ShifterOperand,
    ) -> Flow {
        // A register-specified shift spends an extra register-file read
        // cycle: R15 reads as instruction + 12 for both operand
        // registers (ARM ARM A5.2/E5.2 note; the external arm suite
        // pins `mov r0, pc, lsl r0` and `add r0, pc, r0, lsl r0`).
        let late_pc = matches!(
            &operand2,
            ShifterOperand::Register {
                shift: RegisterShift {
                    amount: ShiftAmount::Register(_),
                    ..
                },
                ..
            }
        );
        let a = if late_pc && rn == Register::Pc {
            self.pc.wrapping_add(12)
        } else {
            self.reg(rn)
        };
        let (b, shift_carry) = self.eval_operand2_late_pc(operand2, late_pc);
        let comparison = matches!(
            opcode,
            DataOp::Tst | DataOp::Teq | DataOp::Cmp | DataOp::Cmn
        );
        // (result, carry, overflow, is_logical)
        let (res, c_out, v_out, logical) = match opcode {
            DataOp::And | DataOp::Tst => (a & b, shift_carry, false, true),
            DataOp::Eor | DataOp::Teq => (a ^ b, shift_carry, false, true),
            DataOp::Sub | DataOp::Cmp => {
                let (res, c, v) = sub_with_borrow(a, b, false);
                (res, c, v, false)
            }
            DataOp::Rsb => {
                let (res, c, v) = sub_with_borrow(b, a, false);
                (res, c, v, false)
            }
            DataOp::Add | DataOp::Cmn => {
                let (res, c, v) = add_with_carry(a, b, false);
                (res, c, v, false)
            }
            DataOp::Adc => {
                let (res, c, v) = add_with_carry(a, b, self.flag_c());
                (res, c, v, false)
            }
            DataOp::Sbc => {
                let (res, c, v) = sub_with_borrow(a, b, !self.flag_c());
                (res, c, v, false)
            }
            DataOp::Rsc => {
                let (res, c, v) = sub_with_borrow(b, a, !self.flag_c());
                (res, c, v, false)
            }
            DataOp::Orr => (a | b, shift_carry, false, true),
            DataOp::Mov => (b, shift_carry, false, true),
            DataOp::Bic => (a & !b, shift_carry, false, true),
            DataOp::Mvn => (!b, shift_carry, false, true),
        };
        if !comparison {
            self.set_reg(rd, res);
        }
        if set_flags || comparison {
            let (c, v) = if logical {
                (Some(c_out), None)
            } else {
                (Some(c_out), Some(v_out))
            };
            self.set_flags(res & 0x8000_0000 != 0, res == 0, c, v);
            if !comparison && rd == Register::Pc && self.mode().bank() != Bank::Usr {
                // `MOVS PC, LR`: restore CPSR from SPSR, which may switch
                // mode and return to THUMB state (ARM ARM A2.6). User and
                // System have no SPSR, so only the flags update applies.
                let spsr = self.spsr_of_current();
                self.restore_cpsr(spsr);
            }
            if comparison && set_flags && rd == Register::Pc {
                self.write_cpsr_control_from_result(res);
            }
        }
        if !comparison && rd == Register::Pc {
            // Data-processing writes to PC do not interwork on ARMv4T:
            // bits 1:0 are ignored (ARM ARM A2.3).
            self.pc = res & !3;
            Flow::Handled
        } else {
            Flow::Sequential
        }
    }

    /// Second-operand evaluation with the register-specified-shift
    /// pipeline rule: `late_pc` makes an R15 operand read instruction
    /// + 12 instead of the normal + 8.
    fn eval_operand2_late_pc(&self, operand: ShifterOperand, late_pc: bool) -> (u32, bool) {
        match operand {
            ShifterOperand::Immediate { rotate, value } => {
                let v = u32::from(value).rotate_right(u32::from(rotate) * 2);
                let carry = if rotate == 0 {
                    self.flag_c()
                } else {
                    v & 0x8000_0000 != 0
                };
                (v, carry)
            }
            ShifterOperand::Register { rm, shift } => {
                let raw = if late_pc && rm == Register::Pc {
                    self.pc.wrapping_add(12)
                } else {
                    self.reg(rm)
                };
                match shift.amount {
                    ShiftAmount::Immediate(n) => {
                        let amount = match shift.kind {
                            // LSR #0 / ASR #0 encode as 32; RRX is its
                            // own kind.
                            ShiftKind::Lsr | ShiftKind::Asr if n == 0 => 32,
                            _ => u32::from(n),
                        };
                        let out = apply_shift(raw, shift.kind, amount, self.flag_c());
                        (out.value, out.carry)
                    }
                    ShiftAmount::Register(rs) => {
                        let amount = u32::from((self.reg(rs) & 0xFF) as u8);
                        let out = apply_shift(raw, shift.kind, amount, self.flag_c());
                        (out.value, out.carry)
                    }
                }
            }
        }
    }

    /// `MSR CPSR/SPSR, <op>` with the field mask (bits 19..16: c, x, s,
    /// f). ARM7TDMI has no S or E bits, so those fields are ignored; in
    /// User mode only the flags field is writable (ARM ARM A2.6/A4.1.8).
    fn msr(&mut self, spsr: bool, fields: u8, value: u32) {
        if spsr {
            if !matches!(self.mode(), Mode::User | Mode::System) {
                let i = bank_index(self.mode().bank());
                let old = self.spsr[i];
                self.spsr[i] = self.apply_psr_fields(old, fields, value);
            }
            return;
        }
        if fields & 0b1000 != 0 {
            // Flags field: NZCV, writable from any mode.
            self.set_flag_bits(0xF000_0000, value);
        }
        if fields & 0b0001 != 0 && self.mode() != Mode::User {
            // Control field: I, F, T and the mode bits. System mode is
            // privileged and may write it (real ROM setup code enables
            // IRQs from System state); only User may not. A mode write
            // banks the registers for the following instructions. The
            // ARMv4T architecture makes T-bit writes via MSR
            // UNPREDICTABLE (ARM ARM); they are honored here, matching
            // common de-facto behavior - THUMB entry should use BX.
            if let Some(m) = Mode::from_bits((value & 0x1F) as u8) {
                self.switch_mode(m);
            }
            self.set_flag_bits(0xE0, value);
        }
        // s (0b0100) and x (0b0010): no SPSR S/E bits on ARM7TDMI.
    }

    /// ARMv2-era CMPP/TSTP/TEQP/CMNP heritage: a comparison with S=1
    /// and Rd=15 has an architecturally moot destination, and the
    /// ARM7TDMI spends the corner writing the result's PSR bits into
    /// the CPSR control tail: I = result bit 27, F = result bit 26,
    /// mode = 0x10 | result[1:0] (GBATEK "Data Processing", "{P}
    /// opcodes"; the external arm suite pins `cmp pc, pc, r0` as a
    /// mode change). NZCV come from the normal flag update. User mode
    /// keeps NZCV only. The pipeline is not flushed.
    fn write_cpsr_control_from_result(&mut self, result: u32) {
        if self.mode() == Mode::User {
            return;
        }
        let i = u32::from(result & 0x0800_0000 != 0) << 7;
        let f = u32::from(result & 0x0400_0000 != 0) << 6;
        self.cpsr = (self.cpsr & !0xC0) | i | f;
        // The old 26-bit mode pair M1,M0 maps onto the low two GBA
        // mode bits; 0x10 | 0..=3 is always a valid mode.
        if let Some(m) = Mode::from_bits(0x10 | (result & 3) as u8) {
            self.switch_mode(m);
        }
    }

    /// Masks `value` through `fields` onto `old` (for SPSR writes,
    /// which have no user-mode restriction on fields).
    fn apply_psr_fields(&self, old: u32, fields: u8, value: u32) -> u32 {
        let mut out = old;
        if fields & 0b1000 != 0 {
            out = (out & !0xF000_0000) | (value & 0xF000_0000);
        }
        if fields & 0b0100 != 0 {
            out = (out & !0x00FF_FF00) | (value & 0x00FF_FF00);
        }
        if fields & 0b0010 != 0 {
            out = (out & !0x0000_00F0) | (value & 0x0000_00F0);
        }
        if fields & 0b0001 != 0 {
            out = (out & !0x0000_00FF) | (value & 0x0000_00FF);
        }
        out
    }

    /// Full CPSR restore from an SPSR (`MOVS PC` / `LDM .. {pc}^`).
    /// A corrupt SPSR mode field keeps the current mode (UNPREDICTABLE
    /// on hardware) and updates the remaining bits.
    fn restore_cpsr(&mut self, value: u32) {
        if let Some(m) = Mode::from_bits((value & 0x1F) as u8) {
            self.switch_mode(m);
            self.cpsr = value;
        } else {
            self.set_flag_bits(0xF000_00E0, value);
        }
    }

    /// Single word/byte load-store with pre/post indexing, writeback,
    /// and the ARM7 quirks: STR of R15 stores the instruction address
    /// plus 12, unaligned word loads rotate the aligned word, and LDR
    /// of R15 forces word alignment without interworking.
    #[allow(clippy::too_many_arguments)]
    fn exec_load_store(
        &mut self,
        load: bool,
        byte: bool,
        writeback: bool,
        up: bool,
        pre_index: bool,
        rd: Register,
        rn: Register,
        offset: decode::TransferOffset,
        bus: &mut Bus,
    ) -> Flow {
        let base = self.reg(rn);
        let off = match offset {
            decode::TransferOffset::Immediate(i) => i,
            decode::TransferOffset::Register { rm, shift } => {
                let raw = self.reg(rm);
                let amount = match shift.amount {
                    ShiftAmount::Immediate(n) => {
                        if matches!(shift.kind, ShiftKind::Lsr | ShiftKind::Asr) && n == 0 {
                            32
                        } else {
                            u32::from(n)
                        }
                    }
                    // Register-specified shift amounts are not encodable
                    // in ARMv4 transfers; treat the register as the
                    // amount defensively.
                    ShiftAmount::Register(r) => self.reg(r) & 0xFF,
                };
                apply_shift(raw, shift.kind, amount, self.flag_c()).value
            }
        };
        let addr = if pre_index {
            if up {
                base.wrapping_add(off)
            } else {
                base.wrapping_sub(off)
            }
        } else {
            base
        };
        self.cycles += u64::from(bus.access_cost(addr, if byte { 1 } else { 4 }));
        let mut flow = Flow::Sequential;
        if load {
            let value = if byte {
                u32::from(bus.read_u8(addr))
            } else {
                bus.read_u32(addr & !3).rotate_right((addr & 3) * 8)
            };
            if rd == Register::Pc {
                self.pc = value & !3;
                flow = Flow::Handled;
            } else {
                self.regs[rd.bits() as usize] = value;
            }
        } else {
            let value = if rd == Register::Pc {
                self.pc.wrapping_add(12)
            } else {
                self.regs[rd.bits() as usize]
            };
            if byte {
                bus.write_u8(addr, value as u8);
            } else {
                bus.write_u32(addr, value);
            }
        }
        // Post-indexed forms always write back (the W bit is the T bit
        // there); pre-indexed write back only with W. A load into the
        // base register wins over writeback (ARM ARM A4.1.6).
        let final_base = if pre_index {
            addr
        } else if up {
            base.wrapping_add(off)
        } else {
            base.wrapping_sub(off)
        };
        if (writeback || !pre_index) && !(load && rn == rd) && rn != Register::Pc {
            self.regs[rn.bits() as usize] = final_base;
        }
        flow
    }

    /// Halfword and signed-byte transfers (STRH/LDRH/LDRSB/LDRSH).
    #[allow(clippy::too_many_arguments)]
    fn exec_halfword_transfer(
        &mut self,
        kind: HalfwordKind,
        writeback: bool,
        up: bool,
        pre_index: bool,
        rd: Register,
        rn: Register,
        offset: decode::HalfwordOffset,
        bus: &mut Bus,
    ) -> Flow {
        let base = self.reg(rn);
        let off = match offset {
            decode::HalfwordOffset::Immediate(i) => u32::from(i),
            decode::HalfwordOffset::Register(r) => self.reg(r),
        };
        let addr = if pre_index {
            if up {
                base.wrapping_add(off)
            } else {
                base.wrapping_sub(off)
            }
        } else {
            base
        };
        self.cycles += u64::from(bus.access_cost(addr, 2));
        let mut flow = Flow::Sequential;
        match kind {
            HalfwordKind::Strh => {
                let value = if rd == Register::Pc {
                    self.pc.wrapping_add(12)
                } else {
                    self.regs[rd.bits() as usize]
                };
                bus.write_u16(addr, value as u16);
            }
            HalfwordKind::Ldrh => {
                // ARM7 quirk (GBATEK "Mis-aligned LDRH,LDRSH"): an odd
                // address reads the aligned halfword rotated right by
                // 8, scattering the bytes into bits 0..7 and 24..31.
                let value = if addr & 1 == 1 {
                    u32::from(bus.read_u16(addr & !1)).rotate_right(8)
                } else {
                    u32::from(bus.read_u16(addr))
                };
                if rd == Register::Pc {
                    self.pc = value & !3;
                    flow = Flow::Handled;
                } else {
                    self.regs[rd.bits() as usize] = value;
                }
            }
            HalfwordKind::Ldrsb => {
                let value = bus.read_u8(addr) as i8 as i32 as u32;
                if rd == Register::Pc {
                    self.pc = value & !3;
                    flow = Flow::Handled;
                } else {
                    self.regs[rd.bits() as usize] = value;
                }
            }
            HalfwordKind::Ldrsh => {
                // Odd addresses sign-expand the single addressed byte
                // instead (ARM7 quirk, same GBATEK note).
                let value = if addr & 1 == 1 {
                    bus.read_u8(addr) as i8 as i32 as u32
                } else {
                    bus.read_u16(addr) as i16 as i32 as u32
                };
                if rd == Register::Pc {
                    self.pc = value & !3;
                    flow = Flow::Handled;
                } else {
                    self.regs[rd.bits() as usize] = value;
                }
            }
        }
        let final_base = if pre_index {
            addr
        } else if up {
            base.wrapping_add(off)
        } else {
            base.wrapping_sub(off)
        };
        if (writeback || !pre_index)
            && !(matches!(
                kind,
                HalfwordKind::Ldrh | HalfwordKind::Ldrsb | HalfwordKind::Ldrsh
            ) && rn == rd)
            && rn != Register::Pc
        {
            self.regs[rn.bits() as usize] = final_base;
        }
        flow
    }

    /// Block transfer (LDM/STM) with the S-bit user-bank force and
    /// exception-return forms, plus the ARM7 de-facto register-list
    /// quirks (GBATEK "Strange Effects on Invalid Rlist's"): an empty
    /// list transfers only R15 and moves the base by 0x40; with the
    /// base inside the list, STM stores the old base only when the base
    /// is the first list entry and the new base otherwise, while LDM
    /// performs no writeback at all (ARMv4).
    #[allow(clippy::too_many_arguments)]
    fn exec_block_transfer(
        &mut self,
        load: bool,
        psr_and_force_user: bool,
        writeback: bool,
        up: bool,
        pre_index: bool,
        rn: Register,
        registers: RegisterList,
        bus: &mut Bus,
    ) -> Flow {
        let base = self.reg(rn);
        let empty = registers.is_empty();
        let total = if empty { 0x40 } else { registers.len() * 4 };
        let first = if up {
            if pre_index {
                base.wrapping_add(4)
            } else {
                base
            }
        } else if pre_index {
            // DB: transfer at base-total..base-4.
            base.wrapping_sub(total)
        } else {
            // DA: transfer at base..base-total+4.
            base.wrapping_sub(total).wrapping_add(4)
        };
        let pc_in_list = empty || registers.contains(Register::Pc);
        let exception_return =
            load && psr_and_force_user && pc_in_list && self.mode().bank() != Bank::Usr;
        // Lowest-numbered register of the list: the first entry stored.
        let first_entry = (0u8..16)
            .map(Register::from_bits)
            .find(|r| !empty && registers.contains(*r));
        let mut addr = first;
        let mut loaded_pc = None;
        for index in 0u8..16 {
            let r = Register::from_bits(index);
            let transfer = if empty {
                r == Register::Pc
            } else {
                registers.contains(r)
            };
            if !transfer {
                continue;
            }
            self.cycles += u64::from(bus.access_cost(addr, 4));
            if load {
                let value = bus.read_u32(addr);
                if r == Register::Pc {
                    loaded_pc = Some(value & !3);
                } else if psr_and_force_user && !pc_in_list {
                    self.set_reg_user(r, value);
                } else {
                    self.set_reg(r, value);
                }
            } else {
                // STM of R15 stores the instruction address plus 12 on
                // ARM7, like STR. With the base in the list, the base
                // slot receives the old base only when it is the first
                // list entry, otherwise the post-writeback value.
                let value = if r == Register::Pc {
                    self.pc.wrapping_add(12)
                } else if r == rn && Some(r) != first_entry {
                    if up {
                        base.wrapping_add(total)
                    } else {
                        base.wrapping_sub(total)
                    }
                } else if psr_and_force_user {
                    self.reg_user(r)
                } else {
                    self.reg(r)
                };
                bus.write_u32(addr, value);
            }
            addr = addr.wrapping_add(4);
        }
        let final_base = if up {
            base.wrapping_add(total)
        } else {
            base.wrapping_sub(total)
        };
        let base_in_list = !empty && registers.contains(rn);
        let do_writeback = if empty {
            true // empty list always adjusts the base by 0x40
        } else if load && base_in_list {
            false // ARMv4: no writeback when the base is loaded
        } else {
            writeback && rn != Register::Pc
        };
        if do_writeback {
            self.regs[rn.bits() as usize] = final_base;
        }
        if let Some(target) = loaded_pc {
            if exception_return {
                // Restore CPSR from the current mode's SPSR; this may
                // switch mode and return to THUMB state.
                let spsr = self.spsr_of_current();
                self.restore_cpsr(spsr);
            }
            self.pc = target;
            return Flow::Handled;
        }
        Flow::Sequential
    }

    /// Executes one decoded THUMB instruction (GBATEK "THUMB Opcodes").
    /// Undefined encodings raise the undefined-instruction exception;
    /// nothing panics.
    fn execute_thumb(&mut self, instr: ThumbInstr, bus: &mut Bus) -> Flow {
        match instr {
            ThumbInstr::ShiftedImmediate { kind, rd, rm, imm5 } => {
                let shift_kind = match kind {
                    ThumbShift::Lsl => ShiftKind::Lsl,
                    ThumbShift::Lsr => ShiftKind::Lsr,
                    ThumbShift::Asr => ShiftKind::Asr,
                };
                // A zero immediate encodes a 32-place shift.
                let amount = match kind {
                    ThumbShift::Lsr | ThumbShift::Asr if imm5 == 0 => 32,
                    _ => u32::from(imm5),
                };
                let out = apply_shift(self.reg(rm), shift_kind, amount, self.flag_c());
                self.set_reg(rd, out.value);
                self.set_flags(
                    out.value & 0x8000_0000 != 0,
                    out.value == 0,
                    Some(out.carry),
                    None,
                );
                self.cycles += 1; // 1I
                Flow::Sequential
            }
            ThumbInstr::AddSubtract {
                subtract,
                immediate,
                rn_or_imm3,
                rs,
                rd,
            } => {
                let b = if immediate {
                    u32::from(rn_or_imm3)
                } else {
                    self.reg(Register::from_bits(rn_or_imm3))
                };
                let a = self.reg(rs);
                let (res, c, v) = if subtract {
                    sub_with_borrow(a, b, false)
                } else {
                    add_with_carry(a, b, false)
                };
                self.set_reg(rd, res);
                self.set_flags(res & 0x8000_0000 != 0, res == 0, Some(c), Some(v));
                Flow::Sequential
            }
            ThumbInstr::ImmediateOperation { op, rd, imm8 } => {
                let b = u32::from(imm8);
                match op {
                    ThumbImmOp::Mov => {
                        self.set_reg(rd, b);
                        self.set_flags(b & 0x8000_0000 != 0, b == 0, None, None);
                    }
                    ThumbImmOp::Cmp => {
                        let (res, c, v) = sub_with_borrow(self.reg(rd), b, false);
                        self.set_flags(res & 0x8000_0000 != 0, res == 0, Some(c), Some(v));
                    }
                    ThumbImmOp::Add => {
                        let (res, c, v) = add_with_carry(self.reg(rd), b, false);
                        self.set_reg(rd, res);
                        self.set_flags(res & 0x8000_0000 != 0, res == 0, Some(c), Some(v));
                    }
                    ThumbImmOp::Sub => {
                        let (res, c, v) = sub_with_borrow(self.reg(rd), b, false);
                        self.set_reg(rd, res);
                        self.set_flags(res & 0x8000_0000 != 0, res == 0, Some(c), Some(v));
                    }
                }
                Flow::Sequential
            }
            ThumbInstr::AluOperation { op, rd, rs } => {
                let a = self.reg(rd);
                let b = self.reg(rs);
                let logical = |cpu: &mut Self, res: u32| {
                    // AND/EOR/TST/ORR/BIC/MVN set N and Z only (GBATEK
                    // THUMB.4 flags table).
                    cpu.set_flags(res & 0x8000_0000 != 0, res == 0, None, None);
                };
                match op {
                    ThumbAluOp::Adc => {
                        let (res, c, v) = add_with_carry(a, b, self.flag_c());
                        self.set_reg(rd, res);
                        self.set_flags(res & 0x8000_0000 != 0, res == 0, Some(c), Some(v));
                    }
                    ThumbAluOp::Sbc => {
                        let (res, c, v) = sub_with_borrow(a, b, !self.flag_c());
                        self.set_reg(rd, res);
                        self.set_flags(res & 0x8000_0000 != 0, res == 0, Some(c), Some(v));
                    }
                    ThumbAluOp::Neg => {
                        let (res, c, v) = sub_with_borrow(0, b, false);
                        self.set_reg(rd, res);
                        self.set_flags(res & 0x8000_0000 != 0, res == 0, Some(c), Some(v));
                    }
                    ThumbAluOp::Cmp => {
                        let (res, c, v) = sub_with_borrow(a, b, false);
                        self.set_flags(res & 0x8000_0000 != 0, res == 0, Some(c), Some(v));
                    }
                    ThumbAluOp::Cmn => {
                        let (res, c, v) = add_with_carry(a, b, false);
                        self.set_flags(res & 0x8000_0000 != 0, res == 0, Some(c), Some(v));
                    }
                    ThumbAluOp::Mul => {
                        let res = a.wrapping_mul(b);
                        self.set_reg(rd, res);
                        // ARMv4: N,Z set, "carry flag destroyed" (GBATEK
                        // THUMB.4); modeled as unchanged.
                        self.set_flags(res & 0x8000_0000 != 0, res == 0, None, None);
                        self.cycles += 3;
                    }
                    ThumbAluOp::Lsl | ThumbAluOp::Lsr | ThumbAluOp::Asr | ThumbAluOp::Ror => {
                        let kind = match op {
                            ThumbAluOp::Lsl => ShiftKind::Lsl,
                            ThumbAluOp::Lsr => ShiftKind::Lsr,
                            ThumbAluOp::Asr => ShiftKind::Asr,
                            _ => ShiftKind::Ror,
                        };
                        let out = apply_shift(a, kind, b & 0xFF, self.flag_c());
                        self.set_reg(rd, out.value);
                        self.set_flags(
                            out.value & 0x8000_0000 != 0,
                            out.value == 0,
                            Some(out.carry),
                            None,
                        );
                        self.cycles += 1; // 1I
                    }
                    ThumbAluOp::And => {
                        let res = a & b;
                        self.set_reg(rd, res);
                        logical(self, res);
                    }
                    ThumbAluOp::Eor => {
                        let res = a ^ b;
                        self.set_reg(rd, res);
                        logical(self, res);
                    }
                    ThumbAluOp::Orr => {
                        let res = a | b;
                        self.set_reg(rd, res);
                        logical(self, res);
                    }
                    ThumbAluOp::Bic => {
                        let res = a & !b;
                        self.set_reg(rd, res);
                        logical(self, res);
                    }
                    ThumbAluOp::Mvn => {
                        let res = !b;
                        self.set_reg(rd, res);
                        logical(self, res);
                    }
                    ThumbAluOp::Tst => {
                        logical(self, a & b);
                    }
                }
                Flow::Sequential
            }
            ThumbInstr::HighRegisterOperation { op, rd, rs } => match op {
                ThumbHiOp::Bx => {
                    if rs == Register::Pc {
                        // BX PC: switches to ARM; PC = ($+4) AND NOT 2
                        // (GBATEK THUMB.5).
                        self.cpsr &= !0x20;
                        self.pc = self.pc.wrapping_add(4) & !2;
                    } else {
                        let target = self.reg(rs);
                        self.cpsr = (self.cpsr & !0x20) | ((target & 1) << 5);
                        self.pc = target & !1;
                    }
                    Flow::Handled
                }
                ThumbHiOp::Add => {
                    let res = self.reg_thumb(rd).wrapping_add(self.reg_thumb(rs));
                    if rd == Register::Pc {
                        // Data-op writes to PC do not interwork on
                        // ARMv4T: bit 0 is ignored.
                        self.pc = res & !1;
                        Flow::Handled
                    } else {
                        self.regs[rd.bits() as usize] = res;
                        Flow::Sequential
                    }
                }
                ThumbHiOp::Mov => {
                    let res = self.reg_thumb(rs);
                    if rd == Register::Pc {
                        self.pc = res & !1;
                        Flow::Handled
                    } else {
                        self.regs[rd.bits() as usize] = res;
                        Flow::Sequential
                    }
                }
                ThumbHiOp::Cmp => {
                    let (res, c, v) =
                        sub_with_borrow(self.reg_thumb(rd), self.reg_thumb(rs), false);
                    self.set_flags(res & 0x8000_0000 != 0, res == 0, Some(c), Some(v));
                    Flow::Sequential
                }
            },
            ThumbInstr::PcRelativeLoad { rd, offset } => {
                let addr = ((self.pc.wrapping_add(4)) & !3) + u32::from(offset) * 4;
                self.cycles += u64::from(bus.access_cost(addr, 4)) + 1;
                self.regs[rd.bits() as usize] = bus.read_u32(addr);
                Flow::Sequential
            }
            ThumbInstr::LoadStoreRegisterOffset { kind, rd, rb, ro } => {
                let addr = self.reg(rb).wrapping_add(self.reg(ro));
                self.thumb_word_byte_transfer(bus, kind, addr, rd);
                Flow::Sequential
            }
            ThumbInstr::LoadStoreSignedRegisterOffset { kind, rd, rb, ro } => {
                let addr = self.reg(rb).wrapping_add(self.reg(ro));
                self.thumb_halfword_transfer(bus, kind, addr, rd);
                Flow::Sequential
            }
            ThumbInstr::LoadStoreImmediateOffset {
                kind,
                rd,
                rb,
                offset,
            } => {
                let scale = if matches!(kind, ThumbMemOp::Str | ThumbMemOp::Ldr) {
                    4
                } else {
                    1
                };
                let addr = self.reg(rb).wrapping_add(u32::from(offset) * scale);
                self.thumb_word_byte_transfer(bus, kind, addr, rd);
                Flow::Sequential
            }
            ThumbInstr::LoadStoreHalfwordImmediate {
                load,
                rd,
                rb,
                offset,
            } => {
                let addr = self.reg(rb).wrapping_add(u32::from(offset) * 2);
                let kind = if load {
                    ThumbSignedMemOp::Ldrh
                } else {
                    ThumbSignedMemOp::Strh
                };
                self.thumb_halfword_transfer(bus, kind, addr, rd);
                Flow::Sequential
            }
            ThumbInstr::LoadStoreStack { load, rd, offset } => {
                let addr = self.reg(Register::Sp).wrapping_add(u32::from(offset) * 4);
                let kind = if load {
                    ThumbMemOp::Ldr
                } else {
                    ThumbMemOp::Str
                };
                self.thumb_word_byte_transfer(bus, kind, addr, rd);
                Flow::Sequential
            }
            ThumbInstr::LoadAddress { to_sp, rd, offset } => {
                let res = if to_sp {
                    self.reg(Register::Sp).wrapping_add(u32::from(offset) * 4)
                } else {
                    ((self.pc.wrapping_add(4)) & !3) + u32::from(offset) * 4
                };
                self.set_reg(rd, res);
                Flow::Sequential
            }
            ThumbInstr::AdjustStackPointer { subtract, offset } => {
                let off = u32::from(offset) * 4;
                let sp = self.reg(Register::Sp);
                self.regs[Register::Sp.bits() as usize] = if subtract {
                    sp.wrapping_sub(off)
                } else {
                    sp.wrapping_add(off)
                };
                Flow::Sequential
            }
            ThumbInstr::PushPop {
                pop,
                include_pc_or_lr,
                registers,
            } => self.exec_thumb_push_pop(pop, include_pc_or_lr, registers, bus),
            ThumbInstr::LoadStoreMultiple {
                load,
                rb,
                registers,
            } => self.exec_thumb_load_store_multiple(load, rb, registers, bus),
            ThumbInstr::ConditionalBranch { cond, offset } => {
                if condition_holds(cond, self.cpsr) {
                    self.pc = self
                        .pc
                        .wrapping_add(4)
                        .wrapping_add((i32::from(offset) * 2) as u32);
                    Flow::Handled
                } else {
                    Flow::Sequential
                }
            }
            ThumbInstr::SoftwareInterrupt { value } => {
                if self.bios_hle {
                    if crate::swi::execute(self, bus, u32::from(value)) {
                        self.pc = self.pc.wrapping_add(2);
                    }
                } else {
                    // THUMB SWI: R14_svc = PC+2, vector 0x08, F
                    // unchanged (GBATEK THUMB.17 "Execution SWI/BKPT").
                    self.exception(Mode::Supervisor, self.pc.wrapping_add(2), 0x08, false);
                }
                Flow::Handled
            }
            ThumbInstr::UnconditionalBranch { offset } => {
                self.pc = self
                    .pc
                    .wrapping_add(4)
                    .wrapping_add((i32::from(offset) * 2) as u32);
                Flow::Handled
            }
            ThumbInstr::LongBranchWithLink { offset } => {
                // One 32-bit instruction: target = $+4+offset*2 and
                // LR = ($+4) with the THUMB bit set (GBATEK THUMB.19,
                // "Second Instruction - PC = LR + (nn SHL 1), and
                // LR = PC+2 OR 1").
                self.regs[Register::Lr.bits() as usize] = (self.pc.wrapping_add(4)) | 1;
                self.pc = self.pc.wrapping_add(4).wrapping_add((offset * 2) as u32) & !1;
                self.cycles += 2; // second fetch + 1N
                Flow::Handled
            }
            ThumbInstr::LongBranchWithLinkFromLr { offset } => {
                // The second BL halfword standing alone: PC = LR+nn*2,
                // LR = $+2 with the THUMB bit set (GBATEK THUMB.19
                // note; Mario Golf Advance Tour uses F800h).
                let target = self
                    .reg(Register::Lr)
                    .wrapping_add((i32::from(offset) * 2) as u32)
                    & !1;
                self.regs[Register::Lr.bits() as usize] = (self.pc.wrapping_add(2)) | 1;
                self.pc = target;
                self.cycles += 1;
                Flow::Handled
            }
            ThumbInstr::Undefined { encoding: _ } => {
                // The ARM7TDMI has no documented behavior for undefined
                // THUMB halfwords; raise the undefined-instruction
                // exception with the THUMB pipeline offset (mirrors the
                // SWI PC+2 rule in GBATEK THUMB.17).
                self.exception(Mode::Undefined, self.pc.wrapping_add(2), 0x04, false);
                Flow::Handled
            }
        }
    }

    /// THUMB word/byte transfer with the ARM7 misalignment rules (the
    /// same quirks as the ARM single-transfer path: unaligned word
    /// loads rotate the aligned word; byte transfers use the exact
    /// address).
    fn thumb_word_byte_transfer(
        &mut self,
        bus: &mut Bus,
        kind: ThumbMemOp,
        addr: u32,
        rd: Register,
    ) {
        match kind {
            ThumbMemOp::Str => {
                self.cycles += u64::from(bus.access_cost(addr, 4));
                bus.write_u32(addr, self.regs[rd.bits() as usize]);
            }
            ThumbMemOp::Ldr => {
                self.cycles += u64::from(bus.access_cost(addr, 4));
                self.regs[rd.bits() as usize] =
                    bus.read_u32(addr & !3).rotate_right((addr & 3) * 8);
            }
            ThumbMemOp::Strb => {
                self.cycles += u64::from(bus.access_cost(addr, 1));
                bus.write_u8(addr, self.regs[rd.bits() as usize] as u8);
            }
            ThumbMemOp::Ldrb => {
                self.cycles += u64::from(bus.access_cost(addr, 1));
                self.regs[rd.bits() as usize] = u32::from(bus.read_u8(addr));
            }
        }
    }

    /// THUMB halfword and signed-byte transfer with the ARM7 quirks:
    /// LDRH from an odd address returns the aligned halfword rotated
    /// right by 8, and LDSH from an odd address sign-extends the single
    /// addressed byte (GBATEK "Mis-aligned LDRH,LDRSH").
    fn thumb_halfword_transfer(
        &mut self,
        bus: &mut Bus,
        kind: ThumbSignedMemOp,
        addr: u32,
        rd: Register,
    ) {
        match kind {
            ThumbSignedMemOp::Strh => {
                self.cycles += u64::from(bus.access_cost(addr, 2));
                bus.write_u16(addr, self.regs[rd.bits() as usize] as u16);
            }
            ThumbSignedMemOp::Ldrh => {
                self.cycles += u64::from(bus.access_cost(addr, 2));
                let value = if addr & 1 == 1 {
                    u32::from(bus.read_u16(addr & !1)).rotate_right(8)
                } else {
                    u32::from(bus.read_u16(addr))
                };
                self.regs[rd.bits() as usize] = value;
            }
            ThumbSignedMemOp::Ldsb => {
                self.cycles += u64::from(bus.access_cost(addr, 1));
                self.regs[rd.bits() as usize] = bus.read_u8(addr) as i8 as i32 as u32;
            }
            ThumbSignedMemOp::Ldsh => {
                self.cycles += u64::from(bus.access_cost(addr, 2));
                let value = if addr & 1 == 1 {
                    bus.read_u8(addr) as i8 as i32 as u32
                } else {
                    bus.read_u16(addr) as i16 as i32 as u32
                };
                self.regs[rd.bits() as usize] = value;
            }
        }
    }

    /// THUMB PUSH/POP (GBATEK THUMB.14). The stack is full descending;
    /// PUSH writes LR highest, POP reads PC highest. POP {PC} ignores
    /// bit 0 and stays in THUMB state on ARMv4 (GBATEK THUMB.14 note).
    /// An empty list transfers R15 and moves SP by 0x40 (GBATEK
    /// "Strange Effects on Invalid Rlist's", ARMv4 column).
    fn exec_thumb_push_pop(
        &mut self,
        pop: bool,
        include_pc_or_lr: bool,
        registers: u8,
        bus: &mut Bus,
    ) -> Flow {
        let empty = registers == 0 && !include_pc_or_lr;
        let count = registers.count_ones() + u32::from(include_pc_or_lr);
        // ARMv4 empty-list quirk: the base moves by 0x40 regardless.
        let total = if empty { 0x40 } else { count * 4 };
        let sp0 = self.reg(Register::Sp);
        let mut addr = if pop { sp0 } else { sp0.wrapping_sub(total) };
        for i in 0..8u16 {
            if registers & (1 << i) == 0 {
                continue;
            }
            let r = Register::from_bits(i as u8);
            self.cycles += u64::from(bus.access_cost(addr, 4));
            if pop {
                self.regs[r.bits() as usize] = bus.read_u32(addr);
            } else {
                bus.write_u32(addr, self.regs[r.bits() as usize]);
            }
            addr = addr.wrapping_add(4);
        }
        let mut flow = Flow::Sequential;
        if empty {
            // ARMv4: R15 is transferred (stored/loaded at the start
            // address), reading as $+4 like any THUMB operand.
            self.cycles += u64::from(bus.access_cost(addr, 4));
            if pop {
                self.pc = bus.read_u32(addr) & !1;
                flow = Flow::Handled;
            } else {
                bus.write_u32(addr, self.pc.wrapping_add(4));
            }
        } else if include_pc_or_lr {
            let tail = if pop { Register::Pc } else { Register::Lr };
            self.cycles += u64::from(bus.access_cost(addr, 4));
            if pop {
                // POP {PC} ignores bit 0 and stays in THUMB state
                // (GBATEK THUMB.14 note).
                self.pc = bus.read_u32(addr) & !1;
                flow = Flow::Handled;
            } else {
                bus.write_u32(addr, self.regs[tail.bits() as usize]);
            }
        }
        self.regs[Register::Sp.bits() as usize] = if pop {
            sp0.wrapping_add(total)
        } else {
            sp0.wrapping_sub(total)
        };
        flow
    }

    /// THUMB LDMIA/STMIA (GBATEK THUMB.15): ascending transfers with
    /// ARMv4 de-facto behavior for invalid lists (empty list transfers
    /// R15 and bumps the base by 0x40; a list containing the base
    /// stores the old base on STM when the base is the first entry, and
    /// never writes back on LDM).
    fn exec_thumb_load_store_multiple(
        &mut self,
        load: bool,
        rb: Register,
        registers: u8,
        bus: &mut Bus,
    ) -> Flow {
        let empty = registers == 0;
        let count = registers.count_ones();
        let total = if empty { 0x40 } else { count * 4 };
        let base = self.reg(rb);
        let mut addr = base;
        let first_entry = (0..8u16)
            .map(|i| (i, Register::from_bits(i as u8)))
            .find(|(i, _)| !empty && registers & (1 << i) != 0)
            .map(|(_, r)| r);
        for i in 0..8u16 {
            if registers & (1 << i) == 0 {
                continue;
            }
            let r = Register::from_bits(i as u8);
            self.cycles += u64::from(bus.access_cost(addr, 4));
            if load {
                let value = bus.read_u32(addr);
                if r == rb {
                    // The loaded base wins; no writeback afterwards.
                    self.regs[rb.bits() as usize] = value;
                } else {
                    self.regs[r.bits() as usize] = value;
                }
            } else {
                // With the base in the list, the base slot receives the
                // old base only when it is the first list entry,
                // otherwise the post-transfer value (ARMv4).
                let value = if r == rb && Some(r) != first_entry {
                    base.wrapping_add(total)
                } else {
                    self.regs[r.bits() as usize]
                };
                bus.write_u32(addr, value);
            }
            addr = addr.wrapping_add(4);
        }
        let mut jumped = false;
        if empty {
            // ARMv4: R15 is transferred at the start address, reading
            // as $+4 like any THUMB operand; the base moves by 0x40.
            self.cycles += u64::from(bus.access_cost(addr, 4));
            if load {
                self.pc = bus.read_u32(addr) & !1;
                jumped = true;
            } else {
                // The block transfer samples R15 late: the +4 operand
                // read gains the 2-byte THUMB pipeline offset (the
                // ARM-state equivalent stores address+12).
                bus.write_u32(addr, self.pc.wrapping_add(6));
            }
        }
        let base_in_list = !empty && registers & (1 << u16::from(rb.bits())) != 0;
        if empty || !(load && base_in_list) {
            self.regs[rb.bits() as usize] = base.wrapping_add(total);
        }
        if jumped {
            // The empty-list load acts as `ldr pc, [rb]`; the caller
            // must not advance pc past the transferred target.
            Flow::Handled
        } else {
            Flow::Sequential
        }
    }
}
