/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii (stream "capture"): the player behind `<video>.srcObject = mediaStream` for video tracks of
//! the live video registry (a camera, a canvas captureStream, a remote track). One thread per player
//! waits for the track's newest frame and hands it to the element's video renderer as BGRA, which is
//! what `drawImage(video)` and `texImage2D(video)` then read.
//!
//! Audio tracks of a srcObject are not played here (the voice stream's `<audio>.srcObject` reader
//! takes them; until then they are accepted and silent).

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use log::warn;
use servo_base::generic_channel::GenericCallback;
use servo_media::MediaInstanceError;
use servo_media_player::metadata::Metadata;
use servo_media_player::video::{Buffer, VideoFrame, VideoFrameData, VideoFrameRenderer};
use servo_media_player::{PlaybackState, Player, PlayerError, PlayerEvent};
use servo_media_streams::live_video::{self, VideoFormat, VideoState};
use servo_media_streams::registry::MediaStreamId;
use servo_media_traits::MediaInstance;

struct Shared {
    events: GenericCallback<PlayerEvent>,
    renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
    playing: AtomicBool,
    /// The reader thread quits when this goes false.
    running: AtomicBool,
    metadata_sent: AtomicBool,
}

impl Shared {
    fn emit(&self, event: PlayerEvent) {
        if let Err(e) = self.events.send(event) {
            warn!("stream player: could not send event: {e:?}");
        }
    }
}

pub struct StreamPlayer {
    id: usize,
    shared: Arc<Shared>,
    video: Mutex<Option<MediaStreamId>>,
    muted: AtomicBool,
    volume: Mutex<f64>,
}

impl StreamPlayer {
    pub fn new(
        id: usize,
        events: GenericCallback<PlayerEvent>,
        renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
    ) -> Self {
        StreamPlayer {
            id,
            shared: Arc::new(Shared {
                events,
                renderer,
                playing: AtomicBool::new(false),
                running: AtomicBool::new(false),
                metadata_sent: AtomicBool::new(false),
            }),
            video: Mutex::new(None),
            muted: AtomicBool::new(false),
            volume: Mutex::new(1.),
        }
    }

    fn start_video(&self, id: MediaStreamId) {
        let Some(mut reader) = live_video::subscribe_video(&id) else {
            return;
        };
        let shared = self.shared.clone();
        shared.running.store(true, Ordering::SeqCst);
        let _ = thread::Builder::new()
            .name("PartiiStreamVideo".to_owned())
            .spawn(move || {
                // The frame already there (a camera that started before srcObject was set) shows at once.
                let mut next = reader.latest();
                while shared.running.load(Ordering::SeqCst) {
                    let frame = match next.take() {
                        Some(f) => f,
                        None => match reader.wait_newer(Duration::from_millis(100)) {
                            Some(f) => f,
                            None => {
                                if reader.state() == VideoState::Ended {
                                    shared.emit(PlayerEvent::EndOfStream);
                                    break;
                                }
                                continue;
                            },
                        },
                    };
                    let first = !shared.metadata_sent.swap(true, Ordering::SeqCst);
                    if first {
                        shared.emit(PlayerEvent::MetadataUpdated(Metadata {
                            duration: None,
                            width: frame.width,
                            height: frame.height,
                            format: String::new(),
                            is_seekable: false,
                            video_tracks: vec!["video".to_owned()],
                            audio_tracks: vec![],
                            is_live: true,
                            title: None,
                        }));
                        shared.emit(PlayerEvent::DurationChanged(None));
                        shared.emit(PlayerEvent::StateChanged(
                            if shared.playing.load(Ordering::SeqCst) {
                                PlaybackState::Playing
                            } else {
                                PlaybackState::Paused
                            },
                        ));
                    }
                    // A paused element keeps its current frame; the first frame always shows.
                    if !first && !shared.playing.load(Ordering::SeqCst) {
                        continue;
                    }
                    let Some(renderer) = shared.renderer.as_ref() else {
                        continue;
                    };
                    let Some(bgra) = to_bgra(&frame) else {
                        continue;
                    };
                    if let Some(f) = VideoFrame::new(
                        frame.width as i32,
                        frame.height as i32,
                        Arc::new(RawFrame(Arc::new(bgra))),
                    ) {
                        renderer.lock().unwrap().render(f);
                        shared.emit(PlayerEvent::VideoFrameUpdated);
                    }
                }
            });
    }
}

struct RawFrame(Arc<Vec<u8>>);

impl Buffer for RawFrame {
    fn to_vec(&self) -> Option<VideoFrameData> {
        Some(VideoFrameData::Raw(self.0.clone()))
    }
}

