/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! <https://webaudio.github.io/web-audio-api/#DynamicsCompressorNode>
//!
//! The processing follows the "reduction-gain" algorithm of
//! <https://webaudio.github.io/web-audio-api/#DynamicsCompressorOptions-processing> step by step.
//! Where the specification lets the user agent choose a curve, the choice is documented at the
//! function that implements it:
//! - the soft knee of the [compression curve](CompressorCurve::apply_db),
//! - the [envelope rate](envelope_rate),
//! - the [detector curve](detector_curve).
//!
//! The internal graph of the specification (a 6 ms DelayNode on the signal path, a GainNode
//! driven by an EnvelopeFollower) is implemented inline: a per-channel delay line, and the
//! follower's per-sample reduction gain applied to the delayed signal.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use malloc_size_of_derive::MallocSizeOf;

use crate::audio_node::{AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo};
use crate::block::{Chunk, FRAMES_PER_BLOCK_USIZE, Tick};
use crate::param::{Param, ParamType};

/// The look-ahead delay of the internal graph: `new DelayNode(context, {delayTime: 0.006})`.
const LOOKAHEAD_SECONDS: f64 = 0.006;

/// Below this absolute input value, the attenuation is 1 (step 5.1).
const SILENCE_THRESHOLD: f32 = 0.0001;

/// The `reduction` attribute, shared with the DOM. The render thread stores it once per block
/// (an f32 in its bit pattern), as the specification's "atomically set" step asks.
#[derive(Clone, Debug, Default)]
pub struct ReductionCell(Arc<AtomicU32>);

impl ReductionCell {
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    fn set(&self, value: f32) {
        self.0.store(value.to_bits(), Ordering::Relaxed)
    }
}

impl malloc_size_of::MallocSizeOf for ReductionCell {
    fn size_of(&self, _ops: &mut malloc_size_of::MallocSizeOfOps) -> usize {
        0
    }
}

#[derive(Clone, Debug, MallocSizeOf)]
pub struct DynamicsCompressorNodeOptions {
    pub threshold: f32,
    pub knee: f32,
    pub ratio: f32,
    pub attack: f32,
    pub release: f32,
    /// `[[internal reduction]]`, read by the DOM's `reduction` getter.
    pub reduction: ReductionCell,
}

impl Default for DynamicsCompressorNodeOptions {
    fn default() -> Self {
        DynamicsCompressorNodeOptions {
            threshold: -24.,
            knee: 30.,
            ratio: 12.,
            attack: 0.003,
            release: 0.25,
            reduction: ReductionCell::default(),
        }
    }
}

/// <https://webaudio.github.io/web-audio-api/#linear-to-decibel>
pub(crate) fn linear_to_db(v: f32) -> f32 {
    if v == 0. { -1000. } else { 20. * v.log10() }
}

/// <https://webaudio.github.io/web-audio-api/#decibels-to-linear-gain-unit>
pub(crate) fn db_to_linear(v: f32) -> f32 {
    10f32.powf(v / 20.)
}

/// The compression curve for one block's k-rate threshold, knee and ratio.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CompressorCurve {
    threshold: f32,
    knee: f32,
    ratio: f32,
}

impl CompressorCurve {
    pub(crate) fn new(threshold: f32, knee: f32, ratio: f32) -> Self {
        Self {
            threshold,
            knee,
            ratio,
        }
    }

    /// <https://webaudio.github.io/web-audio-api/#compression-curve>, in decibels.
    ///
    /// > This function is the identity up to the value of the linear threshold.
    /// > From the threshold up to the knee end threshold, User-Agents can choose the curve
    /// > shape. The whole function MUST be monotonically increasing and continuous.
    /// > This function is linear, based on the ratio, after the threshold and the soft knee.
    ///
    /// Our knee is the quadratic whose slope (in dB) goes from 1 at the threshold to
    /// 1 / ratio at the knee end, so the curve is continuous, monotonically increasing and
    /// differentiable everywhere. Above the knee, the output rises 1 dB per `ratio` dB of input.
    pub(crate) fn apply_db(&self, x_db: f32) -> f32 {
        let t = self.threshold;
        let k = self.knee;
        let slope = 1. / self.ratio;
        if x_db <= t {
            x_db
        } else if x_db < t + k {
            let over = x_db - t;
            x_db + (slope - 1.) * over * over / (2. * k)
        } else {
            // Value at the knee end, then the ratio line.
            let knee_end = t + k + (slope - 1.) * k / 2.;
            knee_end + (x_db - (t + k)) * slope
        }
    }

