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
//! resumes interpretation at the recorded PC. Opaque (SWI) blocks
//! hand the interpreter the block-leader state, so the HLE call and
//! its continuation run on the interpreter's cycle model; native
//! chunks feed the scheduler through [`System::advance_clock`] with
//! the chunk's charged cycles.
//!
//! Self-modifying code: every translated function's guest
//! range marks 1 KiB pages ([`PAGE_SHIFT`]) in a flat map the store
//! callback consults. A native store into a marked page aborts the
//! chunk before the store happens and resumes interpretation at the
//! storing instruction, so the store re-executes interpreted and the
//! patched bytes are honored from then on. Interpreted stores, DMA
//! included, are watched per step; a write touching any page of a
//! translated function drops that function permanently (nothing
//! retranslates at runtime). Documented limits: page granularity
//! means data sharing a 1 KiB page with code invalidates that code;
//! extents are the caller's analysis regions, so trailing padding
//! marks extra pages; only the canonical 28-bit address is checked, a
//! patch written through a non-canonical mirror of translated code is
//! not detected; and a DMA armed mid-chunk drains at that chunk's
//! clock advance (the deferred drain lives in
//! [`System::advance_clock`]), still inside the step's watched
//! window.
//!
//! Accuracy level: native chunks charge the interpreter's
//! exact per-instruction model. The lifted IR carries one cycle
//! charge per guest instruction (fetch address plus internal-cycle
//! extras); the emitter plays them in program order through the
//! host's `fetch` callback, which prices the bus with the same
//! `fetch_cost` the interpreter uses (WAITCNT waitstates, sequential
//! tracking, and the Game Pak prefetch buffer included), and the
//! load/store callbacks charge the data accesses as before. Failed
//! ARM conditions add their one internal cycle. On top of the
//! per-call instruction budget, [`Driver::run_frame`] bounds every
//! native call by the video cycles left before the frame boundary
//! (`GuestOps::cycle_budget`): a unit exits at a backward edge or
//! call once its charged cycles reach the bound, so idle loops cannot
//! overshoot the boundary and the frame runner stops where
//! [`System::step_frame`] does. Remaining gaps, all documented: IRQ
//! boundary checks inside a chunk (an interrupt
//! waits for the chunk to end), hardware interleaving within a chunk
//! (timers and the PPU advance when the chunk's cycles feed the
//! scheduler at its end), ARM function returns via unlifted
//! `ldr pc`/`ldm .., pc` (no charge; the unit falls back to the
//! interpreter), and a fully eliminated conditional instruction's
//! failed-condition cycle. State equality stays the contract; cycle
//! equality per instruction sequence is not yet guaranteed.

use core::ffi::c_void;

use crate::bus::{Bus, PAGE_SHIFT};
use crate::system::System;
use recomp::Module;
use recomp::abi::{GuestOps, GuestState, MODE_SWAP, SIZE_BYTE, SIZE_HALF, SIZE_WORD};

/// The default native-instruction budget per call.
pub const DEFAULT_BUDGET: u64 = 1_000_000;

/// The link-register value [`Driver::call_function`] plants for a
/// host-initiated call: a guest that returns lands here, and the host
/// owns the machine from that point. Not a mapped address.
pub const HOST_RETURN_SENTINEL: u32 = 0xFFFF_0000;

/// Number of 1 KiB pages over the canonical 28-bit bus.
const PAGE_COUNT: usize = 1 << (28 - PAGE_SHIFT);

/// The 1 KiB page index of a canonical bus address.
fn page_of(addr: u32) -> u32 {
    (addr & 0x0FFF_FFFF) >> PAGE_SHIFT
}

/// One translated function: the guest extent the analysis proved and
/// the instruction state at its entry. The extent marks the invalidation
/// pages, so conservative regions only cost spurious invalidations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TranslatedFn {
    /// Address of the first instruction; the dispatch key.
    pub start: u32,
    /// One past the last byte of the function's code.
    pub end: u32,
    /// Instruction state at `start`.
    pub thumb: bool,
}

/// Host context the emitted code hands back to every callback. Opaque
/// to the C side; the host callbacks dereference it here.
struct HostCtx {
    system: *mut System,
    code_pages: *const u8,
    /// The chunk's charged cycles when the current guest instruction
    /// began (before its fetch).
    inst_start: u64,
    /// Cycles already fed to the scheduler during this chunk.
    synced: u64,
}

