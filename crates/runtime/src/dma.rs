//! The four DMA channels (GBATEK "DMA Transfer Channels").
//!
//! Registers live in the bus IO array (`0x040000B0 + 12*N`: SAD, DAD,
//! CNT_L, CNT_H); this module holds the latched internal state and the
//! transfer engine. Latching follows GBATEK: internal SAD/DAD/count are
//! captured when the channel enable rises (0 -> 1) and re-captured on
//! every repeat re-arm (count always, destination too when the dst
//! adjust is inc+reload), so later register writes do not affect an
//! armed channel. A word count of 0 means the channel maximum.
//! Transfers are synchronous and run in channel order (channel 0 has
//! highest priority); cycle charging follows the WAITCNT waitstate
//! model on [`Bus::access_cost`] (GBATEK WAITCNT), not the full
//! timing window.

use crate::bus::Bus;

/// Timing-field selectors (CNT_H bits 12-13).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaTiming {
    /// Fires when the channel enable rises.
    Immediate = 0,
    /// Fires on the rising DISPSTAT V-Blank flag edge.
    VBlank = 1,
    /// Fires on every rising H-Blank flag edge.
    HBlank = 2,
    /// Channel-specific: FIFO empty (channels 1/2), video capture (3).
    Special = 3,
}

/// Latched internal state of one channel.
#[derive(Clone, Copy, Default)]
struct Channel {
    sad: u32,
    dad: u32,
    /// Latched count in transfer units; 0 is normalized to the maximum.
    count: u32,
}

/// DMA engine state owned by the bus.
#[derive(Default)]
pub struct Dma {
    channels: [Channel; 4],
    /// Channels armed by an immediate enable, run by the system driver
    /// right after the enabling instruction (deferring keeps the
    /// transfer out of the middle of that instruction).
    deferred: u8,
}

/// DMAxCNT_H fields (GBATEK "DMA Transfer Channels"): destination
/// control in bits 5-6, source control in bits 7-8, repeat in bit 9,
/// 32-bit units in bit 10, start timing in bits 12-13, the end IRQ in
/// bit 14, and the enable in bit 15.
fn dst_control(cnt_h: u16) -> u8 {
    ((cnt_h >> 5) & 0x0003) as u8
}

fn src_control(cnt_h: u16) -> u8 {
    ((cnt_h >> 7) & 0x0003) as u8
}

fn timing_of(cnt_h: u16) -> u8 {
    ((cnt_h >> 12) & 0x0003) as u8
}

const REPEAT: u16 = 0x0200;
const WORD_UNITS: u16 = 0x0400;

/// Halfword IO offsets of CNT_H per channel.
const CNT_H: [u32; 4] = [0x0BA, 0x0C6, 0x0D2, 0x0DE];
/// CNT_L (word count) halfword IO offsets per channel.
const CNT_L: [u32; 4] = [0x0B8, 0x0C4, 0x0D0, 0x0DC];
/// Maximum transfer length in 16-bit units per channel (GBATEK: a
/// count of 0 means the maximum; channels 0-2 transfer at most 0x4000
/// units, channel 3 0x10000).
const MAX_COUNT: [u32; 4] = [0x4000, 0x4000, 0x4000, 0x1_0000];
/// The FIFO A and FIFO B data addresses.
const FIFO_ADDR: [u32; 2] = [0x0400_00A0, 0x0400_00A4];

impl Dma {
    /// Latches the channel's internal state and arms immediate-timing
    /// channels; called on the CNT_H enable 0 -> 1 write edge.
    pub(super) fn on_enable(&mut self, io: &[u8], ch: usize) {
        let sad = word_at(io, 0xB0 + 12 * ch);
        let dad = word_at(io, 0xB4 + 12 * ch);
        let cnt_l = half(io, CNT_L[ch] as usize);
        let count = if cnt_l == 0 {
            MAX_COUNT[ch]
        } else {
            u32::from(cnt_l)
        };
        self.channels[ch] = Channel { sad, dad, count };
        if timing_of(half(io, CNT_H[ch] as usize)) == DmaTiming::Immediate as u8 {
            self.deferred |= 1 << ch;
        }
    }

