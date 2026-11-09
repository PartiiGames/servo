/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! <https://webaudio.github.io/web-audio-api/#ScriptProcessorNode>: the render side.
//!
//! The node gathers its input into buffers of `buffer_size` frames. Each full buffer goes to the
//! page (`ScriptProcessorHandle::audio_process`, which fires `audioprocess` on the main thread),
//! and the page's output for input buffer k is heard from frame (k + 2) * buffer_size of the
//! node's life: a fixed latency of two buffers, as in Blink and Gecko (WPT
//! simple-input-output.html checks exactly that). An output that is not back in time is dropped
//! (silence for that buffer), so a slow page doesn't build up latency.
//!
//! In an OfflineAudioContext the render thread waits, between render quanta, for every
//! audioprocess it handed out (`pending`), still serving control messages meanwhile, so offline
//! rendering is deterministic.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use malloc_size_of_derive::MallocSizeOf;

use crate::audio_node::{AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo};
use crate::block::{Block, Chunk, FRAMES_PER_BLOCK_USIZE};

/// Dropped (or `done()`) once the page has answered an audioprocess, its output queued; also
/// when the event never runs (the page is gone), so offline rendering never waits forever.
pub struct ScriptProcessorDone(Option<Arc<AtomicUsize>>);

impl ScriptProcessorDone {
    pub fn done(self) {}
}

impl Drop for ScriptProcessorDone {
    fn drop(&mut self) {
        if let Some(pending) = self.0.take() {
            // Saturating: the render thread resets the count if it gave up waiting.
            let _ = pending.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                Some(n.saturating_sub(1))
            });
        }
    }
}

/// The page's side of a ScriptProcessorNode.
pub trait ScriptProcessorHandle: Send {
    /// Input buffer `index` (one Vec per input channel), whose output is played back from
    /// `playback_time` (seconds, in the context's time). Call `done` after queueing the output
    /// (or if the page is gone).
    fn audio_process(
        &mut self,
        index: u64,
        input: Vec<Vec<f32>>,
        playback_time: f64,
        done: ScriptProcessorDone,
    );
    /// The next output buffer the page filled, with the index of its input buffer.
    fn take_output(&mut self) -> Option<(u64, Vec<Vec<f32>>)>;
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
    /// Offline only: audioprocess events handed out and not answered yet.
    pending: Option<Arc<AtomicUsize>>,
    /// The input buffer being filled, and its index.
    input: Vec<Vec<f32>>,
    filled: usize,
    sent: u64,
    /// Frames processed since the node was made.
    frames: u64,
    /// Answers waiting for their turn.
    ready: VecDeque<(u64, Vec<Vec<f32>>)>,
    /// The output buffer being played (none: silence).
    output: Option<Vec<Vec<f32>>>,
}

