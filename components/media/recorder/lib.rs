/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii: the encoder behind the DOM's MediaRecorder. It reads one video track of the live registry
//! (servo_media_streams::live_video) on its own thread, converts each new frame to I420, encodes VP8
//! (the vendored libvpx, realtime) and writes WebM. The bytes go to the caller's sink at each timeslice,
//! on requestData and at the end. Video only: audio tracks are not recorded (yet).
//!
//! The thread never touches the producer: it takes the latest frame when it's free, so a slow encode
//! skips frames instead of slowing down the page.

mod webm;
mod yuv;

use std::ffi::{CStr, c_char, c_int, c_longlong, c_uint, c_ulong};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use partii_libvpx_sys as _;
use servo_media_streams::MediaStreamId;
use servo_media_streams::live_video::{VideoReader, VideoState, subscribe_video};

#[repr(C)]
struct Vp8 {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn partii_vp8_new(w: c_uint, h: c_uint, kbps: c_uint, threads: c_uint, cpu_used: c_int) -> *mut Vp8;
    fn partii_vp8_encode(e: *mut Vp8, i420: *const u8, pts: c_longlong, duration: c_ulong, key: c_int) -> c_int;
    fn partii_vp8_next(
        e: *mut Vp8,
        buf: *mut *const u8,
        size: *mut usize,
        pts: *mut c_longlong,
        key: *mut c_int,
    ) -> c_int;
    fn partii_vp8_error(e: *mut Vp8) -> *const c_char;
    fn partii_vp8_free(e: *mut Vp8);
}

/// The container/codec pairs `MediaRecorder.isTypeSupported` accepts (VP8 in WebM, no audio).
pub fn is_type_supported(mime: &str) -> bool {
    let mime = mime.trim().to_ascii_lowercase();
    let mut parts = mime.split(';');
    let base = parts.next().unwrap_or("").trim();
    if base != "video/webm" {
        return false;
    }
    for p in parts {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        let Some(codecs) = p.strip_prefix("codecs=") else {
            return false;
        };
        let codecs = codecs.trim_matches('"');
        if !codecs.split(',').all(|c| matches!(c.trim(), "vp8" | "vp8.0")) {
            return false;
        }
    }
    true
}

/// The type a recording made with `mime` ("" picks the default) reports.
pub fn recorded_type(mime: &str) -> String {
    if mime.trim().is_empty() {
        "video/webm;codecs=vp8".to_owned()
    } else {
        mime.trim().to_owned()
    }
}

/// What the encoder hands back. `Data` comes in order and always before `Stopped`.
pub enum RecorderEvent {
    /// Bytes of the WebM file since the last `Data` (possibly empty).
    Data(Vec<u8>),
    /// Encoding failed; `Stopped` follows.
    Error(String),
    /// The recording ended: by `stop`, or because the track ended (`by_track`).
    Stopped { by_track: bool },
}

enum Control {
    RequestData,
    Pause,
    Resume,
    Stop,
}

pub struct RecorderOptions {
    pub video_bits_per_second: u32,
    /// Hand bytes out every `timeslice` (None: only on requestData and at the end).
    pub timeslice: Option<Duration>,
}

/// A running recording. Dropping it stops it (its sink still gets the final bytes).
pub struct Recorder {
    control: Sender<Control>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Starts recording `track`. Fails when the id is not a live video track.
    pub fn start(
        track: &MediaStreamId,
        options: RecorderOptions,
        sink: Box<dyn FnMut(RecorderEvent) + Send>,
    ) -> Result<Recorder, String> {
        // Subscribe here, not on the thread: the source sees a reader at once and starts pushing.
        let reader = subscribe_video(track).ok_or("not a video track")?;
        let (control, controls) = channel();
        let thread = std::thread::Builder::new()
            .name("MediaRecorder".into())
            .spawn(move || Encoder::new(reader, options, sink, controls).run())
            .map_err(|e| e.to_string())?;
        Ok(Recorder { control, thread: Some(thread) })
    }

    pub fn request_data(&self) {
        let _ = self.control.send(Control::RequestData);
    }

    pub fn pause(&self) {
        let _ = self.control.send(Control::Pause);
    }

    pub fn resume(&self) {
        let _ = self.control.send(Control::Resume);
    }

    /// Ends the recording; the sink gets the last `Data` and then `Stopped`. Doesn't wait.
    pub fn stop(&self) {
        let _ = self.control.send(Control::Stop);
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.control.send(Control::Stop);
        // Detach: the thread ends within one wait (<= 50 ms) and owns everything it uses.
        drop(self.thread.take());
    }
}

