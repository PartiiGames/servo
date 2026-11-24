/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Live audio in Web Audio (Partii, stream voice): MediaStreamAudioSourceNode reads a live track
//! (live.rs) here, and MediaStreamAudioDestinationNode writes one.

use std::any::Any;
use std::sync::Mutex;
use std::time::Instant;

use servo_media_audio::AudioStreamReader;
use servo_media_audio::block::{Block, FRAMES_PER_BLOCK_USIZE};
use servo_media_streams::MediaSocket;
use servo_media_streams::live::{self, AudioFormat, AudioReader, AudioSource, ReadState};
use servo_media_streams::registry::MediaStreamId;

/// Headroom a live reader waits for before it plays, and the backlog it trims above.
const START_MS: u32 = 20;
const MAX_MS: u32 = 60;

/// MediaStreamAudioSourceNode's reader: one 128-frame block per render quantum, at the context's
/// rate, in the track's channel count (the graph mixes from there).
pub struct LiveStreamReader {
    inner: Mutex<ReaderState>,
}

struct ReaderState {
    reader: AudioReader,
    out_rate: f64,
    channels: usize,
    /// Interleaved input frames not yet consumed, and the read position in them (frames).
    buf: Vec<f32>,
    pos: f64,
    playing: bool,
    started: bool,
}

impl LiveStreamReader {
    pub fn new(id: MediaStreamId, sample_rate: f32) -> Option<LiveStreamReader> {
        let reader = live::subscribe_audio(&id)?;
        let channels = reader.format().channels.max(1) as usize;
        Some(LiveStreamReader {
            inner: Mutex::new(ReaderState {
                reader,
                out_rate: sample_rate.max(1.) as f64,
                channels,
                buf: Vec::new(),
                pos: 0.,
                playing: false,
                started: true,
            }),
        })
    }
}

impl ReaderState {
    fn pull(&mut self) -> Block {
        let n = FRAMES_PER_BLOCK_USIZE;
        let ch = self.channels;
        let in_rate = self.reader.format().sample_rate.max(1) as f64;
        let step = in_rate / self.out_rate;
        // Bring in what the ring has.
        let avail = self.reader.buffered();
        if avail > 0 {
            let old = self.buf.len();
            self.buf.resize(old + avail, 0.);
            let got = self.reader.read(&mut self.buf[old..]);
            self.buf.truncate(old + got);
        }
        let frames = self.buf.len() / ch;
        let ms = |f: usize| (f as f64 * 1000. / in_rate) as u32;
        if !self.playing && ms(frames) >= START_MS {
            self.playing = true;
        }
        if self.playing && ms(frames) > MAX_MS {
            // Too far behind the live edge: keep START_MS.
            let keep = (START_MS as f64 * in_rate / 1000.) as usize;
            let cut = (frames - keep) * ch;
            self.buf.drain(..cut);
            self.pos = 0.;
        }
        let needed = (n as f64 * step + self.pos).ceil() as usize + 1;
        if !self.playing || !self.started || self.reader.state() == ReadState::Ended {
            return Block::default();
        }
        if self.buf.len() / ch < needed {
            // Underrun: silence, and wait for the headroom again.
            self.playing = false;
            return Block::default();
        }
        let mut out = vec![0f32; n * ch];
        for i in 0..n {
            let p = self.pos + i as f64 * step;
            let i0 = p.floor() as usize;
            let frac = (p - i0 as f64) as f32;
            for c in 0..ch {
                let a = self.buf[i0 * ch + c];
                let b = self.buf[(i0 + 1) * ch + c];
                out[c * n + i] = a + (b - a) * frac;
            }
        }
        let advance = self.pos + n as f64 * step;
        let whole = advance.floor() as usize;
        self.pos = advance - whole as f64;
        self.buf.drain(..whole * ch);
        Block::for_vec(out)
    }
}

impl AudioStreamReader for LiveStreamReader {
    fn pull(&self) -> Block {
        self.inner.lock().unwrap().pull()
    }
    fn start(&self) {
        self.inner.lock().unwrap().started = true;
    }
    fn stop(&self) {
        self.inner.lock().unwrap().started = false;
    }
}

/// The socket of a MediaStreamAudioDestinationNode: the live source its `.stream`'s track views.
pub struct LiveSocket(pub AudioSource);

impl MediaSocket for LiveSocket {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A MediaStreamAudioDestinationNode's stream: stereo at the output's preferred rate (the
/// context's, unless a page asks for another rate, which the sink then converts).
pub fn destination_stream(rate: u32) -> (Box<dyn MediaSocket>, MediaStreamId) {
    let (id, src) = live::create_audio_source(
        AudioFormat {
            sample_rate: rate,
            channels: 2,
        },
        "MediaStreamAudioDestinationNode",
    );
    (Box::new(LiveSocket(src)), id)
}

/// Push one rendered block (planar, 1 or more channels, at `in_rate`) into a stereo live source.
pub fn push_block(src: &AudioSource, block: &Block, in_rate: f32, scratch: &mut Vec<f32>) {
    let n = FRAMES_PER_BLOCK_USIZE;
    let out_rate = src.format().sample_rate as f32;
    scratch.clear();
    if block.is_silence() {
        scratch.resize(n * 2, 0.);
    } else {
        let chans = block.chan_count();
        let l = block.data_chan(0);
        let r = if chans > 1 { block.data_chan(1) } else { l };
        for i in 0..n {
            scratch.push(l[i]);
            scratch.push(r[i]);
        }
    }
    if (in_rate - out_rate).abs() > 0.5 {
        // Rare (a page-chosen context rate): nearest-sample conversion.
        let frames = (n as f32 * out_rate / in_rate).round() as usize;
        let src_frames: Vec<f32> = std::mem::take(scratch);
        for i in 0..frames {
            let j = ((i as f32 * in_rate / out_rate) as usize).min(n - 1);
            scratch.push(src_frames[j * 2]);
            scratch.push(src_frames[j * 2 + 1]);
        }
    }
    src.push(scratch, Instant::now());
}
