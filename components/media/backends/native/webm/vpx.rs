/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! VP8 and VP9 through libvpx (BSD-3-Clause), the vendored 1.17.0 that partii-libvpx-sys builds and
//! links statically. Only the decoder half of the API is declared here.

use std::ffi::{CStr, c_char, c_int, c_long, c_uint, c_void};
use std::ptr;

use partii_libvpx_sys as _;

use super::yuv::{self, Matrix, Plane, Planes};
use super::{Picture, VideoDecoder};

/// `vpx_image_t` (vpx_image.h, VPX_IMAGE_ABI_VERSION 5).
#[repr(C)]
struct VpxImage {
    fmt: c_uint,
    cs: c_uint,
    range: c_uint,
    w: c_uint,
    h: c_uint,
    bit_depth: c_uint,
    d_w: c_uint,
    d_h: c_uint,
    r_w: c_uint,
    r_h: c_uint,
    x_chroma_shift: c_uint,
    y_chroma_shift: c_uint,
    planes: [*mut u8; 4],
    stride: [c_int; 4],
    bps: c_int,
    user_priv: *mut c_void,
    img_data: *mut u8,
    img_data_owner: c_int,
    self_allocd: c_int,
    fb_priv: *mut c_void,
}

/// `vpx_codec_dec_cfg_t`.
#[repr(C)]
struct DecCfg {
    threads: c_uint,
    w: c_uint,
    h: c_uint,
}

/// `vpx_codec_ctx_t` is seven pointer-sized fields; this is room for it, zeroed, as libvpx wants.
#[repr(C, align(16))]
struct Ctx([u8; 256]);

unsafe extern "C" {
    fn vpx_codec_vp8_dx() -> *const c_void;
    fn vpx_codec_vp9_dx() -> *const c_void;
    fn vpx_codec_dec_init_ver(
        ctx: *mut Ctx,
        iface: *const c_void,
        cfg: *const DecCfg,
        flags: c_long,
        ver: c_int,
    ) -> c_int;
    fn vpx_codec_decode(
        ctx: *mut Ctx,
        data: *const u8,
        data_sz: c_uint,
        user_priv: *mut c_void,
        deadline: c_long,
    ) -> c_int;
    fn vpx_codec_get_frame(ctx: *mut Ctx, iter: *mut *const c_void) -> *const VpxImage;
    fn vpx_codec_destroy(ctx: *mut Ctx) -> c_int;
    fn vpx_codec_error(ctx: *const Ctx) -> *const c_char;
}

/// vpx_decoder.h: 3 + VPX_CODEC_ABI_VERSION (4 + VPX_IMAGE_ABI_VERSION 5), libvpx 1.17.0.
const VPX_DECODER_ABI_VERSION: c_int = 12;
const VPX_IMG_FMT_HIGHBITDEPTH: c_uint = 0x800;
const VPX_IMG_FMT_PLANAR: c_uint = 0x100;
// vpx_color_space_t
const VPX_CS_BT_709: c_uint = 2;
const VPX_CS_BT_2020: c_uint = 5;
const VPX_CR_FULL_RANGE: c_uint = 1;

pub(crate) struct VpxDecoder {
    ctx: Box<Ctx>,
}

// The context is only used from the player thread that owns the decoder.
unsafe impl Send for VpxDecoder {}

impl VpxDecoder {
    pub(crate) fn new(vp9: bool) -> Result<Self, String> {
        let mut ctx = Box::new(Ctx([0; 256]));
        let iface = unsafe {
            if vp9 {
                vpx_codec_vp9_dx()
            } else {
                vpx_codec_vp8_dx()
            }
        };
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().min(4))
            .unwrap_or(1) as c_uint;
        let cfg = DecCfg {
            threads,
            w: 0,
            h: 0,
        };
        let rc = unsafe {
            vpx_codec_dec_init_ver(&mut *ctx, iface, &cfg, 0, VPX_DECODER_ABI_VERSION)
        };
        if rc != 0 {
            return Err(format!("libvpx: decoder init failed ({rc})"));
        }
        Ok(VpxDecoder { ctx })
    }

    fn error(&self) -> String {
        let msg = unsafe { vpx_codec_error(&*self.ctx) };
        if msg.is_null() {
            return "libvpx: decode error".to_owned();
        }
        format!(
            "libvpx: {}",
            unsafe { CStr::from_ptr(msg) }.to_string_lossy()
        )
    }
}

impl Drop for VpxDecoder {
    fn drop(&mut self) {
        unsafe { vpx_codec_destroy(&mut *self.ctx) };
    }
}

impl VideoDecoder for VpxDecoder {
    fn decode(&mut self, data: &[u8], ts: f64, out: &mut Vec<Picture>) -> Result<(), String> {
        let rc = unsafe {
            vpx_codec_decode(
                &mut *self.ctx,
                data.as_ptr(),
                data.len() as c_uint,
                ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(self.error());
        }
        let mut iter: *const c_void = ptr::null();
        loop {
            let img = unsafe { vpx_codec_get_frame(&mut *self.ctx, &mut iter) };
            if img.is_null() {
                break;
            }
            let img = unsafe { &*img };
            if img.fmt & VPX_IMG_FMT_PLANAR == 0 || img.d_w == 0 || img.d_h == 0 {
                continue;
            }
            let wide = img.fmt & VPX_IMG_FMT_HIGHBITDEPTH != 0;
            let plane = |i: usize| Plane {
                ptr: img.planes[i],
                stride: img.stride[i] as usize,
            };
            let planes = Planes {
                y: plane(0),
                uv: Some((plane(1), plane(2))),
                width: img.d_w as usize,
                height: img.d_h as usize,
                shift_x: img.x_chroma_shift,
                shift_y: img.y_chroma_shift,
                wide,
                bit_depth: if wide { img.bit_depth.max(8) } else { 8 },
                matrix: match img.cs {
                    VPX_CS_BT_709 => Matrix::Bt709,
                    VPX_CS_BT_2020 => Matrix::Bt2020,
                    _ => Matrix::Bt601,
                },
                full_range: img.range == VPX_CR_FULL_RANGE,
            };
            out.push(Picture {
                ts,
                width: img.d_w,
                height: img.d_h,
                bgra: unsafe { yuv::to_bgra(&planes) },
            });
        }
        Ok(())
    }

    fn reset(&mut self) {
        // Seeks land on keyframes, which reset libvpx's references.
    }
}
