/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! macOS cameras through AVFoundation: an AVCaptureSession with the device's input and an
//! AVCaptureVideoDataOutput in 32BGRA (scaled by AVFoundation to the wanted size), whose sample
//! buffers a delegate copies into the sink on a serial dispatch queue.
//!
//! Permission: the app's Info.plist needs NSCameraUsageDescription (the launcher has it) and a
//! hardened runtime needs the camera entitlement. The first open asks the OS
//! (requestAccessForMediaType) and waits for the answer.
//!
//! `PARTII_CAPTURE_LOG=1`: one stderr line when a session starts ("avf: session started ..."), one a second while it
//! runs (frames AVFoundation delivered and frames copied to readers), and one when it stops: what the attended
//! camera check reads to know the frames are the device's.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use log::{debug, warn};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput,
    AVCaptureDevicePosition, AVCaptureOutput, AVCaptureSession, AVCaptureSessionPreset640x480,
    AVCaptureSessionPreset1280x720, AVCaptureSessionPreset1920x1080, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeAudio, AVMediaTypeVideo,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelBufferHeightKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

use super::{Camera, CameraInfo, CameraSettings, CaptureError, FrameSink, Want};

struct Ivars {
    sink: Arc<dyn FrameSink>,
    /// Sample buffers AVFoundation delivered, and the ones copied to readers (PARTII_CAPTURE_LOG).
    counts: Arc<(AtomicU64, AtomicU64)>,
}

fn capture_log() -> bool {
    std::env::var("PARTII_CAPTURE_LOG").as_deref() == Ok("1")
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "PartiiCameraDelegate"]
    #[ivars = Ivars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for Delegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(
            &self,
            _output: &AVCaptureOutput,
            sample: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            let sink = &self.ivars().sink;
            self.ivars().counts.0.fetch_add(1, Ordering::Relaxed);
            if sink.live() && sink.has_readers() {
                copy_frame(sample, &**sink);
                self.ivars().counts.1.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
);

impl Delegate {
    fn new(sink: Arc<dyn FrameSink>, counts: Arc<(AtomicU64, AtomicU64)>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { sink, counts });
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }
}

/// One 32BGRA pixel buffer into the sink (tightly packed).
fn copy_frame(sample: &CMSampleBuffer, sink: &dyn FrameSink) {
    // SAFETY: a sample buffer AVFoundation handed the delegate.
    let Some(pixels) = (unsafe { sample.image_buffer() }) else {
        return;
    };
    if CVPixelBufferGetPixelFormatType(&pixels) != kCVPixelFormatType_32BGRA {
        return;
    }
    // SAFETY: locked read-only around the copy.
    unsafe {
        if CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return;
        }
    }
    let (w, h) = (
        CVPixelBufferGetWidth(&pixels),
        CVPixelBufferGetHeight(&pixels),
    );
    let stride = CVPixelBufferGetBytesPerRow(&pixels);
    let base = CVPixelBufferGetBaseAddress(&pixels) as *const u8;
    if !base.is_null() && w > 0 && h > 0 && stride >= w * 4 {
        let mut out = vec![0u8; w * h * 4];
        for y in 0..h {
            // SAFETY: rows of `stride` bytes, `h` of them, while locked.
            let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), w * 4) };
            out[y * w * 4..(y + 1) * w * 4].copy_from_slice(row);
        }
        sink.push_bgra(w as u32, h as u32, w * 4, out.into());
    }
    // SAFETY: unlocks the lock above.
    unsafe {
        CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly);
    }
}

/// A CFString constant as the NSString it is (toll-free bridged).
fn key(k: &'static objc2_core_foundation::CFString) -> &'static NSString {
    // SAFETY: CFString and NSString are toll-free bridged.
    unsafe { &*(k as *const objc2_core_foundation::CFString as *const NSString) }
}

fn media_type_video() -> &'static objc2_av_foundation::AVMediaType {
    // SAFETY: an AVFoundation constant.
    unsafe { AVMediaTypeVideo }.expect("AVMediaTypeVideo")
}

#[allow(deprecated)]
fn devices() -> Retained<NSArray<AVCaptureDevice>> {
    // SAFETY: a class method returning the connected video devices.
    unsafe { AVCaptureDevice::devicesWithMediaType(media_type_video()) }
}

fn info_of(device: &AVCaptureDevice) -> CameraInfo {
    // SAFETY: plain property reads.
    let (unique, name, position) = unsafe {
        (
            device.uniqueID().to_string(),
            device.localizedName().to_string(),
            device.position(),
        )
    };
    CameraInfo {
        id: super::device_id("videoinput", &unique),
        label: name,
        facing: if position == AVCaptureDevicePosition::Front {
            "user".to_owned()
        } else if position == AVCaptureDevicePosition::Back {
            "environment".to_owned()
        } else {
            String::new()
        },
        handle: unique,
    }
}