/// Time between forced keyframes (each starts a cluster; a viewer can seek to it).
const KEYFRAME_EVERY_MS: i64 = 2000;
/// The longest the thread waits for a frame before looking at its controls again.
const POLL: Duration = Duration::from_millis(50);

struct Encoder {
    reader: VideoReader,
    options: RecorderOptions,
    sink: Box<dyn FnMut(RecorderEvent) + Send>,
    controls: Receiver<Control>,
    vp8: *mut Vp8,
    webm: Option<webm::WebmWriter>,
    size: (usize, usize),
    i420: Vec<u8>,
    first: Option<Instant>,
    paused_at: Option<Instant>,
    paused_total: Duration,
    last_key: i64,
    next_slice: Option<Instant>,
    encode_time: Duration,
}

impl Encoder {
    fn new(
        reader: VideoReader,
        options: RecorderOptions,
        sink: Box<dyn FnMut(RecorderEvent) + Send>,
        controls: Receiver<Control>,
    ) -> Encoder {
        let next_slice = options.timeslice.map(|t| Instant::now() + t);
        Encoder {
            reader,
            options,
            sink,
            controls,
            vp8: std::ptr::null_mut(),
            webm: None,
            size: (0, 0),
            i420: Vec::new(),
            first: None,
            paused_at: None,
            paused_total: Duration::ZERO,
            last_key: i64::MIN / 2,
            next_slice,
            encode_time: Duration::ZERO,
        }
    }

    fn run(mut self) {
        let by_track = loop {
            let mut stop = false;
            loop {
                match self.controls.try_recv() {
                    Ok(Control::RequestData) => self.hand_out(),
                    Ok(Control::Pause) => {
                        self.paused_at.get_or_insert_with(Instant::now);
                    },
                    Ok(Control::Resume) => {
                        if let Some(at) = self.paused_at.take() {
                            self.paused_total += at.elapsed();
                        }
                    },
                    Ok(Control::Stop) | Err(TryRecvError::Disconnected) => {
                        stop = true;
                        break;
                    },
                    Err(TryRecvError::Empty) => break,
                }
            }
            if stop {
                break false;
            }
            let wait = self
                .next_slice
                .map_or(POLL, |t| t.saturating_duration_since(Instant::now()).min(POLL));
            if let Some(frame) = self.reader.wait_newer(wait) {
                if self.paused_at.is_none() {
                    if let Err(e) = self.encode(&frame) {
                        (self.sink)(RecorderEvent::Error(e));
                        break false;
                    }
                }
            } else if self.reader.state() == VideoState::Ended {
                break true;
            }
            if let (Some(t), Some(slice)) = (self.next_slice, self.options.timeslice) {
                if Instant::now() >= t {
                    self.hand_out();
                    let mut next = t + slice;
                    if next <= Instant::now() {
                        next = Instant::now() + slice;
                    }
                    self.next_slice = Some(next);
                }
            }
        };
        self.flush();
        self.hand_out();
        if let Some(w) = &self.webm {
            log::debug!(
                "MediaRecorder: {} frames, {} ms, encode {:?} total",
                w.frames,
                w.last_pts,
                self.encode_time
            );
        }
        (self.sink)(RecorderEvent::Stopped { by_track });
    }

    fn hand_out(&mut self) {
        let bytes = self.webm.as_mut().map(|w| w.take()).unwrap_or_default();
        (self.sink)(RecorderEvent::Data(bytes));
    }

