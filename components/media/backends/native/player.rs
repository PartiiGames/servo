/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! An audio-only `<audio>`/`<video>` player: buffers the fetched bytes, decodes them with
//! symphonia on its own thread a little ahead of playback, and plays through the shared output.
//! Files with no decodable audio track (e.g. video-only) report an error, as unsupported media.

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::ops::Range;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use log::warn;
use servo_base::generic_channel::GenericCallback;
use servo_media::MediaInstanceError;
use servo_media_player::audio::AudioRenderer;
use servo_media_player::metadata::Metadata;
use servo_media_player::{PlaybackState, Player, PlayerError, PlayerEvent};
use servo_media_streams::registry::MediaStreamId;
use servo_media_traits::MediaInstance;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

use crate::output::{OutputSource, add_stereo, context_rate, output};
use crate::queue::FrameQueue;

/// Decode ahead of playback by this much (seconds).
const LOW_WATER_SECS: f64 = 0.5;
const HIGH_WATER_SECS: f64 = 1.0;
const POSITION_INTERVAL: Duration = Duration::from_millis(100);
const TICK: Duration = Duration::from_millis(15);

struct PlayState {
    fifo: FrameQueue,
    src_rate: u32,
    playing: bool,
    suspended: bool,
    volume: f32,
    muted: bool,
    context_muted: bool,
    playback_rate: f64,
    /// Seconds at the start of the FIFO's timeline (set by seeks).
    base_time: f64,
    /// Frames played since `base_time`.
    consumed: u64,
    /// The media's duration once known (the whole file is in memory, so all of it is buffered).
    duration: Option<f64>,
}

impl PlayState {
    fn position(&self) -> f64 {
        if self.src_rate == 0 {
            return self.base_time;
        }
        self.base_time + self.consumed as f64 / self.src_rate as f64
    }
}

// MediaElementAudioSourceNode renders at the context's rate, which the player isn't told: it uses
// the default context rate for this output (`output::context_rate`). A page that picks another
// sampleRate gets its element audio at the wrong speed.

pub(crate) struct PlayerShared {
    state: Mutex<PlayState>,
    /// Set for `createMediaElementSource`: the audio goes to the Web Audio graph, not the output.
    renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
    /// Fractional renderer frames carried between output callbacks.
    renderer_carry: Mutex<f64>,
}

impl PlayerShared {
    /// Pull `frames` frames at `rate` from the FIFO (resampled), calling `put` per frame.
    fn pull(st: &mut PlayState, frames: usize, rate: u32, mut put: impl FnMut(usize, f32, f32)) {
        let gain = if st.muted || st.context_muted {
            0.
        } else {
            st.volume
        };
        let step = st.src_rate as f64 * st.playback_rate / rate as f64;
        st.consumed += st
            .fifo
            .read(frames, step, |i, l, r| put(i, l * gain, r * gain));
    }
}

impl OutputSource for PlayerShared {
    fn mix(&self, out: &mut [f32], channels: usize, rate: u32) {
        let mut guard = self.state.lock().unwrap();
        let st = &mut *guard;
        if !st.playing || st.suspended || st.src_rate == 0 || st.playback_rate <= 0. {
            return;
        }
        let frames_out = out.len() / channels;
        match self.renderer {
            None => Self::pull(st, frames_out, rate, |i, l, r| {
                add_stereo(&mut out[i * channels..(i + 1) * channels], l, r)
            }),
            Some(ref renderer) => {
                // The output's callbacks are the clock: render the same span of time at the
                // renderer's rate.
                let renderer_rate = context_rate(rate);
                let mut carry = self.renderer_carry.lock().unwrap();
                let exact = frames_out as f64 * renderer_rate as f64 / rate as f64 + *carry;
                let frames = exact.floor() as usize;
                *carry = exact - frames as f64;
                let mut left = vec![0f32; frames];
                let mut right = vec![0f32; frames];
                Self::pull(st, frames, renderer_rate, |i, l, r| {
                    left[i] = l;
                    right[i] = r;
                });
                drop(guard);
                let mut renderer = renderer.lock().unwrap();
                renderer.render(Box::new(left), 1);
                renderer.render(Box::new(right), 2);
            },
        }
    }
}

enum Cmd {
    Open(Arc<Vec<u8>>),
    Play,
    Pause,
    Seek(f64),
    Quit,
}