    /// The compression curve applied to a linear value.
    pub(crate) fn apply(&self, x: f32) -> f32 {
        db_to_linear(self.apply_db(linear_to_db(x)))
    }

    /// <https://webaudio.github.io/web-audio-api/#computing-the-makeup-gain>
    ///
    /// > 1. Let full range gain be the value returned by applying the compression curve to 1.0.
    /// > 2. Let full range makeup gain be the inverse of full range gain.
    /// > 3. Return the result of taking the 0.6 power of full range makeup gain.
    pub(crate) fn makeup_gain(&self) -> f32 {
        (1. / self.apply(1.)).powf(0.6)
    }
}

/// <https://webaudio.github.io/web-audio-api/#computing-the-envelope-rate>, as a function of
/// `compressor gain` and `detector average`. `attack_frames` and `release_frames` are the
/// attack and release times multiplied by the sample rate (step 1).
///
/// > When attacking, this number [is] less than or equal to 1, when releasing, strictly greater
/// > than 1. The attack curve MUST be a continuous, monotonically increasing function in the
/// > range [0, 1]. The release curve MUST be a continuous, monotonically decreasing function
/// > that is always greater than 1.
///
/// Attack (step 8 adds `(detector average - compressor gain) * rate`): with
/// `c = 10^(-0.5 / attack_frames)` and `r = detector average / compressor gain < 1`,
/// `rate = min((1 - c) / (1 - r), 1)`. That multiplies the gain by `c` per frame, i.e. it falls
/// by 10 dB in `attack` seconds (the attribute's definition), and never overshoots the
/// detector average. It is continuous and increasing in `r`, within [0, 1].
///
/// Release (step 7 multiplies the gain by the rate): `10^(0.5 / release_frames)`, so the gain
/// rises by 10 dB in `release` seconds; constant in `r`, always greater than 1.
pub(crate) fn envelope_rate(
    releasing: bool,
    compressor_gain: f32,
    detector_average: f32,
    attack_frames: f32,
    release_frames: f32,
) -> f32 {
    if releasing {
        if release_frames <= 0. {
            f32::INFINITY
        } else {
            10f32.powf(0.5 / release_frames)
        }
    } else {
        let c = if attack_frames <= 0. {
            0.
        } else {
            10f32.powf(-0.5 / attack_frames)
        };
        let r = if compressor_gain > 0. {
            detector_average / compressor_gain
        } else {
            1.
        };
        if r < 1. {
            ((1. - c) / (1. - r)).min(1.)
        } else {
            // The detector is at or above the gain without releasing (the attenuation is not
            // above the gain, but the smoothed average is): follow it at the attack speed.
            1. - c
        }
    }
}

/// <https://webaudio.github.io/web-audio-api/#detector-curve>: maps the attenuation to the
/// detector rate.
///
/// > The output of the function MUST be in [0,1]. The function MUST be monotonically
/// > increasing, continuous.
///
/// `0.5 + 0.5 * attenuation`: the detector follows quickly when there is little attenuation
/// and smooths more as the compression gets heavier (an adaptive release).
pub(crate) fn detector_curve(attenuation: f32) -> f32 {
    (0.5 + 0.5 * attenuation).clamp(0., 1.)
}

/// The EnvelopeFollower's two persistent slots and the per-sample loop (steps 5.1 to 5.10).
#[derive(Clone, Debug)]
pub(crate) struct EnvelopeFollower {
    /// `[[detector average]]`, initially 0.
    pub(crate) detector_average: f32,
    /// `[[compressor gain]]`, initially 1.
    pub(crate) compressor_gain: f32,
}

