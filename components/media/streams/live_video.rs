/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii: the video half of the live media registry (the audio half and the shared track model are
//! `live.rs`, the voice stream's; the interface is in the runtime repo's docs/handover/voice/STATUS.md,
//! "Live media stream registry").
//!
//! A **source** produces frames (a canvas's `captureStream`, a camera, a remote WebRTC track). A **track** is a
//! `MediaStreamId` viewing one source (one per DOM `MediaStreamTrack`; `clone_video_track` adds another on
//! the same source, with its own enabled and ended state). Readers get **latest-frame** semantics: a slow
//! reader skips frames, nothing queues, and a producer is never slowed down by a reader.
//!
//! Threads: everything here is `Send + Sync`; `push` takes one short uncontended lock and never waits on a
//! reader. `VideoReader::wait_newer` blocks its own thread only.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::live::TrackEvent;
use crate::registry::MediaStreamId;

/// The pixel layout of a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoFormat {
    /// Three planes: Y (full size), U and V (half width, half height).
    I420,
    /// Two planes: Y, then interleaved UV (half width, half height).
    Nv12,
    /// One plane, 4 bytes per pixel in R, G, B, A order. Alpha is ignored by video readers (frames are
    /// opaque); premultiplied pixels therefore read as composited over black.
    Rgba8,
    /// One plane, 4 bytes per pixel in B, G, R, A order (what a 2D canvas usually holds).
    Bgra8,
}

/// One frame. Cheap to clone: the pixels are shared, not copied.
#[derive(Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub format: VideoFormat,
    /// Every plane, back to back, starting at `offsets[i]`.
    pub planes: Arc<[u8]>,
    /// Bytes per row of each plane (unused planes 0).
    pub strides: [u32; 3],
    /// Where each plane starts in `planes` (unused planes 0).
    pub offsets: [usize; 3],
    /// When the frame was captured.
    pub timestamp: Instant,
    /// The source's frame number, from 1, set by `VideoSource::push`. A reader compares it to tell a new frame
    /// from one it has seen.
    pub seq: u64,
}

impl VideoFrame {
    /// A one-plane packed frame (Rgba8 or Bgra8), `stride` bytes per row.
    pub fn packed(
        width: u32,
        height: u32,
        format: VideoFormat,
        stride: u32,
        pixels: Arc<[u8]>,
        timestamp: Instant,
    ) -> VideoFrame {
        VideoFrame {
            width,
            height,
            format,
            planes: pixels,
            strides: [stride, 0, 0],
            offsets: [0, 0, 0],
            timestamp,
            seq: 0,
        }
    }

    /// A black frame of this size (what a disabled track reads).
    fn black_like(&self) -> VideoFrame {
        let (w, h) = (self.width as usize, self.height as usize);
        let pixels: Arc<[u8]> = vec![0u8; w * h * 4].into();
        VideoFrame {
            seq: self.seq,
            ..VideoFrame::packed(
                self.width,
                self.height,
                VideoFormat::Rgba8,
                self.width * 4,
                pixels,
                self.timestamp,
            )
        }
    }
}

/// A reader's view of its track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoState {
    Live,
    Muted,
    Ended,
}

/// What the DOM reads about a video track.
#[derive(Clone, Debug)]
pub struct VideoTrackInfo {
    pub label: String,
    pub enabled: bool,
    pub muted: bool,
    pub ended: bool,
    /// The last frame's size (0 before the first frame).
    pub width: u32,
    pub height: u32,
    /// The source's nominal frame rate (0 when it has none).
    pub frame_rate: f64,
}

type UnusedCallback = Box<dyn Fn() + Send + Sync>;
type Watcher = Arc<dyn Fn(TrackEvent) + Send + Sync>;

