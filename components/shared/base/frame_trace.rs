/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii (stream frame-attr): per-frame attribution of the runtime's time, for GOAL-CHROME
//! criterion 2(a) ("pauses the runtime causes never last longer than one frame").
//!
//! Off unless `PARTII_FRAME_TRACE=<dir>` is set (the soaks set it). Then every instrumented phase
//! (script turns, the game's JS, GC, layout, display lists, WebRender, GL submit, WebGL, image
//! decode, shader compile, texture upload, IPC waits) records a span on the thread it ran on, and
//! the start of each rAF tick and each present records a mark. Each thread writes its spans to
//! `<dir>/ft-<pid>-<n>.bin`, flushed every second and when its buffer fills: a header (`PFT1`,
//! the pid as u32, the thread name's length as u16 and the name), then 16-byte little-endian
//! records `start_ns: u64, dur_ns: u32, phase: u8, depth: u8, aux: u16`, times since this
//! module's clock started (`<dir>/ft-<pid>-info.jsonl` holds that start as Unix ms, and facts such
//! as the real GL renderer). Spans nest per thread (`depth`); tauri-runtime/tools/lx/frame_attr.py
//! turns them into each phase's own time per frame.
//!
//! Cost when on: two `Instant::now()` and a 16-byte push per span (~40-60 ns), a few thousand
//! spans a second; when off, one load of a lazily set flag.

use std::cell::RefCell;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

/// What a span or mark is. The numbers are the file format: never reuse one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Phase {
    /// Mark: a document's rAF callbacks start (aux bit 0: a top-level document).
    Frame = 1,
    /// Mark: the paint thread presented a frame.
    Present = 2,
    /// One turn of the script thread's event loop, after it woke (its own time: the engine's).
    ScriptTurn = 10,
    /// Inside a JS entry point (callbacks, script and module evaluation, promise jobs): the game's
    /// JS, less what is nested in it (GC, compile, IPC waits).
    Js = 11,
    /// Compiling a classic or module script.
    ScriptCompile = 12,
    GcMinor = 13,
    GcMajor = 14,
    /// Partii's black remark before a major GC slice (the slice's pause includes it).
    GcRemark = 15,
    /// Update the rendering (rAF, resize, reflow requests): its own time.
    Rendering = 16,
    Layout = 17,
    DisplayList = 18,
    /// A blocking receive inside another span (a synchronous reply: WebGL getters, storage, ...).
    IpcWait = 19,
    ImageDecode = 20,
    /// The paint thread handling its messages (display lists, frame-ready, input).
    PaintMsg = 30,
    /// WebRender's `Renderer::update`: taking built frames, resource updates.
    WrUpdate = 31,
    /// WebRender's `Renderer::render`: drawing the frame through GL (the driver under it).
    GlSubmit = 32,
    /// Presenting the frame (swap buffers).
    PresentSwap = 33,
    WrScene = 40,
    /// The render backend handling a document update (resources), its own time.
    WrBackend = 41,
    WrFrame = 42,
    TextureUpload = 43,
    ShaderCompile = 44,
    /// The WebGL thread running the page's GL commands (merged when back to back).
    WebGl = 50,
    WebGlSwap = 51,
    WebGlContext = 52,
    WebGlCompile = 53,
    WebGlUpload = 54,
}

/// Spans of these phases that follow each other within this gap on one thread, at the same
/// depth, are merged into one record (the WebGL thread runs thousands of commands a second).
const MERGE_NS: u64 = 50_000;
/// Spans shorter than this aren't written (their time stays with the span around them).
const MIN_NS: u64 = 5_000;
const FLUSH_NS: u64 = 1_000_000_000;
const FLUSH_BYTES: usize = 64 * 1024;

static DIR: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    let dir = std::env::var_os("PARTII_FRAME_TRACE")?;
    if dir.is_empty() || dir == "0" {
        return None;
    }
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).ok()?;
    LazyLock::force(&START);
    Some(dir)
});
static START: LazyLock<Instant> = LazyLock::new(Instant::now);
static SEQ: AtomicU32 = AtomicU32::new(0);
static INFO: Mutex<Option<File>> = Mutex::new(None);

/// Whether the trace is on (`PARTII_FRAME_TRACE`).
#[inline]
pub fn enabled() -> bool {
    DIR.is_some()
}

#[inline]
fn now_ns() -> u64 {
    START.elapsed().as_nanos() as u64
}

fn ns_of(at: Instant) -> u64 {
    at.saturating_duration_since(*START).as_nanos() as u64
}

