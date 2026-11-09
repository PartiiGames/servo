/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! <https://webaudio.github.io/web-audio-api/#ScriptProcessorNode>: the render side.
//!
//! The node gathers its input into buffers of `buffer_size` frames. Each full buffer goes to the
//! page (`ScriptProcessorHandle::audio_process`, which fires `audioprocess` on the main thread),
//! and the node plays the output buffers the page filled, in order, as they arrive
//! (`take_output`). Until one has arrived it outputs silence, so the page's output is heard at
//! least one buffer after the input it was computed from, as in Blink and Gecko.

use malloc_size_of_derive::MallocSizeOf;

use crate::audio_node::{AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo};
use crate::block::{Block, Chunk, FRAMES_PER_BLOCK_USIZE};

/// The page's side of a ScriptProcessorNode.
pub trait ScriptProcessorHandle: Send {
    /// A full input buffer (one Vec per input channel), to be played back from `playback_time`
    /// (seconds, in the context's time).
    fn audio_process(&mut self, input: Vec<Vec<f32>>, playback_time: f64);
    /// The next output buffer the page filled (one Vec per output channel), if one is ready.
    fn take_output(&mut self) -> Option<Vec<Vec<f32>>>;
}

#[derive(MallocSizeOf)]
pub struct ScriptProcessorNodeOptions {
    pub buffer_size: u32,
    pub input_channels: u8,
    pub output_channels: u8,
    #[ignore_malloc_size_of = "trait object"]
    pub handle: Box<dyn ScriptProcessorHandle>,
}

#[derive(AudioNodeCommon)]
pub(crate) struct ScriptProcessorNode {
    channel_info: ChannelInfo,
    buffer_size: usize,
    input_channels: u8,
    output_channels: u8,
    handle: Box<dyn ScriptProcessorHandle>,
    /// The input buffer being filled.
    input: Vec<Vec<f32>>,
    filled: usize,
    /// The output buffer being played and the next frame of it.
    output: Option<Vec<Vec<f32>>>,
    played: usize,
}

impl ScriptProcessorNode {
    pub fn new(options: ScriptProcessorNodeOptions, channel_info: ChannelInfo) -> Self {
        let size = options.buffer_size as usize;
        ScriptProcessorNode {
            channel_info,
            buffer_size: size,
            input_channels: options.input_channels,
            output_channels: options.output_channels,
            handle: options.handle,
            input: vec![vec![0.; size]; options.input_channels as usize],
            filled: 0,
            output: None,
            played: 0,
        }
    }
}

impl AudioNodeEngine for ScriptProcessorNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::ScriptProcessorNode
    }

    /// The page may produce sound with no input at all (a generator, as Emscripten's SDL and
    /// OpenAL drive it): the node always runs.
    fn removable(&self, _now: crate::block::Tick) -> bool {
        false
    }

    fn process(&mut self, mut inputs: Chunk, info: &BlockInfo) -> Chunk {
        debug_assert!(inputs.len() == 1);
        let frames = FRAMES_PER_BLOCK_USIZE;

        // Input: mixed to the node's input channel count (channelCountMode is "explicit").
        let mut block = inputs.blocks[0].take();
        if self.input_channels > 0 && !block.is_silence() {
            block.mix(self.input_channels, self.channel_info.interpretation);
        }
        for (c, channel) in self.input.iter_mut().enumerate() {
            let dst = &mut channel[self.filled..self.filled + frames];
            if block.is_silence() || c >= block.chan_count() as usize {
                dst.fill(0.);
            } else {
                dst.copy_from_slice(block.data_chan(c as u8));
            }
        }
        self.filled += frames;
        if self.filled >= self.buffer_size {
            self.filled = 0;
            let input = std::mem::replace(
                &mut self.input,
                vec![vec![0.; self.buffer_size]; self.input_channels as usize],
            );
            // This buffer's output is heard after the one the page is filling now.
            let end = info.frame.0 as f64 + frames as f64;
            let playback_time = (end + self.buffer_size as f64) / info.sample_rate as f64;
            self.handle.audio_process(input, playback_time);
        }

        // Output: the page's buffers in order, silence while none is ready.
        let channels = self.output_channels.max(1);
        let mut out = Block::for_channels_explicit(channels);
        if self.output.is_none() || self.played >= self.buffer_size {
            self.output = self.handle.take_output();
            self.played = 0;
        }
        if let Some(output) = self.output.as_ref() {
            for c in 0..self.output_channels as usize {
                if let Some(src) = output.get(c) {
                    let start = self.played.min(src.len());
                    let end = (self.played + frames).min(src.len());
                    out.data_chan_mut(c as u8)[..end - start].copy_from_slice(&src[start..end]);
                }
            }
            self.played += frames;
        }
        inputs.blocks[0] = out;
        inputs
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::audio_node::{ChannelCountMode, ChannelInterpretation};
    use crate::block::Tick;

    #[derive(Default)]
    struct Page {
        /// (first input sample of each channel, playback time) per audioprocess.
        calls: Vec<(Vec<f32>, f64)>,
        outputs: Vec<Vec<Vec<f32>>>,
    }

    struct Handle(Arc<Mutex<Page>>);

    impl ScriptProcessorHandle for Handle {
        fn audio_process(&mut self, input: Vec<Vec<f32>>, playback_time: f64) {
            let mut page = self.0.lock().unwrap();
            page.calls
                .push((input.iter().map(|c| c[0]).collect(), playback_time));
            // The page answers with a ramp, one value per buffer.
            let n = page.calls.len() as f32;
            let len = input.first().map_or(256, |c| c.len());
            page.outputs.push(vec![vec![n; len]]);
        }
        fn take_output(&mut self) -> Option<Vec<Vec<f32>>> {
            let mut page = self.0.lock().unwrap();
            if page.outputs.is_empty() {
                None
            } else {
                Some(page.outputs.remove(0))
            }
        }
    }

    #[test]
    fn buffers_in_and_out() {
        let page = Arc::new(Mutex::new(Page::default()));
        let mut node = ScriptProcessorNode::new(
            ScriptProcessorNodeOptions {
                buffer_size: 256,
                input_channels: 1,
                output_channels: 1,
                handle: Box::new(Handle(page.clone())),
            },
            ChannelInfo {
                count: 1,
                mode: ChannelCountMode::Explicit,
                interpretation: ChannelInterpretation::Speakers,
                context_channel_count: 2,
            },
        );
        let mut heard = Vec::new();
        for i in 0..6u64 {
            let mut chunk = Chunk::default();
            chunk
                .blocks
                .push(Block::for_vec(vec![i as f32 + 1.; FRAMES_PER_BLOCK_USIZE]));
            let info = BlockInfo {
                sample_rate: 128.,
                frame: Tick(i * 128),
                time: i as f64,
            };
            let out = node.process(chunk, &info);
            heard.push(out.blocks[0].data_chan(0)[0]);
        }
        let page = page.lock().unwrap();
        // audioprocess after blocks 2, 4 and 6, with the first sample of each buffer.
        assert_eq!(
            page.calls.iter().map(|c| c.0[0]).collect::<Vec<_>>(),
            vec![1., 3., 5.]
        );
        // playbackTime: the end of the buffer plus one buffer (256 frames at 128 Hz = 2 s).
        assert_eq!(page.calls[0].1, 4.);
        // Silence until the first answer (this page answers at once, inside audio_process),
        // then each answer for one buffer.
        assert_eq!(heard, vec![0., 1., 1., 2., 2., 3.]);
    }
}
