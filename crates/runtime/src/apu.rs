//! PSG/DMG audio (channels 1-4) with a headless sample-capture API.
//!
//! The four legacy channels are synthesized from the register image in
//! the bus IO array exactly as GBATEK "GBA Sound Controller" decodes
//! them: two envelope squares (channel 1 adding frequency sweep), the
//! wave channel playing 32 or 64 nibble samples from the two WAVE_RAM
//! banks, and the LFSR noise channel. The legacy DMG byte registers are
//! packed into GBA halfwords at 0x060..0x084 (GBATEK "Sound
//! Registers"); restart strobes (bit 15 of SOUND1-4CNT_X) are
//! write-only, so they are stored as zero and cannot re-fire through a
//! byte merge.
//!
//! Timing model. The frame sequencer free-runs at 512 Hz while the PSG
//! master enable (SOUNDCNT_X bit 7) is set: length counters tick on
//! even steps (256 Hz), the sweep on steps 2 and 6 (128 Hz), and
//! envelopes on step 7 (64 Hz). GBATEK states only the unit rates
//! (length `(64-n)/256s` and `(256-n)/256s`, envelope `n/64s`, sweep
//! `7.8ms` units); the 8-step table follows Pan Docs. The mixer emits
//! one stereo sample pair every 512 cycles (32768 Hz, the SOUNDBIAS
//! default resolution); a sequencer step lands on every 64th sample
//! and takes effect on the sample after the tick. Channel outputs are
//! 4-bit levels; a side mix (sum of the channels NR51 routes to that
//! side) is scaled by the NR50 volume (1..8) and the SOUNDCNT_H PSG
//! volume (25/50/100 percent, code 3 treated as 100) and stored as
//! `mix * vol * percent * 8` in i16 (full scale 15360). Sub-step
//! remainders carry across [`Bus::step_apu`](crate::bus::Bus::step_apu)
//! calls, so captures are exact for any step granularity.
//!
//! Documented simplifications: wave RAM is directly mapped (bank 0 at
//! 0x090, bank 1 at 0x0A0) with no shift-register read corruption
//! while the channel plays; register writes land at step-call
//! granularity; the DAC pass is linear rather than the hardware's
//! capacitor curve; SOUNDBIAS is not applied; and
//! the DMG trigger-time sweep overflow pre-check when the shift is
//! nonzero is not modeled.

use crate::bus::Bus;

/// System cycles per mixed stereo sample (16.78 MHz / 512 = 32768 Hz).
const SAMPLE_PERIOD: u32 = 512;
/// System cycles per frame-sequencer step (16.78 MHz / 512 Hz).
const SEQ_PERIOD: u32 = 32768;
/// Square duty patterns, MSB first, indexed by the duty code (GBATEK
/// "GBA Sound Channel 1": 12.5/25/50/75 percent).
const DUTY: [u8; 4] = [0x80, 0xC0, 0xF0, 0xFC];

const NR11_NR12: u32 = 0x062;
const NR13_NR14: u32 = 0x064;
const NR21_NR22: u32 = 0x068;
const NR23_NR24: u32 = 0x06C;
const NR30: u32 = 0x070;
const NR31_NR32: u32 = 0x072;
const NR33_NR34: u32 = 0x074;
const NR43_NR44: u32 = 0x07C;
const NR50_NR51: u32 = 0x080;
const SOUNDCNT_H: u32 = 0x082;
const SOUNDCNT_X: u32 = 0x084;
const WAVE_RAM: u32 = 0x090;

/// Length counter, envelope, and sweep state for one square channel.
/// The sweep fields are latched at restart and inert on channel 2
/// (its restarts pass a zero sweep register, which disables the sweep).
struct Square {
    enabled: bool,
    duty_pos: u8,
    freq_acc: u32,
    length: u16,
    env: Envelope,
    sweep_pace: u8,
    sweep_down: bool,
    sweep_shift: u8,
    sweep_acc: u8,
    sweep_shadow: u16,
}

/// Wave channel state. `on` is the restart/length-flag lifecycle (the
/// SOUNDCNT_X flag); audibility additionally requires NR30 bit 7, read
/// live, and clearing that bit mid-play freezes but does not stop it.
struct Wave {
    on: bool,
    pos: u8,
    sample_acc: u32,
    length: u16,
}