struct Source {
    label: String,
    latest: Mutex<Option<VideoFrame>>,
    newer: Condvar,
    seq: AtomicU64,
    ended: AtomicBool,
    muted: AtomicBool,
    readers: AtomicUsize,
    live_tracks: AtomicUsize,
    frame_rate: Mutex<f64>,
    on_unused: Mutex<Option<UnusedCallback>>,
    watchers: Mutex<Vec<(MediaStreamId, Watcher)>>,
}

impl Source {
    /// Calls the watchers (of `only`, or every track), outside the lock.
    fn notify(&self, only: Option<&MediaStreamId>, event: TrackEvent) {
        let watchers: Vec<Watcher> = self
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

    fn end(&self) {
        if self.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        // Drop the last frame (tracks outlive their source in the map until the DOM collects them), under
        // the lock, so a reader between its check and its wait still sees the notification.
        *self.latest.lock().unwrap() = None;
        self.newer.notify_all();
        self.notify(None, TrackEvent::Ended);
    }
}

struct Track {
    source: Arc<Source>,
    enabled: AtomicBool,
    ended: AtomicBool,
}

impl Track {
    fn ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst) || self.source.ended.load(Ordering::SeqCst)
    }
}

static VIDEO_TRACKS: LazyLock<Mutex<HashMap<MediaStreamId, Arc<Track>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn add_track(source: Arc<Source>) -> MediaStreamId {
    let id = MediaStreamId::new();
    source.live_tracks.fetch_add(1, Ordering::SeqCst);
    let track = Arc::new(Track {
        source,
        enabled: AtomicBool::new(true),
        ended: AtomicBool::new(false),
    });
    VIDEO_TRACKS.lock().unwrap().insert(id, track);
    id
}

fn track(id: &MediaStreamId) -> Option<Arc<Track>> {
    VIDEO_TRACKS.lock().unwrap().get(id).cloned()
}

struct SourceOwner(Arc<Source>);

impl Drop for SourceOwner {
    fn drop(&mut self) {
        self.0.end();
    }
}

/// The producing side of a video source. Cheap to clone; when the last clone drops, the source ends.
#[derive(Clone)]
pub struct VideoSource(Arc<SourceOwner>);

/// Registers a new video source and its first track.
pub fn create_video_source(label: &str) -> (MediaStreamId, VideoSource) {
    let source = Arc::new(Source {
        label: label.to_owned(),
        latest: Mutex::new(None),
        newer: Condvar::new(),
        seq: AtomicU64::new(0),
        ended: AtomicBool::new(false),
        muted: AtomicBool::new(false),
        readers: AtomicUsize::new(0),
        live_tracks: AtomicUsize::new(0),
        frame_rate: Mutex::new(0.0),
        on_unused: Mutex::new(None),
        watchers: Mutex::new(Vec::new()),
    });
    let id = add_track(source.clone());
    (id, VideoSource(Arc::new(SourceOwner(source))))
}

impl VideoSource {
    fn src(&self) -> &Arc<Source> {
        &self.0.0
    }

    /// Publishes a frame (its `seq` is set here). Never blocks on a reader.
    pub fn push(&self, mut frame: VideoFrame) {
        let src = self.src();
        if src.ended.load(Ordering::SeqCst) {
            return;
        }
        frame.seq = src.seq.fetch_add(1, Ordering::SeqCst) + 1;
        *src.latest.lock().unwrap() = Some(frame);
        src.newer.notify_all();
    }

    /// Whether any reader is subscribed (a producer skips its work while nobody reads).
    pub fn has_readers(&self) -> bool {
        self.src().readers.load(Ordering::SeqCst) > 0
    }

    /// Runs when the source's last track stops (a camera closes its device there).
    pub fn on_unused(&self, callback: Box<dyn Fn() + Send + Sync>) {
        *self.src().on_unused.lock().unwrap() = Some(callback);
    }

    /// The source itself is muted (no frames for a while: a remote track without packets).
    pub fn set_muted(&self, muted: bool) {
        let src = self.src();
        if src.muted.swap(muted, Ordering::SeqCst) != muted && !src.ended.load(Ordering::SeqCst) {
            src.notify(None, if muted { TrackEvent::Muted } else { TrackEvent::Unmuted });
        }
    }