impl HostCtx {
    fn bus(&mut self) -> &mut Bus {
        unsafe { (*self.system).bus_mut() }
    }

    /// Before an IO access, brings the PPU, timers, and APU up to the
    /// start of the accessing instruction: where the interpreter has
    /// them when the same instruction runs. A native poll of VCOUNT or
    /// a timer counter then sees time pass as on hardware.
    fn sync_for_io(&mut self, addr: u32) {
        if (addr >> 24) & 0x0F != 0x04 || self.inst_start <= self.synced {
            return;
        }
        let pending = self.inst_start - self.synced;
        self.synced = self.inst_start;
        unsafe { (*self.system).advance_clock(u32::try_from(pending).unwrap_or(u32::MAX)) };
    }
}

/// A loaded translation plus its entry table.
pub struct Driver {
    module: Module,
    /// Translated functions, ascending by entry address.
    entries: Vec<TranslatedFn>,
    /// Per-1 KiB-page map over the canonical bus: nonzero when some
    /// translated function has code in the page.
    code_pages: Vec<u8>,
    budget: u64,
    /// Cycle bound for one native call: backward edges and calls exit
    /// once the unit's charged cycles reach it. `u64::MAX` (the
    /// default) leaves the instruction [`DEFAULT_BUDGET`] alone as the
    /// only bound; the frame runner sets it to the remaining video
    /// cycles so a unit cannot overshoot the frame boundary.
    cycle_budget: u64,
}

impl Driver {
    /// Wraps a loaded module: `entries` maps each translated function
    /// to its guest extent and entry instruction state.
    #[must_use]
    pub fn new(module: Module, entries: Vec<TranslatedFn>) -> Self {
        let mut entries = entries;
        entries.sort_unstable_by_key(|e| e.start);
        let mut code_pages = vec![0u8; PAGE_COUNT];
        for entry in &entries {
            Self::mark_pages(&mut code_pages, entry);
        }
        Driver {
            module,
            entries,
            code_pages,
            budget: DEFAULT_BUDGET,
            cycle_budget: u64::MAX,
        }
    }

    /// Marks every page the function's extent touches.
    fn mark_pages(code_pages: &mut [u8], entry: &TranslatedFn) {
        let first = page_of(entry.start) as usize;
        let last = page_of(entry.end.saturating_sub(1).max(entry.start)) as usize;
        for page in &mut code_pages[first..=last] {
            *page = 1;
        }
    }

    /// Drops every translated function overlapping any written page
    /// and rebuilds the page map. Writes outside translated pages are
    /// the common case and leave the module untouched.
    fn invalidate(&mut self, written: &[u32]) {
        if !written
            .iter()
            .any(|&addr| self.code_pages[page_of(addr) as usize] != 0)
        {
            return;
        }
        let pages: Vec<u32> = written.iter().copied().map(page_of).collect();
        self.entries.retain(|e| {
            let first = page_of(e.start);
            let last = page_of(e.end.saturating_sub(1).max(e.start));
            !(first..=last).any(|p| pages.contains(&p))
        });
        self.code_pages.fill(0);
        for entry in &self.entries {
            Self::mark_pages(&mut self.code_pages, entry);
        }
    }

    /// Whether `pc` is a translated function entry.
    #[must_use]
    pub fn is_translated(&self, pc: u32) -> bool {
        self.entry(pc).is_some()
    }

    /// How many translated function entries the driver holds.
    #[must_use]
    pub fn translated_count(&self) -> usize {
        self.entries.len()
    }

    /// The instruction state of a translated function entry.
    fn thumb_at(&self, pc: u32) -> Option<bool> {
        self.entry(pc).map(|e| e.thumb)
    }

    /// The translated function starting at `pc`; `entries` stays
    /// sorted by start, so this is a binary search on every step.
    fn entry(&self, pc: u32) -> Option<&TranslatedFn> {
        self.entries
            .binary_search_by_key(&pc, |e| e.start)
            .ok()
            .map(|index| &self.entries[index])
    }

