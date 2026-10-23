//! Golden waveform snapshots per PSG channel and
//! master volume/enable coverage.
//!
//! Each scenario writes the sound registers on a bare bus, steps the
//! APU a frame at a time (350,208 cycles = exactly 684 stereo samples
//! at 32768 Hz), and drains the mix-down per frame. Golden files hold
//! one labeled section per frame: 8 stereo cells per row, each cell
//! `llllrrrr` in hex. Regenerate with `OXYBOY_UPDATE_GOLDEN=1 cargo
//! test`. Register addresses and bit layouts follow GBATEK "GBA Sound
//! Controller"; the frame-sequencer step table follows Pan Docs.

use runtime::bus::Bus;
use runtime::ppu::{CYCLES_PER_LINE, LINES_PER_FRAME};
use runtime::system::System;
use testutil::golden;

const BASE: u32 = 0x0400_0000;
const CYCLES_PER_FRAME: u32 = CYCLES_PER_LINE * LINES_PER_FRAME as u32;
/// Stereo sample pairs per frame (350,208 / 512).
const SAMPLES_PER_FRAME: usize = 684;

/// Bus with the PSG master on, full volumes, 100 percent master level,
/// and all four channels routed to both sides.
fn bus_with_sound() -> Bus {
    let mut bus = Bus::new(Vec::new());
    let w = |bus: &mut Bus, off: u32, value: u16| bus.write_u16(BASE + off, value);
    w(&mut bus, 0x084, 0x0080); // SOUNDCNT_X: master enable
    w(&mut bus, 0x080, 0x77 | 0xFF00); // NR50 vol 7/7, NR51 all on
    w(&mut bus, 0x082, 0x0002); // SOUNDCNT_H: PSG 100 percent
    bus
}

/// Wave RAM bank `bank` (0 or 1) filled with the nibble pattern
/// `v, v, v+1, v+1, ...` (two equal nibbles per byte). The CPU window
/// at 0x090 addresses the bank NOT selected by NR30 bit 6 (GBATEK
/// SOUND3CNT_L), so the helper flips the selection while writing.
fn set_wave_ram(bus: &mut Bus, bank: u32, down: bool) {
    let nr30 = bus.read_u16(BASE + 0x070);
    let sel = if bank == 0 { 0x0040u16 } else { 0 };
    bus.write_u16(BASE + 0x070, (nr30 & !0x0040) | sel);
    for i in 0..16u32 {
        let n = if down { 15 - i as u8 } else { i as u8 };
        bus.write_u8(BASE + 0x090 + i, (n << 4) | n);
    }
    bus.write_u16(BASE + 0x070, nr30);
}

/// Steps `frames` frames and drains the captured samples.
fn capture(bus: &mut Bus, frames: usize) -> Vec<i16> {
    bus.step_apu(CYCLES_PER_FRAME * frames as u32);
    bus.take_audio()
}

/// Formats frame sections into the golden text: one header line per
/// frame, then 8 stereo cells (`llllrrrr`) per row.
fn render(sections: &[(String, Vec<i16>)]) -> String {
    let mut out = String::new();
    for (name, samples) in sections {
        assert_eq!(samples.len(), SAMPLES_PER_FRAME * 2, "{name}");
        out.push_str(name);
        out.push('\n');
        for chunk in samples.chunks(16) {
            for pair in chunk.chunks(2) {
                out += &format!("{:04x}{:04x} ", pair[0] as u16, pair[1] as u16);
            }
            out.push('\n');
        }
    }
    out
}

fn assert_golden(name: &str, sections: &[(String, Vec<i16>)]) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    golden::assert_matches(&path, &render(sections));
}

#[test]
fn channel1_sweep_and_envelope_waveform() {
    let mut bus = bus_with_sound();
    let w = |bus: &mut Bus, off: u32, value: u16| bus.write_u16(BASE + off, value);
    // NR10: pace 1, down (pitch rises), shift 1. NR11/NR12: duty 50,
    // envelope 15 decaying at pace 1. NR13/NR14: restart at n=2040
    // (16 output samples per duty cycle, so the glide is visible).
    w(&mut bus, 0x060, 0x0019);
    w(&mut bus, 0x062, 0xF180);
    w(&mut bus, 0x064, 0x8000 | 2040);
    let a = capture(&mut bus, 1);
    let b = capture(&mut bus, 1);
    assert_golden(
        "apu-sq1.txt",
        &[
            (
                "A: ch1 frame 1, sweep down x2 (n 2040->510), envelope 15->14".into(),
                a,
            ),
            (
                "A: ch1 frame 2, sweep down x3 (n 510->64), envelope 14->13".into(),
                b,
            ),
        ],
    );
}