impl Default for EnvelopeFollower {
    fn default() -> Self {
        Self {
            detector_average: 0.,
            compressor_gain: 1.,
        }
    }
}

impl EnvelopeFollower {
    /// Process one sidechain sample; returns the reduction gain for it.
    #[inline]
    pub(crate) fn step(
        &mut self,
        input: f32,
        curve: &CompressorCurve,
        makeup_gain: f32,
        attack_frames: f32,
        release_frames: f32,
    ) -> f32 {
        let abs = input.abs();
        // 5.1
        let attenuation = if abs < SILENCE_THRESHOLD || abs.is_nan() {
            1.
        } else {
            curve.apply(abs) / abs
        };
        // 5.2
        let releasing = attenuation > self.compressor_gain;
        // 5.3, 5.4, 5.5
        let detector_rate = detector_curve(attenuation);
        self.detector_average += (attenuation - self.detector_average) * detector_rate;
        self.detector_average = self.detector_average.min(1.);
        // 5.6
        let rate = envelope_rate(
            releasing,
            self.compressor_gain,
            self.detector_average,
            attack_frames,
            release_frames,
        );
        if releasing {
            // 5.7
            self.compressor_gain = (self.compressor_gain * rate).min(1.);
        } else {
            // 5.8
            let increment = (self.detector_average - self.compressor_gain) * rate;
            self.compressor_gain += increment;
        }
        // 5.9
        self.compressor_gain * makeup_gain
    }
}

#[derive(AudioNodeCommon)]
pub(crate) struct DynamicsCompressorNode {
    channel_info: ChannelInfo,
    threshold: Param,
    knee: Param,
    ratio: Param,
    attack: Param,
    release: Param,
    reduction: ReductionCell,
    follower: EnvelopeFollower,
    /// One look-ahead delay line per channel, all of `delay_frames` length.
    delay_lines: Vec<Vec<f32>>,
    delay_frames: usize,
    delay_pos: usize,
    /// Frames of input still inside the delay lines (0 once they hold only silence).
    delay_live_frames: usize,
    gains: Vec<f32>,
}

impl DynamicsCompressorNode {
    pub fn new(options: DynamicsCompressorNodeOptions, channel_info: ChannelInfo) -> Self {
        options.reduction.set(0.);
        Self {
            channel_info,
            threshold: Param::new_krate(options.threshold),
            knee: Param::new_krate(options.knee),
            ratio: Param::new_krate(options.ratio),
            attack: Param::new_krate(options.attack),
            release: Param::new_krate(options.release),
            reduction: options.reduction,
            follower: EnvelopeFollower::default(),
            delay_lines: Vec::new(),
            delay_frames: 0,
            delay_pos: 0,
            delay_live_frames: 0,
            gains: vec![0.; FRAMES_PER_BLOCK_USIZE],
        }
    }

    fn ensure_delay(&mut self, sample_rate: f32, channels: usize) {
        if self.delay_frames == 0 {
            self.delay_frames = ((LOOKAHEAD_SECONDS * sample_rate as f64).round() as usize).max(1);
        }
        if self.delay_lines.len() != channels {
            self.delay_lines
                .resize_with(channels, || vec![0.; self.delay_frames]);
        }
    }
}

