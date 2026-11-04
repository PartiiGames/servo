/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The render-thread side of an `AudioWorkletNode`.
//!
//! servo-media knows nothing about JavaScript: the processor is an
//! [`AudioWorkletProcessorHandle`] supplied by the embedder (Servo's script crate), which runs the
//! `AudioWorkletProcessor`'s JS on this same thread. This engine shapes the inputs, sizes the
//! outputs, computes the AudioParams for the quantum and hands everything to the handle.
//!
//! <https://webaudio.github.io/web-audio-api/#AudioWorkletNode>

use malloc_size_of_derive::MallocSizeOf;

use crate::audio_node::{
    AudioNodeEngine, AudioNodeType, BlockInfo, ChannelCountMode, ChannelInfo,
};
use crate::block::{Block, Chunk, FRAMES_PER_BLOCK_USIZE, Tick};
use crate::param::{Param, ParamRate, ParamType};

/// The embedder's processor: called on the render thread only.
pub trait AudioWorkletProcessorHandle: Send {
    /// Construct the processor. Called once, on the render thread, when the node is created.
    fn construct(&mut self, info: &BlockInfo);

    /// Process one render quantum.
    ///
    /// - `inputs[n]`: the n-th input. A silent input (nothing actively connected) is a silent
    ///   [`Block`]; the handle presents it as an empty channel array.
    /// - `outputs[n]`: the n-th output, already sized to its channel count and zeroed.
    /// - `params[p]`: the p-th parameter descriptor's values: 128 of them, or 1 when the value is
    ///   constant for the whole quantum.
    ///
    /// Returns the processor's keep-alive result (the ToBoolean of `process()`'s return value);
    /// false also when the processor has failed.
    fn process(
        &mut self,
        inputs: &[Block],
        outputs: &mut [Block],
        params: &[&[f32]],
        info: &BlockInfo,
    ) -> bool;
}

/// One `AudioParamDescriptor`, resolved by the DOM.
#[derive(Clone, Debug, MallocSizeOf)]
pub struct AudioWorkletParamOptions {
    pub default_value: f32,
    pub min_value: f32,
    pub max_value: f32,
    pub rate: ParamRate,
    /// The node's initial value (`parameterData`, or the default).
    pub initial_value: f32,
}

#[derive(MallocSizeOf)]
pub struct AudioWorkletNodeOptions {
    pub number_of_inputs: u32,
    pub number_of_outputs: u32,
    /// The explicit channel count of each output (`outputChannelCount`), when given.
    pub output_channel_count: Option<Vec<u8>>,
    pub params: Vec<AudioWorkletParamOptions>,
    #[ignore_malloc_size_of = "trait object"]
    pub processor: Box<dyn AudioWorkletProcessorHandle>,
}

#[derive(AudioNodeCommon)]
pub(crate) struct AudioWorkletNode {
    channel_info: ChannelInfo,
    number_of_inputs: u32,
    number_of_outputs: u32,
    output_channel_count: Option<Vec<u8>>,
    params: Vec<Param>,
    /// One 128-frame scratch buffer per param, reused every quantum.
    param_values: Vec<[f32; FRAMES_PER_BLOCK_USIZE]>,
    /// Whether each param was constant this quantum (its view is then 1 value long).
    param_constant: Vec<bool>,
    processor: Box<dyn AudioWorkletProcessorHandle>,
    constructed: bool,
    /// The last result of `process()`. When false and every input is silent, the processor is
    /// no longer actively processing and the node outputs silence without calling it.
    keep_alive: bool,
}

impl AudioWorkletNode {
    pub fn new(options: AudioWorkletNodeOptions, channel_info: ChannelInfo) -> Self {
        let params: Vec<Param> = options
            .params
            .iter()
            .map(|p| {
                let mut param = match p.rate {
                    ParamRate::ARate => Param::new(p.initial_value),
                    ParamRate::KRate => Param::new_krate(p.initial_value),
                };
                param.update_range((p.min_value, p.max_value));
                param
            })
            .collect();
        let count = params.len();
        AudioWorkletNode {
            channel_info,
            number_of_inputs: options.number_of_inputs,
            number_of_outputs: options.number_of_outputs,
            output_channel_count: options.output_channel_count,
            params,
            param_values: vec![[0.; FRAMES_PER_BLOCK_USIZE]; count],
            param_constant: vec![true; count],
            processor: options.processor,
            constructed: false,
            keep_alive: true,
        }
    }

    /// Construct the processor (the render thread does this as it creates the node).
    pub fn construct(&mut self, info: &BlockInfo) {
        if !self.constructed {
            self.constructed = true;
            self.processor.construct(info);
        }
    }

    /// The channel count of output `index` for this quantum.
    /// <https://webaudio.github.io/web-audio-api/#configuring-channels-with-audioworkletnodeoptions>
    fn output_channels(&self, index: usize, inputs: &Chunk) -> u8 {
        if let Some(counts) = &self.output_channel_count {
            return counts.get(index).copied().unwrap_or(1).max(1);
        }
        if self.number_of_inputs == 1 && self.number_of_outputs == 1 {
            // The output follows the input's computedNumberOfChannels.
            let input = &inputs.blocks[0];
            if input.is_silence() && self.channel_info.mode != ChannelCountMode::Explicit {
                return 1;
            }
            return input.chan_count().max(1);
        }
        1
    }

