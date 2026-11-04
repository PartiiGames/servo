/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The sound device, through cpal (feature `device`, on by default). Without the feature there is
//! no device code and no libasound link: the output is always the null output.

#[cfg(feature = "device")]
pub(crate) use imp::*;

/// Stands in for the cpal stream when there is no device support.
#[cfg(not(feature = "device"))]
pub(crate) struct NoStream;

#[cfg(not(feature = "device"))]
pub(crate) fn open(
    _output: std::sync::Arc<crate::output::Output>,
    _err_tx: std::sync::mpsc::Sender<String>,
) -> Result<(NoStream, u32, usize), String> {
    Err("built without device output (servo-media-native feature `device`)".to_owned())
}

#[cfg(feature = "device")]
mod imp {
    use std::sync::{Arc, Mutex, mpsc};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample};
    use log::debug;

    use crate::output::Output;

    /// `open_device`, with alsa-lib's probe errors (no card, unknown PCM) collected on this thread and
    /// logged at debug level instead of printed to stderr.
    pub(crate) fn open(
        output: Arc<Output>,
        err_tx: mpsc::Sender<String>,
    ) -> Result<DeviceStream, String> {
        #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd"
        ))]
        let alsa_errors = alsa::Output::local_error_handler().ok();
        let opened = open_device(output, err_tx);
        #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd"
        ))]
        if let Some(errors) = alsa_errors {
            let text = errors.borrow().to_string();
            if !text.trim().is_empty() {
                debug!("audio output: alsa-lib said: {}", text.trim());
            }
        }
        opened
    }

    pub(crate) type DeviceStream = (cpal::Stream, u32, usize);

    fn open_device(
        output: Arc<Output>,
        err_tx: mpsc::Sender<String>,
    ) -> Result<DeviceStream, String> {
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
                move |error| match error {
                    // A glitch, not a dead stream.
                    cpal::StreamError::BufferUnderrun => debug!("audio output: underrun"),
                    error => {
                        let _ = err_tx.lock().unwrap().send(error.to_string());
                    },
                },
                None,
            )
            .map_err(|e| format!("build_output_stream: {e}"))
    }
}