/// kIOAudioDeviceTransportType* (IOAudioTypes.h): built-in, and Continuity (an iPhone, wired or wireless).
const TRANSPORT_BUILT_IN: i32 = i32::from_be_bytes(*b"bltn");
const TRANSPORT_CONTINUITY_WIRED: i32 = i32::from_be_bytes(*b"ccwd");
const TRANSPORT_CONTINUITY_WIRELESS: i32 = i32::from_be_bytes(*b"ccwl");

fn is_continuity(device: &AVCaptureDevice) -> bool {
    // SAFETY: a property read.
    let t = unsafe { device.transportType() };
    t == TRANSPORT_CONTINUITY_WIRED || t == TRANSPORT_CONTINUITY_WIRELESS
}

/// The cameras, in the order getUserMedia without a deviceId picks from: the system default (unless it is a
/// Continuity iPhone, which macOS may make the default when it is near), then built-in cameras, the others, and
/// Continuity cameras last, so a phone is opened only when a page asks for its deviceId (Sterling, 11-24).
/// Listing reads properties only; no device is opened.
pub(super) fn list() -> Vec<CameraInfo> {
    // SAFETY: a class method returning the default video device, if any.
    let default = unsafe { AVCaptureDevice::defaultDeviceWithMediaType(media_type_video()) }
        .filter(|d| !is_continuity(d))
        .map(|d| unsafe { d.uniqueID() }.to_string());
    let mut cams: Vec<(u8, CameraInfo)> = devices()
        .iter()
        .map(|d| {
            let info = info_of(&d);
            // SAFETY: a property read.
            let rank = if default.as_deref() == Some(info.handle.as_str()) {
                0
            } else if unsafe { d.transportType() } == TRANSPORT_BUILT_IN {
                1
            } else if is_continuity(&d) {
                3
            } else {
                2
            };
            (rank, info)
        })
        .collect();
    cams.sort_by_key(|(rank, _)| *rank);
    cams.into_iter().map(|(_, c)| c).collect()
}

/// The names of Continuity microphones (an iPhone's), as AVFoundation lists them: capture/mod.rs puts them last.
#[allow(deprecated)]
pub(super) fn continuity_mic_names() -> Vec<String> {
    // SAFETY: an AVFoundation constant and a class method returning the connected audio devices.
    let Some(audio) = (unsafe { AVMediaTypeAudio }) else {
        return vec![];
    };
    let all = unsafe { AVCaptureDevice::devicesWithMediaType(audio) };
    all.iter()
        .filter(|d| is_continuity(d))
        .map(|d| unsafe { d.localizedName() }.to_string())
        .collect()
}

/// Asks the OS for camera access (once per app; later calls answer at once).
fn authorized() -> bool {
    authorized_for(media_type_video())
}

/// Asks the OS for microphone access before the mic opens (capture/mic.rs): cpal's CoreAudio input would trigger
/// the same prompt by itself, but the mic's open gives up after 10 s, so a player still reading the prompt got
/// NotReadableError ("no microphone"). Never called with the fake mic.
pub(super) fn mic_authorized() -> bool {
    // SAFETY: an AVFoundation constant.
    authorized_for(unsafe { AVMediaTypeAudio }.expect("AVMediaTypeAudio"))
}

fn authorized_for(media_type: &objc2_av_foundation::AVMediaType) -> bool {
    // SAFETY: class methods on AVCaptureDevice.
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) };
    if status == AVAuthorizationStatus::Authorized {
        return true;
    }
    if status != AVAuthorizationStatus::NotDetermined {
        return false;
    }
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |granted: Bool| {
        let _ = tx.send(granted.as_bool());
    });
    // SAFETY: the block is copied by AVFoundation and called once.
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &block)
    };
    rx.recv_timeout(Duration::from_secs(120)).unwrap_or(false)
}

/// The session and what it needs alive, owned by the camera's thread.
struct Running {
    session: Retained<AVCaptureSession>,
    device: Retained<AVCaptureDevice>,
    _output: Retained<AVCaptureVideoDataOutput>,
    _delegate: Retained<Delegate>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
}

// SAFETY: AVCaptureSession and friends are thread-safe for start/stop/isConnected; the delegate
// only touches its Arc'd sink.
unsafe impl Send for Running {}

