/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Packed RGBA/BGRA (and I420) frames to the encoder's I420 (BT.601, limited range, as libyuv's
//! ARGBToI420 and Chrome's canvas capture). The frame is cropped or padded with black to the
//! recording's size (the size of its first frame) when a later frame differs.

use servo_media_streams::live_video::{VideoFormat, VideoFrame};

/// Fills `out` (w*h Y, then U and V at (w/2)*(h/2)) from `frame`. `w` and `h` are even.
pub(crate) fn to_i420(frame: &VideoFrame, w: usize, h: usize, out: &mut Vec<u8>) -> bool {
    let (cw, ch) = (w / 2, h / 2);
    out.clear();
    out.resize(w * h, 16);
    out.resize(w * h + 2 * cw * ch, 128);
    let fw = (frame.width as usize).min(w);
    let fh = (frame.height as usize).min(h);
    let data = &frame.planes[..];
    match frame.format {
        VideoFormat::Rgba8 | VideoFormat::Bgra8 => {
            let (ri, bi) = if frame.format == VideoFormat::Rgba8 { (0, 2) } else { (2, 0) };
            let stride = frame.strides[0] as usize;
            let base = frame.offsets[0];
            if stride < fw * 4 || data.len() < base + stride * fh.saturating_sub(1) + fw * 4 {
                return false;
            }
            let (y_plane, uv) = out.split_at_mut(w * h);
            let (u_plane, v_plane) = uv.split_at_mut(cw * ch);
            for row in 0..fh {
                let src = &data[base + row * stride..base + row * stride + fw * 4];
                let dst = &mut y_plane[row * w..row * w + fw];
                for (x, px) in src.chunks_exact(4).enumerate() {
                    let (r, g, b) = (px[ri] as i32, px[1] as i32, px[bi] as i32);
                    dst[x] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                }
            }
            for crow in 0..fh / 2 {
                let r0 = base + 2 * crow * stride;
                let r1 = r0 + stride;
                for cx in 0..fw / 2 {
                    let (mut r, mut g, mut b) = (0i32, 0i32, 0i32);
                    for o in [r0 + cx * 8, r0 + cx * 8 + 4, r1 + cx * 8, r1 + cx * 8 + 4] {
                        r += data[o + ri] as i32;
                        g += data[o + 1] as i32;
                        b += data[o + bi] as i32;
                    }
                    let (r, g, b) = ((r + 2) >> 2, (g + 2) >> 2, (b + 2) >> 2);
                    u_plane[crow * cw + cx] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                    v_plane[crow * cw + cx] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
                }
            }
            true
        },
        VideoFormat::I420 | VideoFormat::Nv12 => {
            let ys = frame.strides[0] as usize;
            let (y_plane, uv) = out.split_at_mut(w * h);
            let (u_plane, v_plane) = uv.split_at_mut(cw * ch);
            for row in 0..fh {
                let s = frame.offsets[0] + row * ys;
                let Some(src) = data.get(s..s + fw) else { return false };
                y_plane[row * w..row * w + fw].copy_from_slice(src);
            }
            for crow in 0..fh / 2 {
                for cx in 0..fw / 2 {
                    let (u, v) = if frame.format == VideoFormat::I420 {
                        let us = frame.offsets[1] + crow * frame.strides[1] as usize + cx;
                        let vs = frame.offsets[2] + crow * frame.strides[2] as usize + cx;
                        (data.get(us), data.get(vs))
                    } else {
                        let s = frame.offsets[1] + crow * frame.strides[1] as usize + cx * 2;
                        (data.get(s), data.get(s + 1))
                    };
                    let (Some(u), Some(v)) = (u, v) else { return false };
                    u_plane[crow * cw + cx] = *u;
                    v_plane[crow * cw + cx] = *v;
                }
            }
            true
        },
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn white_and_red() {
        let px: Vec<u8> = [255u8, 255, 255, 255, 0, 0, 255, 255].repeat(4);
        // 4x2 BGRA: columns white, red, white, red.
        let f = VideoFrame::packed(4, 2, VideoFormat::Bgra8, 16, px.into(), Instant::now());
        let mut out = Vec::new();
        assert!(to_i420(&f, 4, 2, &mut out));
        assert_eq!(out.len(), 8 + 4);
        assert_eq!(out[0], 235);
        assert_eq!(out[1], 82);
        // Black padding when the recording is bigger than the frame.
        assert!(to_i420(&f, 6, 2, &mut out));
        assert_eq!(out[5], 16);
    }
}
