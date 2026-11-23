/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii (stream webgl-upload, diagnostic): `PARTII_WEBGL_FRAME_MS=<ms>` logs, for every interval between two
//! SwapBuffers in which the WebGL thread was busy for longer than <ms>, where that time went: per kind of message
//! (refined by the path the handler took, e.g. `GenerateMipmap:gl:minLINEAR`, `TexImage2D:lazy`), the count, total
//! and longest time, the bytes uploaded and the largest size, and for pixel uploads the split between the CPU's
//! conversion (prepare_pixels) and the GL call. Criterion 2(a) judges the WebGL thread's time per frame summed by
//! phase (tools/lx/frame_attr.py), so a boot of many small uploads fails although no single one is long; this says
//! which. Off (unset) by default; nothing is measured then.

use std::cell::RefCell;
use std::time::Instant;

use rustc_hash::FxHashMap;

/// The threshold of PARTII_WEBGL_FRAME_MS, or None (off).
pub(crate) fn threshold_ms() -> Option<f64> {
    static MS: std::sync::LazyLock<Option<f64>> = std::sync::LazyLock::new(|| {
        std::env::var("PARTII_WEBGL_FRAME_MS")
            .ok()
            .and_then(|v| v.parse().ok())
    });
    *MS
}

#[derive(Default)]
struct Kind {
    n: u32,
    ms: f64,
    max_ms: f64,
    bytes: u64,
    max_size: (u32, u32),
    prep_ms: f64,
    gl_ms: f64,
}

#[derive(Default)]
struct Frame {
    start: Option<Instant>,
    busy_ms: f64,
    msgs: u32,
    kinds: FxHashMap<String, Kind>,
    /// The current message's refinements (note) and sub-times.
    note: Option<&'static str>,
    prep_ms: f64,
    gl_ms: f64,
}

thread_local!(static FRAME: RefCell<Frame> = RefCell::new(Frame::default()));

/// One message being handled; its time is added to the frame when it drops.
pub(crate) struct MsgGuard {
    start: Instant,
    kind: &'static str,
    bytes: u64,
    size: (u32, u32),
    swap: bool,
    threshold: f64,
}

/// Starts timing a message (only call when threshold_ms() is Some).
pub(crate) fn begin(threshold: f64, kind: &'static str, bytes: u64, size: (u32, u32), swap: bool) -> MsgGuard {
    FRAME.with(|f| {
        let mut f = f.borrow_mut();
        f.start.get_or_insert_with(Instant::now);
        f.note = None;
        f.prep_ms = 0.0;
        f.gl_ms = 0.0;
    });
    MsgGuard {
        start: Instant::now(),
        kind,
        bytes,
        size,
        swap,
        threshold,
    }
}

/// Refines the current message's kind with the path its handler took.
pub(crate) fn note(path: &'static str) {
    if threshold_ms().is_some() {
        FRAME.with(|f| f.borrow_mut().note = Some(path));
    }
}

/// Adds CPU pixel-conversion time (prepare_pixels) to the current message.
pub(crate) fn add_prep(since: Instant) {
    if threshold_ms().is_some() {
        FRAME.with(|f| f.borrow_mut().prep_ms += since.elapsed().as_secs_f64() * 1000.0);
    }
}

/// Adds GL-call time (glTexImage2D / glTexSubImage2D / glGenerateMipmap) to the current message.
pub(crate) fn add_gl(since: Instant) {
    if threshold_ms().is_some() {
        FRAME.with(|f| f.borrow_mut().gl_ms += since.elapsed().as_secs_f64() * 1000.0);
    }
}

impl Drop for MsgGuard {
    fn drop(&mut self) {
        let ms = self.start.elapsed().as_secs_f64() * 1000.0;
        FRAME.with(|f| {
            let mut f = f.borrow_mut();
            let key = match f.note.take() {
                Some(note) => format!("{}:{}", self.kind, note),
                None => self.kind.to_owned(),
            };
            let (prep, gl) = (f.prep_ms, f.gl_ms);
            f.busy_ms += ms;
            f.msgs += 1;
            let k = f.kinds.entry(key).or_default();
            k.n += 1;
            k.ms += ms;
            k.max_ms = k.max_ms.max(ms);
            k.bytes += self.bytes;
            if self.size.0 as u64 * self.size.1 as u64 > k.max_size.0 as u64 * k.max_size.1 as u64 {
                k.max_size = self.size;
            }
            k.prep_ms += prep;
            k.gl_ms += gl;
            if !self.swap {
                return;
            }
            let frame = std::mem::take(&mut *f);
            if frame.busy_ms <= self.threshold {
                return;
            }
            let wall = frame.start.map_or(0.0, |s| s.elapsed().as_secs_f64() * 1000.0);
            let mut kinds: Vec<_> = frame.kinds.into_iter().collect();
            kinds.sort_by(|a, b| b.1.ms.total_cmp(&a.1.ms));
            let mut line = format!(
                "partii-webgl-frame: busy {:.1} ms over {:.1} ms, {} msgs:",
                frame.busy_ms, wall, frame.msgs
            );
            for (name, k) in kinds.iter().filter(|(_, k)| k.ms >= 0.2).take(14) {
                line.push_str(&format!(" | {name} {}x {:.1} ms (max {:.2}", k.n, k.ms, k.max_ms));
                if k.bytes > 0 {
                    line.push_str(&format!(", {:.2} MB", k.bytes as f64 / 1048576.0));
                }
                if k.max_size != (0, 0) {
                    line.push_str(&format!(", largest {}x{}", k.max_size.0, k.max_size.1));
                }
                if k.prep_ms > 0.0 || k.gl_ms > 0.0 {
                    line.push_str(&format!(", prep {:.1} gl {:.1}", k.prep_ms, k.gl_ms));
                }
                line.push(')');
            }
            eprintln!("{line}");
        });
    }
}
