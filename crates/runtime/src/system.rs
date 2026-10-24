//! The composed machine: CPU, bus, and PPU advancing in lockstep.
//!
//! [`System::step`] runs one CPU instruction and charges the video
//! clock for the cycles it consumed; [`System::step_frame`] runs until
//! the scanline counter wraps. This is the walking-skeleton driver the
//! headless CLI and the integration tests build on.

use crate::bus::Bus;
use crate::cpu::Cpu;
use crate::input::Key;
use crate::ppu::{Frame, Ppu};

/// Execution base of a multiboot image, which the BIOS copies to EWRAM
/// (GBATEK "GBA Multiboot").
const MULTIBOOT_BASE: u32 = 0x0200_0000;

/// A GBA machine: interpreter CPU, system bus, and video.
pub struct System {
    /// The interpreter CPU.
    pub cpu: Cpu,
    /// The system bus (memory, IO registers).
    bus: Bus,
    /// The video clock and framebuffer.
    ppu: Ppu,
}

impl System {
    /// Boots a machine directly at `entry` with no BIOS reset sequence
    /// (the documented boot decision: the CPU comes up in SVC mode
    /// with IRQ and FIQ masked, ARM state).
    pub fn new(rom: Vec<u8>, entry: u32) -> Self {
        System {
            cpu: Cpu::new(entry),
            bus: Bus::new(rom),
            ppu: Ppu::new(),
        }
    }

    /// Boots a machine through a real BIOS image at 0x00000000: the
    /// CPU starts at the reset vector in supervisor mode, BIOS-call
    /// HLE is disabled (SWI vectors into the image), and the cartridge
    /// hands off through the header checks the real BIOS performs (a
    /// synthetic ROM therefore needs a genuine logo and header for the
    /// boot to reach it). The image is padded or truncated to 16 KiB.
    pub fn with_bios(rom: Vec<u8>, bios: Vec<u8>) -> Self {
        let mut cpu = Cpu::new(0x0000_0000);
        cpu.set_bios_hle(false);
        System {
            cpu,
            bus: Bus::with_bios(rom, bios),
            ppu: Ppu::new(),
        }
    }

    /// Boots a multiboot image the way the BIOS would: the bytes land
    /// in EWRAM at 0x02000000 and the CPU enters at the header's entry
    /// branch (the `rom` multiboot classification guarantees the first
    /// word branches into EWRAM). The image also stays mapped at the
    /// cart window; a real multiboot boot streams it over the client
    /// cable, so programs must not rely on that mapping.
    pub fn with_multiboot(rom: Vec<u8>) -> Self {
        let mut bus = Bus::new(rom.clone());
        bus.load_ewram(&rom);
        System {
            cpu: Cpu::new(MULTIBOOT_BASE),
            bus,
            ppu: Ppu::new(),
        }
    }

    /// Runs one CPU instruction, advances the video clock by the
    /// cycles it consumed plus any immediate DMA the instruction armed,
    /// drives the timers and the APU on the same cycle budget, and
    /// refreshes the CPU IRQ input line from the interrupt controller
    /// state.
    pub fn step(&mut self) -> u32 {
        let cycles = self.cpu.step(&mut self.bus);
        let dma_cycles = self.bus.dma_run_deferred();
        self.ppu.step(&mut self.bus, cycles + dma_cycles);
        self.bus.step_timers(cycles + dma_cycles);
        self.bus.step_apu(cycles + dma_cycles);
        let pending = self.bus.irq_pending();
        self.cpu.set_irq_line(pending);
        cycles
    }

    /// Runs until the scanline counter wraps to 0, completing the
    /// frame in progress (or, if called at a frame boundary, the frame
    /// that starts next). VCOUNT reads 0 when this returns only if the
    /// last instruction landed exactly on the boundary; the wrap point
    /// is defined by [`Ppu::take_frame_ended`], not by VCOUNT alone.
    pub fn step_frame(&mut self) {
        while !self.ppu.take_frame_ended() {
            self.step();
        }
    }

