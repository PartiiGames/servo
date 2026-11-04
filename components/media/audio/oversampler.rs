/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Integer-factor up/down sampling for the WaveShaperNode's "2x" and "4x" oversampling.
//!
//! <https://webaudio.github.io/web-audio-api/#dom-waveshapernode-oversample> leaves the filters
//! to the implementation. We use one linear-phase windowed-sinc low-pass (Blackman window) per
//! factor, run as a polyphase interpolator on the way up and as a decimator on the way down.
//! Every polyphase branch of the interpolator, and the decimator as a whole, is normalised to a
//! DC gain of exactly 1, so a constant signal comes out unchanged once the filters have filled.

use std::f64::consts::PI;

/// Taps per polyphase branch. The full filter has `TAPS_PER_PHASE * factor` taps.
const TAPS_PER_PHASE: usize = 24;

/// Pass band edge, as a fraction of the base-rate Nyquist frequency.
const CUTOFF: f64 = 0.9;

/// Design the low-pass prototype for `factor`, at the oversampled rate, normalised to DC gain 1.
fn design(factor: usize) -> Vec<f32> {
    let n = TAPS_PER_PHASE * factor;
    // Cutoff in cycles per (oversampled) sample.
    let fc = CUTOFF * 0.5 / factor as f64;
    let mid = (n - 1) as f64 / 2.;
    let mut h: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 - mid;
            let sinc = if t == 0. {
                2. * fc
            } else {
                (2. * PI * fc * t).sin() / (PI * t)
            };
            let w = 0.42 - 0.5 * (2. * PI * i as f64 / (n - 1) as f64).cos() +
                0.08 * (4. * PI * i as f64 / (n - 1) as f64).cos();
            sinc * w
        })
        .collect();
    let sum: f64 = h.iter().sum();
    h.iter_mut().for_each(|v| *v /= sum);
    h.into_iter().map(|v| v as f32).collect()
}

/// Up and down sampling state for one channel.
pub(crate) struct Oversampler {
    factor: usize,
    /// Interpolator branches: `up_phases[p][k]` weights the input sample `k` steps back for
    /// output phase `p`. Each branch sums to 1.
    up_phases: Vec<Vec<f32>>,
    /// The decimator's taps (the prototype, summing to 1).
    down_taps: Vec<f32>,
    /// The last `TAPS_PER_PHASE - 1` base-rate input samples, oldest first.
    up_history: Vec<f32>,
    /// The last `taps - 1` oversampled samples, oldest first.
    down_history: Vec<f32>,
    scratch: Vec<f32>,
}

impl Oversampler {
    pub(crate) fn new(factor: usize) -> Self {
        assert!(factor == 2 || factor == 4, "oversampling factor must be 2 or 4");
        let h = design(factor);
        let up_phases = (0..factor)
            .map(|p| {
                let mut branch: Vec<f32> = (0..TAPS_PER_PHASE).map(|k| h[p + k * factor]).collect();
                let sum: f64 = branch.iter().map(|v| *v as f64).sum();
                branch.iter_mut().for_each(|v| *v = (*v as f64 / sum) as f32);
                branch
            })
            .collect();
        Oversampler {
            factor,
            up_phases,
            down_history: vec![0.; h.len() - 1],
            down_taps: h,
            up_history: vec![0.; TAPS_PER_PHASE - 1],
            scratch: Vec::new(),
        }
    }

    pub(crate) fn factor(&self) -> usize {
        self.factor
    }

    /// Latency added by one up/down round trip, in base-rate frames (rounded down).
    #[cfg(test)]
    pub(crate) fn latency_frames(&self) -> usize {
        // Each linear-phase filter delays by (taps - 1) / 2 oversampled samples. On the way up
        // the interpolator's delay is counted at the oversampled rate too.
        (self.down_taps.len() - 1) / self.factor
    }

