//! The four GBA timers (GBATEK "Timers").
//!
//! Reload and control registers live in the bus IO array
//! (`0x04000100 + 4*N`: CNT_L reload/counter readback, CNT_H control);
//! this module owns the live counters and prescaler sub-count. The
//! counter increments once per prescaler period (1/64/256/1024 cycles)
//! or, in cascade mode, once per overflow of the preceding timer. On
//! the 0xFFFF -> 0x0000 wrap the channel reloads, raises its overflow
//! IRQ (IF bits 3-6) when CNT_H bit 6 is set, and ticks the next
//! channel when that channel's cascade bit is set. Overflows of timers
//! 0/1 also latch the sound FIFOs and can trigger a sound-DMA refill
//! (see `Bus::apu_timer_latch`).
//!
//! Cycle accounting is instruction-granular: `System::step` feeds the
//! whole step's cycles (CPU plus DMA) into `Timers::step`, and the
//! per-channel sub-count carries the remainder across steps, so a
//! prescaler-1024 timer ticks exactly once per 1024 elapsed cycles
//! even when steps are 3 cycles apart. The counter readback exposes
//! the live counter; real hardware pipelines the readable value, which
//! is not modeled.

use crate::bus::Bus;

/// Prescaler periods in cycles, indexed by CNT_H bits 0-1.
const PERIODS: [u32; 4] = [1, 64, 256, 1024];

/// Live timer state owned by the bus.
#[derive(Default)]
pub struct Timers {
    counters: [u16; 4],
    /// Cycles accumulated toward the next prescaler tick.
    subcounts: [u32; 4],
}

impl Timers {
    /// Advances every enabled timer by `cycles`, chaining overflows.
    pub(super) fn step(&mut self, bus: &mut Bus, cycles: u32) {
        for ch in 0..4usize {
            let ctrl = bus.io_half(0x0102 + 4 * ch as u32);
            if ctrl & 0x0080 == 0 {
                self.subcounts[ch] = 0;
                continue;
            }
            // Cascade channels tick from the predecessor's overflow,
            // not from the clock (CNT_H bit 2; ignored on channel 0).
            if ctrl & 0x0004 != 0 && ch > 0 {
                continue;
            }
            let period = PERIODS[(ctrl & 0x0003) as usize];
            self.subcounts[ch] += cycles;
            while self.subcounts[ch] >= period {
                self.subcounts[ch] -= period;
                self.tick(bus, ch);
            }
        }
    }

    /// One counter increment of channel `ch` with overflow side
    /// effects (reload, IRQ, cascade tick).
    fn tick(&mut self, bus: &mut Bus, ch: usize) {
        self.counters[ch] = self.counters[ch].wrapping_add(1);
        if self.counters[ch] != 0 {
            return;
        }
        // Overflow: reload, then latch the sound FIFOs (timers 0/1
        // drive direct sound; the latch can trigger a sound-DMA
        // refill, GBATEK "DMA-Sound Playback Procedure").
        self.counters[ch] = bus.io_half(0x0100 + 4 * ch as u32);
        if ch < 2 {
            bus.apu_timer_latch(ch);
        }
        if bus.io_half(0x0102 + 4 * ch as u32) & 0x0040 != 0 {
            bus.raise_if(0x0008 << ch);
        }
        if ch < 3 {
            let next = bus.io_half(0x0102 + 4 * (ch + 1) as u32);
            if next & 0x0080 != 0 && next & 0x0004 != 0 {
                self.tick(bus, ch + 1);
            }
        }
    }

    /// Applies the CNT_H enable 0 -> 1 edge: the counter reloads and
    /// the prescaler sub-count restarts (GBATEK "TMxCNT_L").
    pub(super) fn on_enable(&mut self, ch: usize, reload: u16) {
        self.counters[ch] = reload;
        self.subcounts[ch] = 0;
    }

    /// System cycles until the next overflow among the timers in
    /// `mask` (bit N = timer N), or `None` when none of them can
    /// overflow. A cascade timer only overflows when its predecessor
    /// does, so it stands in for that predecessor.
    pub(super) fn cycles_to_overflow(&self, bus: &Bus, mask: u8) -> Option<u32> {
        let ctrl = |ch: usize| bus.io_half(0x0102 + 4 * ch as u32);
        let mut mask = mask;
        for ch in (1..4usize).rev() {
            if mask >> ch & 1 != 0 && ctrl(ch) & 0x0004 != 0 {
                mask |= 1 << (ch - 1);
            }
        }
        (0..4usize)
            .filter_map(|ch| {
                let ctrl = ctrl(ch);
                if mask >> ch & 1 == 0 || ctrl & 0x0080 == 0 || (ctrl & 0x0004 != 0 && ch > 0) {
                    return None;
                }
                let period = PERIODS[(ctrl & 0x0003) as usize];
                let ticks = 0x1_0000 - u32::from(self.counters[ch]);
                Some(ticks * period - self.subcounts[ch])
            })
            .min()
    }

    /// System cycles until timer `ch` overflows for the `n`th time
    /// (`n` >= 1), or `None` when it is stopped. A cascade timer only
    /// bounds its first overflow from below by its predecessor's.
    pub(super) fn cycles_to_nth_overflow(&self, bus: &Bus, ch: usize, n: u32) -> Option<u32> {
        let ctrl = bus.io_half(0x0102 + 4 * ch as u32);
        if ctrl & 0x0080 == 0 {
            return None;
        }
        if ctrl & 0x0004 != 0 && ch > 0 {
            return self.cycles_to_overflow(bus, 1 << ch);
        }
        let period = PERIODS[(ctrl & 0x0003) as usize];
        let first = (0x1_0000 - u32::from(self.counters[ch])) * period - self.subcounts[ch];
        let reload = u32::from(bus.io_half(0x0100 + 4 * ch as u32));
        let full = (0x1_0000 - reload) * period;
        Some(first.saturating_add(full.saturating_mul(n.saturating_sub(1))))
    }

    /// The live counter of channel `ch` (TMxCNT_L readback).
    pub(super) fn counter(&self, ch: usize) -> u16 {
        self.counters[ch]
    }
}
