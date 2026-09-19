//! A real (not simulated) spectrum visualizer: taps the actual decoded PCM
//! samples as they flow to the audio device, and periodically runs an FFT
//! over the most recent window to produce a small set of frequency-band
//! levels for the TUI to render as bars.
//!
//! The tap lives on the audio thread's hot path (every decoded sample passes
//! through it), so it must be cheap — it only pushes into a fixed-capacity
//! ring buffer. The actual FFT runs on the existing 250ms poll tick
//! (`playback::poll_progress`), not per-sample, keeping the expensive part
//! off the hot path entirely.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// Re-exported so existing `visualizer::BARS`/`visualizer::MAX_LEVEL`
// references elsewhere in this crate keep working — `maraetai_common::spectrum`
// is the actual source of truth, shared with the TUI's renderer.
pub use maraetai_common::spectrum::{BARS, MAX_LEVEL};
use rodio::Source;
use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

/// Samples analyzed per FFT — a power of two, large enough for reasonable
/// low-frequency resolution (~43Hz/bin at 44.1kHz) without costing much CPU
/// on a 250ms tick.
const FFT_SIZE: usize = 1024;

const MIN_FREQ_HZ: f32 = 20.0;

/// A fixed-capacity ring buffer of raw (interleaved, not yet downmixed) i16
/// samples, shared between the audio-thread tap (writer) and the periodic
/// analyzer (reader). Capacity is generous relative to `FFT_SIZE` so the
/// analyzer always has a full window even right after a track/seek starts.
pub struct SampleRing {
    buf: Mutex<VecDeque<i16>>,
    capacity: usize,
}

impl SampleRing {
    fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self { buf: Mutex::new(VecDeque::with_capacity(capacity)), capacity })
    }

    fn push(&self, sample: i16) {
        let mut buf = self.buf.lock().expect("sample ring poisoned");
        if buf.len() >= self.capacity {
            buf.pop_front();
        }
        buf.push_back(sample);
    }

    fn clear(&self) {
        self.buf.lock().expect("sample ring poisoned").clear();
    }

    /// The most recent `n` raw (interleaved) samples, oldest first. Shorter
    /// than `n` (e.g. just after starting playback) is padded with zeros at
    /// the front, so callers always get a fixed-length slice.
    fn latest(&self, n: usize) -> Vec<i16> {
        let buf = self.buf.lock().expect("sample ring poisoned");
        let have = buf.len().min(n);
        let mut out = vec![0i16; n - have];
        out.extend(buf.iter().rev().take(have).rev());
        out
    }
}

/// Wraps a `Source<Item = i16>` (what `rodio::Decoder` produces regardless of
/// input codec), forwarding every sample to the `Sink` unchanged while also
/// pushing a copy into the shared ring buffer for the visualizer to read.
pub struct VisualizerTap<S> {
    inner: S,
    ring: Arc<SampleRing>,
}

impl<S> VisualizerTap<S> {
    pub fn new(inner: S, ring: Arc<SampleRing>) -> Self {
        Self { inner, ring }
    }
}

