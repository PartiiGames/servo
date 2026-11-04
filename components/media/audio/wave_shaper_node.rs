/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! <https://webaudio.github.io/web-audio-api/#WaveShaperNode>

use malloc_size_of_derive::MallocSizeOf;

use crate::audio_node::{AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo};
use crate::block::{Chunk, FRAMES_PER_BLOCK_USIZE, Tick};
use crate::oversampler::Oversampler;

#[derive(Clone, Copy, Debug, PartialEq, Eq, MallocSizeOf)]
pub enum OverSampleType {
    None,
    Double,
    Quadruple,
}

impl OverSampleType {
    fn factor(&self) -> usize {
        match self {
            OverSampleType::None => 1,
            OverSampleType::Double => 2,
            OverSampleType::Quadruple => 4,
        }
    }
}

type WaveShaperCurve = Option<Vec<f32>>;

#[derive(Clone, Debug, MallocSizeOf)]
pub struct WaveShaperNodeOptions {
    pub curve: WaveShaperCurve,
    pub oversample: OverSampleType,
}

impl Default for WaveShaperNodeOptions {
    fn default() -> Self {
        WaveShaperNodeOptions {
            curve: None,
            oversample: OverSampleType::None,
        }
    }
}

#[derive(Clone, Debug, MallocSizeOf)]
pub enum WaveShaperNodeMessage {
    /// The DOM side enforces `[[curve set]]` and the minimum length; the render thread just
    /// takes the new internal copy (or `None` for the identity).
    SetCurve(WaveShaperCurve),
    SetOversample(OverSampleType),
}

/// Blocks to keep processing after the input goes silent, so the oversampling filters drain.
/// Their latency is about 24 frames, well within one block; two blocks is generous.
const OVERSAMPLE_TAIL_BLOCKS: u8 = 2;

#[derive(AudioNodeCommon, MallocSizeOf)]
pub(crate) struct WaveShaperNode {
    curve: WaveShaperCurve,
    oversample: OverSampleType,
    channel_info: ChannelInfo,
    /// One per channel; empty unless oversampling.
    #[ignore_malloc_size_of = "filter state"]
    oversamplers: Vec<Oversampler>,
    /// Oversampled scratch buffer.
    scratch: Vec<f32>,
    tail_blocks_left: u8,
}

impl WaveShaperNode {
    pub fn new(options: WaveShaperNodeOptions, channel_info: ChannelInfo) -> Self {
        if let Some(curve) = &options.curve {
            debug_assert!(curve.len() > 1, "the DOM rejects curves shorter than 2");
        }
        Self {
            curve: options.curve.filter(|c| c.len() > 1),
            oversample: options.oversample,
            channel_info,
            oversamplers: Vec::new(),
            scratch: Vec::new(),
            tail_blocks_left: 0,
        }
    }

    fn handle_waveshaper_message(&mut self, message: WaveShaperNodeMessage, _sample_rate: f32) {
        match message {
            WaveShaperNodeMessage::SetCurve(curve) => self.curve = curve.filter(|c| c.len() > 1),
            WaveShaperNodeMessage::SetOversample(oversample) => {
                if oversample != self.oversample {
                    self.oversample = oversample;
                    // New filters for the new factor; the old state belongs to another rate.
                    self.oversamplers.clear();
                }
            },
        }
    }
}

/// Apply the curve to one sample, as in
/// <https://webaudio.github.io/web-audio-api/#dom-waveshapernode-curve>:
///
/// > v = (N - 1) / 2 * (x + 1), k = floor(v), f = v - k
/// > y = c_0 if v < 0; c_(N-1) if v >= N - 1; (1 - f) c_k + f c_(k+1) otherwise.
///
/// The index is computed in f64 so long curves keep their resolution.
#[inline]
pub(crate) fn shape(curve: &[f32], x: f32) -> f32 {
    let n = curve.len();
    let v = (n - 1) as f64 / 2. * (x as f64 + 1.);
    if v < 0. {
        curve[0]
    } else if v >= (n - 1) as f64 {
        curve[n - 1]
    } else if v.is_nan() {
        // Not covered by the algorithm; a NaN input stays NaN.
        f32::NAN
    } else {
        let k = v.floor();
        let f = v - k;
        let k = k as usize;
        ((1. - f) * curve[k] as f64 + f * curve[k + 1] as f64) as f32
    }
}

fn apply_curve(buf: &mut [f32], curve: &[f32]) {
    buf.iter_mut().for_each(|s| *s = shape(curve, *s));
}

