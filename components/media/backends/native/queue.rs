/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A queue of stereo frames read out at another rate: the Web Audio sink and the `<audio>` player
//! both feed one at their own rate and the output reads it at the device's. Reading resamples
//! with a windowed sinc (16 taps, 128 phases); at equal rates it copies.

use std::collections::VecDeque;
use std::f64::consts::PI;

/// Taps on each side of the read position.
const HALF: usize = 8;
const TAPS: usize = 2 * HALF;
const PHASES: usize = 128;

struct Kernel {
    step: f64,
    /// `(PHASES + 1) * TAPS` coefficients; row `p` is for a fractional position `p / PHASES`.
    table: Vec<f32>,
}

impl Kernel {
    fn new(step: f64) -> Self {
        // Low-pass at the lower Nyquist rate (relative to the input), a little under it when
        // rates differ.
        let cutoff = if step == 1. {
            1.
        } else {
            (1. / step).min(1.) * 0.95
        };
        let mut table = Vec::with_capacity((PHASES + 1) * TAPS);
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let row: Vec<f64> = (0..TAPS)
                .map(|k| {
                    // Distance from the read position (index HALF - 1 + frac) to tap k.
                    let d = frac - (k as f64 - (HALF as f64 - 1.));
                    let w = 0.5 * (1. + (PI * d / HALF as f64).cos());
                    let x = PI * d * cutoff;
                    let sinc = if x.abs() < 1e-9 { 1. } else { x.sin() / x };
                    if d.abs() >= HALF as f64 { 0. } else { sinc * w }
                })
                .collect();
            let sum: f64 = row.iter().sum();
            table.extend(row.iter().map(|c| (c / sum) as f32));
        }
        Self { step, table }
    }

    fn taps(&self, frac: f64) -> &[f32] {
        let p = ((frac * PHASES as f64).round() as usize).min(PHASES);
        &self.table[p * TAPS..(p + 1) * TAPS]
    }
}

pub struct FrameQueue {
    /// History (`HALF - 1` frames before the read position) and queued frames.
    frames: VecDeque<[f32; 2]>,
    /// Fractional read position past `frames[HALF - 1]`.
    frac: f64,
    kernel: Option<Kernel>,
}

impl Default for FrameQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameQueue {
    pub fn new() -> Self {
        let mut q = Self {
            frames: VecDeque::new(),
            frac: 0.,
            kernel: None,
        };
        q.clear();
        q
    }

    /// Drop everything queued (a seek).
    pub fn clear(&mut self) {
        self.frames.clear();
        self.frames.extend(std::iter::repeat_n([0.; 2], HALF - 1));
        self.frac = 0.;
    }

    pub fn extend(&mut self, frames: impl IntoIterator<Item = [f32; 2]>) {
        self.frames.extend(frames);
    }

    /// Append the silence the filter needs to play out the last frames (end of stream).
    pub fn flush(&mut self) {
        self.frames.extend(std::iter::repeat_n([0.; 2], HALF + 1));
    }

    /// Frames queued ahead of the read position.
    pub fn len(&self) -> usize {
        self.frames.len().saturating_sub(HALF - 1)
    }

    /// Nothing more can be read until more frames arrive.
    pub fn is_empty(&self) -> bool {
        self.frames.len() < TAPS || (self.frames.len() == TAPS && self.frac >= 1.)
    }

    /// Read up to `n` frames, advancing `step` input frames per output frame; `put(i, l, r)` gets
    /// each. Stops early on underrun. Returns the number of input frames consumed.
    pub fn read(&mut self, n: usize, step: f64, mut put: impl FnMut(usize, f32, f32)) -> u64 {
        if step <= 0. {
            return 0;
        }
        if self.kernel.as_ref().is_none_or(|k| k.step != step) {
            self.kernel = Some(Kernel::new(step));
        }
        let kernel = self.kernel.as_ref().unwrap();
        let mut consumed = 0;
        for i in 0..n {
            while self.frac >= 1. {
                if self.frames.len() <= TAPS {
                    break;
                }
                self.frames.pop_front();
                self.frac -= 1.;
                consumed += 1;
            }
            if self.frac >= 1. || self.frames.len() < TAPS {
                break;
            }
            let (mut l, mut r) = (0f32, 0f32);
            if step == 1. && self.frac == 0. {
                let f = self.frames[HALF - 1];
                l = f[0];
                r = f[1];
            } else {
                for (k, c) in kernel.taps(self.frac).iter().enumerate() {
                    let f = self.frames[k];
                    l += f[0] * c;
                    r += f[1] * c;
                }
            }
            put(i, l, r);
            self.frac += step;
        }
        consumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: f64, n: usize) -> Vec<[f32; 2]> {
        (0..n)
            .map(|i| {
                let s = (2. * PI * 1000. * i as f64 / rate).sin() as f32;
                [s, -s]
            })
            .collect()
    }

    #[test]
    fn copies_at_equal_rates() {
        let mut q = FrameQueue::new();
        q.extend((0..100).map(|i| [i as f32, 0.]));
        let mut out = vec![];
        let consumed = q.read(50, 1., |_, l, _| out.push(l));
        assert_eq!(out, (0..50).map(|i| i as f32).collect::<Vec<_>>());
        assert_eq!(consumed, 49);
    }

    #[test]
    fn resamples_a_sine() {
        let mut q = FrameQueue::new();
        q.extend(sine(44100., 44100));
        let mut out = vec![];
        q.read(48000, 44100. / 48000., |_, l, r| out.push((l, r)));
        assert!(out.len() > 47900);
        // Compare against the ideal 1 kHz sine at 48 kHz, past the filter's start-up.
        let err = out[100..47000]
            .iter()
            .enumerate()
            .map(|(i, (l, _))| {
                let t = (i + 100) as f64 / 48000.;
                (*l as f64 - (2. * PI * 1000. * t).sin()).abs()
            })
            .fold(0., f64::max);
        assert!(err < 0.01, "max error {err}");
    }

    #[test]
    fn drains_after_flush() {
        let mut q = FrameQueue::new();
        q.extend(sine(48000., 100));
        q.flush();
        let mut n = 0;
        q.read(1000, 1., |_, _, _| n += 1);
        assert!(q.is_empty());
        assert!(n >= 100, "{n}");
        let mut q = FrameQueue::new();
        q.extend(sine(44100., 100));
        q.flush();
        q.read(1000, 44100. / 48000., |_, _, _| {});
        assert!(q.is_empty());
    }

    #[test]
    fn underrun_keeps_place() {
        let mut q = FrameQueue::new();
        q.extend(sine(48000., 20));
        let mut n = 0;
        q.read(100, 1., |_, _, _| n += 1);
        assert!(n < 20);
        q.extend(sine(48000., 100));
        let mut m = 0;
        q.read(50, 1., |_, _, _| m += 1);
        assert_eq!(m, 50);
    }
}