    /// Whether a frame boundary was crossed since the last
    /// [`System::step_frame`] (peek, without consuming it).
    #[must_use]
    pub fn frame_ended(&self) -> bool {
        self.ppu.frame_ended()
    }

    /// Consumes the frame-ended flag: true once per completed frame.
    /// Per-instruction drivers (the debugger, the IO logger) step
    /// until this returns true instead of calling
    /// [`System::step_frame`], which does not expose step granularity.
    pub fn take_frame_ended(&mut self) -> bool {
        self.ppu.take_frame_ended()
    }

    /// System cycles from the current video dot to the next frame
    /// boundary (the frame runner's native-unit budget).
    #[must_use]
    pub fn cycles_to_frame_end(&self) -> u32 {
        self.ppu.cycles_to_frame_end()
    }

    /// Advances the video clock, timers, and APU by `cycles` and
    /// drains any DMA the guest armed, then refreshes the CPU IRQ
    /// input line. [`System::step`] applies this to its instruction's
    /// cycle count; the recompiled driver applies it per native chunk.
    pub fn advance_clock(&mut self, cycles: u32) {
        let dma_cycles = self.bus.dma_run_deferred();
        self.ppu.step(&mut self.bus, cycles + dma_cycles);
        self.bus.step_timers(cycles + dma_cycles);
        self.bus.step_apu(cycles + dma_cycles);
        let pending = self.bus.irq_pending();
        self.cpu.set_irq_line(pending);
    }

    /// Runs one step and records what it did as a
    /// [`trace::Record`]: the PC after the instruction, the Thumb and
    /// halted state bits, the register writes (indices 0-14; R15 is
    /// the recorded PC itself), and every bus write the step
    /// performed, including immediate DMA. This is the oracle-mode
    /// driver: run it in a loop over a headless system and feed the
    /// records to [`trace::Writer`], advancing by the returned cycle
    /// count exactly as [`System::step`] would.
    ///
    /// The record covers one [`System::step`]; halted steps produce a
    /// record with the halted flag and no deltas.
    pub fn step_recorded(&mut self) -> (u32, trace::Record) {
        let before: [u32; 15] = core::array::from_fn(|i| self.cpu.reg_for_test(i as u8));
        self.bus.set_write_trace(true);
        let cycles = self.step();
        let writes = self.bus.take_writes();
        self.bus.set_write_trace(false);

        let mut reg_delta = Vec::new();
        for (i, was) in before.iter().enumerate() {
            let now = self.cpu.reg_for_test(i as u8);
            if now != *was {
                reg_delta.push(trace::RegWrite {
                    index: i as u8,
                    value: now,
                });
            }
        }
        let mut flags = 0;
        if self.cpu.cpsr() & 0x20 != 0 {
            flags |= trace::FLAG_THUMB;
        }
        if self.cpu.is_halted() {
            flags |= trace::FLAG_HALTED;
        }
        (
            cycles,
            trace::Record {
                pc: self.cpu.pc(),
                flags,
                reg_delta,
                writes,
            },
        )
    }

    /// The captured framebuffer: exact RGBA8888 bytes, row-major,
    /// 240x160.
    pub fn framebuffer(&self) -> &Frame {
        self.ppu.frame()
    }

    /// Drains the APU's interleaved stereo sample buffer; call after
    /// [`System::step_frame`] for one mix-down per frame.
    pub fn take_audio(&mut self) -> Vec<i16> {
        self.bus.take_audio()
    }

    /// The current scanline (the VCOUNT register view).
    pub fn vcount(&self) -> u16 {
        self.bus.vcount()
    }

    /// The DISPSTAT register image.
    pub fn dispstat(&self) -> u16 {
        self.bus.dispstat()
    }

    /// The bus, for tests and tools that poke memory directly.
    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    /// The bus, mutably.
    pub fn bus_mut(&mut self) -> &mut Bus {
        &mut self.bus
    }

    /// Presses a keypad button (scripted input; call at a frame
    /// boundary between `step_frame` calls).
    pub fn press(&mut self, key: Key) {
        self.bus.press(key);
    }

    /// Releases a keypad button.
    pub fn release(&mut self, key: Key) {
        self.bus.release(key);
    }
}
