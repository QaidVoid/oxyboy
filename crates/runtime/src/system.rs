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
