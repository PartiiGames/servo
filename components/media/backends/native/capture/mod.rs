/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii (stream "capture"): cameras for getUserMedia({video}).
//!
//! - Linux: V4L2 (`v4l2.rs`, raw ioctls, YUYV converted to BGRA here).
//! - macOS: AVFoundation (`avf.rs`, 32BGRA from AVCaptureVideoDataOutput).
//! - Everywhere: the fake camera (`fake.rs`), on with `PARTII_FAKE_CAMERA=pattern` or the pref
//!   `media_fake_video_input` (the DOM passes it through `Backend::set_capture_mocking`). With the fake
//!   on, it is the only camera, as with Chrome's `--use-fake-device-for-media-stream`.
//!
//! Frames go to a [`FrameSink`] as BGRA8 (the live video registry's `Bgra8`).

#[cfg(target_os = "macos")]
mod avf;
pub(crate) mod fake;
pub(crate) mod mic;
#[cfg(target_os = "linux")]
mod v4l2;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use log::warn;
use servo_media_streams::capture::{Constrain, MediaTrackConstraintSet};
use servo_media_streams::device_monitor::{MediaDeviceInfo, MediaDeviceKind, MediaDeviceMonitor};
use servo_media_streams::live_video::{self, VideoFormat, VideoFrame, VideoSource};
use servo_media_streams::registry::MediaStreamId;

static MOCK: AtomicBool = AtomicBool::new(false);

/// The pref `media_fake_video_input` (through `set_capture_mocking`).
pub(crate) fn set_mocking(on: bool) {
    MOCK.store(on, Ordering::Relaxed);
}

/// Whether the fake camera replaces the real ones.
pub(crate) fn fake_camera() -> bool {
    MOCK.load(Ordering::Relaxed)
        || std::env::var("PARTII_FAKE_CAMERA").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Testing the "no camera" path: `PARTII_NO_CAMERA=1` hides every camera, fake or real.
fn no_camera() -> bool {
    std::env::var("PARTII_NO_CAMERA").is_ok_and(|v| !v.is_empty() && v != "0")
}

#[derive(Clone, Debug)]
pub(crate) struct CameraInfo {
    /// Stable per device (what `MediaDeviceInfo.deviceId` and `getSettings().deviceId` report).
    pub id: String,
    pub label: String,
    /// "user", "environment" or "".
    pub facing: String,
    /// The backend's own handle (a /dev/videoN path, an AVCaptureDevice uniqueID, "fake").
    pub(crate) handle: String,
}

/// What a camera was opened with.
#[derive(Clone, Debug)]
pub(crate) struct CameraSettings {
    pub width: u32,
    pub height: u32,
    pub frame_rate: f64,
    pub info: CameraInfo,
}

/// Where a camera's frames go (bound to the live video registry's `VideoSource`).
pub(crate) trait FrameSink: Send + Sync + 'static {
    /// One BGRA8 frame, rows `stride` bytes apart (`stride >= width * 4`).
    fn push_bgra(&self, width: u32, height: u32, stride: usize, data: Arc<[u8]>);
    /// False once every track of the source stopped: the capture thread closes the device and quits.
    fn live(&self) -> bool;
    /// Whether anyone reads frames now (a camera may skip converting while nobody does).
    fn has_readers(&self) -> bool {
        true
    }
    /// The device went away or failed: the tracks end.
    fn end(&self);
}

/// The wanted size and rate from a constraint set (ideal or exact, else the range's ideal/min/max).
pub(crate) struct Want {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
    /// `exact` values the device must meet (OverconstrainedError otherwise).
    pub exact_width: Option<u32>,
    pub exact_height: Option<u32>,
}

impl Want {
    pub(crate) fn from(set: &MediaTrackConstraintSet) -> Want {
        fn pick_u(c: &Option<Constrain<u32>>) -> (Option<u32>, Option<u32>) {
            match c {
                None => (None, None),
                Some(Constrain::Value(v)) => (Some(*v), None),
                Some(Constrain::Range(r)) => (r.ideal.or(r.min).or(r.max), None),
            }
        }
        let (width, exact_width) = pick_u(&set.width);
        let (height, exact_height) = pick_u(&set.height);
        let frame_rate = match &set.frame_rate {
            None => None,
            Some(Constrain::Value(v)) => Some(*v),
            Some(Constrain::Range(r)) => r.ideal.or(r.max).or(r.min),
        };
        Want {
            width,
            height,
            frame_rate,
            exact_width,
            exact_height,
        }
    }
}

/// A running camera. Dropping it does not stop it; the sink's `live()` going false does.
pub(crate) struct Camera {
    pub settings: CameraSettings,
}

#[derive(Debug)]
pub(crate) enum CaptureError {
    /// No camera (or none matching deviceId).
    NotFound,
    /// A camera exists but couldn't start (busy, unsupported format).
    NotReadable(String),
}

/// The cameras getUserMedia and enumerateDevices see.
pub(crate) fn cameras() -> Vec<CameraInfo> {
    if no_camera() {
        return vec![];
    }
    if fake_camera() {
        return vec![fake::info()];
    }
    platform_cameras()
}

