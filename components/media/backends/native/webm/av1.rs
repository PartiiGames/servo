/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! AV1 through rav1d (BSD-2-Clause), the Rust port of dav1d, through its dav1d-compatible API.

use std::mem::MaybeUninit;
use std::ptr::{self, NonNull};

use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{
    DAV1D_MC_BT709, DAV1D_MC_BT2020_CL, DAV1D_MC_BT2020_NCL, DAV1D_PIXEL_LAYOUT_I400,
    DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422,
};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_flush,
    dav1d_get_picture, dav1d_open, dav1d_picture_unref, dav1d_send_data,
};

use super::yuv::{self, Matrix, Plane, Planes};
use super::{Picture, VideoDecoder};

pub(crate) struct Av1Decoder {
    ctx: Option<Dav1dContext>,
}

// rav1d's context is thread-safe (Rav1dContext is Send); it is only used from the player thread.
unsafe impl Send for Av1Decoder {}

const EAGAIN: i32 = -libc::EAGAIN;

impl Av1Decoder {
    pub(crate) fn new() -> Result<Self, String> {
        let mut settings = MaybeUninit::<Dav1dSettings>::uninit();
        let mut settings = unsafe {
            dav1d_default_settings(NonNull::new_unchecked(settings.as_mut_ptr()));
            settings.assume_init()
        };
        settings.n_threads = std::thread::available_parallelism()
            .map(|n| n.get().min(4))
            .unwrap_or(1) as i32;
        // Frame threading (rav1d picks the delay from n_threads): pictures come out a few frames
        // after their data, with their timestamps, and drain() takes the last ones.
        settings.max_frame_delay = 0;
        let mut ctx: Option<Dav1dContext> = None;
        let rc = unsafe { dav1d_open(Some(NonNull::from(&mut ctx)), Some(NonNull::from(&mut settings))) };
        if rc.0 != 0 || ctx.is_none() {
            return Err(format!("rav1d: open failed ({})", rc.0));
        }
        Ok(Av1Decoder { ctx })
    }

    /// Takes every picture rav1d has ready.
    fn collect(&mut self, out: &mut Vec<Picture>) -> Result<(), String> {
        loop {
            let mut pic = Dav1dPicture::default();
            let rc = unsafe { dav1d_get_picture(self.ctx, Some(NonNull::from(&mut pic))) };
            if rc.0 == EAGAIN {
                return Ok(());
            }
            if rc.0 != 0 {
                return Err(format!("rav1d: decode error ({})", rc.0));
            }
            out.extend(convert(&pic));
            unsafe { dav1d_picture_unref(Some(NonNull::from(&mut pic))) };
        }
    }
}

fn convert(pic: &Dav1dPicture) -> Option<Picture> {
    let (w, h) = (pic.p.w as usize, pic.p.h as usize);
    let y = pic.data[0]?;
    if w == 0 || h == 0 {
        return None;
    }
    let seq = pic.seq_hdr.map(|s| unsafe { s.as_ref() });
    let (shift_x, shift_y) = match pic.p.layout {
        DAV1D_PIXEL_LAYOUT_I420 => (1, 1),
        DAV1D_PIXEL_LAYOUT_I422 => (1, 0),
        _ => (0, 0),
    };
    let uv = if pic.p.layout == DAV1D_PIXEL_LAYOUT_I400 {
        None
    } else {
        Some((
            Plane {
                ptr: pic.data[1]?.as_ptr() as *const u8,
                stride: pic.stride[1] as usize,
            },
            Plane {
                ptr: pic.data[2]?.as_ptr() as *const u8,
                stride: pic.stride[1] as usize,
            },
        ))
    };
    let planes = Planes {
        y: Plane {
            ptr: y.as_ptr() as *const u8,
            stride: pic.stride[0] as usize,
        },
        uv,
        width: w,
        height: h,
        shift_x,
        shift_y,
        wide: pic.p.bpc > 8,
        bit_depth: pic.p.bpc as u32,
        matrix: match seq.map(|s| s.mtrx) {
            Some(DAV1D_MC_BT709) => Matrix::Bt709,
            Some(DAV1D_MC_BT2020_NCL) | Some(DAV1D_MC_BT2020_CL) => Matrix::Bt2020,
            _ => Matrix::Bt601,
        },
        full_range: seq.is_some_and(|s| s.color_range != 0),
    };
    Some(Picture {
        ts: pic.m.timestamp as f64 / 1e9,
        width: w as u32,
        height: h as u32,
        bgra: unsafe { yuv::to_bgra(&planes) },
    })
}

impl Drop for Av1Decoder {
    fn drop(&mut self) {
        unsafe { dav1d_close(Some(NonNull::from(&mut self.ctx))) };
    }
}

impl VideoDecoder for Av1Decoder {
    fn decode(&mut self, data: &[u8], ts: f64, out: &mut Vec<Picture>) -> Result<(), String> {
        if data.is_empty() {
            return Ok(());
        }
        let mut buf = Dav1dData::default();
        let dst = unsafe { dav1d_data_create(Some(NonNull::from(&mut buf)), data.len()) };
        if dst.is_null() {
            return Err("rav1d: out of memory".to_owned());
        }
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len()) };
        // Nanoseconds, carried to the picture.
        buf.m.timestamp = (ts * 1e9).round() as i64;
        let mut tries = 0;
        let result = loop {
            tries += 1;
            if tries > 64 {
                break Err("rav1d: decoder stalled".to_owned());
            }
            let rc = unsafe { dav1d_send_data(self.ctx, Some(NonNull::from(&mut buf))) };
            if rc.0 != 0 && rc.0 != EAGAIN {
                break Err(format!("rav1d: bad data ({})", rc.0));
            }
            if let Err(e) = self.collect(out) {
                break Err(e);
            }
            if rc.0 == 0 && buf.sz == 0 {
                break Ok(());
            }
        };
        if buf.sz > 0 {
            unsafe { dav1d_data_unref(Some(NonNull::from(&mut buf))) };
        }
        result
    }

    fn drain(&mut self, out: &mut Vec<Picture>) {
        if let Err(e) = self.collect(out) {
            log::warn!("video: {e} draining");
        }
    }

    fn reset(&mut self) {
        if let Some(ctx) = self.ctx {
            unsafe { dav1d_flush(ctx) };
        }
    }
}
