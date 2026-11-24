/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Linux cameras through V4L2 (raw ioctls; no libv4l). Streaming I/O with mmap'd buffers, YUYV,
//! converted to BGRA8 on the capture thread. v4l2loopback devices work the same way.

use std::ffi::CStr;
use std::fs;
use std::os::raw::{c_int, c_ulong, c_void};
use std::sync::Arc;
use std::thread;

use log::{debug, warn};

use super::{Camera, CameraInfo, CameraSettings, CaptureError, FrameSink, Want, yuyv_to_bgra};

const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
const FIELD_NONE: u32 = 1;
const CAP_VIDEO_CAPTURE: u32 = 0x1;
const CAP_STREAMING: u32 = 0x0400_0000;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const fn fourcc(s: &[u8; 4]) -> u32 {
    s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}
const PIX_YUYV: u32 = fourcc(b"YUYV");

#[repr(C)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

#[repr(C)]
union FormatUnion {
    pix: PixFormat,
    raw: [u64; 25],
}

#[repr(C)]
struct Format {
    type_: u32,
    fmt: FormatUnion,
}

#[repr(C)]
struct RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Timecode {
    type_: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

#[repr(C)]
union BufferM {
    offset: u32,
    userptr: c_ulong,
    planes: *mut c_void,
    fd: i32,
}

#[repr(C)]
struct Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: libc::timeval,
    timecode: Timecode,
    sequence: u32,
    memory: u32,
    m: BufferM,
    length: u32,
    reserved2: u32,
    request_fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Fract {
    numerator: u32,
    denominator: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CaptureParm {
    capability: u32,
    capturemode: u32,
    timeperframe: Fract,
    extendedmode: u32,
    readbuffers: u32,
    reserved: [u32; 4],
}

#[repr(C)]
union ParmUnion {
    capture: CaptureParm,
    raw: [u8; 200],
}

#[repr(C)]
struct StreamParm {
    type_: u32,
    parm: ParmUnion,
}

const fn ioc(dir: c_ulong, nr: c_ulong, size: usize) -> c_ulong {
    (dir << 30) | ((size as c_ulong) << 16) | ((b'V' as c_ulong) << 8) | nr
}
const R: c_ulong = 2;
const W: c_ulong = 1;
const RW: c_ulong = 3;
const VIDIOC_QUERYCAP: c_ulong = ioc(R, 0, size_of::<Capability>());
const VIDIOC_S_FMT: c_ulong = ioc(RW, 5, size_of::<Format>());
const VIDIOC_REQBUFS: c_ulong = ioc(RW, 8, size_of::<RequestBuffers>());
const VIDIOC_QUERYBUF: c_ulong = ioc(RW, 9, size_of::<Buffer>());
const VIDIOC_QBUF: c_ulong = ioc(RW, 15, size_of::<Buffer>());
const VIDIOC_DQBUF: c_ulong = ioc(RW, 17, size_of::<Buffer>());
const VIDIOC_STREAMON: c_ulong = ioc(W, 18, size_of::<c_int>());
const VIDIOC_STREAMOFF: c_ulong = ioc(W, 19, size_of::<c_int>());
const VIDIOC_S_PARM: c_ulong = ioc(RW, 22, size_of::<StreamParm>());

fn xioctl<T>(fd: c_int, req: c_ulong, arg: &mut T) -> Result<(), std::io::Error> {
    loop {
        // SAFETY: `arg` is the ioctl's argument struct, laid out as the kernel's (repr(C)).
        let r = unsafe { libc::ioctl(fd, req as _, arg as *mut T as *mut c_void) };
        if r != -1 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    }
}

fn zeroed<T>() -> T {
    // SAFETY: only used for the plain-data ioctl structs above, where all-zero is valid.
    unsafe { std::mem::zeroed() }
}

struct Fd(c_int);
impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: we own the descriptor.
        unsafe { libc::close(self.0) };
    }
}

