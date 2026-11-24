/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The fake camera (Chrome's `--use-fake-device-for-media-stream`, for tests and the farm).
//!
//! Frame `seq` (0, 1, 2…) at W×H, every colour 0 or 255 so it survives any copy exactly:
//! - rows `y < H*3/4`: 8 vertical bars, bar `x*8/W`: white, yellow, cyan, green, magenta, red, blue, black;
//! - rows `H*3/4 <= y < H*7/8`: 16 cells, cell `x*16/W` white when bit `cell` of `seq` is set, else black;
//! - rows `y >= H*7/8`: black, with a white block over `x` in `[(seq*8) % W, (seq*8) % W + W/16)`.
//!
//! The test page (partii-runtime harness/capture) rebuilds the same picture in JS.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use super::{Camera, CameraInfo, CameraSettings, FrameSink, Want};

pub(crate) const HANDLE: &str = "fake";
pub(crate) const LABEL: &str = "Partii fake camera";

pub(crate) fn info() -> CameraInfo {
    CameraInfo {
        id: super::device_id("videoinput", HANDLE),
        label: LABEL.to_owned(),
        facing: "user".to_owned(),
        handle: HANDLE.to_owned(),
    }
}

const BARS: [[u8; 3]; 8] = [
    [255, 255, 255],
    [255, 255, 0],
    [0, 255, 255],
    [0, 255, 0],
    [255, 0, 255],
    [255, 0, 0],
    [0, 0, 255],
    [0, 0, 0],
];

/// The fake frame `seq` as BGRA8.
pub(crate) fn frame(width: u32, height: u32, seq: u64) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u8; w * h * 4];
    let bits_top = h * 3 / 4;
    let block_top = h * 7 / 8;
    let block_x = ((seq * 8) % width as u64) as usize;
    let block_w = (w / 16).max(1);
    for y in 0..h {
        let row = &mut out[y * w * 4..(y + 1) * w * 4];
        for x in 0..w {
            let rgb = if y < bits_top {
                BARS[x * 8 / w]
            } else if y < block_top {
                if seq >> (x * 16 / w) & 1 == 1 {
                    [255; 3]
                } else {
                    [0; 3]
                }
            } else if x >= block_x && x < block_x + block_w {
                [255; 3]
            } else {
                [0; 3]
            };
            let d = &mut row[x * 4..x * 4 + 4];
            d[0] = rgb[2];
            d[1] = rgb[1];
            d[2] = rgb[0];
            d[3] = 255;
        }
    }
    out
}

pub(crate) fn open(info: CameraInfo, want: &Want, sink: Arc<dyn FrameSink>) -> Camera {
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
    let frame_rate = want.frame_rate.unwrap_or(30.).clamp(1., 60.);
    let period = Duration::from_secs_f64(1. / frame_rate);
    thread::Builder::new()
        .name("PartiiFakeCamera".to_owned())
        .spawn(move || {
            let mut seq = 0u64;
            let mut next = Instant::now();
            while sink.live() {
                if sink.has_readers() {
                    let data: Arc<[u8]> = frame(width, height, seq).into();
                    sink.push_bgra(width, height, width as usize * 4, data);
                }
                seq += 1;
                next += period;
                let now = Instant::now();
                if next > now {
                    thread::sleep(next - now);
                } else {
                    next = now;
                }
            }
        })
        .expect("spawn the fake camera");
    Camera {
        settings: CameraSettings {
            width,
            height,
            frame_rate,
            info,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern() {
        let f = frame(64, 32, 5);
        let px = |x: usize, y: usize| &f[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4];
        assert_eq!(px(0, 0), [255, 255, 255, 255]); // white bar
        assert_eq!(px(8, 0), [0, 255, 255, 255]); // yellow (BGRA)
        assert_eq!(px(63, 0), [0, 0, 0, 255]); // black bar
        assert_eq!(px(0, 24), [255, 255, 255, 255]); // bit 0 of 5
        assert_eq!(px(4, 24), [0, 0, 0, 255]); // bit 1
        assert_eq!(px(8, 24), [255, 255, 255, 255]); // bit 2
        assert_eq!(px(40, 30), [255, 255, 255, 255]); // block at 40..44
        assert_eq!(px(39, 30), [0, 0, 0, 255]);
    }
}