#[cfg(target_os = "linux")]
fn platform_cameras() -> Vec<CameraInfo> {
    v4l2::list()
}

#[cfg(target_os = "macos")]
fn platform_cameras() -> Vec<CameraInfo> {
    avf::list()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_cameras() -> Vec<CameraInfo> {
    vec![]
}

#[cfg(target_os = "linux")]
fn platform_open(
    info: CameraInfo,
    want: &Want,
    sink: Arc<dyn FrameSink>,
) -> Result<Camera, CaptureError> {
    v4l2::open(info, want, sink)
}

#[cfg(target_os = "macos")]
fn platform_open(
    info: CameraInfo,
    want: &Want,
    sink: Arc<dyn FrameSink>,
) -> Result<Camera, CaptureError> {
    avf::open(info, want, sink)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_open(_: CameraInfo, _: &Want, _: Arc<dyn FrameSink>) -> Result<Camera, CaptureError> {
    Err(CaptureError::NotFound)
}

/// Opens a camera (`device_id`: an exact deviceId constraint) and starts pushing frames to `sink`.
pub(crate) fn open(
    device_id: Option<&str>,
    facing: Option<&str>,
    set: &MediaTrackConstraintSet,
    sink: Arc<dyn FrameSink>,
) -> Result<Camera, CaptureError> {
    let all = cameras();
    let chosen = match device_id {
        Some(id) => all.into_iter().find(|c| c.id == id),
        None => {
            let pos = facing
                .and_then(|f| all.iter().position(|c| c.facing == f))
                .unwrap_or(0);
            all.into_iter().nth(pos)
        },
    }
    .ok_or(CaptureError::NotFound)?;
    let want = Want::from(set);
    if chosen.handle == fake::HANDLE {
        return Ok(fake::open(chosen, &want, sink));
    }
    platform_open(chosen, &want, sink)
}

/// A stable short id for a device from its backend name (not the raw path: pages see this).
pub(crate) fn device_id(kind: &str, name: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in kind.bytes().chain(name.bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// A camera's frames into the live video registry.
struct RegistrySink {
    source: VideoSource,
    /// Every track of the source stopped.
    unused: Arc<AtomicBool>,
}

impl FrameSink for RegistrySink {
    fn push_bgra(&self, width: u32, height: u32, stride: usize, data: Arc<[u8]>) {
        self.source.push(VideoFrame::packed(
            width,
            height,
            VideoFormat::Bgra8,
            stride as u32,
            data,
            std::time::Instant::now(),
        ));
    }
    fn live(&self) -> bool {
        !self.unused.load(Ordering::SeqCst) && !self.source.ended()
    }
    fn has_readers(&self) -> bool {
        self.source.has_readers()
    }
    fn end(&self) {
        self.source.end();
    }
}

/// getUserMedia({video}): opens the camera `set.device_id` (or the first / the one facing
/// `set.facing_mode`) and returns its first track. None when it can't start (NotReadableError).
pub(crate) fn open_video_track(set: &MediaTrackConstraintSet) -> Option<MediaStreamId> {
    let label = cameras()
        .into_iter()
        .find(|c| set.device_id.as_deref().is_none_or(|id| c.id == id))
        .map(|c| c.label)?;
    let (id, source) = live_video::create_video_source(&label);
    let unused = Arc::new(AtomicBool::new(false));
    let u = unused.clone();
    source.on_unused(Box::new(move || u.store(true, Ordering::SeqCst)));
    let sink = Arc::new(RegistrySink {
        source: source.clone(),
        unused,
    });
    match open(
        set.device_id.as_deref(),
        set.facing_mode.as_deref(),
        set,
        sink,
    ) {
        Ok(camera) => {
            source.set_frame_rate(camera.settings.frame_rate);
            // The capture thread holds the sink (and so the source) until the camera closes.
            drop(source);
            Some(id)
        },
        Err(e) => {
            warn!("getUserMedia: the camera didn't start: {e:?}");
            source.end();
            live_video::unregister_video_track(&id);
            None
        },
    }
}

/// `PARTII_LIVE_STATS=1`: print the live registry's track counts to stderr when they change
/// ("partii-live: video=N audio=M"), for the capture checks' leak test.
pub(crate) fn start_stats() {
    if !std::env::var("PARTII_LIVE_STATS").is_ok_and(|v| !v.is_empty() && v != "0") {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("PartiiLiveStats".to_owned())
        .spawn(|| {
            let mut last = (usize::MAX, usize::MAX);
            loop {
                let now = (
                    live_video::video_track_count(),
                    servo_media_streams::live::audio_track_count(),
                );
                if now != last {
                    eprintln!("partii-live: video={} audio={}", now.0, now.1);
                    last = now;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        });
}

/// Testing the "no microphone" path: `PARTII_NO_MIC=1`.
fn no_mic() -> bool {
    std::env::var("PARTII_NO_MIC").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// The fake mic (the voice stream's `PARTII_FAKE_MIC=<hz>`), label "Partii fake mic".
pub(crate) fn fake_mic() -> bool {
    MOCK.load(Ordering::Relaxed)
        || std::env::var("PARTII_FAKE_MIC").is_ok_and(|v| !v.is_empty() && v != "0")
}

pub(crate) const FAKE_MIC_LABEL: &str = "Partii fake mic";

/// Microphones: (deviceId, label).
pub(crate) fn microphones() -> Vec<(String, String)> {
    if no_mic() {
        return vec![];
    }
    if fake_mic() {
        return vec![(device_id("audioinput", "fake"), FAKE_MIC_LABEL.to_owned())];
    }
    device_microphones()
}

#[cfg(feature = "device")]
fn device_microphones() -> Vec<(String, String)> {
    use cpal::traits::{DeviceTrait, HostTrait};
    #[cfg(target_os = "linux")]
    let _alsa_errors = alsa::Output::local_error_handler().ok();
    let host = cpal::default_host();
    // getUserMedia without a deviceId takes the first microphone (mediadevices.rs), so the system's default input
    // goes first, as Chrome and WebKit list it: in input_devices() order a Continuity iPhone microphone could come
    // first and be opened (waking the phone) instead of the Mac's own.
    let default_id = host.default_input_device().and_then(|d| {
        let name = d.description().ok()?.name().to_string();
        let key = d.id().map(|i| i.to_string()).unwrap_or_else(|_| name);
        Some(device_id("audioinput", &key))
    });
    let mut mics: Vec<(String, String)> = host
        .input_devices()
        .map(|devices| {
            devices
                .filter_map(|d| {
                    let name = d.description().ok()?.name().to_string();
                    let key = d
                        .id()
                        .map(|i| i.to_string())
                        .unwrap_or_else(|_| name.clone());
                    Some((device_id("audioinput", &key), name))
                })
                .collect()
        })
        .unwrap_or_default();
    // macOS: Continuity microphones (an iPhone's) go last, even when the system made one the default, so the phone
    // is only opened (and woken) when a page asks for its deviceId (Sterling, 11-24).
    #[cfg(target_os = "macos")]
    let continuity = avf::continuity_mic_names();
    #[cfg(not(target_os = "macos"))]
    let continuity: Vec<String> = vec![];
    // Stable sorts: the others keep the host's order.
    if let Some(default_id) = default_id {
        mics.sort_by_key(|(id, _)| *id != default_id);
    }
    mics.sort_by_key(|(_, name)| continuity.contains(name));
    mics
}

#[cfg(not(feature = "device"))]
fn device_microphones() -> Vec<(String, String)> {
    vec![]
}

/// enumerateDevices: cameras, microphones and the default output.
pub(crate) struct NativeDeviceMonitor;

impl MediaDeviceMonitor for NativeDeviceMonitor {
    fn enumerate_devices(&self) -> Option<Vec<MediaDeviceInfo>> {
        let mut out = vec![];
        for (device_id, label) in microphones() {
            out.push(MediaDeviceInfo {
                device_id,
                kind: MediaDeviceKind::AudioInput,
                label,
            });
        }
        for c in cameras() {
            out.push(MediaDeviceInfo {
                device_id: c.id,
                kind: MediaDeviceKind::VideoInput,
                label: c.label,
            });
        }
        out.push(MediaDeviceInfo {
            device_id: "default".to_owned(),
            kind: MediaDeviceKind::AudioOutput,
            label: "Default".to_owned(),
        });
        Some(out)
    }
}

/// YUYV (YUY2, 4:2:2) to BGRA8, BT.601 limited range (what UVC cameras send).
pub(crate) fn yuyv_to_bgra(src: &[u8], width: u32, height: u32, src_stride: usize, out: &mut [u8]) {
    let w = width as usize;
    for y in 0..height as usize {
        let row = &src[y * src_stride..];
        let dst = &mut out[y * w * 4..(y + 1) * w * 4];
        for x2 in 0..w / 2 {
            let s = &row[x2 * 4..x2 * 4 + 4];
            let (y0, u, y1, v) = (
                s[0] as i32,
                s[1] as i32 - 128,
                s[2] as i32,
                s[3] as i32 - 128,
            );
            for (i, yy) in [y0, y1].into_iter().enumerate() {
                let c = 298 * (yy - 16);
                let r = (c + 409 * v + 128) >> 8;
                let g = (c - 100 * u - 208 * v + 128) >> 8;
                let b = (c + 516 * u + 128) >> 8;
                let d = &mut dst[(x2 * 2 + i) * 4..(x2 * 2 + i) * 4 + 4];
                d[0] = b.clamp(0, 255) as u8;
                d[1] = g.clamp(0, 255) as u8;
                d[2] = r.clamp(0, 255) as u8;
                d[3] = 255;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuyv_white_black() {
        // Y=235 U=V=128 is white, Y=16 black.
        let src = [235u8, 128, 16, 128];
        let mut out = [0u8; 8];
        yuyv_to_bgra(&src, 2, 1, 4, &mut out);
        assert_eq!(out, [255, 255, 255, 255, 0, 0, 0, 255]);
    }
}
