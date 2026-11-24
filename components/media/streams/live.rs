/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Live media: the backend-agnostic registry that every producer and consumer of live audio and
//! video goes through (Partii, stream voice; the interface is in the runtime repo's
//! docs/handover/voice/STATUS.md, "Live media stream registry").
//!
//! A *source* produces media: a microphone, a camera, a remote WebRTC track, a Web Audio
//! MediaStreamAudioDestinationNode, a canvas. A *track* is a [`MediaStreamId`] (one per DOM
//! `MediaStreamTrack`) that views one source. `MediaStreamTrack.clone()` is [`clone_track`]: a new
//! id on the same source with its own `enabled` and ended state.
//!
//! Audio: a producer [`AudioSource::push`]es interleaved f32 PCM in its own format, from any thread
//! (a real-time device callback included). Every reader has its own bounded ring; a full ring loses
//! its oldest samples. Producers are never slowed down by readers.
//!
//! Video is live_video.rs (the clips stream's file): latest-frame readers, no queue. The track
//! functions here (clone, stop, unregister, enabled, info) serve both kinds: video ids go there.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::MediaStreamType;
use crate::live_video;
use crate::registry::MediaStreamId;

/// How long a reader's ring holds before it loses its oldest samples.
pub const READER_RING: Duration = Duration::from_millis(200);

/// The format of an audio source, fixed when it's made. Producers don't resample; readers convert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// What a track is, for the DOM (`kind`, `label`, `enabled`, `muted`, `readyState`).
#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub kind: MediaStreamType,
    pub label: String,
    pub enabled: bool,
    pub muted: bool,
    pub ended: bool,
    /// Audio tracks only.
    pub format: Option<AudioFormat>,
    /// Video tracks only (live_video): the last frame's size (0 before the first) and the nominal rate.
    pub width: u32,
    pub height: u32,
    pub frame_rate: f64,
}

/// Events the DOM turns into `mute`, `unmute` and `ended` on a track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackEvent {
    Muted,
    Unmuted,
    Ended,
}

/// A reader's view of its track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadState {
    Live,
    /// The source is muted (no data is coming for now); reads return silence or nothing new.
    Muted,
    /// The track or its source ended: nothing more will come.
    Ended,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReaderStats {
    /// Samples (per channel: frames) lost because the ring was full.
    pub dropped_frames: u64,
    /// Reads that found less than asked for while live.
    pub underruns: u64,
}

type Watcher = Box<dyn Fn(TrackEvent) + Send + Sync>;

struct Source {
    kind: MediaStreamType,
    label: String,
    format: Option<AudioFormat>,
    muted: AtomicBool,
    ended: AtomicBool,
    /// Live (not stopped) tracks of this source.
    tracks: AtomicUsize,
    audio_readers: Mutex<Vec<Arc<AudioRing>>>,
    on_unused: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Track watchers, by track: told about the source's mute and end.
    watchers: Mutex<Vec<(MediaStreamId, Arc<Watcher>)>>,
}

struct Track {
    source: Arc<Source>,
    enabled: AtomicBool,
    ended: AtomicBool,
}

impl Track {
    fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire) || self.source.ended.load(Ordering::Acquire)
    }
}

static TRACKS: LazyLock<Mutex<HashMap<MediaStreamId, Arc<Track>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn track(id: &MediaStreamId) -> Option<Arc<Track>> {
    TRACKS.lock().unwrap().get(id).cloned()
}

fn new_source(kind: MediaStreamType, label: &str, format: Option<AudioFormat>) -> Arc<Source> {
    Arc::new(Source {
        kind,
        label: label.to_owned(),
        format,
        muted: AtomicBool::new(false),
        ended: AtomicBool::new(false),
        tracks: AtomicUsize::new(0),
        audio_readers: Mutex::new(Vec::new()),
        on_unused: Mutex::new(None),
        watchers: Mutex::new(Vec::new()),
    })
}

fn add_track(source: &Arc<Source>) -> MediaStreamId {
    let id = MediaStreamId::new();
    source.tracks.fetch_add(1, Ordering::AcqRel);
    TRACKS.lock().unwrap().insert(
        id,
        Arc::new(Track {
            source: source.clone(),
            enabled: AtomicBool::new(true),
            ended: AtomicBool::new(false),
        }),
    );
    id
}

fn notify(source: &Source, only: Option<&MediaStreamId>, event: TrackEvent) {
    let watchers: Vec<_> = source
        .watchers
        .lock()
        .unwrap()
        .iter()
        .filter(|(id, _)| only.is_none_or(|o| o == id))
        .map(|(_, w)| w.clone())
        .collect();
    for w in watchers {
        w(event);
    }
}

