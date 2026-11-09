/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `<video>`: the video track of a WebM/Matroska file, demuxed with matroska-demuxer and decoded
//! to BGRA pictures (VP8/VP9 through libvpx with the `vpx` feature, AV1 through rav1d with `av1`).
//! The player (player.rs) times the pictures against its clock and hands them to the renderer.

#[cfg(feature = "av1")]
mod av1;
#[cfg(feature = "vpx")]
mod vpx;
mod yuv;

use std::io::Cursor;
use std::sync::Arc;

use matroska_demuxer::{Frame, MatroskaFile, TrackType};

use crate::player::SharedBytes;

/// A decoded picture, BGRA, tightly packed.
pub(crate) struct Picture {
    /// Presentation time in seconds.
    pub ts: f64,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

pub(crate) trait VideoDecoder: Send {
    /// Decodes one frame (a block's data), appending any pictures it shows, stamped `ts`.
    fn decode(&mut self, data: &[u8], ts: f64, out: &mut Vec<Picture>) -> Result<(), String>;
    /// Forgets pending state before a seek (decoding restarts at a keyframe).
    fn reset(&mut self);
    /// At the end of the stream: the pictures the decoder still holds (frame threading).
    fn drain(&mut self, _out: &mut Vec<Picture>) {}
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Codec {
    Vp8,
    Vp9,
    Av1,
}

impl Codec {
    fn from_codec_id(id: &str) -> Option<Codec> {
        match id {
            "V_VP8" => Some(Codec::Vp8),
            "V_VP9" => Some(Codec::Vp9),
            "V_AV1" => Some(Codec::Av1),
            _ => None,
        }
    }

    fn decoder(self) -> Result<Box<dyn VideoDecoder>, String> {
        match self {
            #[cfg(feature = "vpx")]
            Codec::Vp8 | Codec::Vp9 => Ok(Box::new(vpx::VpxDecoder::new(self == Codec::Vp9)?)),
            #[cfg(feature = "av1")]
            Codec::Av1 => Ok(Box::new(av1::Av1Decoder::new()?)),
            #[allow(unreachable_patterns)]
            other => Err(format!("{other:?} video is not built in")),
        }
    }

    /// Whether a frame is a keyframe, from its bitstream (for blocks that don't say).
    fn is_keyframe(self, data: &[u8]) -> Option<bool> {
        let b = *data.first()?;
        match self {
            // VP8 frame tag: bit 0 is the frame type, 0 = key frame.
            Codec::Vp8 => Some(b & 1 == 0),
            // VP9 uncompressed header: frame_marker(2) profile_low(1) profile_high(1)
            // [reserved_zero(1) if profile 3] show_existing_frame(1) frame_type(1), 0 = key frame.
            Codec::Vp9 => {
                let profile = ((b >> 5) & 1) | (((b >> 4) & 1) << 1);
                let mut bit = if profile == 3 { 3 } else { 4 };
                let show_existing = (b >> (bit - 1)) & 1;
                if show_existing == 1 {
                    return Some(false);
                }
                bit -= 1;
                Some((b >> (bit - 1)) & 1 == 0)
            },
            Codec::Av1 => None,
        }
    }
}

/// Whether a `codecs=` value names a video codec this build decodes.
pub(crate) fn codec_supported(codec: &str) -> bool {
    let codec = codec.to_ascii_lowercase();
    if codec == "vp8" || codec == "vp8.0" || codec == "vp9" || codec == "vp9.0" ||
        codec.starts_with("vp09.")
    {
        return cfg!(feature = "vpx");
    }
    if codec == "av1" || codec.starts_with("av01.") {
        return cfg!(feature = "av1");
    }
    false
}

/// Whether any video codec is built in (video/webm is then playable).
pub(crate) fn any_decoder() -> bool {
    cfg!(any(feature = "vpx", feature = "av1"))
}

pub(crate) struct VideoStream {
    file: MatroskaFile<SharedBytes>,
    bytes: Arc<Vec<u8>>,
    track: u64,
    codec: Codec,
    /// Seconds per timestamp tick.
    scale: f64,
    decoder: Box<dyn VideoDecoder>,
    /// Keyframe timestamps (ticks), ascending.
    keyframes: Vec<u64>,
    frame: Frame,
    pub width: u32,
    pub height: u32,
    pub duration: Option<f64>,
    /// The demuxer reached the end of the file.
    pub finished: bool,
    /// Decoding waits for a keyframe (after an error).
    need_keyframe: bool,
    /// The decoder was drained at the end of the stream.
    drained: bool,
}

fn open_file(bytes: &Arc<Vec<u8>>) -> Result<MatroskaFile<SharedBytes>, String> {
    MatroskaFile::open(SharedBytes(Cursor::new(bytes.clone())))
        .map_err(|e| format!("webm: {e}"))
}

impl VideoStream {
    /// Opens the first video track of a Matroska/WebM file. Errors if there isn't one this build
    /// decodes.
    pub(crate) fn open(bytes: Arc<Vec<u8>>) -> Result<Self, String> {
        // EBML magic: not Matroska, nothing to do here.
        if !bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
            return Err("not a WebM/Matroska file".to_owned());
        }
        let file = open_file(&bytes)?;
        let entry = file
            .tracks()
            .iter()
            .find(|t| {
                t.track_type() == TrackType::Video && Codec::from_codec_id(t.codec_id()).is_some()
            })
            .ok_or_else(|| "no VP8/VP9/AV1 video track".to_owned())?;
        let codec = Codec::from_codec_id(entry.codec_id()).unwrap();
        let track = entry.track_number().get();
        let (width, height) = entry
            .video()
            .map(|v| (v.pixel_width().get() as u32, v.pixel_height().get() as u32))
            .unwrap_or((0, 0));
        let scale = file.info().timestamp_scale().get() as f64 / 1e9;
        let duration = file.info().duration().map(|d| d * scale);
        let decoder = codec.decoder()?;