    /// Runs one unit: a whole native function call when the PC is at
    /// a translated entry, one interpreter instruction otherwise.
    /// Returns the cycles charged.
    ///
    /// A native call that leaves the PC where it started handed
    /// control to the interpreter at its own entry (an opaque block at
    /// the function head); interpreting one instruction makes progress
    /// where re-entering the same entry would loop forever.
    ///
    /// # Errors
    ///
    /// Returns a printable message when the native call fails (missing
    /// symbol or loader error).
    pub fn step(&mut self, system: &mut System) -> Result<u32, String> {
        system.bus_mut().arm_write_watch();
        let result = self.step_unit(system);
        let watched = system.bus_mut().take_write_watch();
        self.invalidate(&watched);
        result
    }

    /// The one execution unit [`Driver::step`] wraps with SMC
    /// watching.
    fn step_unit(&mut self, system: &mut System) -> Result<u32, String> {
        let pc = system.cpu.pc();
        match self.thumb_at(pc) {
            Some(thumb) => {
                let cycles = self.run_native(system, pc, thumb)?;
                if system.cpu.pc() == pc {
                    return Ok(system.step());
                }
                Ok(cycles)
            }
            None => Ok(system.step()),
        }
    }

    /// Runs whole frames through [`Driver::step`], ending at a frame
    /// boundary (the same point plain `step_frame` stops at).
    ///
    /// Each step unit is bounded by the video cycles left before the
    /// next frame boundary ([`System::cycles_to_frame_end`]), so a
    /// translated unit cannot run far past it: backward edges and
    /// calls exit the native code once its charged cycles reach the
    /// bound, and the leftover charge is added through
    /// [`System::advance_clock`].
    ///
    /// # Errors
    ///
    /// Propagates native-call failures.
    pub fn run_frame(&mut self, system: &mut System) -> Result<(), String> {
        while !system.frame_ended() {
            let bound = system
                .cycles_to_frame_end()
                .min(system.cycles_to_interrupt_event());
            self.cycle_budget = u64::from(bound).max(1);
            self.step(system)?;
        }
        self.cycle_budget = u64::MAX;
        system.step_frame();
        Ok(())
    }

    /// One [`Driver::step`] unit with execution recording (the
    /// differential runner's trace capture). Interpreted units produce
    /// a full [`trace::Record`]; a native chunk produces `None` - the
    /// compiled code does not emit per-instruction records, so a trace
    /// built from this method carries gaps at every translated span
    /// and compares against the pure-interpreter oracle only up to the
    /// first chunk.
    ///
    /// # Errors
    ///
    /// Propagates native-call failures.
    pub fn step_recorded(
        &mut self,
        system: &mut System,
    ) -> Result<(u32, Option<trace::Record>), String> {
        system.bus_mut().arm_write_watch();
        let result = match self.thumb_at(system.cpu.pc()) {
            Some(thumb) => {
                let entry = system.cpu.pc();
                let cycles = self.run_native(system, entry, thumb)?;
                if system.cpu.pc() == entry {
                    // Opaque block at the entry: the interpreter made
                    // the progress, so the unit records normally.
                    let (cycles, record) = system.step_recorded();
                    Ok((cycles, Some(record)))
                } else {
                    Ok((cycles, None))
                }
            }
            None => Ok({
                let (cycles, record) = system.step_recorded();
                (cycles, Some(record))
            }),
        };
        let watched = system.bus_mut().take_write_watch();
        self.invalidate(&watched);
        result
    }