fn end_source(source: &Source) {
    if source.ended.swap(true, Ordering::AcqRel) {
        return;
    }
    notify(source, None, TrackEvent::Ended);
}

fn set_source_muted(source: &Source, muted: bool) {
    if source.muted.swap(muted, Ordering::AcqRel) != muted && !source.ended.load(Ordering::Acquire)
    {
        notify(
            source,
            None,
            if muted {
                TrackEvent::Muted
            } else {
                TrackEvent::Unmuted
            },
        );
    }
}

fn set_on_unused(source: &Source, f: Box<dyn Fn() + Send + Sync>) {
    *source.on_unused.lock().unwrap() = Some(Arc::from(f));
}

/// Is `id` a live-media track (made through this module)?
pub fn is_live(id: &MediaStreamId) -> bool {
    live_video::is_video_track(id) || TRACKS.lock().unwrap().contains_key(id)
}

/// `MediaStreamTrack.clone()`: a new track on the same source (enabled, not ended).
pub fn clone_track(id: &MediaStreamId) -> Option<MediaStreamId> {
    if live_video::is_video_track(id) {
        return live_video::clone_video_track(id);
    }
    let t = track(id)?;
    if t.ended() {
        // A clone of an ended track is ended too.
        let new = add_track(&t.source);
        stop_track(&new);
        return Some(new);
    }
    Some(add_track(&t.source))
}

/// `MediaStreamTrack.stop()`: the track ends. When its source has no live track left, the source's
/// `on_unused` callback runs (a microphone closes its device there).
pub fn stop_track(id: &MediaStreamId) {
    if live_video::is_video_track(id) {
        return live_video::stop_video_track(id);
    }
    let Some(t) = track(id) else { return };
    if t.ended.swap(true, Ordering::AcqRel) {
        return;
    }
    if t.source.tracks.fetch_sub(1, Ordering::AcqRel) == 1 {
        let f = t.source.on_unused.lock().unwrap().clone();
        if let Some(f) = f {
            f();
        }
    }
}

/// Forget a track (its DOM object is gone). Stops it first.
pub fn unregister_track(id: &MediaStreamId) {
    if live_video::is_video_track(id) {
        return live_video::unregister_video_track(id);
    }
    stop_track(id);
    if let Some(t) = TRACKS.lock().unwrap().remove(id) {
        t.source.watchers.lock().unwrap().retain(|(w, _)| w != id);
    }
}

/// `MediaStreamTrack.enabled`: a disabled track's readers get silence (audio) or black (video);
/// the source and its other tracks are untouched.
pub fn set_enabled(id: &MediaStreamId, enabled: bool) {
    if live_video::is_video_track(id) {
        return live_video::set_video_enabled(id, enabled);
    }
    if let Some(t) = track(id) {
        t.enabled.store(enabled, Ordering::Release);
    }
}

pub fn track_info(id: &MediaStreamId) -> Option<TrackInfo> {
    if live_video::is_video_track(id) {
        let v = live_video::video_track_info(id)?;
        return Some(TrackInfo {
            kind: MediaStreamType::Video,
            label: v.label,
            enabled: v.enabled,
            muted: v.muted,
            ended: v.ended,
            format: None,
            width: v.width,
            height: v.height,
            frame_rate: v.frame_rate,
        });
    }
    let t = track(id)?;
    let s = &t.source;
    Some(TrackInfo {
        kind: s.kind,
        label: s.label.clone(),
        enabled: t.enabled.load(Ordering::Acquire),
        muted: s.muted.load(Ordering::Acquire),
        ended: t.ended(),
        format: s.format,
        width: 0,
        height: 0,
        frame_rate: 0.0,
    })
}

/// Tell `f` about this track's mute, unmute and end (the source's, or its own stop). Called on the
/// producer's thread: forward to the script thread. One watcher per track; a new one replaces it.
/// Audio tracks only for now (live_video has no watcher yet).
pub fn watch_track(id: &MediaStreamId, f: Box<dyn Fn(TrackEvent) + Send + Sync>) {
    let Some(t) = track(id) else { return };
    let mut w = t.source.watchers.lock().unwrap();
    w.retain(|(x, _)| x != id);
    w.push((*id, Arc::new(f)));
}

// ---------- audio ----------