    /// The nominal frame rate, for `track_info` (`captureStream(fps)`, a camera's mode).
    pub fn set_frame_rate(&self, fps: f64) {
        *self.src().frame_rate.lock().unwrap() = fps;
    }

    /// Ends the source: every track of it ends and its readers see `Ended`.
    pub fn end(&self) {
        self.src().end();
    }

    pub fn ended(&self) -> bool {
        self.src().ended.load(Ordering::SeqCst)
    }
}

/// `MediaStreamTrack.clone()` of a video track: a new track id on the same source.
pub fn clone_video_track(id: &MediaStreamId) -> Option<MediaStreamId> {
    let track = track(id)?;
    if track.ended() {
        return None;
    }
    Some(add_track(track.source.clone()))
}

/// `track.stop()`: this track ends. When the last track of a source stops, its `on_unused` runs.
pub fn stop_video_track(id: &MediaStreamId) {
    let Some(track) = track(id) else { return };
    if track.ended.swap(true, Ordering::SeqCst) {
        return;
    }
    {
        let _latest = track.source.latest.lock().unwrap();
        track.source.newer.notify_all();
    }
    // track.stop() fires no event on the track itself (as in the spec and live::stop_track), and a stopped
    // track hears nothing more (not the source's end either).
    track.source.watchers.lock().unwrap().retain(|(x, _)| x != id);
    if track.source.live_tracks.fetch_sub(1, Ordering::SeqCst) == 1 {
        if let Some(callback) = track.source.on_unused.lock().unwrap().as_ref() {
            callback();
        }
    }
}

/// Tells `f` about this track's mute, unmute and end (its source's end; not its own stop), as
/// live::watch_track does for audio. Called on the producer's thread (or the stopping one): forward to
/// the script thread. One watcher per track; a new one replaces it. Forgotten with the track.
pub fn watch_video_track(id: &MediaStreamId, f: Box<dyn Fn(TrackEvent) + Send + Sync>) {
    let Some(track) = track(id) else { return };
    let mut w = track.source.watchers.lock().unwrap();
    w.retain(|(x, _)| x != id);
    w.push((*id, Arc::from(f)));
}

/// Forgets a track id (its DOM object was collected). Stops it first.
pub fn unregister_video_track(id: &MediaStreamId) {
    stop_video_track(id);
    if let Some(track) = VIDEO_TRACKS.lock().unwrap().remove(id) {
        track.source.watchers.lock().unwrap().retain(|(x, _)| x != id);
    }
}

/// `track.enabled`: a disabled track reads black frames.
pub fn set_video_enabled(id: &MediaStreamId, enabled: bool) {
    if let Some(track) = track(id) {
        track.enabled.store(enabled, Ordering::SeqCst);
    }
}

/// Whether `id` is a video track of this registry.
pub fn is_video_track(id: &MediaStreamId) -> bool {
    VIDEO_TRACKS.lock().unwrap().contains_key(id)
}

pub fn video_track_info(id: &MediaStreamId) -> Option<VideoTrackInfo> {
    let track = track(id)?;
    let src = &track.source;
    let (width, height) = src
        .latest
        .lock()
        .unwrap()
        .as_ref()
        .map_or((0, 0), |f| (f.width, f.height));
    Some(VideoTrackInfo {
        label: src.label.clone(),
        enabled: track.enabled.load(Ordering::SeqCst),
        muted: src.muted.load(Ordering::SeqCst),
        ended: track.ended(),
        width,
        height,
        frame_rate: *src.frame_rate.lock().unwrap(),
    })
}

/// A reader of one video track, starting at "now" (the frame already there counts as seen).
pub fn subscribe_video(id: &MediaStreamId) -> Option<VideoReader> {
    let track = track(id)?;
    track.source.readers.fetch_add(1, Ordering::SeqCst);
    let seen = track.source.seq.load(Ordering::SeqCst);
    Some(VideoReader { track, seen })
}

