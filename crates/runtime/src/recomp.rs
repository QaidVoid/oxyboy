//! The recompiled-execution driver: translated functions running over
//! the real bus.
//!
//! [`Driver::step`] replaces the interpreter for one execution unit:
//! when the PC sits at a translated function's entry, the compiled C
//! function runs natively over a [`GuestState`] snapshot of the CPU,
//! with every memory operation going through the host callbacks here
//! (the same bus paths the interpreter uses, waitstate charging
//! included). Any control transfer the IR model does not cover (SWI,
//! CPSR intrinsics, indirect exits, untranslated callees) or a
//! backward-branch budget exhaustion sets `exited`, and the driver
//! resumes interpretation at the recorded PC.
//!
//! Accuracy level: native chunks charge
//! memory ops at real waitstate costs plus one cycle per IR
//! instruction; opcode fetch, internal cycles, and IRQ boundary checks
//! inside a chunk are not modeled, so video timing shifts within a
//! chunk. State equality is the contract; cycle equality is not.

use core::ffi::c_void;

use crate::bus::Bus;
use crate::system::System;
use recomp::Module;
use recomp::abi::{GuestOps, GuestState, SIZE_BYTE, SIZE_HALF, SIZE_WORD};

/// The default native-instruction budget per call.
pub const DEFAULT_BUDGET: u64 = 1_000_000;

/// A loaded translation plus its entry table.
pub struct Driver {
    module: Module,
    /// `(entry address, Thumb state)` pairs, ascending by address.
    entries: Vec<(u32, bool)>,
    budget: u64,
}

impl Driver {
    /// Wraps a loaded module: `entries` maps each translated function
    /// to its guest entry address and instruction state.
    #[must_use]
    pub fn new(module: Module, entries: Vec<(u32, bool)>) -> Self {
        let mut entries = entries;
        entries.sort_unstable();
        Driver {
            module,
            entries,
            budget: DEFAULT_BUDGET,
        }
    }

    /// Whether `pc` is a translated function entry.
    #[must_use]
    pub fn is_translated(&self, pc: u32) -> bool {
        self.entries.iter().any(|&(addr, _)| addr == pc)
    }

    /// The instruction state of a translated function entry.
    fn thumb_at(&self, pc: u32) -> Option<bool> {
        self.entries
            .iter()
            .find(|&&(addr, _)| addr == pc)
            .map(|&(_, thumb)| thumb)
    }

    /// Runs one unit: a whole native function call when the PC is at
    /// a translated entry, one interpreter instruction otherwise.
    /// Returns the cycles charged.
    ///
    /// # Errors
    ///
    /// Returns a printable message when the native call fails (missing
    /// symbol or loader error).
    pub fn step(&mut self, system: &mut System) -> Result<u32, String> {
        let pc = system.cpu.pc();
        match self.thumb_at(pc) {
            Some(thumb) => self.run_native(system, pc, thumb),
            None => Ok(system.step()),
        }
    }

    /// Runs whole frames through [`Driver::step`], ending at a frame
    /// boundary (the same point plain `step_frame` stops at).
    ///
    /// # Errors
    ///
    /// Propagates native-call failures.
    pub fn run_frame(&mut self, system: &mut System) -> Result<(), String> {
        while !system.frame_ended() {
            self.step(system)?;
        }
        system.step_frame();
        Ok(())
    }

    fn run_native(&mut self, system: &mut System, entry: u32, thumb: bool) -> Result<u32, String> {
        let mut st = GuestState {
            r: [0; 16],
            n: 0,
            z: 0,
            c: 0,
            v: 0,
            pc: entry,
            exited: 0,
            cycles: 0,
            icount: 0,
        };
        // r0-r14 are the current-mode register file; r15 is the PC
        // (a separate field on the CPU) and is taken below.
        for (i, slot) in st.r.iter_mut().take(15).enumerate() {
            *slot = system.cpu.reg_for_test(i as u8);
        }
        st.r[15] = entry;
        let cpsr = system.cpu.cpsr();
        st.n = u32::from(cpsr & 0x8000_0000 != 0);
        st.z = u32::from(cpsr & 0x4000_0000 != 0);
        st.c = u32::from(cpsr & 0x2000_0000 != 0);
        st.v = u32::from(cpsr & 0x1000_0000 != 0);

        let budget = self.budget;
        let ops = GuestOps {
            ctx: std::ptr::from_mut(system.bus_mut()).cast::<c_void>(),
            load: host_load,
            store: host_store,
            budget,
        };
        self.module.call(entry, &mut st, &ops)?;

        // Store the registers and flags back; the resume PC is the
        // return state's r15, or the exit PC.
        for (i, value) in st.r.iter().take(15).enumerate() {
            system.cpu.set_reg_for_test(i as u8, *value);
        }
        let flag_bits = (st.n << 31) | (st.z << 30) | (st.c << 29) | (st.v << 28);
        let mut cpsr = system.cpu.cpsr() & !0xF000_0000;
        cpsr |= flag_bits;
        // Resumes leave the function in its own instruction state.
        if thumb {
            cpsr |= 0x20;
        } else {
            cpsr &= !0x20;
        }
        system.cpu.set_cpsr_raw(cpsr);
        // A native return leaves `exited` clear with the outgoing pc
        // in r15; an exit sets `exited` and the resume pc.
        let resume = if st.exited != 0 { st.pc } else { st.r[15] };
        system.cpu.set_pc(resume);
        system.advance_clock(u32::try_from(st.cycles).unwrap_or(u32::MAX));
        Ok(u32::try_from(st.cycles).unwrap_or(u32::MAX))
    }
}

unsafe extern "C" fn host_load(
    ctx: *mut c_void,
    st: *mut GuestState,
    addr: u32,
    size: u32,
    sext: u32,
    _mode: u32,
) -> u32 {
    let bus = unsafe { &mut *(ctx.cast::<Bus>()) };
    let width = match size {
        SIZE_BYTE => 1,
        SIZE_HALF => 2,
        SIZE_WORD => 4,
        _ => 0,
    };
    if width > 0 {
        let cost = bus.access_cost(addr, width);
        unsafe { (*st).cycles += u64::from(cost) };
    }
    let raw = match size {
        SIZE_BYTE => u32::from(bus.read_u8(addr)),
        SIZE_HALF => u32::from(bus.read_u16(addr)),
        _ => bus.read_u32(addr),
    };
    // Signed half/byte loads extend the value the bus returned; the
    // interpreter's path sign-extends the same read.
    if sext == 0 {
        return raw;
    }
    match size {
        SIZE_BYTE => (raw as u8) as i8 as i32 as u32,
        SIZE_HALF => (raw as u16) as i16 as i32 as u32,
        _ => raw,
    }
}

unsafe extern "C" fn host_store(
    ctx: *mut c_void,
    st: *mut GuestState,
    addr: u32,
    value: u32,
    size: u32,
) {
    let bus = unsafe { &mut *(ctx.cast::<Bus>()) };
    let width = match size {
        SIZE_BYTE => 1,
        SIZE_HALF => 2,
        SIZE_WORD => 4,
        _ => 0,
    };
    if width > 0 {
        let cost = bus.access_cost(addr, width);
        unsafe { (*st).cycles += u64::from(cost) };
    }
    match size {
        SIZE_BYTE => bus.write_u8(addr, value as u8),
        SIZE_HALF => bus.write_u16(addr, value as u16),
        _ => bus.write_u32(addr, value),
    }
}
