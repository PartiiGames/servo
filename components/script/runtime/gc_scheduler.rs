/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A GC schedule for script and worker runtimes, on top of SpiderMonkey's own triggers.
//!
//! SpiderMonkey starts a major GC when a zone's GC heap or its counted malloc memory passes a
//! threshold (27 MB / 38 MB to begin with). Most of the memory a DOM object holds is malloc'd on
//! the Rust side and only its shallow size is counted, so a runtime whose JS heap stays small
//! never reaches either threshold. A worker that receives a few dozen messages a second is
//! exactly that: every `MessageEvent` is garbage once its handler returns, but nothing ever
//! collects it (a soak saw 497k live events, 258 MB, in a game's pool workers after 24 minutes,
//! and no worker GC at all). The page collects only every few minutes, and then the pause is
//! proportional to minutes of garbage (60-180 ms).
//!
//! Gecko runs workers' GCs from timers (a periodic one and an idle one). This does the same, from
//! the event loops (after each batch of tasks, and from a wake-up when a worker goes idle):
//! - **Periodic:** a full GC when the last major GC (ours or SpiderMonkey's) is at least
//!   `PARTII_GC_PERIOD_MS` old (default 5000) and the GC heap grew since. The period stretches
//!   to 50x the last pause, so collecting never takes more than ~2% of the thread.
//! - **Idle (workers):** after `PARTII_GC_IDLE_MS` (default 2000) without a task, one shrinking
//!   GC, which also decommits empty GC arenas and releases empty chunks.
//!
//! `PARTII_GC_SCHED=0` turns all of it off (SpiderMonkey's triggers only, as before).
//! `PARTII_GC_SCHED_LOG=1` prints one line per scheduled GC to stderr.
//! `PARTII_GC_DECOMMIT_EVERY_GC=1`: decommit after every GC again (default: only the idle shrink).
//!
//! Keeping each pause short (lx-5 counts every GC pause, page and worker):
//! - **Nursery first:** a periodic GC empties the nursery in its own short pause and runs the
//!   major GC on a later turn of the event loop (at least `PARTII_GC_SPLIT_GAP_MS`, default 8,
//!   later), so the major GC's "evict nursery" step finds it nearly empty.
//!   `PARTII_GC_SPLIT_NURSERY=0` runs both in one pause again.
//! - **One at a time:** the page and its workers each run their own runtime, and their periodic
//!   GCs start together. A runtime waits until no other one in the process started a periodic GC
//!   in the last `PARTII_GC_STAGGER_MS` (default 100; 0 = no waiting), so they don't compete for
//!   CPUs (and the GC helper threads) in the same few ms.
//! - `PARTII_GC_FINALIZE_STATS=1` adds the DOM objects each GC finalized, and their finalizers'
//!   time, to the log line.

#![allow(unsafe_code)]

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use js::context::JSContext;
use js::jsapi::{GCOptions, GCReason, JSGCParamKey};
use js::rust::wrappers2::{
    JS_GetGCParameter, JS_GetRuntime, JS_SetGCParameter, NonIncrementalGC, PrepareForFullGC,
};

struct Config {
    enabled: bool,
    period: Duration,
    idle: Duration,
    log: bool,
    /// Decommit free arenas only in the idle shrinking GC, not after every periodic GC.
    decommit_idle_only: bool,
    /// Empty the nursery in a pause of its own before a periodic GC.
    split_nursery: bool,
    /// Least time between that nursery collection and the major GC.
    split_gap: Duration,
    /// Least time between two runtimes' periodic GCs in the process.
    stagger: Duration,
    /// Count DOM finalizers in the log line.
    finalize_stats: bool,
}

fn env_ms(name: &str, default: u64) -> Duration {
    Duration::from_millis(
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default),
    )
}

fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| Config {
        enabled: !matches!(std::env::var("PARTII_GC_SCHED").as_deref(), Ok("0")),
        period: env_ms("PARTII_GC_PERIOD_MS", 5000),
        idle: env_ms("PARTII_GC_IDLE_MS", 2000),
        log: matches!(std::env::var("PARTII_GC_SCHED_LOG").as_deref(), Ok("1")),
        decommit_idle_only: !matches!(
            std::env::var("PARTII_GC_DECOMMIT_EVERY_GC").as_deref(),
            Ok("1")
        ),
        split_nursery: !matches!(std::env::var("PARTII_GC_SPLIT_NURSERY").as_deref(), Ok("0")),
        split_gap: env_ms("PARTII_GC_SPLIT_GAP_MS", 8),
        stagger: env_ms("PARTII_GC_STAGGER_MS", 100),
        finalize_stats: matches!(
            std::env::var("PARTII_GC_FINALIZE_STATS").as_deref(),
            Ok("1")
        ),
    })
}

