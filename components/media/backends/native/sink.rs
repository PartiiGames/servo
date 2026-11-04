/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The Web Audio sink. The render thread pushes 128-frame blocks at the context's rate; the output
//! pulls at the device's rate. Between them sits a small FIFO of stereo frames, resampled on the
//! pull side (queue.rs). When the FIFO drops under its target the sink wakes the render
//! thread with `SinkNeedData` (the push/pull model `AudioRenderThread::event_loop` expects).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, Weak};

use servo_media_audio::block::Chunk;
use servo_media_audio::render_thread::{AudioRenderThreadMsg, SinkEosCallback};
use servo_media_audio::sink::{AudioSink, AudioSinkError};
use servo_media_streams::MediaSocket;

use crate::output::{OutputSource, add_stereo, output};
use crate::queue::FrameQueue;

/// Minimum FIFO level (context-rate frames) before the render thread may block.
pub(crate) const MIN_TARGET_FRAMES: usize = 1024;

pub(crate) struct SinkShared {
    /// Stereo frames at the context's rate.
    fifo: Mutex<FrameQueue>,
    sample_rate: Mutex<f32>,
    playing: AtomicBool,
    target: AtomicUsize,
    render_thread: Mutex<Option<Sender<AudioRenderThreadMsg>>>,
    need_data_sent: AtomicBool,
    /// Set once a callback has been filled completely: earlier shortfalls are start-up, not
    /// underruns.
    primed: AtomicBool,
    underrun_events: AtomicU64,
    underrun_frames: AtomicU64,
}

impl SinkShared {
    fn level(&self) -> usize {
        self.fifo.lock().unwrap().len()
    }

    fn wake_render_thread(&self) {
        if !self.need_data_sent.swap(true, Ordering::AcqRel) {
            if let Some(sender) = self.render_thread.lock().unwrap().as_ref() {
                let _ = sender.send(AudioRenderThreadMsg::SinkNeedData);
            }
        }
    }
}

impl OutputSource for SinkShared {
    fn mix(&self, out: &mut [f32], channels: usize, rate: u32) {
        if !self.playing.load(Ordering::Acquire) {
            return;
        }
        let frames_out = out.len() / channels;
        let src_rate = *self.sample_rate.lock().unwrap() as f64;
        let step = src_rate / rate as f64;
        // Keep about two device callbacks' worth buffered, so the render thread can refill in time.
        let wanted = ((frames_out as f64 * step * 2.) as usize).max(MIN_TARGET_FRAMES);
        if wanted > self.target.load(Ordering::Relaxed) {
            self.target.store(wanted, Ordering::Relaxed);
        }
        // On underrun the rest stays silent and the queue keeps its place.
        let mut produced = 0;
        self.fifo.lock().unwrap().read(frames_out, step, |i, l, r| {
            produced = i + 1;
            add_stereo(&mut out[i * channels..(i + 1) * channels], l, r)
        });
        if produced == frames_out {
            self.primed.store(true, Ordering::Relaxed);
        } else if self.primed.load(Ordering::Relaxed) {
            self.underrun_events.fetch_add(1, Ordering::Relaxed);
            self.underrun_frames
                .fetch_add((frames_out - produced) as u64, Ordering::Relaxed);
        }
        if self.level() < self.target.load(Ordering::Relaxed) {
            self.wake_render_thread();
        }
    }
}

pub struct NativeAudioSink {
    shared: Arc<SinkShared>,
    /// A MediaStreamDestinationNode sink: no output, data is dropped (media streams are
    /// unsupported, like the dummy backend).
    stream_only: AtomicBool,
}

impl NativeAudioSink {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(SinkShared {
                fifo: Mutex::new(FrameQueue::new()),
                sample_rate: Mutex::new(44100.),
                playing: AtomicBool::new(false),
                target: AtomicUsize::new(MIN_TARGET_FRAMES),
                render_thread: Mutex::new(None),
                need_data_sent: AtomicBool::new(false),
                primed: AtomicBool::new(false),
                underrun_events: AtomicU64::new(0),
                underrun_frames: AtomicU64::new(0),
            }),
            stream_only: AtomicBool::new(false),
        }
    }
}

impl Default for NativeAudioSink {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSink for NativeAudioSink {
    fn init(
        &self,
        sample_rate: f32,
        render_thread_channel: Sender<AudioRenderThreadMsg>,
    ) -> Result<(), AudioSinkError> {
        *self.shared.sample_rate.lock().unwrap() = sample_rate;
        *self.shared.render_thread.lock().unwrap() = Some(render_thread_channel);
        let shared: Arc<dyn OutputSource> = self.shared.clone();
        output().add_source(Arc::downgrade(&shared) as Weak<dyn OutputSource>);
        Ok(())
    }

    fn init_stream(
        &self,
        _channels: u8,
        sample_rate: f32,
        _socket: Box<dyn MediaSocket>,
    ) -> Result<(), AudioSinkError> {
        *self.shared.sample_rate.lock().unwrap() = sample_rate;
        self.stream_only.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn play(&self) -> Result<(), AudioSinkError> {
        // A resumed stream refills from empty: that start-up isn't an underrun.
        self.shared.primed.store(false, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Release);
        Ok(())
    }

    fn stop(&self) -> Result<(), AudioSinkError> {
        self.shared.playing.store(false, Ordering::Release);
        Ok(())
    }

    fn underrun_stats(&self) -> Option<(u64, u64)> {
        Some((
            self.shared.underrun_events.load(Ordering::Relaxed),
            self.shared.underrun_frames.load(Ordering::Relaxed),
        ))
    }

    fn has_enough_data(&self) -> bool {
        if self.stream_only.load(Ordering::Relaxed) {
            return true;
        }
        self.shared.level() >= self.shared.target.load(Ordering::Relaxed)
    }

    fn push_data(&self, chunk: Chunk) -> Result<(), AudioSinkError> {
        if self.stream_only.load(Ordering::Relaxed) {
            return Ok(());
        }
        let n = servo_media_audio::block::FRAMES_PER_BLOCK_USIZE;
        {
            let mut fifo = self.shared.fifo.lock().unwrap();
            match chunk.blocks.first() {
                Some(block) if !block.is_silence() => {
                    let chans = block.chan_count();
                    if chans == 1 {
                        let c = block.data_chan(0);
                        fifo.extend(c.iter().map(|s| [*s, *s]));
                    } else {
                        let l = block.data_chan(0);
                        let r = block.data_chan(1);
                        fifo.extend(l.iter().zip(r.iter()).map(|(l, r)| [*l, *r]));
                    }
                },
                _ => fifo.extend(std::iter::repeat_n([0.; 2], n)),
            }
        }
        self.shared.need_data_sent.store(false, Ordering::Release);
        Ok(())
    }

    fn set_eos_callback(&self, _: SinkEosCallback) {}
}

impl Drop for NativeAudioSink {
    fn drop(&mut self) {
        self.shared.playing.store(false, Ordering::Release);
        self.shared.render_thread.lock().unwrap().take();
    }
}