impl<S: Iterator<Item = i16>> Iterator for VisualizerTap<S> {
    type Item = i16;
    fn next(&mut self) -> Option<i16> {
        let sample = self.inner.next()?;
        self.ring.push(sample);
        Some(sample)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source<Item = i16>> Source for VisualizerTap<S> {
    fn current_frame_len(&self) -> Option<usize> {
        self.inner.current_frame_len()
    }
    fn channels(&self) -> u16 {
        self.inner.channels()
    }
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<std::time::Duration> {
        self.inner.total_duration()
    }
    /// Without this override, `Source::try_seek`'s default (`Err(NotSupported)`)
    /// wins — the inner decoder can seek fine, but `Sink::try_seek` only ever
    /// sees the outermost wrapping source, so a tap that doesn't forward the
    /// call makes every seek silently fail regardless of the decoder underneath.
    fn try_seek(&mut self, pos: std::time::Duration) -> Result<(), rodio::source::SeekError> {
        let result = self.inner.try_seek(pos);
        if result.is_ok() {
            // Stale samples from before the seek would otherwise linger in
            // the ring for one tick, briefly showing the spectrum of audio
            // that's no longer playing.
            self.ring.clear();
        }
        result
    }
}

/// Owns the FFT machinery and the auto-gain state (a slowly-decaying peak
/// magnitude, so the visualizer scales to whatever's actually playing rather
/// than being permanently maxed-out or permanently flat).
pub struct SpectrumAnalyzer {
    ring: Arc<SampleRing>,
    fft: std::sync::Arc<dyn rustfft::Fft<f32>>,
    window: Vec<f32>,
    running_max: f32,
}

impl SpectrumAnalyzer {
    pub fn new() -> (Self, Arc<SampleRing>) {
        let ring = SampleRing::new(FFT_SIZE * 2 /* headroom for stereo interleaving */);
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let window = hann_window(FFT_SIZE);
        (
            Self { ring: Arc::clone(&ring), fft, window, running_max: 0.0 },
            ring,
        )
    }

    /// Clears buffered samples from whatever was playing before — called on
    /// track change so the visualizer doesn't show a stale frame from the
    /// previous track for one tick.
    pub fn reset(&self) {
        self.ring.clear();
    }

    /// Computes the current bar levels (0..=MAX_LEVEL each), or all-zero
    /// bars if there aren't enough samples buffered yet (e.g. right after
    /// starting playback) or no channel/sample-rate info is available.
    pub fn compute(&mut self, channels: u16, sample_rate: u32) -> [u8; BARS] {
        let channels = channels.max(1) as usize;
        let raw = self.ring.latest(FFT_SIZE * channels);

        // Downmix interleaved channels to mono, normalized to -1.0..1.0.
        let mono: Vec<f32> = raw
            .chunks(channels)
            .map(|frame| frame.iter().map(|&s| s as f32 / i16::MAX as f32).sum::<f32>() / channels as f32)
            .collect();

        let mut buffer: Vec<Complex<f32>> = mono
            .iter()
            .zip(&self.window)
            .map(|(s, w)| Complex { re: s * w, im: 0.0 })
            .collect();
        buffer.resize(FFT_SIZE, Complex { re: 0.0, im: 0.0 });
        self.fft.process(&mut buffer);

        let nyquist_bin = FFT_SIZE / 2;
        let bin_hz = sample_rate as f32 / FFT_SIZE as f32;
        let max_freq = (sample_rate as f32 / 2.0).max(MIN_FREQ_HZ * 2.0);

        let mut bars = [0f32; BARS];
        for (i, bar) in bars.iter_mut().enumerate() {
            let f_lo = MIN_FREQ_HZ * (max_freq / MIN_FREQ_HZ).powf(i as f32 / BARS as f32);
            let f_hi = MIN_FREQ_HZ * (max_freq / MIN_FREQ_HZ).powf((i + 1) as f32 / BARS as f32);
            let bin_lo = ((f_lo / bin_hz) as usize).min(nyquist_bin.saturating_sub(1));
            let bin_hi = ((f_hi / bin_hz) as usize).clamp(bin_lo + 1, nyquist_bin);
            *bar = buffer[bin_lo..bin_hi].iter().map(|c| (c.re * c.re + c.im * c.im).sqrt()).fold(0.0, f32::max);
        }

        let frame_max = bars.iter().cloned().fold(0.0f32, f32::max);
        // Decay slowly toward the current frame's peak rather than snapping
        // to it, so the scale doesn't flicker bar-to-bar; rise instantly to
        // a new, louder peak so a sudden loud passage doesn't clip at first.
        self.running_max = (self.running_max * 0.95).max(frame_max);

        let scale = self.running_max.max(1e-6);
        let mut levels = [0u8; BARS];
        for (level, &bar) in levels.iter_mut().zip(bars.iter()) {
            let normalized = (bar / scale).clamp(0.0, 1.0);
            *level = (normalized * MAX_LEVEL as f32).round() as u8;
        }
        levels
    }
}

fn hann_window(len: usize) -> Vec<f32> {
    (0..len)
        .map(|n| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * n as f32 / (len - 1) as f32).cos()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_pads_with_zeros_when_not_enough_samples_yet() {
        let ring = SampleRing::new(100);
        ring.push(1);
        ring.push(2);
        let latest = ring.latest(5);
        assert_eq!(latest, vec![0, 0, 0, 1, 2]);
    }

    #[test]
    fn ring_drops_oldest_beyond_capacity() {
        let ring = SampleRing::new(3);
        for s in [1, 2, 3, 4, 5] {
            ring.push(s);
        }
        assert_eq!(ring.latest(3), vec![3, 4, 5]);
    }

    #[test]
    fn silence_produces_all_zero_bars() {
        let (mut analyzer, ring) = SpectrumAnalyzer::new();
        for _ in 0..(FFT_SIZE * 2) {
            ring.push(0);
        }
        let bars = analyzer.compute(2, 44100);
        assert_eq!(bars, [0u8; BARS], "silence must not produce non-zero bars");
    }

    #[test]
    fn a_pure_tone_lights_up_a_bar_near_its_frequency() {
        let (mut analyzer, ring) = SpectrumAnalyzer::new();
        let sample_rate = 44100.0f32;
        let tone_hz = 1000.0f32; // solidly mid-range, away from bucket edges
        for n in 0..(FFT_SIZE * 2) {
            let t = (n / 2) as f32 / sample_rate; // stereo: same value both channels
            let v = (2.0 * std::f32::consts::PI * tone_hz * t).sin();
            ring.push((v * i16::MAX as f32 * 0.8) as i16);
        }
        let bars = analyzer.compute(2, 44100);
        let loudest = bars.iter().enumerate().max_by_key(|(_, &v)| v).unwrap().0;

        // The bucket containing 1kHz, given BARS log-spaced bands from 20Hz
        // to ~22050Hz — computed the same way `compute` does, to avoid a
        // brittle hardcoded bar index if BARS/MIN_FREQ_HZ ever change.
        let max_freq = sample_rate / 2.0;
        let expected_bar = ((tone_hz / MIN_FREQ_HZ).ln() / (max_freq / MIN_FREQ_HZ).ln() * BARS as f32) as usize;

        assert!(
            loudest.abs_diff(expected_bar) <= 1,
            "loudest bar {loudest} should be near the 1kHz bucket {expected_bar}, bars={bars:?}"
        );
        assert!(bars[loudest] >= MAX_LEVEL - 1, "a strong pure tone should nearly max out its bar");
    }
}
