/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii (stream perf, diagnostic): time sums for a few fixed hot paths (the worker canvas path:
//! putImageData, the canvas thread's copies, WebRender image updates, frame waits), printed to
//! stderr as `PARTII_PROF <name> n=<count> avg=<ms> max=<ms>` every `PARTII_PROF_EVERY` (default
//! 300) samples of each counter. Off unless `PARTII_PROF=1`: then one relaxed load per site.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// The instrumented paths. The order is the table's.
#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Counter {
    /// Script: putImageData's snapshot, its copy to shared memory and the send (the caller's thread).
    PutScript = 0,
    /// Canvas thread: one PutImageData (copy, premultiply, copy into the pixmap).
    PutCanvas,
    /// Canvas thread: one UpdateImage (opaque scan, copy to shared memory, send to the painter).
    UpdateCanvas,
    /// Painter: one canvas UpdateImage handled (into a WebRender transaction).
    UpdatePaint,
    /// Painter: from a frame delay for a canvas epoch until that epoch's image arrived.
    DelayWait,
    /// Script: a document waiting on canvas image updates (not running update the rendering).
    DocWait,
    /// Script: an OffscreenCanvas commit task (UpdateImage send plus the placeholder message).
    Commit,
    /// Script: the placeholder's commit message from the worker reaching the page (queue latency).
    CommitToPage,
}

const NAMES: [&str; 8] = [
    "put_script",
    "put_canvas",
    "update_canvas",
    "update_paint",
    "delay_wait",
    "doc_wait",
    "commit",
    "commit_to_page",
];

struct Slot {
    n: AtomicU64,
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
}

struct Prof {
    every: u64,
    slots: [Slot; 8],
}

static PROF: LazyLock<Option<Prof>> = LazyLock::new(|| {
    if std::env::var("PARTII_PROF").as_deref() != Ok("1") {
        return None;
    }
    let every = std::env::var("PARTII_PROF_EVERY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    Some(Prof {
        every,
        slots: std::array::from_fn(|_| Slot {
            n: AtomicU64::new(0),
            sum_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
        }),
    })
});

/// Whether `PARTII_PROF=1`.
#[inline]
pub fn enabled() -> bool {
    PROF.is_some()
}

/// The start of a timed section, when profiling is on.
#[inline]
pub fn start() -> Option<Instant> {
    enabled().then(Instant::now)
}

/// Adds the time since `start` (if any) to `counter`.
#[inline]
pub fn stop(counter: Counter, start: Option<Instant>) {
    if let Some(start) = start {
        record(counter, start.elapsed().as_nanos() as u64);
    }
}

/// Adds one sample of `ns` to `counter`; prints and resets its window every `PARTII_PROF_EVERY`.
pub fn record(counter: Counter, ns: u64) {
    let Some(prof) = (*PROF).as_ref() else {
        return;
    };
    let slot = &prof.slots[counter as usize];
    let sum = slot.sum_ns.fetch_add(ns, Ordering::Relaxed) + ns;
    slot.max_ns.fetch_max(ns, Ordering::Relaxed);
    let n = slot.n.fetch_add(1, Ordering::Relaxed) + 1;
    if n >= prof.every {
        let max = slot.max_ns.swap(0, Ordering::Relaxed);
        slot.n.store(0, Ordering::Relaxed);
        slot.sum_ns.fetch_sub(sum, Ordering::Relaxed);
        eprintln!(
            "PARTII_PROF {} n={} avg={:.3} max={:.3}",
            NAMES[counter as usize],
            n,
            sum as f64 / n as f64 / 1e6,
            max as f64 / 1e6
        );
    }
}
