/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! <https://webaudio.github.io/web-audio-api/#ConvolverNode>
//!
//! Uniformly partitioned overlap-save convolution: the impulse response is cut into
//! render-quantum-sized partitions, each transformed once (256-point real FFT); every quantum the
//! input's spectrum enters a frequency-domain delay line, and the output is the inverse
//! transform of the sum of delayed input spectra times partition spectra. No added latency.

use std::sync::Arc;

use malloc_size_of_derive::MallocSizeOf;
use num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use crate::audio_node::{
    AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo, ChannelInterpretation,
};
use crate::block::{Block, Chunk, FRAMES_PER_BLOCK_USIZE, Tick};

const B: usize = FRAMES_PER_BLOCK_USIZE;
const FFT: usize = 2 * B;
const BINS: usize = FFT / 2 + 1;

/// An impulse response as set on the `buffer` attribute: 1, 2 or 4 channels (the DOM checks),
/// with the `normalize` value at the time it was set.
#[derive(Clone, Debug, MallocSizeOf)]
pub struct ConvolverBuffer {
    pub channels: Vec<Vec<f32>>,
    pub sample_rate: f32,
    pub normalize: bool,
}

#[derive(Clone, Debug, Default, MallocSizeOf)]
pub struct ConvolverNodeOptions {
    pub buffer: Option<ConvolverBuffer>,
}

#[derive(Clone, Debug, MallocSizeOf)]
pub enum ConvolverNodeMessage {
    SetBuffer(Option<ConvolverBuffer>),
}

/// <https://webaudio.github.io/web-audio-api/#dom-convolvernode-normalize>:
/// `calculateNormalizationScale`, line by line.
pub(crate) fn normalization_scale(buffer: &ConvolverBuffer) -> f32 {
    const GAIN_CALIBRATION: f64 = 0.00125;
    const GAIN_CALIBRATION_SAMPLE_RATE: f64 = 44100.;
    const MIN_POWER: f64 = 0.000125;
    let number_of_channels = buffer.channels.len();
    let length = buffer.channels.first().map_or(0, |c| c.len());
    let mut power = 0f64;
    for channel in &buffer.channels {
        let mut channel_power = 0f64;
        for sample in channel {
            channel_power += (*sample as f64) * (*sample as f64);
        }
        power += channel_power;
    }
    power = (power / (number_of_channels * length) as f64).sqrt();
    // Protect against accidental overload.
    if !power.is_finite() || power < MIN_POWER {
        power = MIN_POWER;
    }
    let mut scale = 1. / power;
    // Calibrate to make perceived volume same as unprocessed.
    scale *= GAIN_CALIBRATION;
    // Scale depends on sample-rate.
    if buffer.sample_rate != 0. {
        scale *= GAIN_CALIBRATION_SAMPLE_RATE / buffer.sample_rate as f64;
    }
    // True-stereo compensation.
    if number_of_channels == 4 {
        scale *= 0.5;
    }
    scale as f32
}

struct Fft {
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    time: Vec<f32>,
    scratch_fwd: Vec<Complex<f32>>,
    scratch_inv: Vec<Complex<f32>>,
}

impl Fft {
    fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(FFT);
        let inverse = planner.plan_fft_inverse(FFT);
        Fft {
            scratch_fwd: forward.make_scratch_vec(),
            scratch_inv: inverse.make_scratch_vec(),
            forward,
            inverse,
            time: vec![0.; FFT],
        }
    }

    /// Spectrum of `first` followed by `second` (each `B` long; missing parts are zeros).
    fn forward(&mut self, first: Option<&[f32]>, second: &[f32], out: &mut [Complex<f32>]) {
        match first {
            Some(f) => self.time[..B].copy_from_slice(f),
            None => self.time[..B].fill(0.),
        }
        self.time[B..B + second.len()].copy_from_slice(second);
        self.time[B + second.len()..].fill(0.);
        self.forward
            .process_with_scratch(&mut self.time, out, &mut self.scratch_fwd)
            .expect("FFT sizes are fixed");
    }

    /// Inverse transform of `spectrum` (destroyed); returns the last `B` samples (overlap-save).
    fn inverse_tail(&mut self, spectrum: &mut [Complex<f32>], out: &mut [f32]) {
        // A real signal's spectrum has real DC and Nyquist bins; rounding can leave a tiny
        // imaginary part, which realfft rejects.
        spectrum[0].im = 0.;
        spectrum[BINS - 1].im = 0.;
        self.inverse
            .process_with_scratch(spectrum, &mut self.time, &mut self.scratch_inv)
            .expect("FFT sizes are fixed");
        out.copy_from_slice(&self.time[B..]);
    }
}