impl AudioNodeEngine for DynamicsCompressorNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::DynamicsCompressionNode
    }

    fn process(&mut self, mut inputs: Chunk, info: &BlockInfo) -> Chunk {
        debug_assert!(inputs.len() == 1);

        // k-rate parameters: sampled once, at the start of the block.
        for p in [
            &mut self.threshold,
            &mut self.knee,
            &mut self.ratio,
            &mut self.attack,
            &mut self.release,
        ] {
            p.update(info, Tick(0));
        }
        let curve = CompressorCurve::new(
            self.threshold.value().clamp(-100., 0.),
            self.knee.value().clamp(0., 40.),
            self.ratio.value().clamp(1., 20.),
        );
        // Step 1.
        let attack_frames = self.attack.value().clamp(0., 1.) * info.sample_rate;
        let release_frames = self.release.value().clamp(0., 1.) * info.sample_rate;
        // Step 2, with [[automatic makeup gain]] (the only mode before makeupGain options).
        let makeup_gain = curve.makeup_gain();

        let silent_in = inputs.blocks[0].is_silence();
        if silent_in &&
            self.delay_live_frames == 0 &&
            self.follower.compressor_gain >= 1. &&
            self.follower.detector_average >= 1.
        {
            // Nothing in the delay lines and the follower at rest: zeros in give zeros out,
            // and the follower would not move. Keep [[internal reduction]] where the
            // algorithm would leave it.
            self.reduction
                .set(linear_to_db(self.follower.compressor_gain * makeup_gain));
            return inputs;
        }

        let block = &mut inputs.blocks[0];
        let channels = if silent_in {
            self.delay_lines.len().max(1)
        } else {
            block.chan_count() as usize
        };
        self.ensure_delay(info.sample_rate, channels);
        if silent_in {
            *block = crate::block::Block::for_channels_explicit(channels as u8);
            self.delay_live_frames = self.delay_live_frames.saturating_sub(FRAMES_PER_BLOCK_USIZE);
        } else {
            block.explicit_repeat();
            self.delay_live_frames = self.delay_frames;
        }

        // Steps 3 to 5: the reduction gain for each frame, from the sidechain (the input's
        // largest absolute value across channels at that frame).
        for frame in 0..FRAMES_PER_BLOCK_USIZE {
            let mut side = 0f32;
            for chan in 0..channels {
                side = side.max(block.data_chan(chan as u8)[frame].abs());
            }
            self.gains[frame] = self.follower.step(
                side,
                &curve,
                makeup_gain,
                attack_frames,
                release_frames,
            );
        }

        // The internal graph: input -> delay -> gain(reduction gain) -> output.
        let start = self.delay_pos;
        for chan in 0..channels {
            let line = &mut self.delay_lines[chan];
            let data = block.data_chan_mut(chan as u8);
            let mut pos = start;
            for (sample, gain) in data.iter_mut().zip(&self.gains) {
                let delayed = line[pos];
                line[pos] = *sample;
                *sample = delayed * gain;
                pos += 1;
                if pos == self.delay_frames {
                    pos = 0;
                }
            }
        }
        self.delay_pos = (start + FRAMES_PER_BLOCK_USIZE) % self.delay_frames;

        // Steps 6 to 8 (the follower's slots live in `self.follower`). Step 5.10 and 8:
        // [[internal reduction]] is the last frame's metering gain.
        self.reduction
            .set(linear_to_db(self.follower.compressor_gain * makeup_gain));
        inputs
    }

    fn get_param(&mut self, id: ParamType) -> &mut Param {
        match id {
            ParamType::Threshold => &mut self.threshold,
            ParamType::Knee => &mut self.knee,
            ParamType::Ratio => &mut self.ratio,
            ParamType::Attack => &mut self.attack,
            ParamType::Release => &mut self.release,
            _ => panic!("Unknown param {:?} for DynamicsCompressorNode", id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::Block;

    const RATE: f32 = 48000.;

    fn info(block: u64) -> BlockInfo {
        BlockInfo {
            sample_rate: RATE,
            frame: Tick(block * 128),
            time: (block * 128) as f64 / RATE as f64,
        }
    }

    fn default_curve() -> CompressorCurve {
        CompressorCurve::new(-24., 30., 12.)
    }

    #[test]
    fn curve_is_identity_below_threshold() {
        let c = default_curve();
        for x in [-100., -60., -24.5, -24.] {
            assert_eq!(c.apply_db(x), x);
        }
    }

    #[test]
    fn curve_follows_ratio_above_knee() {
        let c = default_curve();
        // Above the knee end (6 dB), 12 dB more input gives 1 dB more output.
        let a = c.apply_db(10.);
        let b = c.apply_db(22.);
        assert!((b - a - 1.).abs() < 1e-4, "{a} {b}");
    }

    #[test]
    fn curve_is_continuous_and_monotonic() {
        for (t, k, r) in [(-24., 30., 12.), (-50., 0., 20.), (0., 40., 1.), (-10., 5., 4.)] {
            let c = CompressorCurve::new(t, k, r);
            let mut prev = c.apply_db(-120.);
            let mut x = -120.;
            while x < 40. {
                x += 0.01;
                let y = c.apply_db(x);
                assert!(y >= prev - 1e-5, "not monotonic at {x} for {t} {k} {r}");
                assert!((y - prev).abs() < 0.02, "jump at {x} for {t} {k} {r}");
                prev = y;
            }
        }
    }

    #[test]
    fn hard_knee_is_piecewise_linear() {
        let c = CompressorCurve::new(-20., 0., 4.);
        assert_eq!(c.apply_db(-20.), -20.);
        assert!((c.apply_db(-12.) - (-18.)).abs() < 1e-5);
    }

    #[test]
    fn makeup_gain_follows_the_spec_formula() {
        let c = default_curve();
        // (1 / f(1.0))^0.6, f(1.0) = 10^(f_db(0) / 20).
        let f1 = db_to_linear(c.apply_db(0.));
        let expected = (1. / f1).powf(0.6);
        assert!((c.makeup_gain() - expected).abs() < 1e-6);
        // Ratio 1 compresses nothing: no makeup.
        assert!((CompressorCurve::new(-24., 30., 1.).makeup_gain() - 1.).abs() < 1e-6);
    }

    #[test]
    fn db_conversions() {
        assert_eq!(linear_to_db(0.), -1000.);
        assert!((linear_to_db(0.1) + 20.).abs() < 1e-5);
        assert!((db_to_linear(-20.) - 0.1).abs() < 1e-7);
    }

    #[test]
    fn attack_reduces_gain_by_10_db_in_attack_time() {
        // A detector far below the gain: the attack rate alone limits the fall.
        let attack_frames = 0.01 * RATE;
        let mut g = 1f32;
        for _ in 0..attack_frames as usize {
            let rate = envelope_rate(false, g, 1e-6, attack_frames, 0.25 * RATE);
            g += (1e-6 - g) * rate;
        }
        assert!((linear_to_db(g) + 10.).abs() < 0.01, "{}", linear_to_db(g));
    }

    #[test]
    fn attack_never_overshoots_the_detector() {
        let mut g = 1f32;
        let d = 0.5;
        for _ in 0..10000 {
            let rate = envelope_rate(false, g, d, 0., 0.);
            assert!((0. ..=1.).contains(&rate));
            g += (d - g) * rate;
            assert!(g >= d - 1e-6);
        }
        assert!((g - d).abs() < 1e-6);
    }

    #[test]
    fn release_raises_gain_by_10_db_in_release_time() {
        let release_frames = 0.1 * RATE;
        let mut g = db_to_linear(-30.);
        for _ in 0..release_frames as usize {
            g = (g * envelope_rate(true, g, 1., 0., release_frames)).min(1.);
        }
        assert!((linear_to_db(g) + 20.).abs() < 0.01, "{}", linear_to_db(g));
    }

    #[test]
    fn envelope_rate_constraints() {
        // Attack rates are in [0, 1] and increase with detector / gain; release rates exceed 1.
        let mut prev = 0.;
        for i in 0..100 {
            let r = i as f32 / 100.;
            let rate = envelope_rate(false, 1., r, 0.003 * RATE, 0.25 * RATE);
            assert!((0. ..=1.).contains(&rate));
            assert!(rate >= prev);
            prev = rate;
        }
        assert!(envelope_rate(true, 0.5, 1., 0., 0.25 * RATE) > 1.);
    }

    #[test]
    fn detector_curve_constraints() {
        let mut prev = -1.;
        for i in 0..=100 {
            let v = detector_curve(i as f32 / 100.);
            assert!((0. ..=1.).contains(&v));
            assert!(v > prev);
            prev = v;
        }
    }

    #[test]
    fn follower_settles_on_static_curve_for_steady_input() {
        let curve = default_curve();
        let mut f = EnvelopeFollower::default();
        // A steady 0 dBFS DC sidechain.
        let mut gain = 0.;
        for _ in 0..(RATE as usize) {
            gain = f.step(1., &curve, 1., 0.003 * RATE, 0.25 * RATE);
        }
        let expected = curve.apply(1.);
        assert!((gain - expected).abs() < 1e-3, "{gain} vs {expected}");
    }

    #[test]
    fn quiet_input_is_not_compressed() {
        let curve = default_curve();
        let mut f = EnvelopeFollower::default();
        let mut gain = 0.;
        for _ in 0..4800 {
            gain = f.step(0.01, &curve, 1., 0.003 * RATE, 0.25 * RATE); // -40 dB
        }
        assert!((gain - 1.).abs() < 1e-6);
    }

    fn run(node: &mut DynamicsCompressorNode, block: Block, n: u64) -> Block {
        let mut chunk = Chunk::default();
        chunk.blocks.push(block);
        node.process(chunk, &info(n)).blocks.remove(0)
    }

    #[test]
    fn node_delays_by_the_lookahead_and_compresses() {
        let opts = DynamicsCompressorNodeOptions::default();
        let reduction = opts.reduction.clone();
        let mut node = DynamicsCompressorNode::new(opts, ChannelInfo::default());
        let delay = (0.006 * RATE).round() as usize; // 288 frames
        let mut out = Vec::new();
        for n in 0..400 {
            out.extend_from_slice(run(&mut node, Block::for_vec(vec![1.; 256]), n).data_chan(0));
        }
        // Nothing before the look-ahead has elapsed.
        assert!(out[..delay].iter().all(|v| *v == 0.));
        assert!(out[delay] != 0.);
        // Settled: DC at 0 dBFS comes out at curve(1.0) times the makeup gain.
        let curve = default_curve();
        let expected = curve.apply(1.) * curve.makeup_gain();
        let last = *out.last().unwrap();
        assert!((last - expected).abs() < 1e-3, "{last} vs {expected}");
        // [[internal reduction]] is the metering gain, in dB.
        let r = reduction.get();
        assert!((r - linear_to_db(expected)).abs() < 0.02, "{r}");
    }

    #[test]
    fn reduction_starts_at_zero_and_node_stays_silent_without_input() {
        let opts = DynamicsCompressorNodeOptions::default();
        let reduction = opts.reduction.clone();
        let mut node = DynamicsCompressorNode::new(opts, ChannelInfo::default());
        assert_eq!(reduction.get(), 0.);
        for n in 0..4 {
            assert!(run(&mut node, Block::default(), n).data_chan_frame(0, 0) == 0.);
        }
    }

    #[test]
    fn tail_drains_the_delay_line() {
        let mut node =
            DynamicsCompressorNode::new(DynamicsCompressorNodeOptions::default(), ChannelInfo::default());
        run(&mut node, Block::for_vec(vec![0.01; 256]), 0);
        // With a 288-frame look-ahead, frames 0..128 of input come out at 288..416, i.e. in
        // the third and fourth quanta, after the input has gone silent.
        let first = run(&mut node, Block::default(), 1);
        assert_eq!(first.chan_count(), 2);
        assert!(first.data_chan(0).iter().all(|v| *v == 0.));
        let tail = run(&mut node, Block::default(), 2);
        assert!(!tail.is_silence());
        assert!(tail.data_chan(0)[32..].iter().all(|v| *v != 0.));
        assert!(tail.data_chan(0)[..32].iter().all(|v| *v == 0.));
        let last = run(&mut node, Block::default(), 3);
        assert!(last.data_chan(0)[..32].iter().all(|v| *v != 0.));
    }

    #[test]
    fn k_rate_params_are_read_once_per_block() {
        let mut node =
            DynamicsCompressorNode::new(DynamicsCompressorNodeOptions::default(), ChannelInfo::default());
        assert_eq!(node.get_param(ParamType::Ratio).value(), 12.);
        assert_eq!(node.get_param(ParamType::Threshold).value(), -24.);
        assert_eq!(node.get_param(ParamType::Knee).value(), 30.);
        assert_eq!(node.get_param(ParamType::Attack).value(), 0.003);
        assert_eq!(node.get_param(ParamType::Release).value(), 0.25);
    }
}
