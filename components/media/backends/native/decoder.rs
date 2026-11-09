/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `decodeAudioData` on symphonia: ogg/vorbis, mp3, wav (PCM, ADPCM), flac, mp3 in mp4,
//! webm/mkv (where the codec is one of these), and Opus in Ogg or WebM with the `opus` feature
//! (codecs.rs). Output is resampled to the context's rate.

use std::io::Cursor;

use servo_media_audio::decoder::{
    AudioDecoder, AudioDecoderCallbacks, AudioDecoderError, AudioDecoderOptions,
};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::resample::resample_offline;

pub struct NativeAudioDecoder;

/// Planar f32 audio at `rate`.
pub struct Decoded {
    pub rate: u32,
    pub channels: Vec<Vec<f32>>,
}

pub fn open_format(data: Vec<u8>) -> Result<Box<dyn FormatReader>, SymError> {
    let mss = MediaSourceStream::new(
        Box::new(Cursor::new(data)),
        MediaSourceStreamOptions::default(),
    );
    let probed = symphonia::default::get_probe().format(
        &Hint::new(),
        mss,
        &FormatOptions {
            enable_gapless: true,
            ..Default::default()
        },
        &MetadataOptions::default(),
    )?;
    Ok(probed.format)
}

/// Decode a whole file into planar f32 at its own rate.
pub fn decode_all(data: Vec<u8>) -> Result<Decoded, AudioDecoderError> {
    let mut format = open_format(data).map_err(|_| AudioDecoderError::InvalidMediaFormat)?;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or(AudioDecoderError::InvalidMediaFormat)?;
    let track_id = track.id;
    let mut decoder = crate::codecs::codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| AudioDecoderError::Backend(format!("no decoder: {e}")))?;
    let mut rate = track.codec_params.sample_rate.unwrap_or(0);
    let mut channels: Vec<Vec<f32>> = Vec::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut sample_buf_key = None;
    let mut decoded_any = false;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => {
                if decoded_any {
                    break;
                }
                return Err(AudioDecoderError::Backend(e.to_string()));
            },
        };
        if packet.track_id() != track_id {
            continue;
        }
        let audio = match decoder.decode(&packet) {
            Ok(a) => a,
            // A corrupt packet: skip it, as players do.
            Err(SymError::DecodeError(_)) => continue,
            Err(SymError::IoError(_)) => continue,
            Err(e) => return Err(AudioDecoderError::Backend(e.to_string())),
        };
        let spec = *audio.spec();
        let n_chans = spec.channels.count();
        if n_chans == 0 {
            continue;
        }
        rate = spec.rate;
        if channels.len() < n_chans {
            channels.resize(n_chans, Vec::new());
        }
        let frames = audio.frames();
        let cap = audio.capacity() as u64;
        if sample_buf_key != Some((cap, spec)) {
            sample_buf = Some(SampleBuffer::new(cap, spec));
            sample_buf_key = Some((cap, spec));
        }
        let buf = sample_buf.as_mut().unwrap();
        buf.copy_planar_ref(audio);
        let samples = buf.samples();
        for (c, ch) in channels.iter_mut().enumerate().take(n_chans) {
            ch.extend_from_slice(&samples[c * frames..(c + 1) * frames]);
        }
        decoded_any = true;
    }
    if !decoded_any || rate == 0 {
        return Err(AudioDecoderError::InvalidMediaFormat);
    }
    // Channels can only grow mid-stream in odd files; pad short ones.
    let len = channels.iter().map(|c| c.len()).max().unwrap_or(0);
    for ch in channels.iter_mut() {
        ch.resize(len, 0.);
    }
    Ok(Decoded { rate, channels })
}

impl AudioDecoder for NativeAudioDecoder {
    fn decode(
        &self,
        data: Vec<u8>,
        callbacks: AudioDecoderCallbacks,
        options: Option<AudioDecoderOptions>,
    ) {
        let options = options.unwrap_or_default();
        let decoded = match decode_all(data) {
            Ok(d) => d,
            Err(e) => return callbacks.error(e),
        };
        let out_rate = options.sample_rate.round() as u32;
        // The resampled result must fit an AudioBuffer (length is an unsigned long). Reject
        // before resampling: an 8 kHz file decoded into a 768 kHz context grows 96x.
        let in_len = decoded.channels.first().map_or(0, |c| c.len()) as u64;
        if out_rate > 0 &&
            decoded.rate > 0 &&
            in_len * out_rate as u64 / decoded.rate as u64 > u32::MAX as u64
        {
            return callbacks.error(AudioDecoderError::Backend(
                "the resampled audio is longer than an AudioBuffer can be".to_owned(),
            ));
        }
        callbacks.ready(decoded.channels.len() as u32);
        for (i, ch) in decoded.channels.into_iter().enumerate() {
            let ch = if out_rate != decoded.rate && out_rate > 0 {
                resample_offline(&ch, decoded.rate, out_rate)
            } else {
                ch
            };
            // The DOM maps the channel position mask back to an index with log2.
            callbacks.progress(Box::new(ch), 1u32 << i);
        }
        callbacks.eos();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16-bit PCM WAV of a 440 Hz sine, `secs` long.
    fn wav(rate: u32, channels: u16, secs: f32) -> Vec<u8> {
        let frames = (rate as f32 * secs) as u32;
        let data_len = frames * channels as u32 * 2;
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        v.extend_from_slice(&(channels * 2).to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            let s = ((i as f32 * 440. * std::f32::consts::TAU / rate as f32).sin() * 16000.) as i16;
            for _ in 0..channels {
                v.extend_from_slice(&s.to_le_bytes());
            }
        }
        v
    }

    #[test]
    fn decodes_wav() {
        let d = decode_all(wav(22050, 2, 0.5)).expect("decodes");
        assert_eq!(d.rate, 22050);
        assert_eq!(d.channels.len(), 2);
        assert_eq!(d.channels[0].len(), 11025);
        let peak = d.channels[1].iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!((peak - 16000. / 32768.).abs() < 0.01, "peak {peak}");
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_all(vec![7u8; 4096]).is_err());
    }
}
