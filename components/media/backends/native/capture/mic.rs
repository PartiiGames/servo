/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Microphones for getUserMedia({audio}), into the live audio registry (`live.rs`, the voice
//! stream's): cpal input at the device's own rate and channels (readers resample), or the fake mic
//! (`PARTII_FAKE_MIC=<hz>`, or 440 Hz with `media_fake_video_input` / set_capture_mocking): a sine at
//! 48 kHz mono, label "Partii fake mic". The device closes when the last track stops.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use log::warn;
use servo_media_streams::capture::MediaTrackConstraintSet;
use servo_media_streams::live::{AudioFormat, AudioSource, create_audio_source};
use servo_media_streams::registry::MediaStreamId;

use super::FAKE_MIC_LABEL;

/// Opens the microphone `set.device_id` (or the default one) and returns its first track.
pub(crate) fn open(set: &MediaTrackConstraintSet) -> Option<MediaStreamId> {
    if super::fake_mic() {
        return Some(open_fake());
    }
    open_device(set.device_id.as_deref())
}

fn fake_hz() -> f64 {
    std::env::var("PARTII_FAKE_MIC")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|hz| *hz > 0.)
        .unwrap_or(440.)
}

/// Stops a producer when every track of its source stopped.
fn stop_flag(source: &AudioSource) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    source.on_unused(Box::new(move || s.store(true, Ordering::SeqCst)));
    stop
}

fn open_fake() -> MediaStreamId {
    const RATE: u32 = 48000;
    let (id, source) = create_audio_source(
        AudioFormat {
            sample_rate: RATE,
            channels: 1,
        },
        FAKE_MIC_LABEL,
    );
    let stop = stop_flag(&source);
    let hz = fake_hz();
    thread::Builder::new()
        .name("PartiiFakeMic".to_owned())
        .spawn(move || {
            // 10 ms chunks on a real-time clock, at 0.5 amplitude.
            let chunk = (RATE / 100) as usize;
            let mut buf = vec![0f32; chunk];
            let mut phase = 0f64;
            let step = 2. * std::f64::consts::PI * hz / RATE as f64;
            let start = Instant::now();
            let mut sent = 0u64;
            while !stop.load(Ordering::SeqCst) {
                for v in buf.iter_mut() {
                    *v = (phase.sin() * 0.5) as f32;
                    phase = (phase + step) % (2. * std::f64::consts::PI);
                }
                source.push(&buf, Instant::now());
                sent += chunk as u64;
                let due = start + Duration::from_secs_f64(sent as f64 / RATE as f64);
                let now = Instant::now();
                if due > now {
                    thread::sleep(due - now);
                }
            }
        })
        .expect("spawn the fake mic");
    id
}

#[cfg(not(feature = "device"))]
fn open_device(_: Option<&str>) -> Option<MediaStreamId> {
    None
}

#[cfg(feature = "device")]
fn open_device(device_id: Option<&str>) -> Option<MediaStreamId> {
    use std::sync::mpsc;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, Sample, SampleFormat, SizedSample};

    fn build<T>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        source: AudioSource,
        stop: Arc<AtomicBool>,
    ) -> Result<cpal::Stream, String>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let mut scratch: Vec<f32> = Vec::new();
        let on_error = source.clone();
        device
            .build_input_stream::<T, _, _>(
                config,
                move |data: &[T], _| {
                    scratch.clear();
                    scratch.extend(data.iter().map(|s| f32::from_sample(*s)));
                    // cpal periods are 5-20 ms; split anything longer into 100 ms pushes.
                    let max = (config_rate_channels(&source) / 10).max(1);
                    for part in scratch.chunks(max) {
                        source.push(part, Instant::now());
                    }
                },
                move |error| match error {
                    cpal::StreamError::BufferUnderrun => {},
                    error => {
                        warn!("microphone: {error}");
                        on_error.end();
                        stop.store(true, Ordering::SeqCst);
                    },
                },
                None,
            )
            .map_err(|e| format!("build_input_stream: {e}"))
    }

    fn config_rate_channels(source: &AudioSource) -> usize {
        let f = source.format();
        f.sample_rate as usize * f.channels.max(1) as usize
    }

    // Opened on its own thread: a cpal stream isn't Send on every host, and it lives until unused.
    let (tx, rx) = mpsc::channel::<Option<MediaStreamId>>();
    let wanted = device_id.map(str::to_owned);
    thread::Builder::new()
        .name("PartiiMic".to_owned())
        .spawn(move || {
            #[cfg(target_os = "linux")]
            let alsa_errors = alsa::Output::local_error_handler().ok();
            let host = cpal::default_host();
            let device = wanted
                .as_deref()
                .and_then(|want| {
                    host.input_devices().ok()?.find(|d| {
                        let Ok(desc) = d.description() else {
                            return false;
                        };
                        let name = desc.name().to_string();
                        let key = d
                            .id()
                            .map(|i| i.to_string())
                            .unwrap_or_else(|_| name.clone());
                        super::device_id("audioinput", &key) == want
                    })
                })
                .or_else(|| host.default_input_device());
            let Some(device) = device else {
                let _ = tx.send(None);
                return;
            };
            let label = device
                .description()
                .map(|d| d.name().to_string())
                .unwrap_or_else(|_| "Microphone".to_owned());
            let Ok(supported) = device.default_input_config() else {
                let _ = tx.send(None);
                return;
            };
            let config = supported.config();
            let (id, source) = create_audio_source(
                AudioFormat {
                    sample_rate: config.sample_rate,
                    channels: config.channels,
                },
                &label,
            );
            let stop = stop_flag(&source);
            let stream = match supported.sample_format() {
                SampleFormat::F32 => build::<f32>(&device, &config, source.clone(), stop.clone()),
                SampleFormat::I16 => build::<i16>(&device, &config, source.clone(), stop.clone()),
                SampleFormat::U16 => build::<u16>(&device, &config, source.clone(), stop.clone()),
                SampleFormat::I32 => build::<i32>(&device, &config, source.clone(), stop.clone()),
                SampleFormat::F64 => build::<f64>(&device, &config, source.clone(), stop.clone()),
                SampleFormat::U8 => build::<u8>(&device, &config, source.clone(), stop.clone()),
                SampleFormat::I8 => build::<i8>(&device, &config, source.clone(), stop.clone()),
                other => Err(format!("unsupported sample format {other:?}")),
            };
            let stream = match stream.and_then(|s| match s.play() {
                Ok(()) => Ok(s),
                Err(e) => Err(e.to_string()),
            }) {
                Ok(s) => s,
                Err(e) => {
                    warn!("microphone {label}: {e}");
                    source.end();
                    servo_media_streams::live::unregister_track(&id);
                    let _ = tx.send(None);
                    return;
                },
            };
            #[cfg(target_os = "linux")]
            drop(alsa_errors);
            let _ = tx.send(Some(id));
            while !stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(100));
            }
            drop(stream);
            drop(source);
        })
        .ok()?;
    rx.recv_timeout(Duration::from_secs(10)).ok().flatten()
}