/// One reader's ring: single producer (the source's pushes, serialized by the readers lock), single
/// consumer (the reader). Samples are stored as f32 bits in atomics, so a reader overtaken by the
/// producer reads glitched audio, never undefined memory.
struct AudioRing {
    buf: Box<[AtomicU32]>,
    /// Samples written (monotonic).
    write: AtomicU64,
    /// Set by the producer when it wrote past the reader: the reader skips ahead.
    lost: AtomicU64,
    track: Arc<Track>,
}

impl AudioRing {
    fn cap(&self) -> u64 {
        self.buf.len() as u64
    }
}

/// The producer's handle of an audio source. Dropping the last clone ends the source.
#[derive(Clone)]
pub struct AudioSource {
    source: Arc<Source>,
    _end: Arc<EndOnDrop>,
}

struct EndOnDrop(Arc<Source>);
impl Drop for EndOnDrop {
    fn drop(&mut self) {
        end_source(&self.0);
    }
}

/// Make an audio source and its first track (the id the DOM wraps in a `MediaStreamTrack`).
pub fn create_audio_source(format: AudioFormat, label: &str) -> (MediaStreamId, AudioSource) {
    let source = new_source(MediaStreamType::Audio, label, Some(format));
    let id = add_track(&source);
    (
        id,
        AudioSource {
            _end: Arc::new(EndOnDrop(source.clone())),
            source,
        },
    )
}

impl AudioSource {
    pub fn format(&self) -> AudioFormat {
        self.source.format.unwrap()
    }

    /// Push interleaved samples in the source's format (any number of frames; 10 ms recommended,
    /// at most 100 ms per call). `_frames_at` is when the first frame was captured. Never blocks
    /// on a reader, never allocates.
    pub fn push(&self, interleaved: &[f32], _frames_at: Instant) {
        if self.source.ended.load(Ordering::Acquire) {
            return;
        }
        let readers = self.source.audio_readers.lock().unwrap();
        for r in readers.iter() {
            let cap = r.cap();
            let w = r.write.load(Ordering::Relaxed);
            for (i, v) in interleaved.iter().enumerate() {
                r.buf[((w + i as u64) % cap) as usize].store(v.to_bits(), Ordering::Relaxed);
            }
            r.write
                .store(w + interleaved.len() as u64, Ordering::Release);
        }
        drop(readers);
        // A push means data is flowing again.
        if self.source.muted.load(Ordering::Relaxed) {
            set_source_muted(&self.source, false);
        }
    }

    /// Does anything read this source now?
    pub fn has_readers(&self) -> bool {
        !self.source.audio_readers.lock().unwrap().is_empty()
    }

    /// Called when the source's last track stops (`track.stop()` on every track).
    pub fn on_unused(&self, f: Box<dyn Fn() + Send + Sync>) {
        set_on_unused(&self.source, f);
    }

    /// The source is muted (the system paused it; a remote track has no packets): `mute`/`unmute`.
    pub fn set_muted(&self, muted: bool) {
        set_source_muted(&self.source, muted);
    }

    /// The source is gone for good (device lost, remote track stopped): every track ends.
    pub fn end(&self) {
        end_source(&self.source);
    }

    /// A new track on this source (e.g. the remote track a second `ontrack` would carry).
    pub fn new_track(&self) -> MediaStreamId {
        add_track(&self.source)
    }
}

/// A reader of one audio track, starting at "now" (no backlog). Drop it to stop reading.
pub struct AudioReader {
    ring: Arc<AudioRing>,
    read: u64,
    format: AudioFormat,
    stats: ReaderStats,
}

/// Read one audio track. `None` if `id` isn't a live audio track.
pub fn subscribe_audio(id: &MediaStreamId) -> Option<AudioReader> {
    let t = track(id)?;
    let format = t.source.format?;
    let cap = (READER_RING.as_secs_f64() * format.sample_rate as f64).ceil() as usize
        * format.channels.max(1) as usize;
    let source = t.source.clone();
    let mut readers = source.audio_readers.lock().unwrap();
    let ring = Arc::new(AudioRing {
        buf: (0..cap.max(1)).map(|_| AtomicU32::new(0)).collect(),
        write: AtomicU64::new(0),
        lost: AtomicU64::new(0),
        track: t,
    });
    readers.push(ring.clone());
    Some(AudioReader {
        ring,
        read: 0,
        format,
        stats: ReaderStats::default(),
    })
}

impl AudioReader {
    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Interleaved samples ready to read (a multiple of the channel count).
    pub fn buffered(&self) -> usize {
        let w = self.ring.write.load(Ordering::Acquire);
        (w - self.read).min(self.ring.cap()) as usize
    }

