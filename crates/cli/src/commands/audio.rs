//! Windowed-run audio output: streams the runtime's APU mix through
//! cpal, resampling to the output device's native rate and channels.
//!
//! The mix rate tracks whatever the runtime actually produces (the
//! per-frame pair count varies with DMA stall time), so a rate tracker
//! measures production against the wall clock and drives the
//! resampler's base step; a small level controller trims residual
//! drift; and playback starts only after a pre-buffer fills. Pacing
//! itself is left to the device clock, which the host enforces once
//! the stream plays.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamError};

/// The APU's nominal mix rate: one stereo pair every 512 cycles. The
/// rate tracker corrects this when DMA stall time stretches frames.
const MIX_RATE: u32 = 32768;

/// Ring level playback aims to hold, in samples (about 125 ms of
/// stereo mix). Playback starts once the ring reaches this depth (or
/// [`START_TIMEOUT`] elapses), and the level controller keeps it there.
const TARGET_LEVEL: usize = 8192;

/// How long the pre-buffer may fill before playback starts anyway.
const START_TIMEOUT: Duration = Duration::from_millis(400);

/// Largest rate correction the level controller may apply on top of
/// the tracked base step (a couple of percent of pitch, inaudible,
/// but enough to hold the level against tracker estimation error).
const MAX_ADJUST: f32 = 0.025;

/// Bounded queue of interleaved stereo samples handed from the emulator
/// loop (producer) to the audio callback (consumer).
struct Ring {
    queue: Mutex<VecDeque<i16>>,
    capacity: usize,
}