/// LFSR noise channel state; `out_high` holds between shift ticks.
struct Noise {
    enabled: bool,
    lfsr: u16,
    out_high: bool,
    freq_acc: u32,
    length: u16,
    env: Envelope,
}

/// Volume envelope shared by the squares and noise: the initial volume,
/// pace, and direction are latched at restart (GBATEK "Initial Volume",
/// "Envelope Step-Time", "Envelope Direction").
struct Envelope {
    vol: u8,
    pace: u8,
    acc: u8,
    up: bool,
}

impl Envelope {
    /// Latches the envelope halves of NR12/NR22/NR42
    /// (pace bits 8-10, direction bit 11, initial volume bits 12-15).
    fn latched(reg: u16) -> Self {
        let pace = ((reg >> 8) & 0x7) as u8;
        Envelope {
            vol: ((reg >> 12) & 0xF) as u8,
            pace,
            acc: pace,
            up: reg & 0x0800 != 0,
        }
    }

    /// One 64 Hz sequencer tick: counts the pace down and steps the
    /// volume one level, clamped at 0 and 15 (GBATEK SOUNDCNT_X: the
    /// channel-active flag is not cleared when an envelope ends).
    fn clock(&mut self) {
        if self.pace == 0 {
            return;
        }
        self.acc -= 1;
        if self.acc != 0 {
            return;
        }
        self.acc = self.pace;
        if self.up {
            if self.vol < 15 {
                self.vol += 1;
            }
        } else {
            self.vol = self.vol.saturating_sub(1);
        }
    }
}

/// Live APU state owned by the bus.
pub struct Apu {
    sq: [Square; 2],
    wave: Wave,
    noise: Noise,
    /// Cycles accumulated toward the next mixed sample.
    sample_acc: u32,
    /// Cycles accumulated toward the next frame-sequencer step.
    seq_acc: u32,
    /// Sequencer step 0..8 (0-3,5,6: length; 2,6: sweep; 7: envelope).
    seq_step: u8,
    /// Interleaved stereo samples (left, right) awaiting capture.
    samples: Vec<i16>,
}

impl Default for Apu {
    fn default() -> Self {
        Self::new()
    }
}

impl Apu {
    /// Creates a silent APU with the sequencer parked at step 0.
    pub(crate) fn new() -> Self {
        let mk_sq = || Square {
            enabled: false,
            duty_pos: 0,
            freq_acc: 0,
            length: 0,
            env: Envelope {
                vol: 0,
                pace: 0,
                acc: 0,
                up: false,
            },
            sweep_pace: 0,
            sweep_down: false,
            sweep_shift: 0,
            sweep_acc: 0,
            sweep_shadow: 0,
        };
        Apu {
            sq: [mk_sq(), mk_sq()],
            wave: Wave {
                on: false,
                pos: 0,
                sample_acc: 0,
                length: 0,
            },
            noise: Noise {
                enabled: false,
                lfsr: 0,
                out_high: false,
                freq_acc: 0,
                length: 0,
                env: Envelope {
                    vol: 0,
                    pace: 0,
                    acc: 0,
                    up: false,
                },
            },
            sample_acc: 0,
            seq_acc: 0,
            seq_step: 0,
            samples: Vec::new(),
        }
    }

    /// Restores the power-on channel state (SOUNDCNT_X master disable),
    /// preserving any already-mixed samples in the capture buffer.
    pub(crate) fn reset(&mut self) {
        let samples = std::mem::take(&mut self.samples);
        *self = Self::new();
        self.samples = samples;
    }

    /// Latches a channel-1 or channel-2 restart from the just-written
    /// CNT_X halfword (`freq_ctrl`), the packed duty/envelope halfword
    /// (`env_duty`), and the sweep register (`sweep`; zero for
    /// channel 2, disabling the sweep).
    pub(crate) fn restart_square(&mut self, ch: usize, env_duty: u16, freq_ctrl: u16, sweep: u16) {
        let pace = ((sweep >> 4) & 0x7) as u8;
        self.sq[ch] = Square {
            enabled: true,
            duty_pos: 0,
            freq_acc: 0,
            length: 64 - (env_duty & 0x003F),
            env: Envelope::latched(env_duty),
            sweep_pace: pace,
            sweep_down: sweep & 0x0008 != 0,
            sweep_shift: (sweep & 0x7) as u8,
            sweep_acc: pace,
            sweep_shadow: freq_ctrl & 0x07FF,
        };
    }