    /// Interpolate `input` into `output`, which must be `factor` times as long.
    pub(crate) fn upsample(&mut self, input: &[f32], output: &mut [f32]) {
        debug_assert_eq!(output.len(), input.len() * self.factor);
        let hist = TAPS_PER_PHASE - 1;
        self.scratch.clear();
        self.scratch.extend_from_slice(&self.up_history);
        self.scratch.extend_from_slice(input);
        for (n, frame) in output.chunks_exact_mut(self.factor).enumerate() {
            // The newest input sample for this frame is scratch[hist + n].
            let newest = hist + n;
            for (p, out) in frame.iter_mut().enumerate() {
                let branch = &self.up_phases[p];
                let mut acc = 0.;
                for (k, w) in branch.iter().enumerate() {
                    acc += w * self.scratch[newest - k];
                }
                *out = acc;
            }
        }
        let len = self.scratch.len();
        self.up_history.copy_from_slice(&self.scratch[len - hist..]);
    }

    /// Low-pass and decimate `input` into `output`, which must be `factor` times shorter.
    pub(crate) fn downsample(&mut self, input: &[f32], output: &mut [f32]) {
        debug_assert_eq!(input.len(), output.len() * self.factor);
        let hist = self.down_taps.len() - 1;
        self.scratch.clear();
        self.scratch.extend_from_slice(&self.down_history);
        self.scratch.extend_from_slice(input);
        for (n, out) in output.iter_mut().enumerate() {
            let newest = hist + n * self.factor + self.factor - 1;
            let mut acc = 0.;
            for (j, w) in self.down_taps.iter().enumerate() {
                acc += w * self.scratch[newest - j];
            }
            *out = acc;
        }
        let len = self.scratch.len();
        self.down_history.copy_from_slice(&self.scratch[len - hist..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(factor: usize, input: &[f32]) -> Vec<f32> {
        let mut os = Oversampler::new(factor);
        let mut out = vec![0.; input.len()];
        let mut up = vec![0.; 128 * factor];
        for (i, o) in input.chunks(128).zip(out.chunks_mut(128)) {
            os.upsample(i, &mut up);
            os.downsample(&up, o);
        }
        out
    }

    #[test]
    fn dc_is_preserved() {
        for factor in [2, 4] {
            let out = round_trip(factor, &[0.5; 1024]);
            // After the filters fill, a constant comes through unchanged.
            for v in &out[256..] {
                assert!((v - 0.5).abs() < 1e-6, "factor {factor}: {v}");
            }
        }
    }

    #[test]
    fn low_frequency_sine_passes_with_fixed_delay() {
        for factor in [2, 4] {
            let rate = 48000.;
            let f = 1000.;
            let input: Vec<f32> = (0..2048)
                .map(|n| (2. * std::f64::consts::PI * f * n as f64 / rate).sin() as f32)
                .collect();
            let out = round_trip(factor, &input);
            // Find the integer delay with the best match; the round trip delays by about
            // (taps - 1) / factor oversampled samples' worth of base frames.
            let delay = Oversampler::new(factor).latency_frames();
            let mut best = f32::MAX;
            for d in delay.saturating_sub(2)..=delay + 2 {
                let err = (512..2048)
                    .map(|n| (out[n] - input[n - d]).abs())
                    .fold(0., f32::max);
                best = best.min(err);
            }
            // The true delay is fractional (taps - 1 is odd); half a frame off, a 1 kHz sine at
            // 48 kHz differs by at most 2 sin(pi * 1000 / 48000 / 2) ~ 0.065 from its
            // integer-delayed copy, plus pass-band ripple.
            assert!(best < 0.1, "factor {factor}: max error {best}");
        }
    }

    #[test]
    fn near_nyquist_tone_is_attenuated_on_the_way_down() {
        // A tone above the base-rate Nyquist at the oversampled rate must not alias back down.
        for factor in [2, 4] {
            let mut os = Oversampler::new(factor);
            let os_rate = 48000. * factor as f64;
            let f = 40000.; // above 24 kHz
            let up: Vec<f32> = (0..128 * factor * 16)
                .map(|n| (2. * std::f64::consts::PI * f * n as f64 / os_rate).sin() as f32)
                .collect();
            let mut out = vec![0.; 128 * 16];
            for (i, o) in up.chunks(128 * factor).zip(out.chunks_mut(128)) {
                os.downsample(i, o);
            }
            let peak = out[512..].iter().fold(0f32, |m, v| m.max(v.abs()));
            assert!(peak < 0.01, "factor {factor}: alias peak {peak}");
        }
    }
}