    fn encode(&mut self, frame: &servo_media_streams::live_video::VideoFrame) -> Result<(), String> {
        let started = Instant::now();
        if self.vp8.is_null() {
            let (w, h) = ((frame.width & !1) as usize, (frame.height & !1) as usize);
            if w < 2 || h < 2 {
                return Ok(());
            }
            let kbps = (self.options.video_bits_per_second / 1000).clamp(50, 20_000);
            let threads = if w * h >= 640 * 360 { 2 } else { 1 };
            // SAFETY: plain values in; null on failure.
            self.vp8 = unsafe { partii_vp8_new(w as c_uint, h as c_uint, kbps, threads, 10) };
            if self.vp8.is_null() {
                return Err("couldn't start the VP8 encoder".into());
            }
            self.size = (w, h);
            let uid = started.elapsed().as_nanos() as u64 ^ (w as u64) << 32 ^ 0x5041_5254;
            self.webm = Some(webm::WebmWriter::new(w as u32, h as u32, uid));
        }
        let first = *self.first.get_or_insert(frame.timestamp);
        let at = frame
            .timestamp
            .saturating_duration_since(first)
            .saturating_sub(self.paused_total);
        let webm = self.webm.as_ref().unwrap();
        let mut pts = at.as_millis() as i64;
        if pts <= webm.last_pts {
            pts = webm.last_pts + 1;
        }
        let duration = if webm.last_pts < 0 { 33 } else { (pts - webm.last_pts) as c_ulong };
        let (w, h) = self.size;
        if !yuv::to_i420(frame, w, h, &mut self.i420) {
            return Ok(());
        }
        let key = pts - self.last_key >= KEYFRAME_EVERY_MS;
        // SAFETY: self.vp8 is a live encoder; i420 holds w*h*3/2 bytes.
        let r = unsafe { partii_vp8_encode(self.vp8, self.i420.as_ptr(), pts, duration, key as c_int) };
        if r != 0 {
            return Err(self.error());
        }
        self.drain();
        self.encode_time += started.elapsed();
        Ok(())
    }

    fn drain(&mut self) {
        let Some(webm) = self.webm.as_mut() else { return };
        loop {
            let (mut buf, mut size, mut pts, mut key) = (std::ptr::null(), 0usize, 0 as c_longlong, 0 as c_int);
            // SAFETY: the packet stays valid until the next encode call; it's copied before then.
            if unsafe { partii_vp8_next(self.vp8, &mut buf, &mut size, &mut pts, &mut key) } == 0 {
                break;
            }
            let data = unsafe { std::slice::from_raw_parts(buf, size) };
            if key != 0 {
                self.last_key = pts;
            }
            webm.frame(pts, key != 0, data);
        }
    }

    fn flush(&mut self) {
        if self.vp8.is_null() {
            return;
        }
        // SAFETY: a null image flushes the (lag-free) encoder.
        if unsafe { partii_vp8_encode(self.vp8, std::ptr::null(), 0, 0, 0) } == 0 {
            self.drain();
        }
    }

    fn error(&self) -> String {
        // SAFETY: a NUL-terminated string owned by libvpx.
        let msg = unsafe { CStr::from_ptr(partii_vp8_error(self.vp8)) };
        format!("VP8 encoder: {}", msg.to_string_lossy())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.vp8.is_null() {
            // SAFETY: created by partii_vp8_new, freed once.
            unsafe { partii_vp8_free(self.vp8) };
        }
    }
}

// SAFETY: the encoder pointer is owned by one Encoder and only used on its thread.
unsafe impl Send for Encoder {}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use servo_media_streams::live_video::{VideoFormat, VideoFrame, create_video_source};

    use super::*;

    #[test]
    fn types() {
        assert!(is_type_supported("video/webm"));
        assert!(is_type_supported("video/webm;codecs=vp8"));
        assert!(is_type_supported("video/webm; codecs=\"vp8\""));
        assert!(!is_type_supported("video/webm;codecs=vp9"));
        assert!(!is_type_supported("video/mp4"));
        assert!(!is_type_supported("audio/webm"));
    }

    #[test]
    fn records_a_webm() {
        let (id, source) = create_video_source("test");
        let got: Arc<Mutex<(Vec<u8>, bool)>> = Default::default();
        let g = got.clone();
        let rec = Recorder::start(
            &id,
            RecorderOptions { video_bits_per_second: 600_000, timeslice: Some(Duration::from_millis(100)) },
            Box::new(move |e| match e {
                RecorderEvent::Data(d) => g.lock().unwrap().0.extend(d),
                RecorderEvent::Stopped { .. } => g.lock().unwrap().1 = true,
                RecorderEvent::Error(e) => panic!("{e}"),
            }),
        )
        .unwrap();
        assert!(source.has_readers());
        for i in 0..10u8 {
            let px: Vec<u8> = (0..64 * 48 * 4).map(|j| (j as u8).wrapping_mul(i)).collect();
            source.push(VideoFrame::packed(64, 48, VideoFormat::Bgra8, 256, px.into(), Instant::now()));
            std::thread::sleep(Duration::from_millis(30));
        }
        rec.stop();
        for _ in 0..100 {
            if got.lock().unwrap().1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let (bytes, stopped) = got.lock().unwrap().clone();
        assert!(stopped);
        assert_eq!(&bytes[..4], &[0x1A, 0x45, 0xDF, 0xA3]);
        assert!(bytes.len() > 200);
    }
}