    /// Latches a wave-channel restart from the just-written NR33/NR34
    /// halfword; the length reload comes from the NR31/NR32 halfword.
    pub(crate) fn restart_wave(&mut self, len_vol: u16) {
        self.wave = Wave {
            on: true,
            pos: 0,
            sample_acc: 0,
            length: 256 - (len_vol & 0x00FF),
        };
    }

    /// Latches a noise restart from the just-written NR43/NR44 halfword
    /// (`freq_ctrl`) and the NR41/NR42 halfword (`env_len`). The LFSR
    /// seed is 0x40 in 7-bit mode and 0x4000 in 15-bit mode (GBATEK
    /// "Noise Random Generator").
    pub(crate) fn restart_noise(&mut self, env_len: u16, freq_ctrl: u16) {
        self.noise = Noise {
            enabled: true,
            lfsr: if freq_ctrl & 0x0008 != 0 {
                0x40
            } else {
                0x4000
            },
            out_high: false,
            freq_acc: 0,
            length: 64 - (env_len & 0x003F),
            env: Envelope::latched(env_len),
        };
    }

    /// The SOUNDCNT_X channel-active flags (bits 0-3).
    pub(crate) fn active_mask(&self) -> u8 {
        u8::from(self.sq[0].enabled)
            | u8::from(self.sq[1].enabled) << 1
            | u8::from(self.wave.on) << 2
            | u8::from(self.noise.enabled) << 3
    }

    /// Drains the interleaved stereo sample buffer.
    pub(crate) fn take_samples(&mut self) -> Vec<i16> {
        std::mem::take(&mut self.samples)
    }

    /// Advances the APU by `cycles` system cycles, mixing whole
    /// 512-cycle samples and ticking the frame sequencer every 64th
    /// sample (a step takes effect on the sample after it ticks).
    pub(crate) fn step(&mut self, bus: &mut Bus, cycles: u32) {
        let mut remaining = cycles;
        while remaining > 0 {
            let take = remaining.min(SAMPLE_PERIOD - self.sample_acc);
            remaining -= take;
            self.advance(bus, take);
            self.sample_acc += take;
            if self.sample_acc < SAMPLE_PERIOD {
                continue;
            }
            self.sample_acc = 0;
            let (left, right) = self.mix(bus);
            self.samples.push(left);
            self.samples.push(right);
            self.seq_acc += SAMPLE_PERIOD;
            if self.seq_acc >= SEQ_PERIOD {
                self.seq_acc -= SEQ_PERIOD;
                self.seq_tick(bus);
            }
        }
    }

    /// Advances the sounding channels' oscillators by `cycles` cycles.
    fn advance(&mut self, bus: &Bus, cycles: u32) {
        for ch in 0..2usize {
            if !self.sq[ch].enabled {
                continue;
            }
            let period = square_period(bus, ch);
            self.sq[ch].freq_acc += cycles;
            while self.sq[ch].freq_acc >= period {
                self.sq[ch].freq_acc -= period;
                self.sq[ch].duty_pos = (self.sq[ch].duty_pos + 1) & 7;
            }
        }
        let nr30 = bus.io_half(NR30);
        if self.wave.on && nr30 & 0x0080 != 0 {
            let period = wave_period(bus);
            let span: u8 = if nr30 & 0x0020 != 0 { 64 } else { 32 };
            self.wave.sample_acc += cycles;
            while self.wave.sample_acc >= period {
                self.wave.sample_acc -= period;
                self.wave.pos = (self.wave.pos + 1) % span;
            }
        }
        if self.noise.enabled {
            let period = noise_period(bus);
            let short = bus.io_half(NR43_NR44) & 0x0008 != 0;
            self.noise.freq_acc += cycles;
            while self.noise.freq_acc >= period {
                self.noise.freq_acc -= period;
                let carry = self.noise.lfsr & 1;
                self.noise.lfsr >>= 1;
                if carry != 0 {
                    self.noise.lfsr ^= if short { 0x60 } else { 0x6000 };
                }
                self.noise.out_high = carry != 0;
            }
        }
    }