/// Which event loop a thread runs: the idle shrink is for workers only (a page that is idle for
/// two seconds is usually still on screen, and a shrinking GC throws away JIT code).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    Window,
    Worker,
}

/// Per-thread state (each script or worker thread has its own runtime).
struct State {
    /// JSGC_MAJOR_GC_NUMBER when we last looked, to notice SpiderMonkey's own GCs.
    major_number: Cell<u32>,
    /// When the last major GC (any reason) was seen to end.
    last_major: Cell<Instant>,
    /// GC heap bytes right after the last major GC.
    bytes_after: Cell<u32>,
    /// Duration of the last GC we ran.
    last_pause: Cell<Duration>,
    /// When the event loop last finished a batch of tasks.
    last_task: Cell<Instant>,
    /// Whether anything ran since the last shrinking GC.
    dirty_since_shrink: Cell<bool>,
    /// Whether this runtime's GC parameters were adjusted for the schedule.
    tuned: Cell<bool>,
    /// When we emptied the nursery for the next periodic GC (split mode), and how long it took.
    nursery_at: Cell<Option<Instant>>,
    nursery_pause: Cell<Duration>,
}

thread_local! {
    static STATE: State = State {
        major_number: Cell::new(0),
        last_major: Cell::new(Instant::now()),
        bytes_after: Cell::new(0),
        last_pause: Cell::new(Duration::ZERO),
        last_task: Cell::new(Instant::now()),
        dirty_since_shrink: Cell::new(false),
        tuned: Cell::new(false),
        nursery_at: Cell::new(None),
        nursery_pause: Cell::new(Duration::ZERO),
    };
}