    fn compute_params(&mut self, info: &BlockInfo) {
        for (i, param) in self.params.iter_mut().enumerate() {
            let values = &mut self.param_values[i];
            for (tick, value) in values.iter_mut().enumerate() {
                param.update(info, Tick(tick as u64));
                *value = param.value();
            }
            let first = values[0];
            self.param_constant[i] = values.iter().all(|v| *v == first);
        }
    }
}

fn block_is_inactive(block: &Block) -> bool {
    if block.is_silence() {
        return true;
    }
    (0..block.chan_count()).all(|c| block.data_chan(c).iter().all(|v| *v == 0.))
}

impl AudioNodeEngine for AudioWorkletNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::AudioWorkletNode
    }

    fn process(&mut self, inputs: Chunk, info: &BlockInfo) -> Chunk {
        debug_assert_eq!(inputs.len(), self.number_of_inputs as usize);

        self.construct(info);

        let mut outputs = Chunk::default();
        for index in 0..self.number_of_outputs as usize {
            let channels = self.output_channels(index, &inputs);
            outputs.blocks.push(Block::for_channels_explicit(channels));
        }

        // Params are computed every quantum, even when the processor is idle, so that automation
        // timelines stay in step with the context's time.
        self.compute_params(info);

        // Servo doesn't track which nodes are "actively processing", so an input counts as
        // inactive when its block is silence or all zeros (a stopped source sends explicit
        // zeros). Only used when process() last returned false.
        if !self.keep_alive && inputs.blocks.iter().all(block_is_inactive) {
            return outputs;
        }

        let params: smallvec::SmallVec<[&[f32]; 4]> = self
            .param_values
            .iter()
            .zip(self.param_constant.iter())
            .map(|(values, constant)| {
                if *constant {
                    &values[..1]
                } else {
                    &values[..]
                }
            })
            .collect();
        self.keep_alive =
            self.processor
                .process(&inputs.blocks, &mut outputs.blocks, &params, info);
        outputs
    }

    fn input_count(&self) -> u32 {
        self.number_of_inputs
    }

    fn output_count(&self) -> u32 {
        self.number_of_outputs
    }

    fn get_param(&mut self, id: ParamType) -> &mut Param {
        match id {
            ParamType::Worklet(index) if (index as usize) < self.params.len() => {
                &mut self.params[index as usize]
            },
            _ => panic!("Unknown param {:?} for AudioWorkletNode", id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `param[0] * (channel + 1)` to every output channel and counts its calls.
    struct Fill {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        keep_alive: bool,
    }

    impl AudioWorkletProcessorHandle for Fill {
        fn construct(&mut self, _: &BlockInfo) {}
        fn process(&mut self, _: &[Block], outputs: &mut [Block], params: &[&[f32]], _: &BlockInfo) -> bool {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            for out in outputs.iter_mut() {
                for c in 0..out.chan_count() {
                    let data = out.data_chan_mut(c);
                    for (i, v) in data.iter_mut().enumerate() {
                        let p = if params[0].len() == 1 { params[0][0] } else { params[0][i] };
                        *v = p * (c as f32 + 1.);
                    }
                }
            }
            self.keep_alive
        }
    }

    fn info(frame: u64) -> BlockInfo {
        BlockInfo { sample_rate: 48000., frame: Tick(frame), time: frame as f64 / 48000. }
    }

    fn node(keep_alive: bool, calls: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> AudioWorkletNode {
        AudioWorkletNode::new(
            AudioWorkletNodeOptions {
                number_of_inputs: 0,
                number_of_outputs: 1,
                output_channel_count: Some(vec![2]),
                params: vec![AudioWorkletParamOptions {
                    default_value: 0.5,
                    min_value: -1.,
                    max_value: 1.,
                    rate: ParamRate::ARate,
                    initial_value: 0.25,
                }],
                processor: Box::new(Fill { calls, keep_alive }),
            },
            ChannelInfo::default(),
        )
    }

    #[test]
    fn outputs_channels_and_constant_param() {
        let calls = Default::default();
        let mut n = node(true, calls);
        let out = n.process(Chunk::default(), &info(0));
        assert_eq!(out.len(), 1);
        assert_eq!(out.blocks[0].chan_count(), 2);
        assert_eq!(out.blocks[0].data_chan(0)[5], 0.25);
        assert_eq!(out.blocks[0].data_chan(1)[127], 0.5);
        assert!(n.param_constant[0]);
    }

    #[test]
    fn idle_after_false_with_silent_inputs() {
        let calls: std::sync::Arc<std::sync::atomic::AtomicUsize> = Default::default();
        let mut n = node(false, calls.clone());
        n.process(Chunk::default(), &info(0));
        let out = n.process(Chunk::default(), &info(128));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(out.blocks[0].data_chan(0)[0], 0.);
    }
}