impl AudioNodeEngine for WaveShaperNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::WaveShaperNode
    }

    /// Tail: the oversampling filters. A curve with c(0) != 0 outputs DC even with no input
    /// (spec), so such a node is never removed while connected downstream.
    fn removable(&self, _now: Tick) -> bool {
        self.tail_blocks_left == 0 && self.curve.as_ref().is_none_or(|c| shape(c, 0.) == 0.)
    }

    fn process(&mut self, mut inputs: Chunk, _info: &BlockInfo) -> Chunk {
        debug_assert!(inputs.len() == 1);

        // > Initially the curve attribute is null, which means that the WaveShaperNode will
        // > pass its input to its output without modification.
        let Some(curve) = self.curve.as_ref() else {
            self.oversamplers.clear();
            self.tail_blocks_left = 0;
            return inputs;
        };

        let factor = self.oversample.factor();
        if inputs.blocks[0].is_silence() {
            // > The use of a curve that produces a non-zero output value for zero input value
            // > will cause this node to produce a DC signal even if there are no inputs
            // > connected to this node.
            let dc = shape(curve, 0.) != 0.;
            if !dc && self.tail_blocks_left == 0 {
                return inputs;
            }
            self.tail_blocks_left = self.tail_blocks_left.saturating_sub(1);
            inputs.blocks[0].explicit_silence();
        } else if factor > 1 {
            self.tail_blocks_left = OVERSAMPLE_TAIL_BLOCKS;
        }

        let block = &mut inputs.blocks[0];
        if factor == 1 {
            // Also covers a "repeat" block: the curve is applied once to the shared channel.
            apply_curve(block.data_mut(), curve);
            return inputs;
        }

        // Oversampled: each channel keeps its own filter state.
        block.explicit_repeat();
        let channels = block.chan_count() as usize;
        if self.oversamplers.len() != channels ||
            self.oversamplers.first().is_some_and(|o| o.factor() != factor)
        {
            self.oversamplers.resize_with(channels, || Oversampler::new(factor));
        }
        self.scratch.resize(FRAMES_PER_BLOCK_USIZE * factor, 0.);
        for chan in 0..channels {
            let data = block.data_chan_mut(chan as u8);
            let os = &mut self.oversamplers[chan];
            os.upsample(data, &mut self.scratch);
            apply_curve(&mut self.scratch, curve);
            os.downsample(&self.scratch, data);
        }
        inputs
    }

    make_message_handler!(WaveShaperNode: handle_waveshaper_message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{Block, Tick};

    fn info() -> BlockInfo {
        BlockInfo {
            sample_rate: 48000.,
            frame: Tick(0),
            time: 0.,
        }
    }

    fn run(node: &mut WaveShaperNode, block: Block) -> Block {
        let mut chunk = Chunk::default();
        chunk.blocks.push(block);
        let mut out = node.process(chunk, &info());
        out.blocks.remove(0)
    }

    fn node(curve: Option<Vec<f32>>, oversample: OverSampleType) -> WaveShaperNode {
        WaveShaperNode::new(
            WaveShaperNodeOptions { curve, oversample },
            ChannelInfo::default(),
        )
    }

    #[test]
    fn curve_maps_minus_one_zero_and_one_exactly() {
        let c = [-0.25, 0.5, 0.75];
        assert_eq!(shape(&c, -1.), -0.25);
        assert_eq!(shape(&c, 0.), 0.5);
        assert_eq!(shape(&c, 1.), 0.75);
    }

    #[test]
    fn curve_interpolates_linearly() {
        // N = 5: v = 2 (x + 1). x = 0.25 -> v = 2.5 -> halfway between c2 and c3.
        let c = [0., 1., 2., 4., 8.];
        assert_eq!(shape(&c, 0.25), 3.);
        // x = -0.875 -> v = 0.25 -> 0.75 c0 + 0.25 c1.
        assert_eq!(shape(&c, -0.875), 0.25);
    }

    #[test]
    fn even_length_curve_interpolates_between_centre_values_at_zero() {
        let c = [0., 1., 3., 4.];
        // v = 1.5 * (0 + 1) = 1.5 -> between c1 and c2.
        assert_eq!(shape(&c, 0.), 2.);
    }

    #[test]
    fn out_of_range_inputs_clamp_to_the_ends() {
        let c = [0., 1., 0.];
        assert_eq!(shape(&c, -1.1), 0.);
        assert_eq!(shape(&c, 7.), 0.);
        assert_eq!(shape(&c, f32::NEG_INFINITY), 0.);
        assert_eq!(shape(&c, f32::INFINITY), 0.);
    }

    #[test]
    fn matches_the_wpt_limits_ramp() {
        // waveshaper-limits.html: curve [0, 1, 0], 23 frames from -1 - scale to 1 + scale.
        let frames = 23;
        let scale = 1. / ((frames - 1) as f32 / 2. - 1.);
        let c = [0., 1., 0.];
        for k in 0..frames {
            let x = k as f32 * scale + (-1. - scale);
            let expected = if x <= -1. || x >= 1. {
                0.
            } else {
                1. - x.abs()
            };
            assert!((shape(&c, x) - expected).abs() <= 1e-6, "x = {x}");
        }
    }

    #[test]
    fn null_curve_passes_input_through() {
        let mut n = node(None, OverSampleType::Quadruple);
        let input = Block::for_vec((0..128).map(|i| i as f32 / 128.).collect());
        let out = run(&mut n, input.clone());
        assert_eq!(out.data_chan(0), input.data_chan(0));
    }

    #[test]
    fn silent_input_with_nonzero_curve_centre_gives_dc() {
        for os in [OverSampleType::None, OverSampleType::Double, OverSampleType::Quadruple] {
            let mut n = node(Some(vec![0.5, 0.5, 0.5]), os);
            let mut last = Block::default();
            for _ in 0..8 {
                last = run(&mut n, Block::default());
            }
            assert!(!last.is_silence(), "{os:?}");
            for v in last.data_chan(0) {
                assert!((v - 0.5).abs() < 1e-6, "{os:?}: {v}");
            }
        }
    }

    #[test]
    fn silent_input_with_zero_curve_centre_stays_silent() {
        let mut n = node(Some(vec![-1., 0., 1.]), OverSampleType::None);
        assert!(run(&mut n, Block::default()).is_silence());
    }

    #[test]
    fn oversampled_tail_then_silence() {
        let mut n = node(Some(vec![-1., 0., 1.]), OverSampleType::Double);
        let loud = Block::for_vec(vec![0.5; 256]); // two channels
        run(&mut n, loud);
        // The filters drain for a couple of blocks, then the node goes back to silence.
        assert!(!run(&mut n, Block::default()).is_silence());
        assert!(!run(&mut n, Block::default()).is_silence());
        assert!(run(&mut n, Block::default()).is_silence());
    }

    #[test]
    fn oversampling_follows_channel_count_changes() {
        let mut n = node(Some(vec![-1., 0., 1.]), OverSampleType::Quadruple);
        let out = run(&mut n, Block::for_vec(vec![0.25; 128]));
        assert_eq!(out.chan_count(), 1);
        let out = run(&mut n, Block::for_vec(vec![0.25; 128 * 2]));
        assert_eq!(out.chan_count(), 2);
        let out = run(&mut n, Block::for_vec(vec![0.25; 128 * 6]));
        assert_eq!(out.chan_count(), 6);
    }

    #[test]
    fn identity_curve_oversampled_is_a_delayed_copy() {
        for os in [OverSampleType::Double, OverSampleType::Quadruple] {
            let mut n = node(Some(vec![-1., 0., 1.]), os);
            let mut out = Vec::new();
            for _ in 0..8 {
                out.extend_from_slice(run(&mut n, Block::for_vec(vec![0.3; 128])).data_chan(0));
            }
            for v in &out[256..] {
                assert!((v - 0.3).abs() < 1e-5, "{os:?}: {v}");
            }
        }
    }

    #[test]
    fn set_oversample_message_switches_factor() {
        let mut n = node(Some(vec![-1., 0., 1.]), OverSampleType::None);
        run(&mut n, Block::for_vec(vec![0.3; 128]));
        n.handle_waveshaper_message(WaveShaperNodeMessage::SetOversample(OverSampleType::Double), 48000.);
        assert_eq!(n.oversample, OverSampleType::Double);
        run(&mut n, Block::for_vec(vec![0.3; 128]));
        assert_eq!(n.oversamplers.len(), 1);
        assert_eq!(n.oversamplers[0].factor(), 2);
        n.handle_waveshaper_message(
            WaveShaperNodeMessage::SetOversample(OverSampleType::Quadruple),
            48000.,
        );
        run(&mut n, Block::for_vec(vec![0.3; 128]));
        assert_eq!(n.oversamplers[0].factor(), 4);
    }
}
