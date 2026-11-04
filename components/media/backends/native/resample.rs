/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Resampling a whole buffer (decodeAudioData, decoded `<audio>` files): a Hann-windowed sinc,
//! low-passed at the lower of the two Nyquist rates.

use std::f64::consts::PI;

const HALF_TAPS: i64 = 16;

pub fn resample_offline(input: &[f32], in_rate: u32, out_rate: u32) -> Vec<f32> {
    if in_rate == out_rate || input.is_empty() || in_rate == 0 || out_rate == 0 {
        return input.to_vec();
    }
    let ratio = in_rate as f64 / out_rate as f64;
    let out_len = ((input.len() as f64) / ratio).round() as usize;
    // Cutoff relative to the input rate.
    let cutoff = (out_rate as f64 / in_rate as f64).min(1.);
    let half = (HALF_TAPS as f64 / cutoff).ceil() as i64;
    let n_in = input.len() as i64;
    let mut out = Vec::with_capacity(out_len);
    for n in 0..out_len {
        let t = n as f64 * ratio;
        let center = t.floor() as i64;
        let mut acc = 0.;
        let mut norm = 0.;
        for k in (center - half + 1)..=(center + half) {
            let x = t - k as f64;
            let w = 0.5 * (1. + (PI * x / half as f64).cos());
            let arg = PI * x * cutoff;
            let sinc = if arg.abs() < 1e-9 {
                1.
            } else {
                arg.sin() / arg
            };
            let coef = sinc * w;
            norm += coef;
            if (0..n_in).contains(&k) {
                acc += input[k as usize] as f64 * coef;
            }
        }
        out.push(if norm != 0. { (acc / norm) as f32 } else { 0. });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_dc_and_length() {
        let input = vec![0.5f32; 44100];
        let out = resample_offline(&input, 44100, 48000);
        assert_eq!(out.len(), 48000);
        assert!((out[24000] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn keeps_a_sine() {
        let input: Vec<f32> = (0..48000)
            .map(|i| (2. * PI * 440. * i as f64 / 48000.).sin() as f32)
            .collect();
        let out = resample_offline(&input, 48000, 44100);
        let expected = (2. * PI * 440. * 22050. / 44100.).sin() as f32;
        assert!((out[22050] - expected).abs() < 1e-2);
    }
}