/// One impulse response channel, as partition spectra (already scaled by the normalization and
/// by 1 / FFT for the unnormalised inverse transform).
struct Partitions(Vec<Vec<Complex<f32>>>);

/// The input's recent spectra, newest first, as many as there are partitions.
#[derive(Clone)]
struct DelayLine {
    spectra: Vec<Vec<Complex<f32>>>,
    newest: usize,
    previous_block: Vec<f32>,
}

impl DelayLine {
    fn new(partitions: usize) -> Self {
        DelayLine {
            spectra: vec![vec![Complex::default(); BINS]; partitions.max(1)],
            newest: 0,
            previous_block: vec![0.; B],
        }
    }

    fn push(&mut self, fft: &mut Fft, block: &[f32]) {
        self.newest = (self.newest + self.spectra.len() - 1) % self.spectra.len();
        let slot = &mut self.spectra[self.newest];
        fft.forward(Some(&self.previous_block), block, slot);
        self.previous_block.copy_from_slice(block);
    }

    /// The spectrum `age` quanta old.
    fn get(&self, age: usize) -> &[Complex<f32>] {
        &self.spectra[(self.newest + age) % self.spectra.len()]
    }
}

/// Which input channel convolves with which response channel into which output channel.
/// <https://webaudio.github.io/web-audio-api/#Convolution-channel-configurations>
fn paths(input_channels: usize, response_channels: usize) -> &'static [(usize, usize, usize)] {
    match (input_channels, response_channels) {
        (1, 1) => &[(0, 0, 0)],
        (1, 2) => &[(0, 0, 0), (0, 1, 1)],
        (2, 1) => &[(0, 0, 0), (1, 0, 1)],
        (2, 2) => &[(0, 0, 0), (1, 1, 1)],
        // True stereo: L = in_L * IR0 + in_R * IR2, R = in_L * IR1 + in_R * IR3; a mono input
        // is up-mixed to stereo first, i.e. feeds both.
        (1, 4) => &[(0, 0, 0), (0, 2, 0), (0, 1, 1), (0, 3, 1)],
        (2, 4) => &[(0, 0, 0), (1, 2, 0), (0, 1, 1), (1, 3, 1)],
        _ => &[],
    }
}

#[derive(AudioNodeCommon, MallocSizeOf)]
pub(crate) struct ConvolverNode {
    channel_info: ChannelInfo,
    #[ignore_malloc_size_of = "FFT plans and spectra"]
    state: Option<State>,
}

struct State {
    fft: Fft,
    responses: Vec<Partitions>,
    partitions: usize,
    /// Per input channel.
    lines: Vec<DelayLine>,
    /// Quanta of output left after the input went silent.
    tail_blocks: usize,
    /// Quanta of stereo output left after the input stopped being stereo.
    stereo_tail_blocks: usize,
    accum: Vec<Vec<Complex<f32>>>,
}

impl State {
    fn new(buffer: ConvolverBuffer) -> Option<Self> {
        let length = buffer.channels.first().map_or(0, |c| c.len());
        if buffer.channels.is_empty() || length == 0 {
            return None;
        }
        let scale = if buffer.normalize {
            normalization_scale(&buffer)
        } else {
            1.
        } / FFT as f32;
        let partitions = length.div_ceil(B);
        let mut fft = Fft::new();
        let responses = buffer
            .channels
            .iter()
            .map(|channel| {
                Partitions(
                    channel
                        .chunks(B)
                        .map(|part| {
                            let scaled: Vec<f32> = part.iter().map(|v| v * scale).collect();
                            let mut spectrum = vec![Complex::default(); BINS];
                            // The partition goes first, zero-padded to the FFT size.
                            fft.time[..scaled.len()].copy_from_slice(&scaled);
                            fft.time[scaled.len()..].fill(0.);
                            fft.forward
                                .process_with_scratch(
                                    &mut fft.time,
                                    &mut spectrum,
                                    &mut fft.scratch_fwd,
                                )
                                .expect("FFT sizes are fixed");
                            spectrum
                        })
                        .collect(),
                )
            })
            .collect();
        Some(State {
            fft,
            responses,
            partitions,
            lines: Vec::new(),
            tail_blocks: 0,
            stereo_tail_blocks: 0,
            accum: vec![vec![Complex::default(); BINS]; 2],
        })
    }
}

