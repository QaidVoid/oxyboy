//! FIFO direct sound.
//!
//! The golden scenarios feed known byte waveforms into the FIFO
//! queues, latch one byte per 512-cycle sample slot through a real
//! timer overflow, and capture the mixed output (GBATEK "DMA-Sound
//! Playback Procedure": a timer overflow latches the oldest byte; an
//! empty queue holds the last sample). The sustain test wires DMA
//! channel 1 in demand mode behind FIFO A and verifies a full
//! two-frame playback with byte-exact phase: any lost or duplicated
//! byte would shift the stream. Golden files hold one labeled section
//! per scenario: 8 stereo cells per row, each cell `llllrrrr` in hex.
//! Regenerate with `OXYBOY_UPDATE_GOLDEN=1 cargo test`.

use runtime::bus::Bus;
use testutil::golden;

const BASE: u32 = 0x0400_0000;
const IWRAM: u32 = 0x0300_0000;
/// Stereo sample pairs per frame (280,896 / 512 = 548.625, so a
/// whole-frame drain yields 548 or 549 pairs; 4389 pairs per 8
/// frames). This test's slot capture rounds down to whole slots.
const SAMPLES_PER_FRAME: usize = 548;
/// Timer reload for one overflow per 512-cycle sample slot: 0x10000
/// minus 512, i.e. 32768 Hz, the SOUNDBIAS default resolution.
const TIMER_RELOAD: u16 = 0xFE00;

/// Advances one 512-cycle sample slot: the timer overflow latches a
/// FIFO byte (and any DMA refill) first, then the APU mixes one
/// stereo pair, mirroring the `System::step` order.
fn sample_slot(bus: &mut Bus) {
    bus.step_timers(512);
    bus.step_apu(512);
}

/// Captures one frame (548 latched stereo pairs).
fn capture(bus: &mut Bus) -> Vec<i16> {
    let mut out = Vec::with_capacity(SAMPLES_PER_FRAME * 2);
    for _ in 0..SAMPLES_PER_FRAME {
        sample_slot(bus);
        out.extend(bus.take_audio());
    }
    out
}

/// Formats frame sections into the golden text: one header line per
/// section, then 8 stereo cells (`llllrrrr`) per row.
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
fn fifo_waveform_golden() {
    let mut bus = Bus::new(Vec::new());
    let w = |bus: &mut Bus, off: u32, value: u16| bus.write_u16(BASE + off, value);
    w(&mut bus, 0x084, 0x0080); // SOUNDCNT_X: master enable
    // Timer 0 and timer 1: one overflow per sample slot.
    w(&mut bus, 0x0100, TIMER_RELOAD);
    w(&mut bus, 0x0102, 0x0080);
    w(&mut bus, 0x0104, TIMER_RELOAD);
    w(&mut bus, 0x0106, 0x0080);

    // Scenario A: FIFO A, 100 percent volume on both sides, timer 0.
    // The 32-byte queue holds an ascending ramp 00h..1Fh; after 32
    // latches the empty queue holds the last sample (the GBATEK
    // playback procedure keeps running either way).
    w(&mut bus, 0x082, 0x0800); // reset FIFO A (GBATEK init step)
    w(&mut bus, 0x082, 0x0002 | 0x0004 | 0x0300);
    for j in 0..8u32 {
        let word = (4 * j) | ((4 * j + 1) << 8) | ((4 * j + 2) << 16) | ((4 * j + 3) << 24);
        bus.write_u32(BASE + 0x0A0, word);
    }
    let a = capture(&mut bus);

    // Scenario B: FIFO B, 50 percent volume, right side only, timer 1.
    // A signed descending spike drains in 9 latches, then holds -128.
    w(&mut bus, 0x082, 0x8000); // reset FIFO B
    w(&mut bus, 0x082, 0x1000 | 0x4000); // B right, timer 1
    bus.write_u32(BASE + 0x0A4, 0x2040_607F);
    bus.write_u32(BASE + 0x0A4, 0xA0C0_E000);
    bus.write_u8(BASE + 0x0A4, 0x80);
    let b = capture(&mut bus);

    assert_golden(
        "apu-fifo.txt",
        &[
            (
                "A: FIFO A, 32-byte ascending ramp at 100 percent on both sides, then hold".into(),
                a,
            ),
            (
                "B: FIFO B, signed descending spike at 50 percent right only, then hold".into(),
                b,
            ),
        ],
    );
}

