/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! <https://webaudio.github.io/web-audio-api/#DelayNode>
//!
//! A per-channel delay line with an a-rate `delayTime` and linear interpolation between
//! frames. When the node is part of a cycle, the graph splits it into a DelayReader and a
//! DelayWriter (<https://webaudio.github.io/web-audio-api/#rendering-loop>, steps 4.2.4 to
//! 4.2.6): [`DelayNode::process_reader`] produces the output before the cycle's other nodes
//! run, and [`DelayNode::process_writer`] stores the input after them. The delay time is then
//! clamped to at least one render quantum, so the reader only needs frames written in earlier
//! quanta.

use malloc_size_of_derive::MallocSizeOf;

use crate::audio_node::{AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo};
use crate::block::{Block, Chunk, FRAMES_PER_BLOCK_USIZE, Tick};
use crate::param::{Param, ParamType};

#[derive(Copy, Clone, Debug, MallocSizeOf)]
pub struct DelayNodeOptions {
    /// Seconds; the DOM checks 0 < max_delay_time < 180.
    pub max_delay_time: f64,
    /// Initial delayTime, in seconds.
    pub delay_time: f32,
}

impl Default for DelayNodeOptions {
    fn default() -> Self {
        DelayNodeOptions {
            max_delay_time: 1.,
            delay_time: 0.,
        }
    }
}

#[derive(AudioNodeCommon)]
pub(crate) struct DelayNode {
    channel_info: ChannelInfo,
    delay_time: Param,
    max_delay_time: f64,
    /// Delay lines, one per channel, all `len` frames long (a multiple of the block size).
    lines: Vec<Vec<f32>>,
    len: usize,
    /// Where the next input frame is written.
    write: usize,
    /// Input frames that can still reach the output: reset to the line length on any
    /// non-silent input, counted down on silent input.
    live_frames: usize,
    /// Per-frame delay in frames for the current quantum.
    delays: Vec<f64>,
    /// For each quantum-sized slot of the lines, the channel count of the input written there
    /// (0: never written, or silence). The output of a quantum has the channel count of the
    /// input it delays: mono silence where the history holds no input.
    slot_channels: Vec<u8>,
}

impl DelayNode {
    pub fn new(options: DelayNodeOptions, channel_info: ChannelInfo) -> Self {
        Self {
            channel_info,
            delay_time: Param::new(options.delay_time),
            max_delay_time: options.max_delay_time,
            lines: Vec::new(),
            len: 0,
            write: 0,
            live_frames: 0,
            delays: vec![0.; FRAMES_PER_BLOCK_USIZE],
            slot_channels: Vec::new(),
        }
    }

    fn ensure_lines(&mut self, sample_rate: f32) {
        if self.len == 0 {
            // Room for the longest delay, one quantum being written and one interpolation
            // neighbour, rounded up to whole quanta (so channel changes can remix block-wise).
            let frames = (self.max_delay_time * sample_rate as f64).ceil() as usize +
                FRAMES_PER_BLOCK_USIZE +
                2;
            self.len = frames.div_ceil(FRAMES_PER_BLOCK_USIZE) * FRAMES_PER_BLOCK_USIZE;
            self.slot_channels = vec![0; self.len / FRAMES_PER_BLOCK_USIZE];
        }
    }

    /// > When the number of channels in a DelayNode's input changes (thus changing the output
    /// > channel count also), there may be delayed audio samples which have not yet been output
    /// > by the node and are part of its internal state. If these samples were received earlier
    /// > with a different channel count, they MUST be upmixed or downmixed before being combined
    /// > with newly received input so that all internal delay-line mixing takes place using the
    /// > single prevailing channel layout.
    fn set_channels(&mut self, channels: usize) {
        if self.lines.len() == channels {
            return;
        }
        if self.lines.is_empty() || self.live_frames == 0 {
            self.lines = vec![vec![0.; self.len]; channels];
            self.slot_channels.fill(0);
            return;
        }
        let interpretation = self.channel_info.interpretation;
        let mut new_lines = vec![Vec::with_capacity(self.len); channels];
        for start in (0..self.len).step_by(FRAMES_PER_BLOCK_USIZE) {
            let mut block = Block::empty();
            for line in &self.lines {
                block.push_chan(&line[start..start + FRAMES_PER_BLOCK_USIZE]);
            }
            block.mix(channels as u8, interpretation);
            for (chan, line) in new_lines.iter_mut().enumerate() {
                if block.is_silence() {
                    line.extend(std::iter::repeat_n(0., FRAMES_PER_BLOCK_USIZE));
                } else {
                    line.extend_from_slice(block.data_chan(chan as u8));
                }
            }
        }
        self.lines = new_lines;
    }