impl ConvolverNode {
    pub fn new(options: ConvolverNodeOptions, channel_info: ChannelInfo) -> Self {
        Self {
            channel_info,
            state: options.buffer.and_then(State::new),
        }
    }

    fn handle_message(&mut self, message: ConvolverNodeMessage, _sample_rate: f32) {
        match message {
            ConvolverNodeMessage::SetBuffer(buffer) => self.state = buffer.and_then(State::new),
        }
    }
}

impl AudioNodeEngine for ConvolverNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::ConvolverNode
    }

    /// Tail: the impulse response's length after the last input.
    fn removable(&self, _now: Tick) -> bool {
        self.state.as_ref().is_none_or(|state| state.tail_blocks == 0)
    }

    fn process(&mut self, mut inputs: Chunk, _info: &BlockInfo) -> Chunk {
        debug_assert!(inputs.len() == 1);
        // > If this node is not actively processing, the output is a single channel of silence.
        let Some(state) = self.state.as_mut() else {
            inputs.blocks[0] = Block::default();
            return inputs;
        };
        let silent_in = inputs.blocks[0].is_silence();
        // Still sounding from earlier input (its tail is not over).
        let sounding = state.tail_blocks > 0;
        if silent_in {
            if state.tail_blocks == 0 {
                inputs.blocks[0] = Block::default();
                return inputs;
            }
            state.tail_blocks -= 1;
        } else {
            state.tail_blocks = state.partitions;
        }

        // The input is mono or stereo (channelCount <= 2, clamped-max or explicit).
        let block_channels = if silent_in {
            0
        } else {
            (inputs.blocks[0].chan_count() as usize).clamp(1, 2)
        };
        // Processing stays stereo while a stereo input's tail is still sounding: a mono
        // input is up-mixed into it rather than cutting off the second channel's tail. Once
        // that tail is over the output is mono again.
        let in_channels = if block_channels == 2 {
            state.stereo_tail_blocks = state.partitions;
            2
        } else if state.lines.len() == 2 && state.stereo_tail_blocks > 0 {
            state.stereo_tail_blocks -= 1;
            2
        } else {
            1
        };
        let speakers = self.channel_info.interpretation == ChannelInterpretation::Speakers;
        if state.lines.len() < in_channels {
            // Mono becoming stereo: the mono history up-mixes too (speakers: into both
            // channels; discrete: into the first only).
            let partitions = state.partitions;
            let extra = match state.lines.first() {
                Some(line) if speakers && sounding => line.clone(),
                _ => DelayLine::new(partitions),
            };
            state.lines.resize(in_channels, extra);
        } else if state.lines.len() > in_channels {
            state.lines.truncate(in_channels);
        }
        let block = &mut inputs.blocks[0];
        if !silent_in {
            block.explicit_repeat();
        }
        let zeros = [0f32; B];
        for (chan, line) in state.lines.iter_mut().enumerate() {
            let data = if silent_in {
                &zeros[..]
            } else if chan < block_channels {
                block.data_chan(chan as u8)
            } else if speakers {
                block.data_chan(0)
            } else {
                &zeros[..]
            };
            line.push(&mut state.fft, data);
        }

        let routes = paths(in_channels, state.responses.len());
        let out_channels = if in_channels == 1 && state.responses.len() == 1 {
            1
        } else {
            2
        };
        for acc in state.accum.iter_mut() {
            acc.fill(Complex::default());
        }
        for &(input, response, output) in routes {
            let line = &state.lines[input];
            let acc = &mut state.accum[output];
            for (age, h) in state.responses[response].0.iter().enumerate() {
                let x = line.get(age);
                for ((a, x), h) in acc.iter_mut().zip(x).zip(h) {
                    *a += x * h;
                }
            }
        }
        let mut out = Block::for_channels_explicit(out_channels as u8);
        for chan in 0..out_channels {
            let mut spectrum = std::mem::take(&mut state.accum[chan]);
            state
                .fft
                .inverse_tail(&mut spectrum, out.data_chan_mut(chan as u8));
            spectrum.fill(Complex::default());
            state.accum[chan] = spectrum;
        }
        inputs.blocks[0] = out;
        inputs
    }

    make_message_handler!(ConvolverNode: handle_message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::Tick;

    fn info() -> BlockInfo {
        BlockInfo {
            sample_rate: 48000.,
            frame: Tick(0),
            time: 0.,
        }
    }

    fn node(channels: Vec<Vec<f32>>, normalize: bool) -> ConvolverNode {
        ConvolverNode::new(
            ConvolverNodeOptions {
                buffer: Some(ConvolverBuffer {
                    channels,
                    sample_rate: 48000.,
                    normalize,
                }),
            },
            ChannelInfo::default(),
        )
    }

    fn run(node: &mut ConvolverNode, input: Block) -> Block {
        let mut c = Chunk::default();
        c.blocks.push(input);
        node.process(c, &info()).blocks.remove(0)
    }

    /// Direct-form convolution, for reference.
    fn convolve(x: &[f32], h: &[f32], n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                (0..h.len())
                    .filter(|k| *k <= i && i - k < x.len())
                    .map(|k| x[i - k] * h[k])
                    .sum()
            })
            .collect()
    }

    fn render_mono(node: &mut ConvolverNode, x: &[f32], blocks: usize) -> Vec<Vec<f32>> {
        let mut out: Vec<Vec<f32>> = vec![Vec::new(), Vec::new()];
        for b in 0..blocks {
            let input = if b * B < x.len() {
                let mut v = vec![0.; B];
                let end = ((b + 1) * B).min(x.len());
                v[..end - b * B].copy_from_slice(&x[b * B..end]);
                Block::for_vec(v)
            } else {
                Block::default()
            };
            let o = run(node, input);
            for c in 0..2 {
                if o.is_silence() || c >= o.chan_count() as usize {
                    out[c].extend(std::iter::repeat_n(0., B));
                } else {
                    out[c].extend_from_slice(o.data_chan(c as u8));
                }
            }
        }
        out
    }

    #[test]
    fn matches_direct_convolution_across_partitions() {
        let h: Vec<f32> = (0..300).map(|i| ((i * 7919) % 13) as f32 / 13. - 0.5).collect();
        let x: Vec<f32> = (0..500).map(|i| ((i * 104729) % 17) as f32 / 17. - 0.5).collect();
        let mut n = node(vec![h.clone()], false);
        let out = render_mono(&mut n, &x, 8);
        let expected = convolve(&x, &h, 8 * B);
        for (i, (a, e)) in out[0].iter().zip(&expected).enumerate() {
            assert!((a - e).abs() < 1e-4, "frame {i}: {a} vs {e}");
        }
    }

    #[test]
    fn impulse_response_is_reproduced() {
        let mut h = vec![0.; 8];
        h[3] = 1.;
        let mut n = node(vec![h], false);
        let mut x = vec![0.; B];
        x[10] = 1.;
        let out = render_mono(&mut n, &x, 2);
        assert!((out[0][13] - 1.).abs() < 3. * 2f32.powi(-22));
        let others: f32 = out[0]
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 13)
            .map(|(_, v)| v.abs())
            .fold(0., f32::max);
        assert!(others < 3. * 2f32.powi(-22), "{others}");
    }

    #[test]
    fn mono_in_mono_response_is_mono_else_stereo() {
        let mut mono = node(vec![vec![1.]], false);
        assert_eq!(run(&mut mono, Block::for_vec(vec![0.5; B])).chan_count(), 1);
        let mut stereo_ir = node(vec![vec![1.], vec![0.5]], false);
        let o = run(&mut stereo_ir, Block::for_vec(vec![0.5; B]));
        assert_eq!(o.chan_count(), 2);
        assert!((o.data_chan(0)[0] - 0.5).abs() < 1e-6);
        assert!((o.data_chan(1)[0] - 0.25).abs() < 1e-6);
    }

    /// Channel 1 of `blocks` processed in order, frames concatenated.
    fn right_channel(node: &mut ConvolverNode, blocks: Vec<Block>) -> Vec<f32> {
        let mut out = vec![];
        for input in blocks {
            let o = run(node, input);
            if o.is_silence() || o.chan_count() < 2 {
                out.extend(std::iter::repeat_n(0., B));
            } else {
                out.extend_from_slice(o.data_chan(1));
            }
        }
        out
    }

    fn stereo(l: f32, r: f32) -> Block {
        let mut b = Block::for_channels_explicit(2);
        b.data_chan_mut(0)[0] = l;
        b.data_chan_mut(1)[0] = r;
        b
    }

    #[test]
    fn stereo_tail_survives_mono_input_and_mono_tail_upmixes() {
        // convolver-upmixing-1-channel-response.html: impulses at 0 and 2 * B - 1.
        let mut h = vec![0.; 2 * B];
        h[0] = 0.5;
        h[2 * B - 1] = 0.5;
        // Stereo, then mono (not silent): the right channel's tail still plays.
        let mut n = node(vec![h.clone()], false);
        let mono_zeros = || Block::for_vec(vec![0.; B]);
        let out = right_channel(&mut n, vec![stereo(1., 1.), mono_zeros(), mono_zeros()]);
        assert!((out[2 * B - 1] - 0.5).abs() < 1e-5, "{}", out[2 * B - 1]);
        // Once that tail is over, mono input gives mono output again.
        assert_eq!(run(&mut n, Block::for_vec(vec![0.5; B])).chan_count(), 1);
        // Mono, then stereo: the mono tail is up-mixed into the right channel too.
        let mut n = node(vec![h], false);
        let mut first = vec![0.; B];
        first[0] = 1.;
        let out = right_channel(&mut n, vec![Block::for_vec(first), stereo(0., 0.), stereo(0., 0.)]);
        assert!((out[2 * B - 1] - 0.5).abs() < 1e-5, "{}", out[2 * B - 1]);
    }

    #[test]
    fn true_stereo_matrix() {
        // IR k is a unit impulse at k + 1 frames, as in convolver-response-4-chan.html.
        let irs: Vec<Vec<f32>> = (0..4)
            .map(|k| {
                let mut v = vec![0.; 8];
                v[k + 1] = 1.;
                v
            })
            .collect();
        let mut n = node(irs, false);
        let mut input = vec![0.; 2 * B];
        input[0] = 1.; // left impulse
        input[B + 50] = 2.; // right impulse
        let o = run(&mut n, Block::for_vec(input));
        let l = o.data_chan(0);
        let r = o.data_chan(1);
        // L = in_L * IR0 + in_R * IR2; R = in_L * IR1 + in_R * IR3.
        assert!((l[1] - 1.).abs() < 1e-6);
        assert!((l[50 + 3] - 2.).abs() < 1e-6);
        assert!((r[2] - 1.).abs() < 1e-6);
        assert!((r[50 + 4] - 2.).abs() < 1e-6);
    }

    #[test]
    fn tail_lasts_the_response_length_then_silence() {
        let mut h = vec![0.; 3 * B];
        h[3 * B - 1] = 1.;
        let mut n = node(vec![h], false);
        let mut x = vec![0.; B];
        x[0] = 1.;
        run(&mut n, Block::for_vec(x));
        let mut nonsilent = 0;
        for _ in 0..6 {
            let o = run(&mut n, Block::default());
            if !o.is_silence() {
                nonsilent += 1;
            }
        }
        assert_eq!(nonsilent, 3);
    }

    #[test]
    fn null_buffer_outputs_one_silent_channel() {
        let mut n = ConvolverNode::new(ConvolverNodeOptions::default(), ChannelInfo::default());
        let o = run(&mut n, Block::for_vec(vec![1.; 2 * B]));
        assert!(o.is_silence());
        assert_eq!(o.chan_count(), 1);
    }

    #[test]
    fn normalization_scale_follows_the_spec() {
        // A unit impulse in a 1-frame mono buffer at 44.1 kHz: power 1, scale 0.00125.
        let b = ConvolverBuffer {
            channels: vec![vec![1.]],
            sample_rate: 44100.,
            normalize: true,
        };
        assert!((normalization_scale(&b) - 0.00125).abs() < 1e-9);
        // Silence: power clamps to MinPower.
        let b = ConvolverBuffer {
            channels: vec![vec![0.; 4]; 4],
            sample_rate: 88200.,
            normalize: true,
        };
        let expected = (1. / 0.000125) * 0.00125 * 0.5 * 0.5;
        assert!((normalization_scale(&b) - expected as f32).abs() < 1e-4);
    }

    #[test]
    fn set_buffer_message_replaces_the_response() {
        let mut n = node(vec![vec![1.]], false);
        n.handle_message(
            ConvolverNodeMessage::SetBuffer(Some(ConvolverBuffer {
                channels: vec![vec![0.5]],
                sample_rate: 48000.,
                normalize: false,
            })),
            48000.,
        );
        let o = run(&mut n, Block::for_vec(vec![1.; B]));
        assert!((o.data_chan(0)[5] - 0.5).abs() < 1e-6);
        n.handle_message(ConvolverNodeMessage::SetBuffer(None), 48000.);
        assert!(run(&mut n, Block::for_vec(vec![1.; B])).is_silence());
    }
}