pub(super) fn open(
    info: CameraInfo,
    want: &Want,
    sink: Arc<dyn FrameSink>,
) -> Result<Camera, CaptureError> {
    if !authorized() {
        warn!("camera: the OS denied access (System Settings > Privacy & Security > Camera)");
        return Err(CaptureError::NotReadable(
            "camera access denied by the OS".to_owned(),
        ));
    }
    let handle = NSString::from_str(&info.handle);
    // SAFETY: a class method.
    let device =
        unsafe { AVCaptureDevice::deviceWithUniqueID(&handle) }.ok_or(CaptureError::NotFound)?;
    let width = want
        .exact_width
        .or(want.width)
        .unwrap_or(640)
        .clamp(16, 3840)
        & !1;
    let height = want
        .exact_height
        .or(want.height)
        .unwrap_or(480)
        .clamp(16, 2160)
        & !1;
    let frame_rate = want.frame_rate.unwrap_or(30.);

    let counts = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
    // SAFETY: the usual AVCaptureSession setup, all objects alive for the calls.
    let running = unsafe {
        let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&device)
            .map_err(|e| CaptureError::NotReadable(e.localizedDescription().to_string()))?;
        let session = AVCaptureSession::new();
        session.beginConfiguration();
        let preset = if width <= 640 && height <= 480 {
            AVCaptureSessionPreset640x480
        } else if width <= 1280 && height <= 720 {
            AVCaptureSessionPreset1280x720
        } else {
            AVCaptureSessionPreset1920x1080
        };
        if session.canSetSessionPreset(preset) {
            session.setSessionPreset(preset);
        }
        if !session.canAddInput(&input) {
            return Err(CaptureError::NotReadable(
                "can't add the camera input".to_owned(),
            ));
        }
        session.addInput(&input);

        let output = AVCaptureVideoDataOutput::new();
        let format = NSNumber::new_u32(kCVPixelFormatType_32BGRA);
        let w = NSNumber::new_u32(width);
        let h = NSNumber::new_u32(height);
        let values: [&AnyObject; 3] = [&format, &w, &h];
        let settings = NSDictionary::<NSString, AnyObject>::from_slices(
            &[
                key(kCVPixelBufferPixelFormatTypeKey),
                key(kCVPixelBufferWidthKey),
                key(kCVPixelBufferHeightKey),
            ],
            &values,
        );
        output.setVideoSettings(Some(&settings));
        output.setAlwaysDiscardsLateVideoFrames(true);
        let delegate = Delegate::new(sink.clone(), counts.clone());
        let queue = DispatchQueue::new("partii.camera", None);
        output.setSampleBufferDelegate_queue(
            Some(ProtocolObject::from_ref(&*delegate)),
            Some(&queue),
        );
        if !session.canAddOutput(&output) {
            return Err(CaptureError::NotReadable(
                "can't add the video output".to_owned(),
            ));
        }
        session.addOutput(&output);
        session.commitConfiguration();
        session.startRunning();
        Running {
            session,
            device,
            _output: output,
            _delegate: delegate,
            _queue: queue,
        }
    };
    debug!("camera {}: {width}x{height} 32BGRA", info.label);
    let log_on = capture_log();
    if log_on {
        // SAFETY: property reads.
        let (running_now, model) = unsafe { (running.session.isRunning(), running.device.modelID().to_string()) };
        eprintln!(
            "avf: session started {:?} uniqueID {} model {model} {width}x{height} running={running_now}",
            info.label, info.handle
        );
    }
    let label = info.label.clone();

    thread::Builder::new()
        .name("PartiiCamera".to_owned())
        .spawn(move || {
            let running = running;
            let mut lost = false;
            let (mut last, mut seen, mut copied) = (Instant::now(), 0, 0);
            while sink.live() {
                thread::sleep(Duration::from_millis(200));
                if log_on && last.elapsed() >= Duration::from_secs(1) {
                    let (d, c) = (counts.0.load(Ordering::Relaxed), counts.1.load(Ordering::Relaxed));
                    // SAFETY: a property read.
                    let on = unsafe { running.session.isRunning() };
                    eprintln!("avf: {label:?} running={on} frames/s delivered {} copied {}", d - seen, c - copied);
                    (last, seen, copied) = (Instant::now(), d, c);
                }
                // SAFETY: property read.
                if !unsafe { running.device.isConnected() } {
                    lost = true;
                    break;
                }
            }
            // SAFETY: stops the session started above.
            unsafe { running.session.stopRunning() };
            if log_on {
                eprintln!(
                    "avf: session stopped {label:?} (frames delivered {}, lost device {lost})",
                    counts.0.load(Ordering::Relaxed)
                );
            }
            drop(running);
            if lost {
                sink.end();
            }
        })
        .map_err(|e| CaptureError::NotReadable(e.to_string()))?;
    Ok(Camera {
        settings: CameraSettings {
            width,
            height,
            frame_rate,
            info,
        },
    })
}