#[test]
fn timer_and_dma_sustain_fifo_playback() {
    let mut bus = Bus::new(Vec::new());
    let w = |bus: &mut Bus, off: u32, value: u16| bus.write_u16(BASE + off, value);
    w(&mut bus, 0x084, 0x0080); // master enable
    // FIFO A: 100 percent, both sides, timer 0.
    w(&mut bus, 0x082, 0x0002 | 0x0004 | 0x0300);
    w(&mut bus, 0x082, 0x0800); // reset FIFO A
    w(&mut bus, 0x082, 0x0002 | 0x0004 | 0x0300);

    // A 2048-byte source ramp in IWRAM: byte k = (7k + 13) mod 256.
    let src: Vec<u8> = (0..2048).map(|k| (7 * k + 13) as u8).collect();
    for (k, byte) in src.iter().enumerate() {
        bus.write_u8(IWRAM + k as u32, *byte);
    }
    // DMA 1 (channel 1 is one of the two sound channels): source
    // inc, FIFO A fixed destination, 4 words, repeat, 32-bit,
    // special (demand) timing bits 8-9, enabled.
    bus.write_u32(BASE + 0x0BC, IWRAM);
    bus.write_u32(BASE + 0x0C0, BASE + 0x0A0);
    w(&mut bus, 0x0C4, 4);
    w(&mut bus, 0x0C6, 0x0002 | 0x0010 | 0x0020 | 0x0300 | 0x8000);

    // Timer 0: one overflow per sample slot.
    w(&mut bus, 0x0100, TIMER_RELOAD);
    w(&mut bus, 0x0102, 0x0080);

    // GBATEK init: manually write the first sample byte, then start.
    bus.write_u8(BASE + 0x0A0, src[0]);

    let two_frames = SAMPLES_PER_FRAME * 2;
    for k in 0..two_frames {
        sample_slot(&mut bus);
        let s = bus.take_audio();
        // Latch 0 consumes the CPU-written byte. The first DMA refill
        // delivers from the buffer start, repeating src[0] exactly
        // once; from sample 1 on the stream advances one source byte
        // per latch (any later drop or duplicate shifts the phase).
        // No under-run, no overflow drop.
        let idx = if k == 0 { 0 } else { k - 1 };
        let expected = i32::from(src[idx] as i8) * 120;
        assert_eq!(i32::from(s[0]), expected, "left sample {k}");
        assert_eq!(i32::from(s[1]), expected, "right sample {k}");
        // The refill loop keeps the queue between 16 (right after the
        // first latch) and 32 bytes (a request at the 16-byte level
        // refills to full capacity); it never empties and never
        // overflows into the drop path.
        let len = bus.fifo_len(0);
        assert!((16..=32).contains(&len), "slot {k}: FIFO at {len} bytes");
    }
    // The demand-mode channel stays armed for the next request.
    assert_eq!(
        bus.read_u16(BASE + 0x0C6) & 0x8000,
        0x8000,
        "DMA 1 still enabled"
    );
}

#[test]
fn fifo_dma_follows_the_destination_not_the_channel_number() {
    let mut bus = Bus::new(Vec::new());
    let w = |bus: &mut Bus, off: u32, value: u16| bus.write_u16(BASE + off, value);
    w(&mut bus, 0x084, 0x0080); // master enable
    w(&mut bus, 0x082, 0x0800); // reset FIFO A
    w(&mut bus, 0x082, 0x0002 | 0x0004 | 0x0300); // A: both sides, timer 0
    for k in 0..256u32 {
        bus.write_u8(IWRAM + k, k as u8);
    }
    // DMA 2, usually FIFO B's channel, feeds FIFO A here.
    bus.write_u32(BASE + 0x0C8, IWRAM);
    bus.write_u32(BASE + 0x0CC, BASE + 0x0A0);
    w(&mut bus, 0x0D0, 4);
    w(&mut bus, 0x0D2, 0x0002 | 0x0010 | 0x0020 | 0x0300 | 0x8000);
    w(&mut bus, 0x0100, TIMER_RELOAD);
    w(&mut bus, 0x0102, 0x0080);

    for _ in 0..64 {
        sample_slot(&mut bus);
        bus.take_audio();
    }
    assert!(bus.fifo_len(0) >= 16, "DMA 2 keeps FIFO A fed");
}