#[derive(Clone, Copy)]
struct Rec {
    start: u64,
    end: u64,
    phase: u8,
    depth: u8,
    aux: u16,
}

struct Buf {
    file: Option<File>,
    failed: bool,
    out: Vec<u8>,
    stack: Vec<(Phase, u64)>,
    pending: Option<Rec>,
    last_flush: u64,
}

impl Buf {
    fn new() -> Self {
        Buf {
            file: None,
            failed: false,
            out: Vec::with_capacity(FLUSH_BYTES + 64),
            stack: Vec::with_capacity(16),
            pending: None,
            last_flush: now_ns(),
        }
    }

    fn push(&mut self, rec: Rec, now: u64) {
        let merges = matches!(rec.phase, p if p == Phase::WebGl as u8 || p == Phase::Js as u8 || p == Phase::IpcWait as u8);
        if let Some(p) = self.pending.as_mut() {
            if merges &&
                p.phase == rec.phase &&
                p.depth == rec.depth &&
                rec.start >= p.end &&
                rec.start - p.end <= MERGE_NS
            {
                p.end = rec.end;
                return;
            }
        }
        if let Some(p) = self.pending.take() {
            self.write(p);
        }
        if merges {
            self.pending = Some(rec);
        } else {
            self.write(rec);
        }
        if self.out.len() >= FLUSH_BYTES || now.saturating_sub(self.last_flush) >= FLUSH_NS {
            self.flush(now);
        }
    }

    fn write(&mut self, r: Rec) {
        let dur = r.end.saturating_sub(r.start);
        if dur < MIN_NS && r.phase >= Phase::ScriptTurn as u8 {
            return;
        }
        self.out.extend_from_slice(&r.start.to_le_bytes());
        self.out.extend_from_slice(&(dur.min(u32::MAX as u64) as u32).to_le_bytes());
        self.out.push(r.phase);
        self.out.push(r.depth);
        self.out.extend_from_slice(&r.aux.to_le_bytes());
    }

    fn flush(&mut self, now: u64) {
        self.last_flush = now;
        if self.out.is_empty() || self.failed {
            self.out.clear();
            return;
        }
        if self.file.is_none() {
            let Some(dir) = DIR.as_ref() else { return };
            let name = format!(
                "ft-{}-{}.bin",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            );
            match File::create(dir.join(name)) {
                Ok(mut file) => {
                    let thread = std::thread::current();
                    let tname = thread.name().unwrap_or("?").as_bytes();
                    let tname = &tname[..tname.len().min(200)];
                    let mut head = Vec::with_capacity(16 + tname.len());
                    head.extend_from_slice(b"PFT1");
                    head.extend_from_slice(&std::process::id().to_le_bytes());
                    head.extend_from_slice(&(tname.len() as u16).to_le_bytes());
                    head.extend_from_slice(tname);
                    if file.write_all(&head).is_err() {
                        self.failed = true;
                        return;
                    }
                    self.file = Some(file);
                },
                Err(_) => {
                    self.failed = true;
                    return;
                },
            }
        }
        if let Some(file) = self.file.as_mut() {
            if file.write_all(&self.out).is_err() {
                self.failed = true;
            }
        }
        self.out.clear();
    }
}

impl Drop for Buf {
    fn drop(&mut self) {
        if let Some(p) = self.pending.take() {
            self.write(p);
        }
        self.flush(now_ns());
    }
}

thread_local!(static BUF: RefCell<Option<Buf>> = const { RefCell::new(None) });

fn with(f: impl FnOnce(&mut Buf)) {
    let _ = BUF.try_with(|cell| {
        if let Ok(mut cell) = cell.try_borrow_mut() {
            f(cell.get_or_insert_with(Buf::new))
        }
    });
}

/// Starts a span of `phase` on this thread; `exit(phase)` ends it. Prefer `span`.
#[inline]
pub fn enter(phase: Phase) {
    if !enabled() {
        return;
    }
    let t = now_ns();
    with(|b| b.stack.push((phase, t)));
}

/// Ends the innermost span if it is a `phase` span (a callback pair seen half, e.g. a GC that
/// began before the trace's callbacks were set, is ignored).
#[inline]
pub fn exit(phase: Phase) {
    if !enabled() {
        return;
    }
    let t = now_ns();
    with(|b| {
        if b.stack.last().is_some_and(|(p, _)| *p == phase) {
            let (p, start) = b.stack.pop().unwrap();
            let depth = b.stack.len().min(255) as u8;
            b.push(
                Rec {
                    start,
                    end: t,
                    phase: p as u8,
                    depth,
                    aux: 0,
                },
                t,
            );
        }
    });
}