    /// The delay, in frames, for each frame of this quantum.
    fn compute_delays(&mut self, info: &BlockInfo, in_cycle: bool) {
        let rate = info.sample_rate as f64;
        let max = self.max_delay_time * rate;
        // > If DelayNode is part of a cycle, then the value of the delayTime attribute is
        // > clamped to a minimum of one render quantum.
        let min = if in_cycle {
            FRAMES_PER_BLOCK_USIZE as f64
        } else {
            0.
        };
        let mut value = self.delay_time.value();
        for frame in 0..FRAMES_PER_BLOCK_USIZE {
            if self.delay_time.update(info, Tick(frame as u64)) {
                value = self.delay_time.value();
            }
            self.delays[frame] = (value as f64 * rate).clamp(min, max.max(min));
        }
    }

    /// Linear interpolation at `write_pos - delay` (the newest frame is at `write_pos`).
    #[inline]
    fn read(line: &[f32], len: usize, write_pos: usize, delay: f64) -> f32 {
        let pos = write_pos as f64 - delay;
        let pos = if pos < 0. { pos + len as f64 } else { pos };
        let k = pos.floor();
        let f = (pos - k) as f32;
        let k = k as usize % len;
        let a = line[k];
        if f == 0. {
            return a;
        }
        let b = line[(k + 1) % len];
        a + (b - a) * f
    }

    /// Store one quantum. `silent` says whether the input was silence (the block may be an
    /// explicit zero block standing in for it).
    fn write_block(&mut self, block: &Block, silent: bool) {
        let start = self.write;
        for (chan, line) in self.lines.iter_mut().enumerate() {
            let data = if block.is_silence() {
                None
            } else {
                Some(block.data_chan(chan as u8))
            };
            let dst = &mut line[start..start + FRAMES_PER_BLOCK_USIZE];
            match data {
                Some(d) => dst.copy_from_slice(d),
                None => dst.fill(0.),
            }
        }
        self.slot_channels[start / FRAMES_PER_BLOCK_USIZE] =
            if silent { 0 } else { block.chan_count() };
        self.write = (start + FRAMES_PER_BLOCK_USIZE) % self.len;
        if silent {
            self.live_frames = self.live_frames.saturating_sub(FRAMES_PER_BLOCK_USIZE);
        } else {
            self.live_frames = self.len;
        }
    }

    /// The output quantum, where frame `i` reads `delays[i]` frames before line position
    /// `base + i`.
    fn render(&self, base: usize) -> Block {
        // The slots these reads touch (with the interpolation neighbour), and the largest input
        // channel count among them.
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for (i, d) in self.delays.iter().enumerate() {
            let p = (base + i) as f64 - d;
            lo = lo.min(p);
            hi = hi.max(p);
        }
        let slots = self.slot_channels.len() as i64;
        let first = (lo.floor() as i64).div_euclid(FRAMES_PER_BLOCK_USIZE as i64);
        let last = (hi.ceil() as i64).div_euclid(FRAMES_PER_BLOCK_USIZE as i64);
        let mut delayed_channels = 0;
        for slot in first..=last.min(first + slots - 1) {
            delayed_channels =
                delayed_channels.max(self.slot_channels[slot.rem_euclid(slots) as usize]);
        }
        if delayed_channels == 0 {
            return Block::default();
        }
        let channels = self.lines.len();
        let mut block = Block::for_channels_explicit(channels as u8);
        for chan in 0..channels {
            let line = &self.lines[chan];
            let data = block.data_chan_mut(chan as u8);
            for (i, sample) in data.iter_mut().enumerate() {
                *sample = Self::read(line, self.len, (base + i) % self.len, self.delays[i]);
            }
        }
        if (delayed_channels as usize) < channels {
            // Earlier input with fewer channels was up-mixed into the current layout; give it
            // back with its own channel count.
            block.mix(delayed_channels, self.channel_info.interpretation);
        }
        block
    }

    fn channels_for(&self, input: &Block) -> usize {
        if input.is_silence() {
            self.lines.len().max(1)
        } else {
            input.chan_count() as usize
        }
    }

    /// The DelayReader half, for a node in a cycle: output from frames written in earlier
    /// quanta only.
    pub(crate) fn process_reader(&mut self, info: &BlockInfo) -> Chunk {
        self.ensure_lines(info.sample_rate);
        if self.lines.is_empty() {
            self.lines = vec![vec![0.; self.len]];
        }
        self.compute_delays(info, true);
        let mut out = Chunk::default();
        if self.live_frames == 0 {
            out.blocks.push(Block::default());
            return out;
        }
        // The frames of this quantum will be written at self.write..; frame i reads relative
        // to where it would be written.
        out.blocks.push(self.render(self.write));
        out
    }