/// Reads the latest frame of a track. Drops on its own.
pub struct VideoReader {
    track: Arc<Track>,
    seen: u64,
}

impl VideoReader {
    fn view(&self, frame: VideoFrame) -> VideoFrame {
        if self.track.enabled.load(Ordering::SeqCst) {
            frame
        } else {
            frame.black_like()
        }
    }

    /// The source's latest frame (seen or not), or None before the first.
    pub fn latest(&mut self) -> Option<VideoFrame> {
        let frame = self.track.source.latest.lock().unwrap().clone()?;
        self.seen = self.seen.max(frame.seq);
        Some(self.view(frame))
    }

    /// Waits up to `timeout` for a frame newer than the last one this reader got; None on timeout or end.
    pub fn wait_newer(&mut self, timeout: Duration) -> Option<VideoFrame> {
        let deadline = Instant::now() + timeout;
        let src = &self.track.source;
        let mut latest = src.latest.lock().unwrap();
        loop {
            if self.track.ended() {
                return None;
            }
            if let Some(frame) = latest.as_ref() {
                if frame.seq > self.seen {
                    let frame = frame.clone();
                    drop(latest);
                    self.seen = frame.seq;
                    return Some(self.view(frame));
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            latest = src.newer.wait_timeout(latest, deadline - now).unwrap().0;
        }
    }

    pub fn state(&self) -> VideoState {
        if self.track.ended() {
            VideoState::Ended
        } else if self.track.source.muted.load(Ordering::SeqCst) {
            VideoState::Muted
        } else {
            VideoState::Live
        }
    }
}

impl Drop for VideoReader {
    fn drop(&mut self) {
        self.track.source.readers.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(v: u8) -> VideoFrame {
        VideoFrame::packed(2, 2, VideoFormat::Bgra8, 8, vec![v; 16].into(), Instant::now())
    }

    #[test]
    fn latest_frame_and_end() {
        let (id, source) = create_video_source("test");
        assert!(!source.has_readers());
        let mut reader = subscribe_video(&id).unwrap();
        assert!(source.has_readers());
        assert!(reader.wait_newer(Duration::from_millis(1)).is_none());
        source.push(frame(1));
        source.push(frame(2));
        let f = reader.wait_newer(Duration::from_millis(1)).unwrap();
        assert_eq!((f.seq, f.planes[0]), (2, 2));
        assert!(reader.wait_newer(Duration::from_millis(1)).is_none());
        set_video_enabled(&id, false);
        source.push(frame(3));
        assert_eq!(reader.wait_newer(Duration::from_millis(1)).unwrap().planes[0], 0);
        drop(source);
        assert_eq!(reader.state(), VideoState::Ended);
        drop(reader);
        unregister_video_track(&id);
    }

    #[test]
    fn clone_stop_unused() {
        let (id, source) = create_video_source("test");
        let hit = Arc::new(AtomicBool::new(false));
        let h = hit.clone();
        source.on_unused(Box::new(move || h.store(true, Ordering::SeqCst)));
        let id2 = clone_video_track(&id).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let e = events.clone();
        watch_video_track(&id2, Box::new(move |ev| e.lock().unwrap().push(ev)));
        source.set_muted(true);
        source.set_muted(false);
        stop_video_track(&id);
        assert!(!hit.load(Ordering::SeqCst));
        stop_video_track(&id2);
        assert!(hit.load(Ordering::SeqCst));
        drop(source);
        // id2 was stopped before the source ended: no Ended for it.
        assert_eq!(*events.lock().unwrap(), vec![TrackEvent::Muted, TrackEvent::Unmuted]);
    }
}

/// How many video track ids are registered (leak checks: PARTII_LIVE_STATS).
pub fn video_track_count() -> usize {
    VIDEO_TRACKS.lock().unwrap().len()
}