pub struct NativePlayer {
    id: usize,
    shared: Arc<PlayerShared>,
    cmd: Mutex<Sender<Cmd>>,
    data: Mutex<Vec<u8>>,
    input_size: Mutex<Option<u64>>,
    opened: Mutex<bool>,
    /// The user's intent (play/pause), as opposed to `PlayState::playing`, which also waits for data.
    want_play: Mutex<bool>,
}

impl NativePlayer {
    pub fn new(
        id: usize,
        sender: GenericCallback<PlayerEvent>,
        renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
    ) -> Self {
        let shared = Arc::new(PlayerShared {
            state: Mutex::new(PlayState {
                fifo: FrameQueue::new(),
                src_rate: 0,
                playing: false,
                suspended: false,
                volume: 1.,
                muted: false,
                context_muted: false,
                playback_rate: 1.,
                base_time: 0.,
                consumed: 0,
                duration: None,
            }),
            renderer,
            renderer_carry: Mutex::new(0.),
        });
        let source: Arc<dyn OutputSource> = shared.clone();
        output().add_source(Arc::downgrade(&source) as Weak<dyn OutputSource>);
        let (tx, rx) = mpsc::channel();
        let shared_ = shared.clone();
        thread::Builder::new()
            .name("PartiiAudioPlayer".to_owned())
            .spawn(move || PlayerThread::new(shared_, sender).run(rx))
            .expect("Failed to spawn the player thread");
        Self {
            id,
            shared,
            cmd: Mutex::new(tx),
            data: Mutex::new(Vec::new()),
            input_size: Mutex::new(None),
            opened: Mutex::new(false),
            want_play: Mutex::new(false),
        }
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.cmd.lock().unwrap().send(cmd);
    }

    fn open_if_complete(&self, force: bool) {
        let mut opened = self.opened.lock().unwrap();
        if *opened {
            return;
        }
        let data = self.data.lock().unwrap();
        let complete = force
            || self
                .input_size
                .lock()
                .unwrap()
                .is_some_and(|size| size > 0 && data.len() as u64 >= size);
        if complete && !data.is_empty() {
            *opened = true;
            self.send(Cmd::Open(Arc::new(data.clone())));
        }
    }
}

impl Drop for NativePlayer {
    fn drop(&mut self) {
        self.send(Cmd::Quit);
    }
}

impl Player for NativePlayer {
    fn play(&self) -> Result<(), PlayerError> {
        *self.want_play.lock().unwrap() = true;
        self.send(Cmd::Play);
        Ok(())
    }

    fn pause(&self) -> Result<(), PlayerError> {
        *self.want_play.lock().unwrap() = false;
        self.send(Cmd::Pause);
        Ok(())
    }

    fn paused(&self) -> bool {
        !*self.want_play.lock().unwrap()
    }

    fn can_resume(&self) -> bool {
        true
    }

    fn stop(&self) -> Result<(), PlayerError> {
        self.pause()
    }

    fn seek(&self, time: f64) -> Result<(), PlayerError> {
        self.send(Cmd::Seek(time.max(0.)));
        Ok(())
    }

    fn seekable(&self) -> Vec<Range<f64>> {
        // Everything is in memory once loaded; the DOM only needs a non-empty range.
        vec![0.0..f64::MAX]
    }

    fn set_mute(&self, muted: bool) -> Result<(), PlayerError> {
        self.shared.state.lock().unwrap().muted = muted;
        Ok(())
    }

    fn muted(&self) -> bool {
        self.shared.state.lock().unwrap().muted
    }

    fn set_volume(&self, volume: f64) -> Result<(), PlayerError> {
        self.shared.state.lock().unwrap().volume = volume.clamp(0., 1.) as f32;
        Ok(())
    }

    fn volume(&self) -> f64 {
        self.shared.state.lock().unwrap().volume as f64
    }

    fn set_input_size(&self, size: u64) -> Result<(), PlayerError> {
        *self.input_size.lock().unwrap() = Some(size);
        self.open_if_complete(false);
        Ok(())
    }

    fn set_seekable(&self, _: bool) -> Result<(), PlayerError> {
        Ok(())
    }

    fn set_download_buffering_enabled(&self, _: bool) -> Result<(), PlayerError> {
        Ok(())
    }

    fn set_playback_rate(&self, playback_rate: f64) -> Result<(), PlayerError> {
        self.shared.state.lock().unwrap().playback_rate = playback_rate.clamp(0., 16.);
        Ok(())
    }