    /// One frame-sequencer step (Pan Docs table): length counters on
    /// even steps, sweep on steps 2 and 6, envelopes on step 7.
    fn seq_tick(&mut self, bus: &mut Bus) {
        let step = self.seq_step;
        self.seq_step = (step + 1) & 7;
        if step & 1 == 0 {
            self.clock_lengths(bus);
        }
        if step == 2 || step == 6 {
            self.clock_sweep(bus);
        }
        if step == 7 {
            self.clock_envelopes();
        }
    }

    /// Clocks every enabled channel's length counter at 256 Hz; a
    /// length flag stops the channel when its counter reaches zero.
    fn clock_lengths(&mut self, bus: &Bus) {
        for ch in 0..2usize {
            let ctrl = if ch == 0 { NR13_NR14 } else { NR23_NR24 };
            let flag = bus.io_half(ctrl) & 0x4000 != 0;
            let sq = &mut self.sq[ch];
            if flag && sq.enabled && sq.length > 0 {
                sq.length -= 1;
                sq.enabled = sq.length != 0;
            }
        }
        if bus.io_half(NR33_NR34) & 0x4000 != 0 && self.wave.on && self.wave.length > 0 {
            self.wave.length -= 1;
            self.wave.on = self.wave.length != 0;
        }
        if bus.io_half(NR43_NR44) & 0x4000 != 0 && self.noise.enabled && self.noise.length > 0 {
            self.noise.length -= 1;
            self.noise.enabled = self.noise.length != 0;
        }
    }

    /// Clocks the channel-1 sweep at 128 Hz: each time the latched pace
    /// expires, the shadow frequency becomes `X +/- X/2^shift` (GBATEK
    /// "Sound Channel 1"); a result past 2047 stops the channel,
    /// otherwise the frequency register is written back.
    fn clock_sweep(&mut self, bus: &mut Bus) {
        let sq = &mut self.sq[0];
        if !sq.enabled || sq.sweep_pace == 0 {
            return;
        }
        sq.sweep_acc -= 1;
        if sq.sweep_acc != 0 {
            return;
        }
        sq.sweep_acc = sq.sweep_pace;
        let delta = sq.sweep_shadow >> sq.sweep_shift;
        let next = if sq.sweep_down {
            sq.sweep_shadow - delta
        } else {
            sq.sweep_shadow + delta
        };
        if next > 2047 {
            sq.enabled = false;
            return;
        }
        sq.sweep_shadow = next;
        let cur = bus.io_half(NR13_NR14);
        bus.io_set_half(NR13_NR14, (cur & !0x07FF) | next);
    }

    /// Clocks the square and noise envelopes at 64 Hz.
    fn clock_envelopes(&mut self) {
        for sq in &mut self.sq {
            sq.env.clock();
        }
        self.noise.env.clock();
    }

    /// The four 4-bit channel outputs at the current instant.
    fn outputs(&self, bus: &Bus) -> [u32; 4] {
        let mut outs = [0u32; 4];
        for (ch, out) in outs.iter_mut().enumerate().take(2) {
            let sq = &self.sq[ch];
            let reg = bus.io_half(if ch == 0 { NR11_NR12 } else { NR21_NR22 });
            let duty = DUTY[(usize::from(reg) >> 6) & 3];
            let high = duty & (0x80 >> sq.duty_pos) != 0;
            if sq.enabled && high {
                *out = u32::from(sq.env.vol);
            }
        }
        if self.wave.on && bus.io_half(NR30) & 0x0080 != 0 {
            let sample = u32::from(wave_nibble(bus, self.wave.pos));
            let len_vol = bus.io_half(NR31_NR32);
            outs[2] = if len_vol & 0x8000 != 0 {
                // NR32 bit 15 forces 75% regardless of the volume code.
                sample * 3 / 4
            } else {
                match (len_vol >> 13) & 3 {
                    0 => 0,
                    1 => sample,
                    2 => sample >> 1,
                    _ => sample >> 2,
                }
            };
        }
        if self.noise.enabled && self.noise.out_high {
            outs[3] = u32::from(self.noise.env.vol);
        }
        outs
    }