/// When some runtime in the process last started a periodic GC, in ms since `epoch()` (+1, so 0
/// means never).
static LAST_PERIODIC_START: AtomicU64 = AtomicU64::new(0);

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Claims the process-wide slot for a periodic GC: false if another runtime started one less than
/// `stagger` ago.
fn claim_periodic_slot(stagger: Duration, now: Instant) -> bool {
    if stagger.is_zero() {
        return true;
    }
    let now_ms = now.saturating_duration_since(epoch()).as_millis() as u64 + 1;
    let last = LAST_PERIODIC_START.load(Ordering::Relaxed);
    if last != 0 && now_ms.saturating_sub(last) < stagger.as_millis() as u64 {
        return false;
    }
    LAST_PERIODIC_START
        .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

/// A split nursery collection older than this is redone before the major GC.
const NURSERY_STALE: Duration = Duration::from_millis(500);

/// Growth of the GC heap since the last major GC below which a periodic GC isn't worth it.
const MIN_GROWTH_BYTES: u32 = 256 * 1024;

fn param(cx: &JSContext, key: JSGCParamKey) -> u32 {
    unsafe { JS_GetGCParameter(cx, key) }
}

/// Notes a major GC SpiderMonkey ran by itself since we last looked.
fn sync_major(cx: &JSContext, state: &State, now: Instant) {
    let number = param(cx, JSGCParamKey::JSGC_MAJOR_GC_NUMBER);
    if number != state.major_number.get() {
        state.major_number.set(number);
        state.last_major.set(now);
        state.bytes_after.set(param(cx, JSGCParamKey::JSGC_BYTES));
        state.nursery_at.set(None);
    }
}

/// Empties the nursery (a minor GC) and notes when, for the major GC that follows.
fn collect_nursery(cx: &mut JSContext, state: &State) {
    let started = Instant::now();
    unsafe {
        js::jsapi::JS::RunNurseryCollection(
            JS_GetRuntime(cx),
            GCReason::EAGER_NURSERY_COLLECTION,
            js::jsapi::mozilla::TimeDuration { mValue: 0 },
        );
    }
    let now = Instant::now();
    state.nursery_pause.set(now - started);
    state.nursery_at.set(Some(now));
}

fn collect(cx: &mut JSContext, state: &State, kind: Kind, shrink: bool) {
    let before = param(cx, JSGCParamKey::JSGC_BYTES);
    if config().finalize_stats {
        script_bindings::take_finalize_stats();
    }
    let started = Instant::now();
    unsafe {
        PrepareForFullGC(cx);
        NonIncrementalGC(
            cx,
            if shrink {
                GCOptions::Shrink
            } else {
                GCOptions::Normal
            },
            if shrink {
                GCReason::USER_INACTIVE
            } else {
                GCReason::FULL_GC_TIMER
            },
        );
    }
    let now = Instant::now();
    let pause = now - started;
    state.last_pause.set(pause);
    state
        .major_number
        .set(param(cx, JSGCParamKey::JSGC_MAJOR_GC_NUMBER));
    state.last_major.set(now);
    let after = param(cx, JSGCParamKey::JSGC_BYTES);
    state.bytes_after.set(after);
    if shrink {
        state.dirty_since_shrink.set(false);
    }
    let nursery = state
        .nursery_at
        .take()
        .map(|at| (state.nursery_pause.get(), now - at));
    if config().log {
        let nursery = nursery
            .map(|(pause, ago)| {
                format!(
                    ", nursery {:.2} ms {} ms before",
                    pause.as_secs_f64() * 1000.,
                    ago.as_millis()
                )
            })
            .unwrap_or_default();
        let finalized = if config().finalize_stats {
            let (n, ns) = script_bindings::take_finalize_stats();
            format!(", finalized {} DOM objects in {:.2} ms", n, ns as f64 / 1e6)
        } else {
            String::new()
        };
        eprintln!(
            "[gc-sched] {} {} GC {:.2} ms, {} -> {} KB{}{}",
            if kind == Kind::Worker {
                "worker"
            } else {
                "window"
            },
            if shrink { "idle shrinking" } else { "periodic" },
            pause.as_secs_f64() * 1000.,
            before / 1024,
            after / 1024,
            nursery,
            finalized
        );
    }
}

/// Called by an event loop after it ran a batch of tasks (and their microtask checkpoints).
pub(crate) fn after_tasks(cx: &mut JSContext, kind: Kind) {
    let config = config();
    if !config.enabled {
        return;
    }
    STATE.with(|state| {
        let now = Instant::now();
        state.last_task.set(now);
        state.dirty_since_shrink.set(true);
        if !state.tuned.replace(true) {
            if config.finalize_stats {
                script_bindings::set_finalize_stats(true);
            }
            if config.decommit_idle_only {
                // A normal GC decommits free arenas unless GCs are "high frequency" (closer
                // together than this limit). At one GC every few seconds that was ~2 ms of every
                // pause (and the arenas were committed again right after); count our period as
                // high frequency, so only the idle shrinking GC decommits.
                // PARTII_GC_DECOMMIT_EVERY_GC=1 keeps SpiderMonkey's default.
                let limit_ms = (config.period.as_millis() as u32)
                    .saturating_mul(2)
                    .max(1000);
                unsafe {
                    JS_SetGCParameter(cx, JSGCParamKey::JSGC_HIGH_FREQUENCY_TIME_LIMIT, limit_ms);
                }
            }
        }
        sync_major(cx, state, now);
        let period = config.period.max(state.last_pause.get() * 50);
        if now - state.last_major.get() < period {
            return;
        }
        let bytes = param(cx, JSGCParamKey::JSGC_BYTES);
        if bytes < state.bytes_after.get().saturating_add(MIN_GROWTH_BYTES) {
            return;
        }
        if config.split_nursery {
            match state.nursery_at.get() {
                // The nursery was emptied for this GC: run the major GC once the gap has passed
                // (the turns of the event loop in between run the page's frames).
                Some(at) if now - at < config.split_gap => return,
                Some(at) if now - at < NURSERY_STALE => {},
                _ => {
                    if !claim_periodic_slot(config.stagger, now) {
                        return;
                    }
                    collect_nursery(cx, state);
                    return;
                },
            }
        } else if !claim_periodic_slot(config.stagger, now) {
            return;
        }
        collect(cx, state, kind, false);
    })
}

/// When a worker's event loop should wake up by itself if no task comes (for the idle GC).
pub(crate) fn idle_deadline() -> Option<Instant> {
    let config = config();
    if !config.enabled || config.idle.is_zero() {
        return None;
    }
    STATE.with(|state| {
        state
            .dirty_since_shrink
            .get()
            .then(|| state.last_task.get() + config.idle)
    })
}

/// Called by a worker's event loop when it woke up at `idle_deadline()` with no task.
pub(crate) fn on_idle(cx: &mut JSContext) {
    STATE.with(|state| {
        if !state.dirty_since_shrink.get() || state.last_task.get().elapsed() < config().idle {
            return;
        }
        collect(cx, state, Kind::Worker, true);
    })
}