    /// Re-latches repeat state for `timing` channels (a DISPSTAT flag
    /// edge just happened): count always, destination too when the dst
    /// adjust is inc+reload (GBATEK repeat reload rules).
    pub(super) fn rearm(&mut self, io: &[u8], timing: DmaTiming) {
        for (ch, &cnt_h_off) in CNT_H.iter().enumerate() {
            let cnt_h = half(io, cnt_h_off as usize);
            if cnt_h & 0x8000 == 0 || timing_of(cnt_h) != timing as u8 {
                continue;
            }
            self.channels[ch].count = latch_count(io, ch);
            if dst_control(cnt_h) == 0b11 {
                self.channels[ch].dad = word_at(io, 0xB4 + 12 * ch);
            }
        }
    }
    /// Runs every enabled `timing` channel in priority order (lowest
    /// channel number first, GBATEK "DMA priorities") and returns the
    /// charged cycle count.
    pub(super) fn run(&mut self, bus: &mut Bus, timing: DmaTiming) -> u32 {
        let mut cycles = 0;
        for (ch, &cnt_h_off) in CNT_H.iter().enumerate() {
            let cnt_h = bus.io_half(cnt_h_off);
            if cnt_h & 0x8000 == 0 || timing_of(cnt_h) != timing as u8 {
                continue;
            }
            cycles += self.transfer(bus, ch);
        }
        cycles
    }

    /// Whether an immediate channel waits for its deferred run.
    pub(super) fn has_deferred(&self) -> bool {
        self.deferred != 0
    }

    /// Runs deferred immediate channels; called once per system step,
    /// after the instruction that armed them.
    pub(super) fn run_deferred(&mut self, bus: &mut Bus) -> u32 {
        if self.deferred == 0 {
            return 0;
        }
        self.deferred = 0;
        self.run(bus, DmaTiming::Immediate)
    }

    /// Runs the sound-FIFO drains the APU requested (bit 0 = FIFO A,
    /// bit 1 = FIFO B): whichever special-timing channel among 1 and
    /// 2 has that FIFO as its destination feeds it (GBATEK
    /// "DMA-Sound Playback Procedure"; either channel can serve either
    /// FIFO). The charged cycles are not added to the clock (the
    /// refill is synchronous with the timer latch; the coarse timing
    /// model applies).
    pub(super) fn sound_refill(&mut self, bus: &mut Bus, requests: u8) {
        for (fifo_bit, fifo_addr) in FIFO_ADDR.into_iter().enumerate() {
            if requests & (1 << fifo_bit) == 0 {
                continue;
            }
            for ch in 1..=2 {
                let cnt_h = bus.io_half(CNT_H[ch]);
                if cnt_h & 0x8000 != 0
                    && timing_of(cnt_h) == DmaTiming::Special as u8
                    && self.channels[ch].dad & 0x0FFF_FFFF == fifo_addr
                {
                    self.transfer(bus, ch);
                }
            }
        }
    }