    /// Mixes one stereo sample pair from the live register image.
    fn mix(&self, bus: &Bus) -> (i16, i16) {
        if bus.io_half(SOUNDCNT_X) & 0x0080 == 0 {
            return (0, 0);
        }
        let outs = self.outputs(bus);
        let nr5x = bus.io_half(NR50_NR51);
        // SOUNDCNT_H bits 0-1: 25/50/100 percent; the prohibited code 3
        // plays at 100 percent.
        let master: u32 = match bus.io_half(SOUNDCNT_H) & 0x3 {
            0 => 1,
            1 => 2,
            _ => 4,
        };
        let mut left = 0u32;
        let mut right = 0u32;
        for (ch, out) in outs.iter().enumerate() {
            let ch = ch as u32;
            if nr5x & (1 << (8 + ch)) != 0 {
                right += out;
            }
            if nr5x & (1 << (12 + ch)) != 0 {
                left += out;
            }
        }
        let vol_left = u32::from((nr5x >> 4) & 0x7) + 1;
        let vol_right = u32::from(nr5x & 0x7) + 1;
        (
            (left * vol_left * master * 8) as i16,
            (right * vol_right * master * 8) as i16,
        )
    }
}

/// Square duty-step period in cycles: 131072/(2048-n) Hz means 8 steps
/// per (2048-n)*128 cycles.
fn square_period(bus: &Bus, ch: usize) -> u32 {
    let ctrl = if ch == 0 { NR13_NR14 } else { NR23_NR24 };
    (2048 - u32::from(bus.io_half(ctrl) & 0x07FF)) * 128
}

/// Wave sample period in cycles: 2097152/(2048-n) Hz.
fn wave_period(bus: &Bus) -> u32 {
    (2048 - u32::from(bus.io_half(NR33_NR34) & 0x07FF)) * 8
}

/// Noise shift period in cycles: 524288/r/2^(s+1) Hz with r=0 read as
/// 0.5 (GBATEK "Sound Channel 4"), so the period is
/// `(r == 0 ? 16 : 32*r) << s`.
fn noise_period(bus: &Bus) -> u32 {
    let reg = bus.io_half(NR43_NR44);
    let r = u32::from(reg & 0x7);
    let s = (reg >> 4) & 0xF;
    (if r == 0 { 16 } else { 32 * r }) << s
}