impl Ring {
    fn new(capacity: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<i16>> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Appends samples, dropping the oldest past `capacity` so a stalled
    /// consumer bounds the playback latency.
    fn push(&self, samples: &[i16]) {
        let mut queue = self.lock();
        queue.extend(samples.iter().copied());
        let excess = queue.len().saturating_sub(self.capacity);
        queue.drain(..excess);
    }

    fn len(&self) -> usize {
        self.lock().len()
    }

    /// Pops one stereo pair, or `None` when the producer has starved.
    fn pop_pair(&self) -> Option<[i16; 2]> {
        let mut queue = self.lock();
        if queue.len() < 2 {
            return None;
        }
        Some([queue.pop_front()?, queue.pop_front()?])
    }

    /// The pair a linear interpolation would blend toward.
    fn peek_pair(&self) -> Option<[i16; 2]> {
        let queue = self.lock();
        Some([*queue.front()?, *queue.get(1)?])
    }
}

/// Linear-interpolating rate converter from the mix rate to the device
/// rate, holding the last pair across underruns. The callback sets
/// `step` each invocation from the tracked base and level trim.
struct Resampler {
    step: f32,
    cursor: f32,
    last: [i16; 2],
}

impl Resampler {
    /// Fills `out` stereo pairs from `ring`, one output frame per entry.
    fn fill(&mut self, ring: &Ring, out: &mut [[i16; 2]]) {
        for pair in out {
            while self.cursor >= 1.0 {
                match ring.pop_pair() {
                    Some(next) => {
                        self.last = next;
                        self.cursor -= 1.0;
                    }
                    None => {
                        // Underrun: hold position instead of racing ahead.
                        self.cursor = self.cursor.min(1.0);
                        break;
                    }
                }
            }
            let toward = ring.peek_pair().unwrap_or(self.last);
            let t = self.cursor;
            *pair = [
                lerp(self.last[0], toward[0], t),
                lerp(self.last[1], toward[1], t),
            ];
            self.cursor += self.step;
        }
    }
}

/// Blends two samples at fractional position `t` in 0..1.
fn lerp(a: i16, b: i16, t: f32) -> i16 {
    (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as i16
}

/// Rate multiplier that pulls the ring level back toward
/// [`TARGET_LEVEL`]: a falling level slows consumption, a rising one
/// speeds it, within a fraction of a percent (imperceptible pitch).
fn step_scale(level: usize) -> f32 {
    let deviation = (level as f32 - TARGET_LEVEL as f32) / TARGET_LEVEL as f32;
    1.0 + (deviation.clamp(-1.0, 1.0) * MAX_ADJUST)
}

/// Measures production pairs against the wall clock and publishes the
/// resampler's base step (source pairs per device frame) for the
/// callback, which cannot see the producer side.
struct RateTracker {
    last: Option<Instant>,
    ema: f32,
    device_rate: f32,
    base: Arc<AtomicU32>,
}

impl RateTracker {
    fn new(device_rate: f32, base: Arc<AtomicU32>) -> Self {
        Self {
            last: None,
            ema: 0.0,
            device_rate,
            base,
        }
    }

    fn observe(&mut self, samples: usize, now: Instant) {
        let pairs = (samples / 2) as f32;
        if let Some(t) = self.last {
            let dt = now.duration_since(t).as_secs_f32();
            if dt >= 1e-3 {
                let instant = pairs / dt;
                self.ema = if self.ema > 0.0 {
                    self.ema + (instant - self.ema) * 0.1
                } else {
                    instant
                };
                let step = (self.ema / self.device_rate).clamp(0.25, 4.0);
                self.base.store(step.to_bits(), Ordering::Relaxed);
            }
        }
        self.last = Some(now);
    }
}

/// Writes one converted stereo pair into one device frame of `channels`
/// channels, zeroing anything beyond stereo.
fn write_frame(frame: &mut [i16], pair: [i16; 2]) {
    match frame.len() {
        0 => {}
        1 => frame[0] = ((i32::from(pair[0]) + i32::from(pair[1])) / 2) as i16,
        _ => {
            frame[0] = pair[0];
            frame[1] = pair[1];
            frame[2..].fill(0);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_i16(
    out: &mut [i16],
    ring: &Ring,
    resampler: &mut Resampler,
    base: &AtomicU32,
    channels: usize,
) {
    let frames = out.len() / channels;
    resampler.step = f32::from_bits(base.load(Ordering::Relaxed)) * step_scale(ring.len());
    let mut pairs = vec![[0i16; 2]; frames];
    resampler.fill(ring, &mut pairs);
    for (frame, pair) in pairs.iter().enumerate() {
        write_frame(&mut out[frame * channels..(frame + 1) * channels], *pair);
    }
    out[frames * channels..].fill(0);
}

#[allow(clippy::too_many_arguments)]
fn fill_f32(
    out: &mut [f32],
    ring: &Ring,
    resampler: &mut Resampler,
    base: &AtomicU32,
    channels: usize,
) {
    let frames = out.len() / channels;
    resampler.step = f32::from_bits(base.load(Ordering::Relaxed)) * step_scale(ring.len());
    let mut pairs = vec![[0i16; 2]; frames];
    resampler.fill(ring, &mut pairs);
    for (frame, pair) in pairs.iter().enumerate() {
        let slot = &mut out[frame * channels..(frame + 1) * channels];
        match slot.len() {
            0 => {}
            1 => slot[0] = (f32::from(pair[0]) + f32::from(pair[1])) / 65_536.0,
            _ => {
                slot[0] = f32::from(pair[0]) / 32_768.0;
                slot[1] = f32::from(pair[1]) / 32_768.0;
                slot[2..].fill(0.0);
            }
        }
    }
    out[frames * channels..].fill(0.0);
}

/// The windowed run's audio output: a producer source and the live
/// stream, with playback started once the pre-buffer is full.
pub struct Output {
    stream: Stream,
    source: Source,
    started: bool,
    first_push: Instant,
}

impl Output {
    /// Appends one frame's worth of interleaved stereo samples and
    /// starts playback once the ring holds the target pre-buffer.
    pub fn push(&mut self, samples: &[i16]) {
        let now = Instant::now();
        self.source.tracker.observe(samples.len(), now);
        self.source.push(samples);
        if !self.started
            && (self.source.ring.len() >= TARGET_LEVEL || now - self.first_push >= START_TIMEOUT)
        {
            if let Err(e) = self.stream.play() {
                eprintln!("oxyboy run: audio stream error: {e}");
            }
            self.started = true;
        }
    }
}

/// Producer-side state: the shared ring, the rate tracker publishing
/// the base step, and a debug tick counter.
struct Source {
    ring: Arc<Ring>,
    tracker: RateTracker,
    ticks: Mutex<u64>,
}

impl Source {
    fn push(&self, samples: &[i16]) {
        if std::env::var_os("OXYBOY_AUDIO_DEBUG").is_some() {
            let mut n = self.ticks.lock().unwrap_or_else(|p| p.into_inner());
            *n += 1;
            if n.is_multiple_of(120) {
                eprintln!(
                    "audio: push #{n}, +{} samples, ring {}, est {:.0} pairs/s",
                    samples.len(),
                    self.ring.len(),
                    self.tracker.ema
                );
            }
        }
        self.ring.push(samples);
    }
}

/// Opens the default output device and prepares a paced stream pulling
/// from a fresh ring. Playback starts on an [`Output::push`] that
/// fills the pre-buffer; dropping the [`Output`] stops it.
///
/// # Errors
///
/// Returns a printable message when no device exists, the device's
/// sample format is not supported, or the stream cannot open.
pub fn start() -> Result<Output, String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no audio output device".to_string())?;
    let config = device
        .default_output_config()
        .map_err(|e| format!("cannot query audio config: {e}"))?;
    let rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    let format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.into();
    let ring = Arc::new(Ring::new(MIX_RATE as usize * 2));
    let base = Arc::new(AtomicU32::new((MIX_RATE as f32 / rate as f32).to_bits()));
    let report = |e: StreamError| eprintln!("oxyboy run: audio stream error: {e}");

    let stream = match format {
        SampleFormat::I16 => {
            let mut resampler = Resampler {
                step: MIX_RATE as f32 / rate as f32,
                cursor: 0.0,
                last: [0; 2],
            };
            let producer = Arc::clone(&ring);
            let step = Arc::clone(&base);
            device
                .build_output_stream(
                    &stream_config,
                    move |out: &mut [i16], _| {
                        fill_i16(out, &producer, &mut resampler, &step, channels);
                    },
                    report,
                    None,
                )
                .map_err(|e| format!("cannot open audio stream: {e}"))?
        }
        SampleFormat::F32 => {
            let mut resampler = Resampler {
                step: MIX_RATE as f32 / rate as f32,
                cursor: 0.0,
                last: [0; 2],
            };
            let producer = Arc::clone(&ring);
            let step = Arc::clone(&base);
            device
                .build_output_stream(
                    &stream_config,
                    move |out: &mut [f32], _| {
                        fill_f32(out, &producer, &mut resampler, &step, channels);
                    },
                    report,
                    None,
                )
                .map_err(|e| format!("cannot open audio stream: {e}"))?
        }
        other => return Err(format!("unsupported audio sample format {other:?}")),
    };
    println!("audio: {rate} Hz, {channels} channel(s), {format:?} via PipeWire/ALSA");
    Ok(Output {
        stream,
        source: Source {
            tracker: RateTracker::new(rate as f32, base),
            ring,
            ticks: Mutex::new(0),
        },
        started: false,
        first_push: Instant::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Duration, Instant, MAX_ADJUST, RateTracker, Resampler, Ring, TARGET_LEVEL, lerp, step_scale,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The pair encoding `n`: `[1000(n+1), -1000(n+1)]`, distinct from
    /// the initial silent pair.
    fn pair(n: i16) -> [i16; 2] {
        let v = 1000 * (n + 1);
        [v, -v]
    }

    fn resampler(step: f32) -> Resampler {
        Resampler {
            step,
            cursor: 0.0,
            last: [0; 2],
        }
    }

    #[test]
    fn lerp_spans_the_full_sample_range() {
        assert_eq!(lerp(-30000, 30000, 0.5), 0);
        assert_eq!(lerp(i16::MIN, i16::MAX, 1.0), i16::MAX);
    }

    #[test]
    fn ring_push_pop_preserves_order() {
        let ring = Ring::new(16);
        ring.push(&[1, 2, 3, 4]);
        assert_eq!(ring.pop_pair(), Some([1, 2]));
        assert_eq!(ring.pop_pair(), Some([3, 4]));
        assert_eq!(ring.pop_pair(), None);
    }

    #[test]
    fn ring_push_drops_oldest_past_capacity() {
        let ring = Ring::new(4);
        ring.push(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(ring.pop_pair(), Some([3, 4]));
        assert_eq!(ring.pop_pair(), Some([5, 6]));
        assert_eq!(ring.pop_pair(), None);
    }

    #[test]
    fn resampler_at_mix_rate_passes_through_one_frame_late() {
        let ring = Ring::new(64);
        for n in 0..8 {
            ring.push(&pair(n));
        }
        let mut resampler = resampler(1.0);
        let mut out = vec![[0i16; 2]; 8];
        resampler.fill(&ring, &mut out);
        assert_eq!(out[0], [0, 0]);
        for (i, value) in out[1..].iter().enumerate() {
            assert_eq!(*value, pair(i as i16), "frame {i}");
        }
    }

    #[test]
    fn resampler_upsamples_by_two_with_interpolation() {
        let ring = Ring::new(64);
        for n in 0..8 {
            ring.push(&pair(n));
        }
        let mut resampler = resampler(0.5);
        let mut out = vec![[0i16; 2]; 4];
        resampler.fill(&ring, &mut out);
        assert_eq!(out[0], [0, 0]);
        assert_eq!(out[1], [500, -500]);
        assert_eq!(out[2], pair(0));
        assert_eq!(out[3], [1500, -1500]);
    }

    #[test]
    fn resampler_underrun_holds_silence() {
        let ring = Ring::new(64);
        let mut resampler = resampler(1.0);
        let mut out = vec![[7i16; 2]; 3];
        resampler.fill(&ring, &mut out);
        assert!(out.iter().all(|pair| *pair == [0, 0]));
    }

    #[test]
    fn step_scale_pulls_the_level_back_to_target() {
        assert!(step_scale(0) < 1.0, "empty ring must slow consumption");
        assert_eq!(step_scale(TARGET_LEVEL), 1.0);
        assert!(
            step_scale(TARGET_LEVEL * 3) > 1.0,
            "overfull ring must speed consumption"
        );
        let overshoot = step_scale(usize::MAX);
        assert!((overshoot - 1.0).abs() <= MAX_ADJUST + 1e-6);
    }

    #[test]
    fn rate_tracker_tracks_production_rate() {
        let base = Arc::new(AtomicU32::new((32768.0f32 / 44100.0).to_bits()));
        let mut tracker = RateTracker::new(44100.0, Arc::clone(&base));
        let t0 = Instant::now();
        tracker.observe(1368, t0);
        tracker.observe(1368, t0 + Duration::from_millis(50));
        let step = f32::from_bits(base.load(Ordering::Relaxed));
        let expected = (1368.0 / 2.0 / 0.05) / 44100.0;
        assert!(
            (step - expected).abs() < 0.01,
            "step {step} should sit at {expected}"
        );
    }
}