/// A span that ends when dropped.
#[must_use]
pub struct Span(Option<Phase>);

impl Drop for Span {
    #[inline]
    fn drop(&mut self) {
        if let Some(phase) = self.0 {
            exit(phase);
        }
    }
}

/// A span of `phase` until the returned guard is dropped (nothing when the trace is off).
#[inline]
pub fn span(phase: Phase) -> Span {
    if !enabled() {
        return Span(None);
    }
    enter(phase);
    Span(Some(phase))
}

/// Whether this thread is inside a span (so a blocking wait now is a wait inside work).
#[inline]
pub fn in_span() -> bool {
    if !enabled() {
        return false;
    }
    let mut inside = false;
    with(|b| inside = !b.stack.is_empty());
    inside
}

/// A span measured elsewhere (`start`..`end`), nested in whatever this thread is inside.
pub fn record(phase: Phase, start: Instant, end: Instant) {
    if !enabled() {
        return;
    }
    let t = now_ns();
    with(|b| {
        let depth = b.stack.len().min(255) as u8;
        b.push(
            Rec {
                start: ns_of(start),
                end: ns_of(end),
                phase: phase as u8,
                depth,
                aux: 0,
            },
            t,
        )
    });
}

/// An instant event (`Phase::Frame`, `Phase::Present`).
#[inline]
pub fn mark(phase: Phase, aux: u16) {
    if !enabled() {
        return;
    }
    let t = now_ns();
    with(|b| {
        let depth = b.stack.len().min(255) as u8;
        b.push(
            Rec {
                start: t,
                end: t,
                phase: phase as u8,
                depth,
                aux,
            },
            t,
        )
    });
}

/// Writes this thread's buffered spans now (a thread about to block for long, or exit).
pub fn flush() {
    if !enabled() {
        return;
    }
    with(|b| {
        if let Some(p) = b.pending.take() {
            b.write(p);
        }
        b.flush(now_ns());
    });
}

/// A fact for the trace's info file (one JSON line): e.g. `info("gl_renderer", ...)`. The first
/// line written is the clock's start as Unix ms.
pub fn info(key: &str, value: &str) {
    if !enabled() {
        return;
    }
    let Ok(mut file) = INFO.lock() else { return };
    if file.is_none() {
        let Some(dir) = DIR.as_ref() else { return };
        let Ok(mut f) = File::create(dir.join(format!("ft-{}-info.jsonl", std::process::id())))
        else {
            return;
        };
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64() * 1000.0) -
            START.elapsed().as_secs_f64() * 1000.0;
        let _ = writeln!(f, "{{\"k\":\"start\",\"unix_ms\":{unix_ms:.3}}}");
        *file = Some(f);
    }
    if let Some(f) = file.as_mut() {
        let _ = writeln!(
            f,
            "{{\"k\":{},\"v\":{},\"t_ns\":{}}}",
            serde_json::Value::from(key),
            serde_json::Value::from(value),
            now_ns()
        );
        let _ = f.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cost of a span with the trace on, and of the off path (printed; run with
    /// `cargo test -p servo-base frame_trace -- --nocapture`).
    #[test]
    #[allow(unsafe_code)]
    fn frame_trace_cost() {
        let dir = std::env::temp_dir().join(format!("ft-test-{}", std::process::id()));
        // SAFETY of the test: the only test in this crate that reads PARTII_FRAME_TRACE.
        unsafe { std::env::set_var("PARTII_FRAME_TRACE", &dir) };
        assert!(enabled());
        let n = 2_000_000u64;
        let t = Instant::now();
        for i in 0..n {
            let _s = span(if i % 2 == 0 { Phase::Js } else { Phase::Layout });
            std::hint::black_box(i);
        }
        let on = t.elapsed().as_nanos() as f64 / n as f64;
        let t = Instant::now();
        for i in 0..n {
            let _s = Span(None);
            std::hint::black_box(i);
        }
        let off = t.elapsed().as_nanos() as f64 / n as f64;
        flush();
        info("test", "ok");
        let bytes: u64 = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok()?.metadata().ok())
            .map(|m| m.len())
            .sum();
        eprintln!("frame_trace: {on:.1} ns per span on, {off:.1} ns off, {bytes} bytes for {n} spans");
        println!("frame_trace: {on:.1} ns per span on, {off:.1} ns off, {bytes} bytes for {n} spans");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(bytes > 0);
    }
}