#[test]
fn channel2_length_counter_cuts_the_note() {
    let mut bus = bus_with_sound();
    let w = |bus: &mut Bus, off: u32, value: u16| bus.write_u16(BASE + off, value);
    // NR51: channel 2 right side only.
    w(&mut bus, 0x080, 0x77 | 0x0200);
    // NR21/NR22: duty 25, constant volume 9, length 56 (8 ticks).
    // NR23/NR24: length flag + restart at n=2040.
    w(&mut bus, 0x068, 0x9078);
    w(&mut bus, 0x06C, 0xC000 | 2040);
    let a = capture(&mut bus, 1);
    let b = capture(&mut bus, 1);
    assert_golden(
        "apu-sq2.txt",
        &[
            ("B: ch2 frame 1, right side only, 25% duty, vol 9".into(), a),
            (
                "B: ch2 frame 2, length counter (8 ticks at 256 Hz) expires mid-frame".into(),
                b,
            ),
        ],
    );
}

#[test]
fn channel3_wave_banks_and_volume_codes() {
    let mut bus = bus_with_sound();
    let w = |bus: &mut Bus, off: u32, value: u16| bus.write_u16(BASE + off, value);
    // NR30: play, bank 0, two-bank dimension (64 samples). Bank 0
    // holds an ascending staircase, bank 1 descending; at n=1792 the
    // wave advances 4 samples per output sample and wraps bank 0 into
    // bank 1 after 128 output samples.
    w(&mut bus, 0x070, 0x00A0);
    set_wave_ram(&mut bus, 0, false);
    set_wave_ram(&mut bus, 1, true);
    w(&mut bus, 0x072, 0x2000); // NR32: 100 percent
    w(&mut bus, 0x074, 0x8000 | 1792);
    let a = capture(&mut bus, 1);
    // Rewriting only NR32 drops the level to 25 percent mid-capture.
    w(&mut bus, 0x072, 0x6000);
    let b = capture(&mut bus, 1);
    assert_golden(
        "apu-wave.txt",
        &[
            ("C: ch3 frame 1, 64-sample banks at 100 percent".into(), a),
            ("C: ch3 frame 2, NR32 rewritten to 25 percent".into(), b),
        ],
    );
}

#[test]
fn channel4_noise_with_envelope_decay() {
    let mut bus = bus_with_sound();
    let w = |bus: &mut Bus, off: u32, value: u16| bus.write_u16(BASE + off, value);
    // NR41/NR42: envelope 6 decaying at pace 1. NR43/NR44: r=2, 15-bit
    // LFSR, shift clock s=2 (one LFSR tick per 256 cycles), restart.
    w(&mut bus, 0x078, 0x6100);
    w(&mut bus, 0x07C, 0x8022);
    let a = capture(&mut bus, 1);
    let b = capture(&mut bus, 1);
    assert_golden(
        "apu-noise.txt",
        &[
            ("D: ch4 frame 1, 15-bit LFSR noise, envelope 6->5".into(), a),
            ("D: ch4 frame 2, envelope 5->4".into(), b),
        ],
    );
}

#[test]
fn mix_routing_master_volume_and_enable() {
    let mut bus = bus_with_sound();
    let w = |bus: &mut Bus, off: u32, value: u16| bus.write_u16(BASE + off, value);
    // ch1 (square, n=2040) to the left side; ch3 (wave ramp) to the
    // right side.
    w(&mut bus, 0x060, 0x0000);
    w(&mut bus, 0x062, 0xF080);
    w(&mut bus, 0x064, 0x8000 | 2040);
    w(&mut bus, 0x070, 0x00A0);
    set_wave_ram(&mut bus, 0, false);
    set_wave_ram(&mut bus, 1, true);
    w(&mut bus, 0x072, 0x2000);
    w(&mut bus, 0x074, 0x8000 | 1792);
    w(&mut bus, 0x080, 0x77 | 0x1400);
    let a = capture(&mut bus, 1);
    w(&mut bus, 0x082, 0x0000); // master 25 percent
    let b = capture(&mut bus, 1);
    w(&mut bus, 0x084, 0x0000); // master disable: silence + PSG reset
    let c = capture(&mut bus, 1);
    // Re-enable and reprogram channel 2 on both sides: the registers
    // were reset by the disable, so this must still sound.
    w(&mut bus, 0x084, 0x0080);
    w(&mut bus, 0x068, 0xF080);
    w(&mut bus, 0x06C, 0x8000 | 2040);
    w(&mut bus, 0x080, 0x77 | 0x2200);
    let d = capture(&mut bus, 1);
    assert_golden(
        "apu-mix.txt",
        &[
            ("E: ch1 left + ch3 right at 100 percent".into(), a),
            ("E: SOUNDCNT_H 25 percent scales both sides".into(), b),
            ("E: SOUNDCNT_X master off silences everything".into(), c),
            (
                "E: re-enabled with reprogrammed ch2 on both sides".into(),
                d,
            ),
        ],
    );
}

#[test]
fn system_frame_mixdown_is_684_silent_samples() {
    let mut system = System::new(vec![0xEA, 0xFF, 0xFF, 0xFE], 0x0800_0000);
    system.step_frame();
    let audio = system.take_audio();
    assert_eq!(audio.len(), SAMPLES_PER_FRAME * 2);
    assert!(audio.iter().all(|&s| s == 0), "no sound configured");
}