impl ScriptProcessorNode {
    pub fn new(
        options: ScriptProcessorNodeOptions,
        channel_info: ChannelInfo,
        pending: Option<Arc<AtomicUsize>>,
    ) -> Self {
        let size = options.buffer_size as usize;
        ScriptProcessorNode {
            channel_info,
            buffer_size: size,
            input_channels: options.input_channels,
            output_channels: options.output_channels,
            handle: options.handle,
            pending,
            input: vec![vec![0.; size]; options.input_channels as usize],
            filled: 0,
            sent: 0,
            frames: 0,
            ready: VecDeque::new(),
            output: None,
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
        let size = self.buffer_size as u64;

        // Output: at each buffer boundary j, the answer for input buffer j - 2.
        let played = (self.frames % size) as usize;
        if played == 0 {
            let j = self.frames / size;
            while let Some(answer) = self.handle.take_output() {
                self.ready.push_back(answer);
            }
            self.output = None;
            if j >= 2 {
                let want = j - 2;
                while self.ready.front().is_some_and(|(index, _)| *index < want) {
                    self.ready.pop_front();
                }
                if self.ready.front().is_some_and(|(index, _)| *index == want) {
                    self.output = self.ready.pop_front().map(|(_, output)| output);
                }
            }
        }
        let channels = self.output_channels.max(1);
        let mut out = Block::for_channels_explicit(channels);
        if let Some(output) = self.output.as_ref() {
            for c in 0..self.output_channels as usize {
                if let Some(src) = output.get(c) {
                    let start = played.min(src.len());
                    let end = (played + frames).min(src.len());
                    out.data_chan_mut(c as u8)[..end - start].copy_from_slice(&src[start..end]);
                }
            }
        }

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
        self.frames += frames as u64;
        if self.filled >= self.buffer_size {
            self.filled = 0;
            let input = std::mem::replace(
                &mut self.input,
                vec![vec![0.; self.buffer_size]; self.input_channels as usize],
            );
            // Buffer k ends here, at (k + 1) * size of the node's frames; its output is heard
            // from (k + 2) * size.
            let end = info.frame.0 as f64 + frames as f64;
            let playback_time = (end + self.buffer_size as f64) / info.sample_rate as f64;
            if let Some(pending) = self.pending.as_ref() {
                pending.fetch_add(1, Ordering::SeqCst);
            }
            let done = ScriptProcessorDone(self.pending.clone());
            let index = self.sent;
            self.sent += 1;
            self.handle.audio_process(index, input, playback_time, done);
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
        outputs: VecDeque<(u64, Vec<Vec<f32>>)>,
    }

    struct Handle(Arc<Mutex<Page>>);

    impl ScriptProcessorHandle for Handle {
        fn audio_process(
            &mut self,
            index: u64,
            input: Vec<Vec<f32>>,
            playback_time: f64,
            done: ScriptProcessorDone,
        ) {
            let mut page = self.0.lock().unwrap();
            page.calls
                .push((input.iter().map(|c| c[0]).collect(), playback_time));
            // The page answers with one value per buffer: its call number.
            let n = page.calls.len() as f32;
            let len = input.first().map_or(256, |c| c.len());
            page.outputs.push_back((index, vec![vec![n; len]]));
            done.done();
        }
        fn take_output(&mut self) -> Option<(u64, Vec<Vec<f32>>)> {
            self.0.lock().unwrap().outputs.pop_front()
        }
    }

    #[test]
    fn buffers_in_and_out() {
        let page = Arc::new(Mutex::new(Page::default()));
        let pending = Arc::new(AtomicUsize::new(0));
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
            Some(pending.clone()),
        );
        let mut heard = Vec::new();
        for i in 0..8u64 {
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
        // audioprocess after blocks 2, 4, 6 and 8, with the first sample of each buffer.
        assert_eq!(
            page.calls.iter().map(|c| c.0[0]).collect::<Vec<_>>(),
            vec![1., 3., 5., 7.]
        );
        // playbackTime: buffer 0 is heard from frame 2 * 256 (4 s at 128 Hz).
        assert_eq!(page.calls[0].1, 4.);
        // Two buffers of silence, then each answer for one buffer.
        assert_eq!(heard, vec![0., 0., 0., 0., 1., 1., 2., 2.]);
        assert_eq!(pending.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn late_answers_are_dropped() {
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
            None,
        );
        let mut heard = Vec::new();
        for i in 0..8u64 {
            let mut chunk = Chunk::default();
            chunk
                .blocks
                .push(Block::for_vec(vec![1.; FRAMES_PER_BLOCK_USIZE]));
            let info = BlockInfo {
                sample_rate: 128.,
                frame: Tick(i * 128),
                time: i as f64,
            };
            // The answer to buffer 0 (queued at block 1) is held back past its turn (frame 512).
            if i == 2 {
                let held = page.lock().unwrap().outputs.pop_front().unwrap();
                assert_eq!(held.0, 0);
            }
            let out = node.process(chunk, &info);
            heard.push(out.blocks[0].data_chan(0)[0]);
        }
        assert_eq!(heard, vec![0., 0., 0., 0., 0., 0., 2., 2.]);
    }
}