    /// Calls a translated guest function from the host with the ARM
    /// procedure-call convention: `args` land in r0-r3, lr is planted
    /// with [`HOST_RETURN_SENTINEL`], and the function runs natively
    /// exactly like the native half of [`Driver::step`]. The outgoing
    /// r0 is the return value; every other register, flag, and memory
    /// effect stays in the machine, and the chunk's cycles feed the
    /// scheduler.
    ///
    /// # Errors
    ///
    /// Returns a printable message when `start` is not a translated
    /// entry or the native call fails.
    pub fn call_function(
        &mut self,
        system: &mut System,
        start: u32,
        args: [u32; 4],
    ) -> Result<u32, String> {
        let Some(thumb) = self.thumb_at(start) else {
            return Err(format!("{start:#010x} is not a translated entry"));
        };
        for (index, value) in args.iter().enumerate() {
            system.cpu.set_reg_for_test(index as u8, *value);
        }
        system.cpu.set_reg_for_test(14, HOST_RETURN_SENTINEL);
        system.bus_mut().arm_write_watch();
        let result = self.run_native(system, start, thumb);
        let watched = system.bus_mut().take_write_watch();
        self.invalidate(&watched);
        result?;
        Ok(system.cpu.reg_for_test(0))
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
            thumb: u32::from(thumb),
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
        let mut ctx = HostCtx {
            system: std::ptr::from_mut(system),
            code_pages: self.code_pages.as_ptr(),
            inst_start: 0,
            synced: 0,
        };
        let ops = GuestOps {
            ctx: std::ptr::from_mut(&mut ctx).cast::<c_void>(),
            load: host_load,
            store: host_store,
            fetch: host_fetch,
            budget,
            cycle_budget: self.cycle_budget,
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
        // Resumes leave in the state the chunk recorded on exit: an
        // interworking exit (bx) overwrote `st.thumb`, everything else
        // still carries the entry state.
        if st.thumb != 0 {
            cpsr |= 0x20;
        } else {
            cpsr &= !0x20;
        }
        system.cpu.set_cpsr_raw(cpsr);
        // A native return leaves `exited` clear with the outgoing pc
        // in r15; an exit sets `exited` and the resume pc.
        let resume = if st.exited != 0 { st.pc } else { st.r[15] };
        system.cpu.set_pc(resume);
        let remaining = st.cycles.saturating_sub(ctx.synced);
        system.advance_clock(u32::try_from(remaining).unwrap_or(u32::MAX));
        Ok(u32::try_from(st.cycles).unwrap_or(u32::MAX))
    }
}

unsafe extern "C" fn host_load(
    ctx: *mut c_void,
    st: *mut GuestState,
    addr: u32,
    size: u32,
    sext: u32,
    mode: u32,
) -> u32 {
    let host = unsafe { &mut *(ctx.cast::<HostCtx>()) };
    host.sync_for_io(addr);
    let bus = host.bus();
    let width = match size {
        SIZE_BYTE => 1,
        SIZE_HALF => 2,
        SIZE_WORD => 4,
        _ => 0,
    };
    // A SWP lifts as an uncharged load plus a charged store: the
    // interpreter prices the exchange once, before the read.
    if width > 0 && mode != MODE_SWAP {
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

/// One opcode fetch inside a native chunk: charges the same bus cost
/// the interpreter's per-instruction fetches charge, so the
/// sequential tracker and the Game Pak prefetch buffer advance in
/// the same order relative to the data callbacks.
unsafe extern "C" fn host_fetch(ctx: *mut c_void, st: *mut GuestState, addr: u32, width: u32) {
    let host = unsafe { &mut *(ctx.cast::<HostCtx>()) };
    host.inst_start = unsafe { (*st).cycles };
    let bus = host.bus();
    let cost = bus.fetch_cost(addr, u8::try_from(width).unwrap_or(4));
    unsafe { (*st).cycles += u64::from(cost) };
}

/// `true` when the store lands in a page holding translated code.
/// The caller has already bailed out by then; this decides whether to
/// charge and perform the write.
unsafe extern "C" fn host_store(
    ctx: *mut c_void,
    st: *mut GuestState,
    addr: u32,
    value: u32,
    size: u32,
    pc: u32,
) -> u32 {
    let host = unsafe { &mut *(ctx.cast::<HostCtx>()) };
    let width = match size {
        SIZE_BYTE => 1,
        SIZE_HALF => 2,
        SIZE_WORD => 4,
        _ => 0,
    };
    if width > 0 {
        // Self-modifying store: refuse to perform it natively, hand
        // the storing instruction back, and let the interpreter rerun
        // it over the patched bytes.
        let first = page_of(addr);
        let last = page_of(addr + width as u32 - 1);
        let hit = (first..=last)
            .map(|p| unsafe { *host.code_pages.add(p as usize) })
            .any(|marked| marked != 0);
        if hit {
            unsafe {
                (*st).exited = 1;
                (*st).pc = pc;
            }
            return 1;
        }
        host.sync_for_io(addr);
        let cost = host.bus().access_cost(addr, width);
        unsafe { (*st).cycles += u64::from(cost) };
    }
    let bus = host.bus();
    match size {
        SIZE_BYTE => bus.write_u8(addr, value as u8),
        SIZE_HALF => bus.write_u16(addr, value as u16),
        _ => bus.write_u32(addr, value),
    }
    0
}
