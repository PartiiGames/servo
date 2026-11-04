/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The process-wide audio output: one cpal stream (or a timer-driven null output when there is no
//! device), mixing every registered source. Web Audio sinks and `<audio>` players are sources.
//!
//! `PARTII_AUDIO=null` forces the null output. `PARTII_AUDIO_RATE` sets the null output's rate
//! (default 48000). The null output pulls from every source in real time, so render threads
//! and players run (and AnalyserNodes see real samples) on machines without a sound card.

use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use log::{info, warn};

/// Something the output mixes in. `mix` adds `out.len() / channels` frames of interleaved audio
/// at `rate` into `out`, which may already hold other sources' audio.
pub trait OutputSource: Send + Sync {
    fn mix(&self, out: &mut [f32], channels: usize, rate: u32);
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OutputKind {
    Device,
    Null,
}

pub struct Output {
    sources: Mutex<Vec<Weak<dyn OutputSource>>>,
    format: Mutex<(u32, usize, OutputKind)>,
}

static OUTPUT: OnceLock<Arc<Output>> = OnceLock::new();

const NULL_DEFAULT_RATE: u32 = 48000;
const NULL_PERIOD: Duration = Duration::from_millis(10);

pub fn output() -> Arc<Output> {
    OUTPUT.get_or_init(Output::start).clone()
}

impl Output {
    /// The output's current (rate, channels, kind).
    pub fn format(&self) -> (u32, usize, OutputKind) {
        *self.format.lock().unwrap()
    }

    /// Start mixing `source` in. The output holds only a weak reference: dropping the last
    /// strong reference removes the source.
    pub fn add_source(&self, source: Weak<dyn OutputSource>) {
        self.sources.lock().unwrap().push(source);
    }

    fn render(&self, out: &mut [f32], channels: usize, rate: u32) {
        out.iter_mut().for_each(|s| *s = 0.);
        let sources: Vec<Arc<dyn OutputSource>> = {
            let mut sources = self.sources.lock().unwrap();
            sources.retain(|s| s.strong_count() > 0);
            sources.iter().filter_map(|s| s.upgrade()).collect()
        };
        for source in sources {
            source.mix(out, channels, rate);
        }
        for s in out.iter_mut() {
            *s = s.clamp(-1., 1.);
        }
    }

    fn start() -> Arc<Output> {
        let output = Arc::new(Output {
            sources: Mutex::new(Vec::new()),
            format: Mutex::new((NULL_DEFAULT_RATE, 2, OutputKind::Null)),
        });
        let force_null = std::env::var("PARTII_AUDIO")
            .map(|v| v.eq_ignore_ascii_case("null") || v == "0" || v.eq_ignore_ascii_case("off"))
            .unwrap_or(false);
        let (ready_tx, ready_rx) = mpsc::channel();
        let output_ = output.clone();
        thread::Builder::new()
            .name("PartiiAudioOutput".to_owned())
            .spawn(move || {
                if !force_null {
                    let (err_tx, err_rx) = mpsc::channel();
                    match open_device(output_.clone(), err_tx) {
                        Ok((stream, rate, channels)) => {
                            *output_.format.lock().unwrap() = (rate, channels, OutputKind::Device);
                            info!("audio output: device, {rate} Hz, {channels} ch");
                            let _ = ready_tx.send(());
                            // Keep the stream alive on this thread (cpal streams aren't Send on
                            // every platform) until it reports an error.
                            let error = err_rx.recv();
                            warn!("audio output: device stream failed ({error:?}); using the null output");
                            drop(stream);
                        },
                        Err(error) => {
                            info!("audio output: no usable device ({error}); using the null output");
                        },
                    }
                }
                run_null(output_, ready_tx);
            })
            .expect("Failed to spawn the audio output thread");
        let _ = ready_rx.recv_timeout(Duration::from_secs(5));
        output
    }
}

fn null_rate() -> u32 {
    std::env::var("PARTII_AUDIO_RATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|r| (8000..=384000).contains(r))
        .unwrap_or(NULL_DEFAULT_RATE)
}

/// The null output: pull every source in real time and drop the samples.
fn run_null(output: Arc<Output>, ready: mpsc::Sender<()>) {
    let rate = null_rate();
    let channels = 2;
    *output.format.lock().unwrap() = (rate, channels, OutputKind::Null);
    let _ = ready.send(());
    let max_frames = (rate as usize) / 10;
    let mut buf = vec![0f32; max_frames * channels];
    let start = Instant::now();
    let mut frames_done: u64 = 0;
    loop {
        let due = (start.elapsed().as_secs_f64() * rate as f64) as u64;
        // Never try to catch up more than 100 ms after a stall.
        if due > frames_done + max_frames as u64 {
            frames_done = due - max_frames as u64;
        }
        let frames = (due - frames_done) as usize;
        if frames > 0 {
            let out = &mut buf[..frames * channels];
            output.render(out, channels, rate);
            frames_done += frames as u64;
        }
        thread::sleep(NULL_PERIOD);
    }
}

type DeviceStream = (cpal::Stream, u32, usize);

fn open_device(output: Arc<Output>, err_tx: mpsc::Sender<String>) -> Result<DeviceStream, String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no default output device".to_owned())?;
    let supported = device
        .default_output_config()
        .map_err(|e| format!("default_output_config: {e}"))?;
    let config = supported.config();
    let rate = config.sample_rate;
    let channels = config.channels as usize;
    if channels == 0 {
        return Err("device has no channels".to_owned());
    }
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, &config, output, err_tx),
        SampleFormat::I16 => build::<i16>(&device, &config, output, err_tx),
        SampleFormat::U16 => build::<u16>(&device, &config, output, err_tx),
        SampleFormat::I32 => build::<i32>(&device, &config, output, err_tx),
        SampleFormat::F64 => build::<f64>(&device, &config, output, err_tx),
        SampleFormat::U8 => build::<u8>(&device, &config, output, err_tx),
        SampleFormat::I8 => build::<i8>(&device, &config, output, err_tx),
        other => return Err(format!("unsupported sample format {other:?}")),
    }?;
    stream.play().map_err(|e| format!("play: {e}"))?;
    Ok((stream, rate, channels))
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    output: Arc<Output>,
    err_tx: mpsc::Sender<String>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    let mut scratch: Vec<f32> = Vec::new();
    let err_tx = Mutex::new(err_tx);
    device
        .build_output_stream::<T, _, _>(
            config,
            move |data: &mut [T], _| {
                scratch.resize(data.len(), 0.);
                output.render(&mut scratch, channels, rate);
                for (d, s) in data.iter_mut().zip(scratch.iter()) {
                    *d = T::from_sample(*s);
                }
            },
            move |error| {
                let _ = err_tx.lock().unwrap().send(error.to_string());
            },
            None,
        )
        .map_err(|e| format!("build_output_stream: {e}"))
}

/// Write one stereo frame into an interleaved output frame of `channels` channels.
#[inline]
pub fn add_stereo(frame: &mut [f32], l: f32, r: f32) {
    match frame.len() {
        0 => {},
        1 => frame[0] += 0.5 * (l + r),
        _ => {
            frame[0] += l;
            frame[1] += r;
        },
    }
}