    /// The DelayWriter half, for a node in a cycle: store this quantum's input.
    pub(crate) fn process_writer(&mut self, mut inputs: Chunk, info: &BlockInfo) {
        self.ensure_lines(info.sample_rate);
        let input = inputs.blocks.remove(0);
        let channels = self.channels_for(&input);
        self.set_channels(channels);
        let mut input = input;
        let silent = input.is_silence();
        if !silent {
            input.explicit_repeat();
        }
        self.write_block(&input, silent);
    }
}

impl AudioNodeEngine for DelayNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::DelayNode
    }

    /// Tail: until the delay line holds no input any more.
    fn removable(&self, _now: Tick) -> bool {
        self.live_frames == 0
    }

    fn process(&mut self, mut inputs: Chunk, info: &BlockInfo) -> Chunk {
        debug_assert!(inputs.len() == 1);
        self.ensure_lines(info.sample_rate);
        self.compute_delays(info, false);

        let silent_in = inputs.blocks[0].is_silence();
        if silent_in && self.live_frames == 0 {
            // Nothing in the line that could reach the output.
            return inputs;
        }
        let channels = self.channels_for(&inputs.blocks[0]);
        self.set_channels(channels);
        let block = &mut inputs.blocks[0];
        if silent_in {
            *block = Block::for_channels_explicit(channels as u8);
        } else {
            block.explicit_repeat();
        }
        // Write the whole quantum first: with a delay under one quantum, frame i reads frames
        // up to i of this same quantum, which are then in the line. Reads of not-yet-current
        // frames cannot happen, as the delay is never negative.
        let start = self.write;
        self.write_block(block, silent_in);
        inputs.blocks[0] = self.render(start);
        inputs
    }

    fn get_param(&mut self, id: ParamType) -> &mut Param {
        match id {
            ParamType::DelayTime => &mut self.delay_time,
            _ => panic!("Unknown param {:?} for DelayNode", id),
        }
    }

    fn as_delay_node(&mut self) -> Option<&mut DelayNode> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::FRAMES_PER_BLOCK;

    const RATE: f32 = 48000.;

    fn info(n: u64) -> BlockInfo {
        BlockInfo {
            sample_rate: RATE,
            frame: Tick(n * FRAMES_PER_BLOCK.0),
            time: (n * FRAMES_PER_BLOCK.0) as f64 / RATE as f64,
        }
    }

    fn impulse_block(at: usize) -> Block {
        let mut v = vec![0.; 128];
        v[at] = 1.;
        Block::for_vec(v)
    }

    fn run(node: &mut DelayNode, block: Block, n: u64) -> Block {
        let mut c = Chunk::default();
        c.blocks.push(block);
        node.process(c, &info(n)).blocks.remove(0)
    }

    fn render(node: &mut DelayNode, input: Vec<Block>, blocks: u64) -> Vec<f32> {
        let mut input = input.into_iter();
        let mut out = Vec::new();
        for n in 0..blocks {
            let b = run(node, input.next().unwrap_or_default(), n);
            if b.is_silence() {
                out.extend(std::iter::repeat_n(0., 128));
            } else {
                out.extend_from_slice(b.data_chan(0));
            }
        }
        out
    }

    #[test]
    fn zero_delay_is_identity() {
        let mut d = DelayNode::new(DelayNodeOptions::default(), ChannelInfo::default());
        let out = render(&mut d, vec![impulse_block(5)], 2);
        assert_eq!(out[5], 1.);
        assert_eq!(out.iter().filter(|v| **v != 0.).count(), 1);
    }

    #[test]
    fn integer_delay_moves_impulse() {
        for frames in [1usize, 64, 127, 128, 129, 1000] {
            let opts = DelayNodeOptions {
                max_delay_time: 1.,
                delay_time: frames as f32 / RATE,
            };
            let mut d = DelayNode::new(opts, ChannelInfo::default());
            let out = render(&mut d, vec![impulse_block(3)], 12);
            let peak = out.iter().position(|v| (*v - 1.).abs() < 1e-4).unwrap();
            assert_eq!(peak, 3 + frames, "delay {frames}");
        }
    }

    #[test]
    fn fractional_delay_interpolates_linearly() {
        let opts = DelayNodeOptions {
            max_delay_time: 1.,
            delay_time: 10.25 / RATE,
        };
        let mut d = DelayNode::new(opts, ChannelInfo::default());
        let out = render(&mut d, vec![impulse_block(0)], 2);
        assert!((out[10] - 0.75).abs() < 1e-4, "{}", out[10]);
        assert!((out[11] - 0.25).abs() < 1e-4, "{}", out[11]);
    }

    #[test]
    fn delay_is_clamped_to_max() {
        let opts = DelayNodeOptions {
            max_delay_time: 200. / RATE as f64,
            delay_time: 1.,
        };
        let mut d = DelayNode::new(opts, ChannelInfo::default());
        let out = render(&mut d, vec![impulse_block(0)], 4);
        assert!((out[200] - 1.).abs() < 1e-4);
    }

    #[test]
    fn tail_then_silence() {
        let opts = DelayNodeOptions {
            max_delay_time: 300. / RATE as f64,
            delay_time: 300. / RATE,
        };
        let mut d = DelayNode::new(opts, ChannelInfo::default());
        let out = render(&mut d, vec![impulse_block(0)], 10);
        let peak = out.iter().position(|v| *v != 0.).unwrap();
        assert_eq!(peak, 300);
        // Once the line holds only silence, the node stops producing blocks at all.
        assert!(run(&mut d, Block::default(), 10).is_silence());
        assert_eq!(d.live_frames, 0);
    }

    #[test]
    fn channel_change_remixes_the_line() {
        // Stereo in, then mono in: the pending stereo frames come out downmixed (speakers).
        let opts = DelayNodeOptions {
            max_delay_time: 1.,
            delay_time: 128. / RATE,
        };
        let mut d = DelayNode::new(opts, ChannelInfo::default());
        let mut stereo = vec![0.; 256];
        stereo[0] = 1.; // left
        stereo[128] = 0.5; // right
        run(&mut d, Block::for_vec(stereo), 0);
        let out = run(&mut d, Block::for_vec(vec![0.; 128]), 1);
        assert_eq!(out.chan_count(), 1);
        // Speakers stereo -> mono: 0.5 * (L + R).
        assert!((out.data_chan(0)[0] - 0.75).abs() < 1e-5, "{}", out.data_chan(0)[0]);
    }

    #[test]
    fn reader_writer_split_matches_one_quantum_delay() {
        // In a cycle the delay is at least 128 frames; reader before writer each quantum.
        let opts = DelayNodeOptions {
            max_delay_time: 1.,
            delay_time: 0.,
        };
        let mut d = DelayNode::new(opts, ChannelInfo::default());
        let mut out = Vec::new();
        for n in 0..3 {
            let r = d.process_reader(&info(n)).blocks.remove(0);
            if r.is_silence() {
                out.extend(std::iter::repeat_n(0., 128));
            } else {
                out.extend_from_slice(r.data_chan(0));
            }
            let mut c = Chunk::default();
            c.blocks.push(if n == 0 { impulse_block(7) } else { Block::default() });
            d.process_writer(c, &info(n));
        }
        let peak = out.iter().position(|v| *v != 0.).unwrap();
        assert_eq!(peak, 7 + 128);
    }

    #[test]
    fn output_channels_follow_the_delayed_input() {
        // delaynode-channel-count-1.html: unfilled history is mono silence, then the delayed
        // stereo input comes out stereo.
        // A power-of-two rate, as in the WPT, so the delay is exactly 256 frames.
        const RATE: f32 = 32768.;
        let opts = DelayNodeOptions {
            max_delay_time: 1.,
            delay_time: 256. / RATE,
        };
        let mut d = DelayNode::new(opts, ChannelInfo::default());
        let mut run = |n: u64| {
            let mut c = Chunk::default();
            c.blocks.push(Block::for_vec(vec![0.5; 256]));
            let info = BlockInfo {
                sample_rate: RATE,
                frame: Tick(n * 128),
                time: 0.,
            };
            d.process(c, &info).blocks.remove(0)
        };
        let out0 = run(0);
        assert!(out0.is_silence());
        assert_eq!(out0.chan_count(), 1);
        assert_eq!(run(1).chan_count(), 1);
        let out2 = run(2);
        assert_eq!(out2.chan_count(), 2);
        assert_eq!(out2.data_chan(1)[0], 0.5);
    }

    #[test]
    fn delay_time_param_is_a_rate() {
        let mut d = DelayNode::new(DelayNodeOptions::default(), ChannelInfo::default());
        assert_eq!(d.get_param(ParamType::DelayTime).value(), 0.);
    }
}