fn open_dev(path: &str) -> Option<Fd> {
    let c = std::ffi::CString::new(path).ok()?;
    // SAFETY: a NUL-terminated path.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    (fd >= 0).then_some(Fd(fd))
}

fn cstr(b: &[u8]) -> String {
    CStr::from_bytes_until_nul(b)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// /dev/video* nodes that capture video with streaming I/O (metadata nodes of UVC cameras are skipped).
pub(super) fn list() -> Vec<CameraInfo> {
    let mut paths: Vec<(u32, String)> = fs::read_dir("/dev")
        .map(|d| {
            d.filter_map(|e| {
                let name = e.ok()?.file_name().into_string().ok()?;
                let n = name.strip_prefix("video")?.parse().ok()?;
                Some((n, format!("/dev/{name}")))
            })
            .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|(_, path)| {
            let fd = open_dev(&path)?;
            let mut cap: Capability = zeroed();
            xioctl(fd.0, VIDIOC_QUERYCAP, &mut cap).ok()?;
            let caps = if cap.capabilities & CAP_DEVICE_CAPS != 0 {
                cap.device_caps
            } else {
                cap.capabilities
            };
            if caps & CAP_VIDEO_CAPTURE == 0 || caps & CAP_STREAMING == 0 {
                return None;
            }
            let card = cstr(&cap.card);
            let bus = cstr(&cap.bus_info);
            Some(CameraInfo {
                id: super::device_id("videoinput", &format!("{bus}/{card}/{path}")),
                label: if card.is_empty() { path.clone() } else { card },
                facing: String::new(),
                handle: path,
            })
        })
        .collect()
}

struct Mapped {
    ptr: *mut c_void,
    len: usize,
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: mapped by us with this length.
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

// SAFETY: the mapping is only touched by the capture thread that owns it.
unsafe impl Send for Mapped {}

pub(super) fn open(
    info: CameraInfo,
    want: &Want,
    sink: Arc<dyn FrameSink>,
) -> Result<Camera, CaptureError> {
    let not_readable = |what: &str, e: std::io::Error| {
        warn!("camera {}: {what}: {e}", info.handle);
        CaptureError::NotReadable(format!("{what}: {e}"))
    };
    let fd = open_dev(&info.handle).ok_or(CaptureError::NotFound)?;

    let mut fmt: Format = zeroed();
    fmt.type_ = BUF_TYPE_VIDEO_CAPTURE;
    // SAFETY: writing the `pix` member of a zeroed union.
    unsafe {
        fmt.fmt.pix.width = want.exact_width.or(want.width).unwrap_or(640);
        fmt.fmt.pix.height = want.exact_height.or(want.height).unwrap_or(480);
        fmt.fmt.pix.pixelformat = PIX_YUYV;
        fmt.fmt.pix.field = FIELD_NONE;
    }
    xioctl(fd.0, VIDIOC_S_FMT, &mut fmt).map_err(|e| not_readable("S_FMT", e))?;
    // SAFETY: the driver filled `pix`.
    let pix = unsafe { fmt.fmt.pix };
    if pix.pixelformat != PIX_YUYV {
        return Err(CaptureError::NotReadable(format!(
            "the camera has no YUYV mode (format {:08x}; MJPEG-only cameras are not supported yet)",
            pix.pixelformat
        )));
    }
    let (width, height) = (pix.width & !1, pix.height);
    let stride = (pix.bytesperline as usize).max(pix.width as usize * 2);

    let mut frame_rate = want.frame_rate.unwrap_or(30.);
    let mut parm: StreamParm = zeroed();
    parm.type_ = BUF_TYPE_VIDEO_CAPTURE;
    // SAFETY: the `capture` member of a zeroed union.
    unsafe {
        parm.parm.capture.timeperframe = Fract {
            numerator: 1000,
            denominator: (frame_rate * 1000.) as u32,
        };
    }
    if xioctl(fd.0, VIDIOC_S_PARM, &mut parm).is_ok() {
        // SAFETY: the driver filled `capture`.
        let tpf = unsafe { parm.parm.capture.timeperframe };
        if tpf.numerator > 0 && tpf.denominator > 0 {
            frame_rate = tpf.denominator as f64 / tpf.numerator as f64;
        }
    }

    let mut req: RequestBuffers = zeroed();
    req.count = 4;
    req.type_ = BUF_TYPE_VIDEO_CAPTURE;
    req.memory = MEMORY_MMAP;
    xioctl(fd.0, VIDIOC_REQBUFS, &mut req).map_err(|e| not_readable("REQBUFS", e))?;
    let mut maps = vec![];
    for index in 0..req.count {
        let mut buf: Buffer = zeroed();
        buf.type_ = BUF_TYPE_VIDEO_CAPTURE;
        buf.memory = MEMORY_MMAP;
        buf.index = index;
        xioctl(fd.0, VIDIOC_QUERYBUF, &mut buf).map_err(|e| not_readable("QUERYBUF", e))?;
        // SAFETY: mapping the driver's buffer at the offset it gave.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                buf.length as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.0,
                buf.m.offset as libc::off_t,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(not_readable("mmap", std::io::Error::last_os_error()));
        }
        maps.push(Mapped {
            ptr,
            len: buf.length as usize,
        });
        xioctl(fd.0, VIDIOC_QBUF, &mut buf).map_err(|e| not_readable("QBUF", e))?;
    }
    let mut ty: c_int = BUF_TYPE_VIDEO_CAPTURE as c_int;
    xioctl(fd.0, VIDIOC_STREAMON, &mut ty).map_err(|e| not_readable("STREAMON", e))?;
    debug!(
        "camera {}: {width}x{height} YUYV at {frame_rate} fps",
        info.handle
    );

    let path = info.handle.clone();
    thread::Builder::new()
        .name("PartiiCamera".to_owned())
        .spawn(move || {
            let fd = fd;
            let maps = maps;
            let mut ok = true;
            while sink.live() {
                let mut pfd = libc::pollfd {
                    fd: fd.0,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one pollfd.
                let r = unsafe { libc::poll(&mut pfd, 1, 200) };
                if r == 0 {
                    continue;
                }
                if r < 0 {
                    if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    ok = false;
                    break;
                }
                if pfd.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                    ok = false;
                    break;
                }
                let mut buf: Buffer = zeroed();
                buf.type_ = BUF_TYPE_VIDEO_CAPTURE;
                buf.memory = MEMORY_MMAP;
                match xioctl(fd.0, VIDIOC_DQBUF, &mut buf) {
                    Ok(()) => {},
                    Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => continue,
                    Err(e) => {
                        warn!("camera {path}: DQBUF: {e}");
                        ok = false;
                        break;
                    },
                }
                if sink.has_readers() {
                    let m = &maps[buf.index as usize];
                    let need = stride * height as usize;
                    if (buf.bytesused as usize) >= need.min(m.len) && m.len >= need {
                        // SAFETY: the driver filled this mapped buffer; we read it before re-queueing.
                        let src = unsafe { std::slice::from_raw_parts(m.ptr as *const u8, need) };
                        let mut out = vec![0u8; width as usize * height as usize * 4];
                        yuyv_to_bgra(src, width, height, stride, &mut out);
                        sink.push_bgra(width, height, width as usize * 4, out.into());
                    }
                }
                if let Err(e) = xioctl(fd.0, VIDIOC_QBUF, &mut buf) {
                    warn!("camera {path}: QBUF: {e}");
                    ok = false;
                    break;
                }
            }
            let mut ty: c_int = BUF_TYPE_VIDEO_CAPTURE as c_int;
            let _ = xioctl(fd.0, VIDIOC_STREAMOFF, &mut ty);
            drop(maps);
            drop(fd);
            if !ok {
                sink.end();
            }
        })
        .map_err(|e| CaptureError::NotReadable(e.to_string()))?;
    Ok(Camera {
        settings: CameraSettings {
            width,
            height,
            frame_rate,
            info,
        },
    })
}