/// A registry frame as tightly packed BGRA8.
fn to_bgra(f: &live_video::VideoFrame) -> Option<Vec<u8>> {
    let (w, h) = (f.width as usize, f.height as usize);
    let mut out = vec![0u8; w * h * 4];
    let p = &f.planes[..];
    match f.format {
        VideoFormat::Bgra8 | VideoFormat::Rgba8 => {
            let stride = f.strides[0] as usize;
            for y in 0..h {
                let src = p.get(f.offsets[0] + y * stride..f.offsets[0] + y * stride + w * 4)?;
                let dst = &mut out[y * w * 4..(y + 1) * w * 4];
                dst.copy_from_slice(src);
                if f.format == VideoFormat::Rgba8 {
                    for px in dst.chunks_exact_mut(4) {
                        px.swap(0, 2);
                    }
                }
                for px in dst.chunks_exact_mut(4) {
                    px[3] = 255;
                }
            }
        },
        VideoFormat::I420 | VideoFormat::Nv12 => {
            let (ys, us) = (f.strides[0] as usize, f.strides[1] as usize);
            for y in 0..h {
                for x in 0..w {
                    let yy = *p.get(f.offsets[0] + y * ys + x)? as i32;
                    let (u, v) = if f.format == VideoFormat::I420 {
                        let vs = f.strides[2] as usize;
                        (
                            *p.get(f.offsets[1] + (y / 2) * us + x / 2)? as i32,
                            *p.get(f.offsets[2] + (y / 2) * vs + x / 2)? as i32,
                        )
                    } else {
                        let o = f.offsets[1] + (y / 2) * us + (x / 2) * 2;
                        (*p.get(o)? as i32, *p.get(o + 1)? as i32)
                    };
                    let (u, v) = (u - 128, v - 128);
                    let c = 298 * (yy - 16);
                    let d = &mut out[(y * w + x) * 4..(y * w + x) * 4 + 4];
                    d[0] = ((c + 516 * u + 128) >> 8).clamp(0, 255) as u8;
                    d[1] = ((c - 100 * u - 208 * v + 128) >> 8).clamp(0, 255) as u8;
                    d[2] = ((c + 409 * v + 128) >> 8).clamp(0, 255) as u8;
                    d[3] = 255;
                }
            }
        },
    }
    Some(out)
}

impl Drop for StreamPlayer {
    fn drop(&mut self) {
        self.shared.running.store(false, Ordering::SeqCst);
    }
}

impl MediaInstance for StreamPlayer {
    fn get_id(&self) -> usize {
        self.id
    }
    fn mute(&self, _val: bool) -> Result<(), MediaInstanceError> {
        Ok(())
    }
    fn suspend(&self) -> Result<(), MediaInstanceError> {
        Ok(())
    }
    fn resume(&self) -> Result<(), MediaInstanceError> {
        Ok(())
    }
}

impl Player for StreamPlayer {
    fn play(&self) -> Result<(), PlayerError> {
        if !self.shared.playing.swap(true, Ordering::SeqCst)
            && self.shared.metadata_sent.load(Ordering::SeqCst)
        {
            self.shared
                .emit(PlayerEvent::StateChanged(PlaybackState::Playing));
        }
        Ok(())
    }
    fn pause(&self) -> Result<(), PlayerError> {
        if self.shared.playing.swap(false, Ordering::SeqCst)
            && self.shared.metadata_sent.load(Ordering::SeqCst)
        {
            self.shared
                .emit(PlayerEvent::StateChanged(PlaybackState::Paused));
        }
        Ok(())
    }
    fn paused(&self) -> bool {
        !self.shared.playing.load(Ordering::SeqCst)
    }
    fn can_resume(&self) -> bool {
        true
    }
    fn stop(&self) -> Result<(), PlayerError> {
        self.shared.running.store(false, Ordering::SeqCst);
        self.shared.playing.store(false, Ordering::SeqCst);
        Ok(())
    }
    fn seek(&self, _: f64) -> Result<(), PlayerError> {
        Ok(())
    }
    fn seekable(&self) -> Vec<Range<f64>> {
        vec![]
    }
    fn set_mute(&self, muted: bool) -> Result<(), PlayerError> {
        self.muted.store(muted, Ordering::SeqCst);
        Ok(())
    }
    fn muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }
    fn set_volume(&self, volume: f64) -> Result<(), PlayerError> {
        *self.volume.lock().unwrap() = volume;
        Ok(())
    }
    fn volume(&self) -> f64 {
        *self.volume.lock().unwrap()
    }
    fn set_input_size(&self, _: u64) -> Result<(), PlayerError> {
        Ok(())
    }
    fn set_seekable(&self, _: bool) -> Result<(), PlayerError> {
        Ok(())
    }
    fn set_download_buffering_enabled(&self, _: bool) -> Result<(), PlayerError> {
        Ok(())
    }
    fn set_playback_rate(&self, _: f64) -> Result<(), PlayerError> {
        Ok(())
    }
    fn playback_rate(&self) -> f64 {
        1.
    }
    fn push_data(&self, _: Vec<u8>) -> Result<(), PlayerError> {
        Ok(())
    }
    fn end_of_stream(&self) -> Result<(), PlayerError> {
        Ok(())
    }
    fn buffered(&self) -> Vec<Range<f64>> {
        vec![]
    }
    fn set_stream(&self, stream: &MediaStreamId, _only_stream: bool) -> Result<(), PlayerError> {
        if live_video::is_video_track(stream) {
            let mut video = self.video.lock().unwrap();
            if video.is_some() {
                // One video track per element: the first one plays (as in Chrome).
                return Ok(());
            }
            *video = Some(*stream);
            drop(video);
            self.start_video(*stream);
        }
        Ok(())
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