        // Index the keyframes (seeking starts decoding at the one before the target).
        let mut keyframes = Vec::new();
        let mut index = open_file(&bytes)?;
        let mut frame = Frame::default();
        while let Ok(true) = index.next_frame(&mut frame) {
            if frame.track != track {
                continue;
            }
            let key = frame
                .is_keyframe
                .or_else(|| codec.is_keyframe(&frame.data))
                .unwrap_or(keyframes.is_empty());
            if key {
                keyframes.push(frame.timestamp);
            }
        }

        Ok(VideoStream {
            file,
            bytes,
            track,
            codec,
            scale,
            decoder,
            keyframes,
            frame,
            width,
            height,
            duration,
            finished: false,
            need_keyframe: false,
            drained: false,
        })
    }

    /// Decodes the next frame of the track into `out` (zero or more pictures). False at the end.
    pub(crate) fn next(&mut self, out: &mut Vec<Picture>) -> bool {
        loop {
            match self.file.next_frame(&mut self.frame) {
                Ok(true) => {},
                Ok(false) | Err(_) => {
                    if !self.drained {
                        self.drained = true;
                        let before = out.len();
                        self.decoder.drain(out);
                        if out.len() > before {
                            return true;
                        }
                    }
                    self.finished = true;
                    return false;
                },
            }
            if self.frame.track != self.track {
                continue;
            }
            let ts = self.frame.timestamp as f64 * self.scale;
            if self.need_keyframe {
                if self.keyframes.binary_search(&self.frame.timestamp).is_err() {
                    continue;
                }
                self.need_keyframe = false;
            }
            if let Err(e) = self.decoder.decode(&self.frame.data, ts, out) {
                log::warn!("video: {e} at {ts:.3}s");
                self.decoder.reset();
                self.need_keyframe = true;
            }
            return true;
        }
    }