    /// Frames ready to read.
    pub fn buffered_frames(&self) -> usize {
        self.buffered() / self.format.channels.max(1) as usize
    }

    /// Copy up to `out.len()` interleaved samples (whole frames); returns how many were written.
    /// Non-blocking. A disabled track reads zeros at the same pace.
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        let ch = self.format.channels.max(1) as usize;
        let cap = self.ring.cap();
        let w = self.ring.write.load(Ordering::Acquire);
        if w - self.read > cap {
            // Overtaken: keep the newest half ring.
            let skip_to = w - cap / 2 / ch as u64 * ch as u64;
            self.stats.dropped_frames += (skip_to - self.read) / ch as u64;
            self.ring
                .lost
                .fetch_add(skip_to - self.read, Ordering::Relaxed);
            self.read = skip_to;
        }
        let avail = (w - self.read) as usize;
        let n = avail.min(out.len()) / ch * ch;
        if n < out.len() / ch * ch && self.state() == ReadState::Live {
            self.stats.underruns += 1;
        }
        let enabled = self.ring.track.enabled.load(Ordering::Acquire);
        for (i, o) in out[..n].iter_mut().enumerate() {
            *o = if enabled {
                f32::from_bits(
                    self.ring.buf[((self.read + i as u64) % cap) as usize].load(Ordering::Relaxed),
                )
            } else {
                0.0
            };
        }
        self.read += n as u64;
        n
    }

    /// Throw away buffered samples above `keep` frames (a live reader trimming latency).
    pub fn trim_to(&mut self, keep_frames: usize) {
        let ch = self.format.channels.max(1) as u64;
        let w = self.ring.write.load(Ordering::Acquire);
        let keep = keep_frames as u64 * ch;
        if w - self.read > keep {
            let to = w - keep;
            self.stats.dropped_frames += (to - self.read) / ch;
            self.read = to;
        }
    }

    pub fn state(&self) -> ReadState {
        let t = &self.ring.track;
        if t.ended() {
            ReadState::Ended
        } else if t.source.muted.load(Ordering::Acquire) {
            ReadState::Muted
        } else {
            ReadState::Live
        }
    }

    /// Is the track enabled (`track.enabled`)? Disabled tracks read zeros.
    pub fn enabled(&self) -> bool {
        self.ring.track.enabled.load(Ordering::Acquire)
    }

    pub fn stats(&self) -> ReaderStats {
        self.stats
    }
}

impl Drop for AudioReader {
    fn drop(&mut self) {
        let mut readers = self.ring.track.source.audio_readers.lock().unwrap();
        readers.retain(|r| !Arc::ptr_eq(r, &self.ring));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_flows_to_each_reader_and_ends() {
        let (id, src) = create_audio_source(
            AudioFormat {
                sample_rate: 48000,
                channels: 1,
            },
            "t",
        );
        let mut r = subscribe_audio(&id).unwrap();
        let c = clone_track(&id).unwrap();
        let mut rc = subscribe_audio(&c).unwrap();
        src.push(&[0.5; 480], Instant::now());
        let mut out = [0.0; 960];
        assert_eq!(r.read(&mut out), 480);
        assert_eq!(out[0], 0.5);
        set_enabled(&c, false);
        assert_eq!(rc.read(&mut out), 480);
        assert_eq!(out[0], 0.0);
        let unused = Arc::new(AtomicBool::new(false));
        let u = unused.clone();
        src.on_unused(Box::new(move || u.store(true, Ordering::SeqCst)));
        stop_track(&id);
        assert!(!unused.load(Ordering::SeqCst));
        assert_eq!(r.state(), ReadState::Ended);
        stop_track(&c);
        assert!(unused.load(Ordering::SeqCst));
        drop(src);
        assert!(track_info(&id).unwrap().ended);
    }

    #[test]
    fn a_full_ring_loses_its_oldest() {
        let (id, src) = create_audio_source(
            AudioFormat {
                sample_rate: 1000,
                channels: 1,
            },
            "t",
        );
        let mut r = subscribe_audio(&id).unwrap();
        for i in 0..10 {
            src.push(&[i as f32; 100], Instant::now()); // 1 s into a 200 ms ring
        }
        let mut out = [0.0; 1000];
        let n = r.read(&mut out);
        assert!(n > 0 && n <= 200);
        assert_eq!(out[n - 1], 9.0);
        assert!(r.stats().dropped_frames >= 800);
    }
}