    fn playback_rate(&self) -> f64 {
        self.shared.state.lock().unwrap().playback_rate
    }

    fn push_data(&self, data: Vec<u8>) -> Result<(), PlayerError> {
        if *self.opened.lock().unwrap() {
            // A refetch after we already have the whole file (e.g. a DOM-side seek refetch).
            return Ok(());
        }
        self.data.lock().unwrap().extend_from_slice(&data);
        self.open_if_complete(false);
        Ok(())
    }

    fn end_of_stream(&self) -> Result<(), PlayerError> {
        self.open_if_complete(true);
        Ok(())
    }

    fn buffered(&self) -> Vec<Range<f64>> {
        match self.shared.state.lock().unwrap().duration {
            Some(d) => vec![0.0..d],
            None => vec![],
        }
    }

    fn set_stream(&self, _: &MediaStreamId, _: bool) -> Result<(), PlayerError> {
        Err(PlayerError::SetStreamFailed)
    }

    fn render_use_gl(&self) -> bool {
        false
    }

    fn set_audio_track(&self, _: i32, _: bool) -> Result<(), PlayerError> {
        Ok(())
    }

    fn set_video_track(&self, _: i32, _: bool) -> Result<(), PlayerError> {
        Ok(())
    }
}

impl MediaInstance for NativePlayer {
    fn get_id(&self) -> usize {
        self.id
    }

    fn mute(&self, val: bool) -> Result<(), MediaInstanceError> {
        self.shared.state.lock().unwrap().context_muted = val;
        Ok(())
    }

    fn suspend(&self) -> Result<(), MediaInstanceError> {
        self.shared.state.lock().unwrap().suspended = true;
        Ok(())
    }

    fn resume(&self) -> Result<(), MediaInstanceError> {
        self.shared.state.lock().unwrap().suspended = false;
        Ok(())
    }
}

/// An in-memory media source that symphonia can seek in, sharing the bytes between reopenings.
struct SharedBytes(Cursor<Arc<Vec<u8>>>);

impl Read for SharedBytes {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let pos = self.0.position() as usize;
        let data = self.0.get_ref();
        let n = buf.len().min(data.len().saturating_sub(pos));
        buf[..n].copy_from_slice(&data[pos..pos + n]);
        self.0.set_position((pos + n) as u64);
        Ok(n)
    }
}

impl Seek for SharedBytes {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let len = self.0.get_ref().len() as i64;
        let new = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(o) => len + o,
            SeekFrom::Current(o) => self.0.position() as i64 + o,
        };
        if new < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.0.set_position(new as u64);
        Ok(new as u64)
    }
}

impl MediaSource for SharedBytes {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.0.get_ref().len() as u64)
    }
}

struct Stream {
    bytes: Arc<Vec<u8>>,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    rate: u32,
    sample_buf: Option<SampleBuffer<f32>>,
    sample_buf_key: Option<(u64, symphonia::core::audio::SignalSpec)>,
    /// Drop decoded frames before this timestamp (accurate seeking).
    skip_until_ts: u64,
    /// The decoder reached the end of the stream.
    finished: bool,
}

fn open_stream(bytes: Arc<Vec<u8>>) -> Result<(Stream, Option<f64>), String> {
    let source = SharedBytes(Cursor::new(bytes.clone()));
    let mss = MediaSourceStream::new(Box::new(source), MediaSourceStreamOptions::default());
    let probed = symphonia::default::get_probe()
        .format(
            &Hint::new(),
            mss,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("unsupported media: {e}"))?;
    let format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| {
            t.codec_params.codec != CODEC_TYPE_NULL
                && crate::codecs::codecs()
                    .get_codec(t.codec_params.codec)
                    .is_some()
        })
        .ok_or_else(|| "no decodable audio track".to_owned())?;
    let decoder = crate::codecs::codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("no decoder: {e}"))?;
    let rate = track.codec_params.sample_rate.unwrap_or(0);
    let duration = match (track.codec_params.n_frames, track.codec_params.time_base) {
        (Some(n), Some(tb)) => {
            let t = tb.calc_time(n);
            Some(t.seconds as f64 + t.frac)
        },
        (Some(n), None) if rate > 0 => Some(n as f64 / rate as f64),
        _ => None,
    };
    let track_id = track.id;
    Ok((
        Stream {
            bytes,
            format,
            decoder,
            track_id,
            rate,
            sample_buf: None,
            sample_buf_key: None,
            skip_until_ts: 0,
            finished: false,
        },
        duration,
    ))
}