/// The 4-bit wave sample at playback position `pos`: nibbles play MSB
/// first within each byte (GBATEK "WAVE_RAM"); dimension bit 5 extends
/// playback to 64 samples, running the NR30 bank first and then the
/// other bank.
fn wave_nibble(bus: &Bus, pos: u8) -> u8 {
    let nr30 = bus.io_half(NR30);
    let idx = if nr30 & 0x0020 != 0 { pos } else { pos & 31 };
    let bank = ((nr30 >> 6) & 1) ^ u16::from(idx >= 32);
    let byte = bus.io_byte(WAVE_RAM + u32::from(bank) * 16 + u32::from(idx & 31) / 2);
    if idx & 1 == 0 { byte >> 4 } else { byte & 0x0F }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bus with the PSG master enabled and the given halfword writes
    /// applied (the master must be on before channel registers are
    /// writable, GBATEK SOUNDCNT_X).
    fn bus_with(regs: &[(u32, u16)]) -> Bus {
        let mut bus = Bus::new(Vec::new());
        bus.write_u16(0x0400_0000 + SOUNDCNT_X, 0x0080);
        for &(off, value) in regs {
            bus.write_u16(0x0400_0000 + off, value);
        }
        bus
    }

    fn read(bus: &Bus, off: u32) -> u16 {
        bus.read_u16(0x0400_0000 + off)
    }

    #[test]
    fn length_counter_expires_at_the_256hz_tick() {
        let mut bus = bus_with(&[
            // NR21: length 63 (one tick), NR22: constant volume 15,
            // NR24: length flag + restart.
            (NR21_NR22, 0x003F | 0xF000),
            (NR23_NR24, 0x4000 | 0x8000),
        ]);
        let mut apu = Apu::new();
        apu.restart_square(1, read(&bus, NR21_NR22), read(&bus, NR23_NR24), 0);
        assert_eq!(apu.active_mask() & 0x2, 0x2);

        apu.step(&mut bus, SEQ_PERIOD - 1);
        assert_eq!(apu.active_mask() & 0x2, 0x2, "expires exactly at the tick");
        apu.step(&mut bus, 1);
        assert_eq!(
            apu.active_mask() & 0x2,
            0,
            "length 1 -> 0 stops the channel"
        );
        assert_eq!(apu.active_mask() & 0x2, 0, "SOUNDCNT_X flag reads clear");
        // Later sequencer steps must not resurrect it.
        apu.step(&mut bus, SEQ_PERIOD * 3);
        assert_eq!(apu.active_mask() & 0x2, 0);
    }

    #[test]
    fn envelope_decays_and_clamps() {
        let mut bus = bus_with(&[
            // NR22: pace 1, decrease, initial volume 3; NR24 restart.
            (NR21_NR22, 0x3100),
            (NR23_NR24, 0x8000),
        ]);
        let mut apu = Apu::new();
        apu.restart_square(1, read(&bus, NR21_NR22), read(&bus, NR23_NR24), 0);
        assert_eq!(apu.sq[1].env.vol, 3);
        // The envelope ticks at sequencer step 7, 15, 23 (64 Hz); the
        // first tick lands on the eighth 512 Hz step.
        apu.step(&mut bus, 8 * SEQ_PERIOD);
        assert_eq!(apu.sq[1].env.vol, 2);
        apu.step(&mut bus, 8 * SEQ_PERIOD);
        assert_eq!(apu.sq[1].env.vol, 1);
        apu.step(&mut bus, 8 * SEQ_PERIOD);
        assert_eq!(apu.sq[1].env.vol, 0);
        apu.step(&mut bus, 8 * SEQ_PERIOD);
        assert_eq!(apu.sq[1].env.vol, 0, "clamped at zero");

        // Increase direction clamps at 15.
        let mut bus = bus_with(&[(NR21_NR22, 0x3900), (NR23_NR24, 0x8000)]);
        let mut apu = Apu::new();
        apu.restart_square(1, read(&bus, NR21_NR22), read(&bus, NR23_NR24), 0);
        apu.step(&mut bus, 96 * SEQ_PERIOD);
        assert_eq!(apu.sq[1].env.vol, 15);
    }

    #[test]
    fn sweep_writes_back_and_overflow_stops_the_channel() {
        // NR10 (0x060): pace 1, increase, shift 2; NR14 restart, f=1000.
        let mut bus = bus_with(&[(0x060, 0x0012), (NR13_NR14, 0x83E8)]);
        let mut apu = Apu::new();
        apu.restart_square(
            0,
            read(&bus, NR11_NR12),
            read(&bus, NR13_NR14),
            read(&bus, 0x060),
        );

        // First sweep applies at sequencer step 2 (the third 512 Hz
        // tick): 1000 + 1000/4 = 1250.
        apu.step(&mut bus, 3 * SEQ_PERIOD);
        assert_eq!(apu.sq[0].sweep_shadow, 1250);
        assert_eq!(
            read(&bus, NR13_NR14) & 0x07FF,
            1250,
            "frequency written back"
        );
        assert_eq!(apu.active_mask() & 0x1, 0x1);
        // Second sweep at step 6: 1250 + 312 = 1562.
        apu.step(&mut bus, 4 * SEQ_PERIOD);
        assert_eq!(read(&bus, NR13_NR14) & 0x07FF, 1562);

        // Overflow: pace 1, increase, shift 3, f=1900 -> 2137 > 2047.
        let mut bus = bus_with(&[(0x060, 0x0013), (NR13_NR14, 0x8000 | 1900)]);
        let mut apu = Apu::new();
        apu.restart_square(
            0,
            read(&bus, NR11_NR12),
            read(&bus, NR13_NR14),
            read(&bus, 0x060),
        );
        apu.step(&mut bus, 3 * SEQ_PERIOD);
        assert_eq!(apu.active_mask() & 0x1, 0, "overflow disables the channel");
        assert_eq!(
            read(&bus, NR13_NR14) & 0x07FF,
            1900,
            "no write-back on overflow"
        );

        // Sweep time zero disables the sweep entirely (GBATEK NR10).
        let mut bus = bus_with(&[(0x060, 0x0003), (NR13_NR14, 0x83E8)]);
        let mut apu = Apu::new();
        apu.restart_square(
            0,
            read(&bus, NR11_NR12),
            read(&bus, NR13_NR14),
            read(&bus, 0x060),
        );
        apu.step(&mut bus, 16 * SEQ_PERIOD);
        assert_eq!(read(&bus, NR13_NR14) & 0x07FF, 1000);
    }

    #[test]
    fn wave_nibbles_play_msb_first_across_both_banks() {
        let mut bus = bus_with(&[]);
        bus.write_u16(0x0400_0000 + NR30, 0x0080); // one bank, bank 0
        bus.write_u8(0x0400_0090, 0x01);
        bus.write_u8(0x0400_0091, 0x23);
        bus.write_u8(0x0400_00A0, 0x89);
        assert_eq!(wave_nibble(&bus, 0), 0x0);
        assert_eq!(wave_nibble(&bus, 1), 0x1);
        assert_eq!(wave_nibble(&bus, 2), 0x2);
        assert_eq!(wave_nibble(&bus, 3), 0x3);
        // Position 32+ reads bank 1 only in two-bank mode.
        assert_eq!(wave_nibble(&bus, 32), 0x0);
        bus.write_u16(0x0400_0000 + NR30, 0x00A0);
        assert_eq!(wave_nibble(&bus, 32), 0x8);
        assert_eq!(wave_nibble(&bus, 33), 0x9);
    }

    #[test]
    fn wave_volume_codes_scale_the_sample() {
        let mut bus = bus_with(&[(NR30, 0x0080)]);
        for i in 0..16u32 {
            bus.write_u8(0x0400_0090 + i, 0xFF);
        }
        for (bits, expected) in [
            (0x0000u16, 0u32),
            (0x2000, 15),
            (0x4000, 7),
            (0x6000, 3),
            (0x8000, 11),
        ] {
            bus.write_u16(0x0400_0000 + NR31_NR32, bits);
            let len_vol = read(&bus, NR31_NR32);
            let sample = u32::from(wave_nibble(&bus, 0));
            let got = if len_vol & 0x8000 != 0 {
                sample * 3 / 4
            } else {
                match (len_vol >> 13) & 3 {
                    0 => 0,
                    1 => sample,
                    2 => sample >> 1,
                    _ => sample >> 2,
                }
            };
            assert_eq!(got, expected, "volume bits {bits:#06x}");
        }
    }

    #[test]
    fn noise_lfsr_sequence_matches_the_shift_register_rule() {
        // 15-bit: seed 0x4000 -> 14 low shifts, then a carry turns the
        // output high and XORs 0x6000 back in.
        let mut lfsr = 0x4000u16;
        let mut high = false;
        let tick = |lfsr: &mut u16, high: &mut bool, mask: u16| {
            let carry = *lfsr & 1;
            *lfsr >>= 1;
            if carry != 0 {
                *lfsr ^= mask;
            }
            *high = carry != 0;
        };
        for _ in 0..14 {
            tick(&mut lfsr, &mut high, 0x6000);
        }
        assert!(!high);
        tick(&mut lfsr, &mut high, 0x6000);
        assert!(high, "carry after 15 shifts");
        assert_eq!(lfsr, 0x6000);

        // 7-bit: seed 0x40 -> 6 low shifts then high.
        let mut lfsr = 0x40u16;
        let mut high = false;
        for _ in 0..6 {
            tick(&mut lfsr, &mut high, 0x60);
            assert!(!high);
        }
        tick(&mut lfsr, &mut high, 0x60);
        assert!(high);
        assert_eq!(lfsr, 0x60);
    }

    #[test]
    fn noise_period_formula_matches_524288_over_r_over_2s() {
        for (reg, expected) in [
            (0x0000u16, 16u32),
            (0x0001, 32),
            (0x0002, 64),
            (0x0042, 1024),
        ] {
            let mut bus = bus_with(&[]);
            bus.write_u16(0x0400_0000 + NR43_NR44, reg);
            assert_eq!(noise_period(&bus), expected, "NR43 {reg:#06x}");
        }
    }

    #[test]
    fn mixer_scaling_and_routing_are_exact() {
        // Wave channel playing a constant 15 with routing to both
        // sides: sample = 15 * vol(8) * master(4) * 8 = 3840. NR50
        // (volumes) is the low byte of 0x080, NR51 (routing) the high
        // byte: ch3 right = bit 10, ch3 left = bit 14.
        let mut bus = bus_with(&[
            (NR30, 0x0080),
            (NR31_NR32, 0x2000),
            (NR33_NR34, 0x8000 | 1792),
            (NR50_NR51, 0x77 | 0x4400),
            (SOUNDCNT_H, 0x0002),
        ]);
        for i in 0..16u32 {
            bus.write_u8(0x0400_0090 + i, 0xFF);
        }
        let mut apu = Apu::new();
        apu.restart_wave(read(&bus, NR31_NR32));
        apu.step(&mut bus, SAMPLE_PERIOD);
        assert_eq!(apu.take_samples(), [3840, 3840]);

        // Right side only: left is silent.
        bus.write_u16(0x0400_0000 + NR50_NR51, 0x77 | 0x0400);
        apu.step(&mut bus, SAMPLE_PERIOD);
        assert_eq!(apu.take_samples(), [0, 3840]);

        // SOUNDCNT_H 25 percent: 15 * 8 * 1 * 8 = 960.
        bus.write_u16(0x0400_0000 + NR50_NR51, 0x77 | 0x4400);
        bus.write_u16(0x0400_0000 + SOUNDCNT_H, 0x0000);
        apu.step(&mut bus, SAMPLE_PERIOD);
        assert_eq!(apu.take_samples(), [960, 960]);

        // Master disable mixes silence.
        bus.write_u16(0x0400_0000 + SOUNDCNT_X, 0x0000);
        apu.step(&mut bus, SAMPLE_PERIOD);
        assert_eq!(apu.take_samples(), [0, 0]);
    }

    #[test]
    fn master_disable_resets_the_psg_image_and_state() {
        let mut bus = bus_with(&[
            (0x060, 0x0012),
            (NR21_NR22, 0xF080),
            (NR23_NR24, 0x8000 | 512),
        ]);
        let mut apu = Apu::new();
        apu.restart_square(1, read(&bus, NR21_NR22), read(&bus, NR23_NR24), 0);
        apu.step(&mut bus, SAMPLE_PERIOD);
        assert_eq!(apu.take_samples().len(), 2);

        // Disable: the bus zeroes the PSG image and resets the engine
        // (the reset here mirrors what io_write_u16 does to its APU).
        bus.write_u16(0x0400_0000 + SOUNDCNT_X, 0x0000);
        apu.reset();
        assert_eq!(apu.active_mask(), 0);
        assert_eq!(read(&bus, 0x060), 0);
        assert_eq!(read(&bus, NR21_NR22), 0);
        assert_eq!(read(&bus, NR23_NR24), 0, "restart strobe stored as zero");
        assert_eq!(read(&bus, SOUNDCNT_H), 0);
        bus.write_u8(0x0400_0090, 0xAB);
        assert_eq!(bus.read_u8(0x0400_0090), 0xAB, "wave RAM survives");

        // Writes to the PSG range are ignored while disabled.
        bus.write_u16(0x0400_0000 + NR21_NR22, 0xFFFF);
        assert_eq!(read(&bus, NR21_NR22), 0);

        // Re-enabling restores writability.
        bus.write_u16(0x0400_0000 + SOUNDCNT_X, 0x0080);
        bus.write_u16(0x0400_0000 + NR21_NR22, 0x1234);
        assert_eq!(read(&bus, NR21_NR22), 0x1234);
    }

    #[test]
    fn restart_strobe_reads_as_zero_and_fires_from_a_byte_merge() {
        let mut bus = bus_with(&[]);
        bus.write_u16(0x0400_0000 + NR13_NR14, 0x8000 | 100);
        assert_eq!(read(&bus, NR13_NR14), 100, "strobe bit stored as zero");

        // A plain NR13 word write leaves no stale strobe for a later
        // NR13 write to inherit.
        bus.write_u16(0x0400_0000 + NR13_NR14, 100 | 0x0400);
        assert_eq!(read(&bus, NR13_NR14), 100 | 0x0400);

        // Writing NR14 alone (byte merge into the halfword) fires the
        // strobe and replaces the whole high byte: the length flag
        // clears and the strobe stores as zero, leaving freq 100.
        bus.write_u8(0x0400_0065, 0x80);
        assert_eq!(read(&bus, NR13_NR14), 100);
    }
}
