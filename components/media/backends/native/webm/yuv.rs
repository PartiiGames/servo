/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Planar YUV (4:2:0, 4:2:2, 4:4:4, 4:4:0, monochrome; 8 to 16 bits) to tightly packed BGRA, what
//! the media element's video renderer and texImage2D take.

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// One plane: its first byte, its stride in bytes and its size in samples.
#[derive(Clone, Copy)]
pub(crate) struct Plane {
    pub ptr: *const u8,
    pub stride: usize,
}

/// A decoded picture as the decoder holds it. The planes stay valid while the decoder's picture is.
pub(crate) struct Planes {
    pub y: Plane,
    /// None for monochrome pictures.
    pub uv: Option<(Plane, Plane)>,
    pub width: usize,
    pub height: usize,
    /// Chroma subsampling shifts (1, 1 for 4:2:0).
    pub shift_x: u32,
    pub shift_y: u32,
    /// Samples are little-endian u16 (bit depths above 8).
    pub wide: bool,
    pub bit_depth: u32,
    pub matrix: Matrix,
    pub full_range: bool,
}

/// Fixed-point (16.16) coefficients for one matrix and range, on 8-bit samples.
struct Coeffs {
    y_mul: i32,
    y_off: i32,
    rv: i32,
    gu: i32,
    gv: i32,
    bu: i32,
}

impl Coeffs {
    fn new(matrix: Matrix, full_range: bool) -> Self {
        let (kr, kb) = match matrix {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt2020 => (0.2627, 0.0593),
        };
        let kg = 1. - kr - kb;
        let (y_scale, c_scale, y_off) = if full_range {
            (1., 1., 0)
        } else {
            (255. / 219., 255. / 224., 16)
        };
        let fix = |v: f64| (v * 65536.).round() as i32;
        Coeffs {
            y_mul: fix(y_scale),
            y_off,
            rv: fix(2. * (1. - kr) * c_scale),
            gu: fix(2. * kb * (1. - kb) / kg * c_scale),
            gv: fix(2. * kr * (1. - kr) / kg * c_scale),
            bu: fix(2. * (1. - kb) * c_scale),
        }
    }
}

#[inline]
fn clamp8(v: i32) -> u8 {
    ((v + 32768) >> 16).clamp(0, 255) as u8
}

/// Reads sample `x` of a row as 8 bits.
#[inline]
unsafe fn sample(row: *const u8, x: usize, wide: bool, down: u32) -> i32 {
    unsafe {
        if wide {
            let p = row.add(x * 2);
            (u16::from_le_bytes([*p, *p.add(1)]) >> down) as i32
        } else {
            *row.add(x) as i32
        }
    }
}

// The planes are only read, from the threads of one `to_bgra` call that the caller's borrow outlives.
unsafe impl Sync for Planes {}

/// Converts rows `first..` of the picture into `out` (whole rows of BGRA).
unsafe fn convert_rows(p: &Planes, c: &Coeffs, first: usize, out: &mut [u8]) {
    let down = p.bit_depth.saturating_sub(8);
    for (n, line) in out.chunks_exact_mut(p.width * 4).enumerate() {
        let j = first + n;
        let yrow = unsafe { p.y.ptr.add(j * p.y.stride) };
        let crow = p.uv.map(|(u, v)| {
            let cj = j >> p.shift_y;
            unsafe { (u.ptr.add(cj * u.stride), v.ptr.add(cj * v.stride)) }
        });
        for (i, px) in line.chunks_exact_mut(4).enumerate() {
            let y = (unsafe { sample(yrow, i, p.wide, down) } - c.y_off) * c.y_mul;
            let (u, v) = match crow {
                Some((urow, vrow)) => {
                    let ci = i >> p.shift_x;
                    unsafe {
                        (
                            sample(urow, ci, p.wide, down) - 128,
                            sample(vrow, ci, p.wide, down) - 128,
                        )
                    }
                },
                None => (0, 0),
            };
            px[0] = clamp8(y + c.bu * u);
            px[1] = clamp8(y - c.gu * u - c.gv * v);
            px[2] = clamp8(y + c.rv * v);
            px[3] = 255;
        }
    }
}

/// Converts the picture to BGRA (opaque), in bands on up to 4 threads for large pictures.
///
/// # Safety
/// Each plane must hold `height` (chroma: the subsampled height) rows of `width` (chroma: the
/// subsampled width) samples at its stride.
pub(crate) unsafe fn to_bgra(p: &Planes) -> Vec<u8> {
    let c = Coeffs::new(p.matrix, p.full_range);
    let mut out = vec![0u8; p.width * p.height * 4];
    let threads = if p.width * p.height >= 640 * 360 {
        std::thread::available_parallelism().map_or(1, |n| n.get().min(4))
    } else {
        1
    };
    if threads <= 1 {
        unsafe { convert_rows(p, &c, 0, &mut out) };
        return out;
    }
    // Bands of an even number of rows, so 4:2:0 chroma rows aren't split.
    let band = (p.height.div_ceil(threads) + 1) & !1;
    std::thread::scope(|scope| {
        for (k, chunk) in out.chunks_mut(band * p.width * 4).enumerate() {
            let c = &c;
            scope.spawn(move || unsafe { convert_rows(p, c, k * band, chunk) });
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(y: u8, u: u8, v: u8, full_range: bool) -> [u8; 4] {
        let (ys, us, vs) = ([y; 4], [u; 1], [v; 1]);
        let planes = Planes {
            y: Plane {
                ptr: ys.as_ptr(),
                stride: 2,
            },
            uv: Some((
                Plane {
                    ptr: us.as_ptr(),
                    stride: 1,
                },
                Plane {
                    ptr: vs.as_ptr(),
                    stride: 1,
                },
            )),
            width: 2,
            height: 2,
            shift_x: 1,
            shift_y: 1,
            wide: false,
            bit_depth: 8,
            matrix: Matrix::Bt601,
            full_range,
        };
        let out = unsafe { to_bgra(&planes) };
        assert!(out.chunks_exact(4).all(|px| px == &out[..4]));
        [out[0], out[1], out[2], out[3]]
    }

    #[test]
    fn colors() {
        // Limited-range BT.601 black, white, red, green, blue (BGRA).
        assert_eq!(convert(16, 128, 128, false), [0, 0, 0, 255]);
        assert_eq!(convert(235, 128, 128, false), [255, 255, 255, 255]);
        let red = convert(81, 90, 240, false);
        assert!(red[2] >= 250 && red[1] <= 5 && red[0] <= 5, "{red:?}");
        let green = convert(145, 54, 34, false);
        assert!(green[1] >= 250 && green[0] <= 5 && green[2] <= 5, "{green:?}");
        let blue = convert(41, 240, 110, false);
        assert!(blue[0] >= 250 && blue[1] <= 5 && blue[2] <= 5, "{blue:?}");
        assert_eq!(convert(0, 128, 128, true), [0, 0, 0, 255]);
        assert_eq!(convert(255, 128, 128, true), [255, 255, 255, 255]);
    }
}