    /// Restarts decoding at the last keyframe at or before `time` seconds.
    pub(crate) fn seek(&mut self, time: f64) {
        let target = (time / self.scale).max(0.) as u64;
        let key = match self.keyframes.binary_search(&target) {
            Ok(i) => self.keyframes[i],
            Err(0) => 0,
            Err(i) => self.keyframes[i - 1],
        };
        self.decoder.reset();
        self.finished = false;
        self.need_keyframe = false;
        self.drained = false;
        let sought = if key == 0 {
            Err(())
        } else {
            self.file.seek(key).map_err(|_| ())
        };
        if sought.is_err() {
            // From the start (seeking to 0, or a file the demuxer can't seek in).
            match open_file(&self.bytes) {
                Ok(file) => self.file = file,
                Err(_) => self.finished = true,
            }
        }
        log::debug!(
            "video: {:?} seek to {time:.3}s from keyframe {:.3}s",
            self.codec,
            key as f64 * self.scale
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframe_bits() {
        // VP9 profile 0 keyframe 0x82 (10 0 0 0 0 1 0): frame_type 0; inter frame 0x86.
        assert_eq!(Codec::Vp9.is_keyframe(&[0x82]), Some(true));
        assert_eq!(Codec::Vp9.is_keyframe(&[0x86]), Some(false));
        // show_existing_frame
        assert_eq!(Codec::Vp9.is_keyframe(&[0x88]), Some(false));
        assert_eq!(Codec::Vp8.is_keyframe(&[0x10]), Some(true));
        assert_eq!(Codec::Vp8.is_keyframe(&[0x11]), Some(false));
    }

    #[test]
    fn codecs() {
        assert_eq!(codec_supported("vp8"), cfg!(feature = "vpx"));
        assert_eq!(codec_supported("vp09.00.10.08"), cfg!(feature = "vpx"));
        assert_eq!(codec_supported("av01.0.04M.08"), cfg!(feature = "av1"));
        assert!(!codec_supported("avc1.42E01E"));
    }

    /// PARTII_VIDEO_DIR=<dir of .webm>: decodes each file (all pictures, BGRA included) and prints
    /// the rate; the probe clips (vp8.webm, vp9-opus.webm, av1.webm: red, then lime at 0.5 s) also
    /// check the colours and a seek. `cargo test -p servo-media-native --features vpx,av1 --release
    /// -- --ignored --nocapture decode_dir`.
    #[test]
    #[ignore]
    fn decode_dir() {
        let Ok(dir) = std::env::var("PARTII_VIDEO_DIR") else {
            return;
        };
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "webm"))
            .collect();
        names.sort();
        for path in names {
            let bytes = Arc::new(std::fs::read(&path).unwrap());
            let mut stream = match VideoStream::open(bytes) {
                Ok(s) => s,
                Err(e) => {
                    println!("{}: {e}", path.display());
                    continue;
                },
            };
            let start = std::time::Instant::now();
            let mut pics = Vec::new();
            let mut n = 0;
            let mut first = None;
            let mut last_ts = 0.;
            while stream.next(&mut pics) {
                for p in pics.drain(..) {
                    if first.is_none() {
                        first = Some(p.bgra[..4].to_vec());
                    }
                    last_ts = p.ts;
                    n += 1;
                }
            }
            let secs = start.elapsed().as_secs_f64();
            println!(
                "DECODE {} {:?} {}x{}: {n} pictures to {last_ts:.3}s in {secs:.3}s = {:.1} fps, {} keyframes",
                path.file_name().unwrap().to_string_lossy(),
                stream.codec,
                stream.width,
                stream.height,
                n as f64 / secs,
                stream.keyframes.len()
            );
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if matches!(name.as_str(), "vp8.webm" | "vp9-opus.webm" | "av1.webm") {
                assert_eq!(n, 30, "{name}");
                let red = first.unwrap();
                assert!(red[2] > 190 && red[1] < 70 && red[0] < 70, "{name} first {red:?}");
                stream.seek(0.75);
                let mut lime = None;
                while lime.is_none() && stream.next(&mut pics) {
                    for p in pics.drain(..) {
                        if p.ts >= 0.75 - 1e-6 && lime.is_none() {
                            lime = Some((p.ts, p.bgra[..4].to_vec()));
                        }
                    }
                }
                let (ts, px) = lime.expect("a picture after the seek");
                assert!((ts - 0.7667).abs() < 0.02, "{name} seek ts {ts}");
                assert!(px[1] > 190 && px[0] < 70 && px[2] < 70, "{name} after seek {px:?}");
            }
        }
    }
}