    /// Executes one channel transfer and returns the charged cycles.
    fn transfer(&mut self, bus: &mut Bus, ch: usize) -> u32 {
        let cnt_h = bus.io_half(CNT_H[ch]);
        let timing = timing_of(cnt_h);
        let repeat = cnt_h & REPEAT != 0;
        let irq_on_complete = cnt_h & 0x4000 != 0;

        // Special timing on channels 1/2 is the sound FIFO drain:
        // always 4 words, 32-bit, fixed destination (GBATEK "FIFO
        // Reset/Drain"). Channel 0 has no special timing; channel 3's
        // (video capture) is not modeled yet.
        let fifo = timing == DmaTiming::Special as u8 && (1..=2).contains(&ch);
        let unit: u32 = if fifo || cnt_h & WORD_UNITS != 0 {
            4
        } else {
            2
        };
        let count = if fifo { 4 } else { self.channels[ch].count };
        // FIFO drains write a fixed address regardless of the CNT_H
        // destination adjust bits (GBATEK: "the destination address
        // will not be incremented in FIFO mode").
        let dst_adj = if fifo { 0b10 } else { dst_control(cnt_h) };
        let src_adj = src_control(cnt_h);
        let mut sad = self.channels[ch].sad;
        let mut dad = self.channels[ch].dad;

        // Dst adjust 11 (inc+reload) increments during the transfer
        // like inc; the reload happens in `rearm` on the next trigger.
        let step = |addr: u32, adj: u8| -> u32 {
            match adj {
                0b01 => addr.wrapping_sub(unit),
                0b10 => addr,
                _ => addr.wrapping_add(unit),
            }
        };

        let mut cycles = 0u32;
        // A request stream is announced by the transfer that carries
        // it: the chip's address width follows from the bit count
        // (GBATEK EEPROM streams go out via DMA3 halfwords).
        if unit == 2 && (0x0D00_0000..0x0E00_0000).contains(&dad) && (9..=81).contains(&count) {
            bus.save_mut().announce_eeprom_stream(count as u8);
        }
        // 2I internal cycles, 4I when both ends are on the Game Pak.
        let gamepak = |addr: u32| (0x08..=0x0E).contains(&((addr >> 24) & 0x0F));
        cycles += if gamepak(sad) && gamepak(dad) { 4 } else { 2 };
        for index in 0..count {
            let seq = index > 0;
            cycles += bus.dma_access_cost(sad, unit as u8, seq);
            cycles += bus.dma_access_cost(dad, unit as u8, seq);
            if unit == 4 {
                let v = bus.read_u32(sad);
                bus.write_u32(dad, v);
            } else {
                let v = bus.read_u16(sad);
                bus.write_u16(dad, v);
            }
            sad = step(sad, src_adj);
            dad = step(dad, dst_adj);
        }

        // End-of-transfer state: the latched addresses carry the final
        // positions into the next repeat trigger, so a FIFO drain's
        // source keeps advancing across refills (only count and, for
        // inc+reload, the destination re-latch per GBATEK repeat
        // rules; the FIFO drain destination is fixed anyway).
        self.channels[ch].sad = sad;
        self.channels[ch].dad = dad;

        if irq_on_complete {
            bus.raise_if(0x0100 << ch);
        }
        // Non-repeat channels self-disable after firing (GBATEK: the
        // enable bit clears when the transfer ends). Repeat channels
        // stay armed, and FIFO drains always do (the request line is
        // level-driven by the APU).
        let keeps_enable = fifo || (repeat && timing != DmaTiming::Immediate as u8);
        if !keeps_enable {
            let off = CNT_H[ch];
            bus.io_set_half(off, bus.io_half(off) & !0x8000);
        }
        cycles
    }
}

/// The latched transfer length for `ch` from the CNT_L register.
fn latch_count(io: &[u8], ch: usize) -> u32 {
    let cnt_l = half(io, CNT_L[ch] as usize);
    if cnt_l == 0 {
        MAX_COUNT[ch]
    } else {
        u32::from(cnt_l)
    }
}

/// Little-endian halfword at `i`, which must leave room for `i + 1`.
fn half(bytes: &[u8], i: usize) -> u16 {
    u16::from(bytes[i]) | (u16::from(bytes[i + 1]) << 8)
}

/// Little-endian word at `off`.
fn word_at(io: &[u8], off: usize) -> u32 {
    u32::from(half(io, off)) | (u32::from(half(io, off + 2)) << 16)
}

#[cfg(test)]
mod tests {
    use crate::bus::Bus;

    /// GBATEK "DMA Transfer Time": 2N + 2(n-1)S + 2I, with the first
    /// unit non-sequential and the rest sequential on each side.
    #[test]
    fn transfer_time_follows_gbatek() {
        // Four words ROM -> IWRAM at the reset WAITCNT (a ROM word is
        // N=6, S=4; IWRAM is 1): 2I + (6 + 1) + 3 * (4 + 1) = 24.
        let mut bus = Bus::new(vec![0u8; 0x100]);
        bus.write_u32(0x0400_00D4, 0x0800_0000);
        bus.write_u32(0x0400_00D8, 0x0300_0000);
        bus.write_u16(0x0400_00DC, 4);
        bus.write_u16(0x0400_00DE, 0x8400); // enable, 32-bit, immediate
        assert_eq!(bus.dma_run_deferred(), 24);

        // Sixteen halfwords EWRAM -> EWRAM (3 cycles each way):
        // 2 + 16 * 6.
        bus.write_u32(0x0400_00D4, 0x0200_0000);
        bus.write_u32(0x0400_00D8, 0x0200_1000);
        bus.write_u16(0x0400_00DC, 16);
        bus.write_u16(0x0400_00DE, 0x8000);
        assert_eq!(bus.dma_run_deferred(), 98);
    }
}