impl Stream {
    /// Decode the next packet into stereo frames. `None` at the end of the stream.
    fn next_frames(&mut self, out: &mut Vec<[f32; 2]>) -> Option<()> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::ResetRequired) => {
                    // A chained stream changed parameters; rebuild the decoder.
                    let track = self.format.default_track()?;
                    self.track_id = track.id;
                    self.decoder = crate::codecs::codecs()
                        .make(&track.codec_params, &DecoderOptions::default())
                        .ok()?;
                    continue;
                },
                Err(_) => {
                    self.finished = true;
                    return None;
                },
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let ts = packet.ts();
            let audio = match self.decoder.decode(&packet) {
                Ok(a) => a,
                Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => continue,
                Err(_) => {
                    self.finished = true;
                    return None;
                },
            };
            let spec = *audio.spec();
            let n_chans = spec.channels.count();
            let frames = audio.frames();
            if n_chans == 0 || frames == 0 {
                continue;
            }
            self.rate = spec.rate;
            let cap = audio.capacity() as u64;
            if self.sample_buf_key != Some((cap, spec)) {
                self.sample_buf = Some(SampleBuffer::new(cap, spec));
                self.sample_buf_key = Some((cap, spec));
            }
            let buf = self.sample_buf.as_mut().unwrap();
            buf.copy_interleaved_ref(audio);
            let samples = buf.samples();
            let skip = self.skip_until_ts.saturating_sub(ts).min(frames as u64) as usize;
            if skip >= frames {
                continue;
            }
            self.skip_until_ts = 0;
            out.extend(samples.chunks_exact(n_chans).skip(skip).map(|f| {
                if n_chans == 1 {
                    [f[0], f[0]]
                } else {
                    [f[0], f[1]]
                }
            }));
            return Some(());
        }
    }

    /// Seek to `time` seconds. Returns false if the format can't seek (the caller reopens).
    fn seek(&mut self, time: f64) -> bool {
        let result = self.format.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: Time::from(time),
                track_id: Some(self.track_id),
            },
        );
        match result {
            Ok(seeked) => {
                self.decoder.reset();
                self.skip_until_ts = seeked.required_ts;
                self.finished = false;
                true
            },
            Err(e) => {
                warn!("player: seek to {time} failed ({e}); decoding from the start");
                false
            },
        }
    }
}

struct PlayerThread {
    shared: Arc<PlayerShared>,
    events: GenericCallback<PlayerEvent>,
    stream: Option<Stream>,
    duration: Option<f64>,
    want_play: bool,
    ended_sent: bool,
    last_position: Instant,
    scratch: Vec<[f32; 2]>,
}

impl PlayerThread {
    fn new(shared: Arc<PlayerShared>, events: GenericCallback<PlayerEvent>) -> Self {
        Self {
            shared,
            events,
            stream: None,
            duration: None,
            want_play: false,
            ended_sent: false,
            last_position: Instant::now(),
            scratch: Vec::new(),
        }
    }

    fn set_duration(&mut self, duration: Option<f64>) {
        self.duration = duration;
        self.shared.state.lock().unwrap().duration = duration;
    }

    fn emit(&self, event: PlayerEvent) {
        log::debug!("player event: {event:?}");
        if let Err(e) = self.events.send(event) {
            warn!("player: could not send event: {e:?}");
        }
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        // The DOM holds the fetched bytes back until the player asks for data.
        self.emit(PlayerEvent::NeedData);
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Cmd::Quit) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {},
            }
            self.fill();
            self.report();
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        if log::log_enabled!(log::Level::Debug) {
            let name = match &cmd {
                Cmd::Open(b) => format!("open ({} bytes)", b.len()),
                Cmd::Play => "play".to_owned(),
                Cmd::Pause => "pause".to_owned(),
                Cmd::Seek(t) => format!("seek {t}"),
                Cmd::Quit => "quit".to_owned(),
            };
            log::debug!("player command: {name}");
        }
        match cmd {
            Cmd::Open(bytes) => self.open(bytes),
            Cmd::Play => {
                self.want_play = true;
                if self.stream.is_some() {
                    self.set_playing(true);
                }
            },
            Cmd::Pause => {
                self.want_play = false;
                if self.stream.is_some() {
                    self.set_playing(false);
                }
            },
            Cmd::Seek(time) => self.seek(time),
            Cmd::Quit => {},
        }
    }

    fn set_playing(&mut self, playing: bool) {
        let changed = {
            let mut st = self.shared.state.lock().unwrap();
            let changed = st.playing != playing;
            st.playing = playing;
            changed
        };
        if changed {
            self.emit(PlayerEvent::StateChanged(if playing {
                PlaybackState::Playing
            } else {
                PlaybackState::Paused
            }));
        }
    }

    fn open(&mut self, bytes: Arc<Vec<u8>>) {
        let (stream, duration) = match open_stream(bytes) {
            Ok(s) => s,
            Err(e) => {
                self.emit(PlayerEvent::Error(e));
                return;
            },
        };
        {
            let mut st = self.shared.state.lock().unwrap();
            st.src_rate = stream.rate;
            st.fifo.clear();
            st.base_time = 0.;
            st.consumed = 0;
        }
        self.stream = Some(stream);
        self.set_duration(duration);
        let metadata = Metadata {
            duration: duration.map(Duration::from_secs_f64),
            width: 0,
            height: 0,
            format: String::new(),
            is_seekable: true,
            video_tracks: vec![],
            audio_tracks: vec!["audio".to_owned()],
            is_live: false,
            title: None,
        };
        self.emit(PlayerEvent::MetadataUpdated(metadata));
        if let Some(d) = duration {
            self.emit(PlayerEvent::DurationChanged(Some(Duration::from_secs_f64(
                d,
            ))));
        }
        // Preroll: decode a little before reporting we can play.
        self.fill();
        if self.want_play {
            self.set_playing(true);
        } else {
            self.emit(PlayerEvent::StateChanged(PlaybackState::Paused));
        }
    }

    fn seek(&mut self, time: f64) {
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        let time = match self.duration {
            Some(d) => time.min(d),
            None => time,
        };
        if !stream.seek(time) {
            // Reopen and skip forward by decoding.
            let bytes = stream.bytes.clone();
            match open_stream(bytes) {
                Ok((mut s, _)) => {
                    let rate = s.rate.max(1) as u64;
                    s.skip_until_ts = (time * rate as f64) as u64;
                    *stream = s;
                },
                Err(e) => {
                    self.emit(PlayerEvent::Error(e));
                    return;
                },
            }
        }
        {
            let mut st = self.shared.state.lock().unwrap();
            st.fifo.clear();
            st.base_time = time;
            st.consumed = 0;
        }
        self.ended_sent = false;
        self.fill();
        self.emit(PlayerEvent::SeekDone(time));
        self.emit(PlayerEvent::PositionChanged(time));
        self.last_position = Instant::now();
    }

    fn fill(&mut self) {
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        if stream.finished {
            return;
        }
        let rate = stream.rate.max(1) as f64;
        let level = self.shared.state.lock().unwrap().fifo.len();
        if (level as f64) >= LOW_WATER_SECS * rate {
            return;
        }
        let target = (HIGH_WATER_SECS * rate) as usize;
        self.scratch.clear();
        while level + self.scratch.len() < target {
            if stream.next_frames(&mut self.scratch).is_none() {
                break;
            }
        }
        let mut st = self.shared.state.lock().unwrap();
        st.src_rate = stream.rate;
        st.fifo.extend(self.scratch.drain(..));
        if stream.finished {
            // Let the resampler play out the last frames.
            st.fifo.flush();
        }
    }

    fn report(&mut self) {
        let Some(stream) = self.stream.as_ref() else {
            return;
        };
        let (playing, position, empty) = {
            let st = self.shared.state.lock().unwrap();
            (
                st.playing && !st.suspended,
                st.position(),
                st.fifo.is_empty(),
            )
        };
        if !playing {
            return;
        }
        if stream.finished && empty {
            if !self.ended_sent {
                self.ended_sent = true;
                // Make the final position the duration, so the DOM sees playback reach the end.
                let end = match self.duration {
                    Some(d) if (d - position).abs() < 0.5 => d,
                    _ => {
                        self.set_duration(Some(position));
                        self.emit(PlayerEvent::DurationChanged(Some(Duration::from_secs_f64(
                            position,
                        ))));
                        position
                    },
                };
                self.emit(PlayerEvent::PositionChanged(end));
                self.emit(PlayerEvent::EndOfStream);
            }
            return;
        }
        if self.last_position.elapsed() >= POSITION_INTERVAL {
            self.last_position = Instant::now();
            self.emit(PlayerEvent::PositionChanged(position));
        }
    }
}
