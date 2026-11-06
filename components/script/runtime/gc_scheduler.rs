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
//!
//! **Incremental marking** (`PARTII_GC_INCREMENTAL=1`, default off; not on Windows): a periodic GC
//! marks in slices of `PARTII_GC_SLICE_MS` (default 5) at most every `PARTII_GC_SLICE_GAP_MS`
//! (default 10), from the event loops, and sweeps in one pause: as soon as sweeping starts the GC is
//! finished before any script runs again. The DOM's Rust edges have no write barriers, so `script_bindings::
//! incremental_gc` shades every reflector Rust hands out while marking, and re-traces the Rust roots
//! and every marked reflector's Rust fields when marking ends (SpiderMonkey's gray-roots hook).
//! The slices have a wall-clock budget, so a preempted slice still ends near its deadline.
//!
//! SpiderMonkey runs slices of its own too (when a background task it waited for is done, or when
//! the page allocates past a trigger). With `PARTII_GC_SM_SLICES=1` (the default with incremental
//! on) those get the same time budget (`JSGC_SLICE_TIME_BUDGET_MS`) instead of an unlimited one,
//! the GCs SpiderMonkey starts by itself are incremental as well (the scheduler adopts them and
//! drives their slices), and the gray-roots hook turns the budget of the slice that starts sweeping
//! unlimited, so sweeping still never interleaves with script. `PARTII_GC_SM_SLICES=0` keeps
//! SpiderMonkey's slices unlimited (then its first continuation slice usually finishes our GC).
//!
//! **Incremental sweeping** (`PARTII_GC_INCREMENTAL_SWEEP=1`, default off, needs the above): the
//! slice that starts sweeping keeps its budget, so sweeping is sliced too and script runs while
//! zones are being swept. The expose barrier and the per-sweep-group remark stay on (later sweep
//! groups still mark), and `WeakRef` treats a dead object that isn't finalized yet as gone.

#![allow(unsafe_code)]

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use std::os::raw::c_void;

use js::context::JSContext;
use js::jsapi::{
    GCDescription, GCOptions, GCProgress, GCReason, GCSliceCallback, JSContext as RawJSContext,
    JSGCParamKey, JSTracer, SliceBudget,
};
use js::rust::wrappers2::{
    FinishIncrementalGC, IncrementalGCSlice, IsIncrementalGCInProgress, JS_GetGCParameter,
    JS_GetRuntime, JS_SetGCParameter, NonIncrementalGC, PrepareForFullGC,
    SetCreateGCSliceBudgetCallback, SetGCSliceCallback, StartIncrementalGC,
};
use script_bindings::incremental_gc;

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
    /// Mark periodic GCs incrementally.
    incremental: bool,
    /// Target length of a marking slice.
    slice: Duration,
    /// Least time between two marking slices.
    slice_gap: Duration,
    /// An incremental GC still running after this long is finished in one go.
    incremental_max: Duration,
    /// SpiderMonkey's own slices get the slice budget too (not unlimited).
    sm_slices: bool,
    /// Sweep in slices too (stage 2), script running in between.
    incremental_sweep: bool,
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
        incremental: cfg!(not(windows))
            && matches!(std::env::var("PARTII_GC_INCREMENTAL").as_deref(), Ok("1")),
        slice: env_ms("PARTII_GC_SLICE_MS", 5),
        slice_gap: env_ms("PARTII_GC_SLICE_GAP_MS", 10),
        incremental_max: env_ms("PARTII_GC_INCREMENTAL_MAX_MS", 2000),
        sm_slices: !matches!(std::env::var("PARTII_GC_SM_SLICES").as_deref(), Ok("0")),
        incremental_sweep: matches!(
            std::env::var("PARTII_GC_INCREMENTAL_SWEEP").as_deref(),
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
    /// The incremental GC in progress (ours), if any.
    incremental: Cell<Option<IncrementalGc>>,
}

/// An incremental GC the scheduler drives (it started it, or adopted one SpiderMonkey started).
#[derive(Clone, Copy)]
struct IncrementalGc {
    adopted: bool,
    started: Instant,
    before: u32,
    slices: u32,
    last_slice: Instant,
    max_slice: Duration,
    total: Duration,
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
        incremental: Cell::new(None),
    };
    /// Set from the gray-roots hook: sweeping began in the current GC.
    static SWEEP_STARTED: Cell<bool> = const { Cell::new(false) };
    /// Between GC_CYCLE_BEGIN and GC_CYCLE_END. SpiderMonkey also calls the gray-roots hook outside a
    /// GC, from `finishRoots` when a runtime is destroyed, with a tracer that crashes on any edge.
    static IN_CYCLE: Cell<bool> = const { Cell::new(false) };
    /// This thread's context, for SpiderMonkey's slice-budget callback (which gets none).
    static RAW_CX: Cell<*mut RawJSContext> = const { Cell::new(std::ptr::null_mut()) };
    /// Black remarks in the current GC: count, edges shaded, longest, total.
    static BLACK_REMARKS: Cell<(u32, usize, Duration, Duration)> =
        const { Cell::new((0, 0, Duration::ZERO, Duration::ZERO)) };
    /// Whether the scheduler itself is running a slice (else a slice is SpiderMonkey's own).
    static OUR_SLICE: Cell<bool> = const { Cell::new(false) };
    /// When SpiderMonkey's own slice in progress began.
    static SM_SLICE_BEGAN: Cell<Option<Instant>> = const { Cell::new(None) };
    /// SpiderMonkey's own slices in the current GC: count, longest, total.
    static SM_SLICES: Cell<(u32, Duration, Duration)> =
        const { Cell::new((0, Duration::ZERO, Duration::ZERO)) };
    /// The Rust roots tracer and its data, for the remark.
    static ROOTS_TRACER: Cell<Option<(RootsTracer, *mut c_void)>> = const { Cell::new(None) };
    /// The GC slice callback that was installed before ours.
    static PREVIOUS_SLICE_CALLBACK: Cell<GCSliceCallback> = const { Cell::new(None) };
}

type RootsTracer = unsafe extern "C" fn(*mut JSTracer, *mut c_void);

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
    black_remark_before_slice(unsafe { cx.raw_cx() });
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
        if config.incremental && incremental_step(cx, state, kind, now) {
            return;
        }
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
        if config.incremental {
            start_incremental(cx, state, kind);
        } else {
            collect(cx, state, kind, false);
        }
    })
}

/// When a worker's event loop should wake up by itself if no task comes (for the idle GC).
pub(crate) fn idle_deadline() -> Option<Instant> {
    let config = config();
    if !config.enabled || config.idle.is_zero() {
        return None;
    }
    STATE.with(|state| {
        if let Some(gc) = state.incremental.get() {
            return Some(gc.last_slice + config.slice_gap);
        }
        state
            .dirty_since_shrink
            .get()
            .then(|| state.last_task.get() + config.idle)
    })
}

/// Called by a worker's event loop when it woke up at `idle_deadline()` with no task.
pub(crate) fn on_idle(cx: &mut JSContext) {
    STATE.with(|state| {
        if state.incremental.get().is_some() {
            incremental_step(cx, state, Kind::Worker, Instant::now());
            return;
        }
        if !state.dirty_since_shrink.get() || state.last_task.get().elapsed() < config().idle {
            return;
        }
        collect(cx, state, Kind::Worker, true);
    })
}

// ---- Incremental marking (PARTII_GC_INCREMENTAL=1) ----

// SpiderMonkey entry points the bindings don't cover (exported from the static library).
#[cfg(not(windows))]
unsafe extern "C" {
    #[cfg_attr(
        target_vendor = "apple",
        link_name = "\u{1}__Z23JS_SetGrayGCRootsTracerP9JSContextPFbP8JSTracerRN2JS11SliceBudgetEPvES6_"
    )]
    #[cfg_attr(
        not(target_vendor = "apple"),
        link_name = "\u{1}_Z23JS_SetGrayGCRootsTracerP9JSContextPFbP8JSTracerRN2JS11SliceBudgetEPvES6_"
    )]
    fn SetGrayGCRootsTracer(
        cx: *mut RawJSContext,
        trace: Option<unsafe extern "C" fn(*mut JSTracer, *mut SliceBudget, *mut c_void) -> bool>,
        data: *mut c_void,
    );
    /// `JS::SliceBudget::SliceBudget(JS::TimeBudget, mozilla::Atomic<bool>*)` (complete-object
    /// constructor; it sets the deadline from now).
    #[cfg_attr(
        target_vendor = "apple",
        link_name = "\u{1}__ZN2JS11SliceBudgetC1ENS_10TimeBudgetEPN7mozilla6AtomicIbLNS2_14MemoryOrderingE0EvEE"
    )]
    #[cfg_attr(
        not(target_vendor = "apple"),
        link_name = "\u{1}_ZN2JS11SliceBudgetC1ENS_10TimeBudgetEPN7mozilla6AtomicIbLNS2_14MemoryOrderingE0EvEE"
    )]
    fn SliceBudgetFromTime(this: *mut SliceBudget, time: TimeBudget, interrupt: *mut c_void);
    /// `JS::SliceBudget::SliceBudget(JS::WorkBudget)`, used once to check the budget's layout.
    #[cfg_attr(
        target_vendor = "apple",
        link_name = "\u{1}__ZN2JS11SliceBudgetC1ENS_10WorkBudgetE"
    )]
    #[cfg_attr(
        not(target_vendor = "apple"),
        link_name = "\u{1}_ZN2JS11SliceBudgetC1ENS_10WorkBudgetE"
    )]
    fn SliceBudgetFromWork(this: *mut SliceBudget, work: i64);
    /// `mozilla::TimeStamp::Now(bool)`: SpiderMonkey's clock, in its own ticks.
    #[cfg_attr(
        target_vendor = "apple",
        link_name = "\u{1}__ZN7mozilla9TimeStamp3NowEb"
    )]
    #[cfg_attr(
        not(target_vendor = "apple"),
        link_name = "\u{1}_ZN7mozilla9TimeStamp3NowEb"
    )]
    fn TimeStampNow(high_resolution: bool) -> u64;
}

#[cfg(windows)]
unsafe fn SetGrayGCRootsTracer(
    _: *mut RawJSContext,
    _: Option<unsafe extern "C" fn(*mut JSTracer, *mut SliceBudget, *mut c_void) -> bool>,
    _: *mut c_void,
) {
}

#[cfg(windows)]
unsafe fn SliceBudgetFromTime(_: *mut SliceBudget, _: TimeBudget, _: *mut c_void) {
    unreachable!()
}

#[cfg(windows)]
unsafe fn TimeStampNow(_: bool) -> u64 {
    unreachable!()
}

#[cfg(windows)]
unsafe fn SliceBudgetFromWork(_: *mut SliceBudget, _: i64) {
    unreachable!()
}

/// Byte offset of the `mozilla::Variant` tag inside `SliceBudget::budget` (after its 16 bytes of
/// storage); the alternatives are TimeBudget (0), WorkBudget (1), UnlimitedBudget (2).
const BUDGET_TAG_OFFSET: usize = 16;
const UNLIMITED_TAG: u8 = 2;

unsafe fn budget_tag(budget: *mut SliceBudget) -> *mut u8 {
    unsafe {
        std::ptr::addr_of_mut!((*budget).budget)
            .cast::<u8>()
            .add(BUDGET_TAG_OFFSET)
    }
}

/// Whether `SliceBudget` is laid out as `unlimit_budget` assumes: checked once on budgets built by
/// SpiderMonkey's own constructors.
fn budget_layout_ok() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| unsafe {
        let mut work = std::mem::MaybeUninit::<SliceBudget>::zeroed();
        SliceBudgetFromWork(work.as_mut_ptr(), 1234);
        let mut time = time_budget(Duration::from_millis(5));
        let work_ok = (*work.as_ptr()).counter == 1234 && *budget_tag(work.as_mut_ptr()) == 1;
        let time_ok = time.counter == 1000 && *budget_tag(&mut time) == 0;
        let ok = std::mem::size_of::<SliceBudget>() == 48 && work_ok && time_ok;
        if !ok {
            eprintln!("[gc-sched] SliceBudget layout is not the expected one: SpiderMonkey's slices stay unlimited");
        }
        ok
    })
}

/// Makes a slice's budget unlimited from inside the slice (`SliceBudget::unlimited()`'s fields).
unsafe fn unlimit_budget(budget: *mut SliceBudget) {
    unsafe {
        (*budget).counter = i64::MAX;
        (*budget).interruptRequested = std::ptr::null_mut();
        (*budget).interrupted = false;
        *budget_tag(budget) = UNLIMITED_TAG;
    }
}

/// `JS::TimeBudget` as passed by value: a `mozilla::TimeDuration` (ticks) and a `TimeStamp`.
#[repr(C)]
#[derive(Clone, Copy)]
struct TimeBudget {
    budget_ticks: i64,
    deadline: u64,
}

/// SpiderMonkey clock ticks per ms (ns on Linux; mach ticks on macOS), measured once against
/// `Instant` over a 2 ms spin.
fn ticks_per_ms() -> f64 {
    static TICKS: OnceLock<f64> = OnceLock::new();
    *TICKS.get_or_init(|| {
        let (t0, i0) = (unsafe { TimeStampNow(true) }, Instant::now());
        while i0.elapsed() < Duration::from_millis(2) {
            std::hint::spin_loop();
        }
        let (t1, i1) = (unsafe { TimeStampNow(true) }, Instant::now());
        (t1.wrapping_sub(t0)) as f64 / ((i1 - i0).as_secs_f64() * 1000.)
    })
}

/// A wall-clock budget: SpiderMonkey checks the clock every so many steps and yields when it's
/// over, so a slice ends near its deadline even when the thread was preempted.
fn time_budget(time: Duration) -> SliceBudget {
    let ticks = (time.as_secs_f64() * 1000. * ticks_per_ms()) as i64;
    let mut budget = std::mem::MaybeUninit::<SliceBudget>::uninit();
    unsafe {
        SliceBudgetFromTime(
            budget.as_mut_ptr(),
            TimeBudget {
                budget_ticks: ticks.max(1),
                deadline: 0,
            },
            std::ptr::null_mut(),
        );
        budget.assume_init()
    }
}

/// Sets a runtime up for the schedule. Called when a script or worker runtime is created, before
/// any DOM object exists on its thread.
pub(crate) unsafe fn init_runtime(
    cx: &mut JSContext,
    roots: Option<RootsTracer>,
    roots_data: *mut c_void,
) {
    let config = config();
    if !config.enabled || !config.incremental {
        return;
    }
    incremental_gc::enable();
    ROOTS_TRACER.with(|r| r.set(roots.map(|f| (f, roots_data))));
    unsafe {
        JS_SetGCParameter(cx, JSGCParamKey::JSGC_INCREMENTAL_GC_ENABLED, 1);
        // SpiderMonkey's own slices get our slice budget (the gray-roots hook makes the one that
        // starts sweeping unlimited), or an unlimited one with PARTII_GC_SM_SLICES=0.
        let sm_budget = if sm_slices() {
            config.slice.as_millis().max(1) as u32
        } else {
            0
        };
        JS_SetGCParameter(cx, JSGCParamKey::JSGC_SLICE_TIME_BUDGET_MS, sm_budget);
        let raw = cx.raw_cx();
        SetGrayGCRootsTracer(raw, Some(gray_roots), roots_data);
        if config().incremental_sweep && !incremental_gc::BLACK_REMARK {
            eprintln!(
                "[gc-sched] PARTII_GC_INCREMENTAL_SWEEP=1 ignored: built without the barrier tracer shim (prebuilt mozjs)"
            );
        }
        RAW_CX.with(|c| c.set(raw));
        if budget_layout_ok() {
            // SpiderMonkey's own slices: the black remark first, then its budget.
            SetCreateGCSliceBudgetCallback(cx, Some(sm_slice_budget));
        }
        let previous = SetGCSliceCallback(cx, Some(slice_callback));
        PREVIOUS_SLICE_CALLBACK.with(|p| p.set(previous));
    }
}

/// Whether sweeping runs in slices with script in between (stage 2; needs budgeted SM slices, else
/// SpiderMonkey's first slice of its own would sweep everything anyway).
fn incremental_sweep() -> bool {
    // Sweeping in slices relies on the black remark (the C++ barrier tracer); builds that link
    // mozjs_sys's prebuilt archive have no headers to build it with, and keep sweeping in one slice.
    config().incremental_sweep && sm_slices() && incremental_gc::BLACK_REMARK
}

/// Whether SpiderMonkey's own slices are budgeted (and its GCs incremental).
fn sm_slices() -> bool {
    config().sm_slices && budget_layout_ok()
}

/// The black remark (`incremental_gc::black_remark`) before a slice of a GC that is still marking
/// (or sweeping in slices: later sweep groups still mark). Outside any slice.
fn black_remark_before_slice(cx: *mut RawJSContext) {
    if cx.is_null() || !unsafe { js::jsapi::IsIncrementalGCInProgress(cx) } {
        return;
    }
    if SWEEP_STARTED.with(Cell::get) && !incremental_sweep() {
        return;
    }
    let started = Instant::now();
    let roots = ROOTS_TRACER.with(Cell::get);
    let done = unsafe { incremental_gc::black_remark(cx, roots) };
    let took = started.elapsed();
    BLACK_REMARKS.with(|b| {
        let (n, edges, max, total) = b.get();
        b.set((n + 1, edges + done.edges, max.max(took), total + took));
    });
}

/// SpiderMonkey's slice-budget callback, for the slices it runs by itself (heap idle here): the
/// black remark, then the budget (`millis` is JSGC_SLICE_TIME_BUDGET_MS; 0 means unlimited).
unsafe extern "C" fn sm_slice_budget(_reason: GCReason, millis: i64) -> SliceBudget {
    black_remark_before_slice(RAW_CX.with(Cell::get));
    let mut budget = time_budget(Duration::from_millis(millis.max(1) as u64));
    if millis <= 0 {
        unsafe { unlimit_budget(&mut budget) };
    }
    budget
}

/// SpiderMonkey calls this when marking of a sweep group is done, before it sweeps it: the remark.
/// From here on the slice runs to the end of the GC (sweeping in one pause).
unsafe extern "C" fn gray_roots(
    trc: *mut JSTracer,
    budget: *mut SliceBudget,
    _data: *mut c_void,
) -> bool {
    if !IN_CYCLE.with(Cell::get) {
        // Runtime teardown (`finishRoots`): no marking, and nothing may be traced.
        return true;
    }
    SWEEP_STARTED.with(|s| s.set(true));
    if !incremental_sweep() && sm_slices() && !budget.is_null() {
        unsafe { unlimit_budget(budget) };
    }
    if let Some((roots, data)) = ROOTS_TRACER.with(Cell::get) {
        unsafe { roots(trc, data) };
    }
    let (registered, traced) = unsafe { incremental_gc::remark(trc) };
    if config().log {
        eprintln!("[gc-sched] remark: {registered} reflectors, {traced} re-traced");
    }
    true
}

/// Turns the expose barrier off inside every GC slice (finalizers must not barrier), and resets
/// our flags when a GC ends.
unsafe extern "C" fn slice_callback(
    cx: *mut RawJSContext,
    progress: GCProgress,
    desc: *const GCDescription,
) {
    match progress {
        GCProgress::GC_CYCLE_BEGIN => {
            IN_CYCLE.with(|c| c.set(true));
            incremental_gc::set_marking(false);
            incremental_gc::set_sweeping(false);
            SWEEP_STARTED.with(|s| s.set(false));
        },
        GCProgress::GC_SLICE_BEGIN => {
            incremental_gc::set_marking(false);
            incremental_gc::set_sweeping(false);
            if !OUR_SLICE.with(Cell::get) {
                SM_SLICE_BEGAN.with(|b| b.set(Some(Instant::now())));
            }
        },
        GCProgress::GC_SLICE_END => {
            if let Some(began) = SM_SLICE_BEGAN.with(Cell::take) {
                let pause = began.elapsed();
                SM_SLICES.with(|s| {
                    let (n, max, total) = s.get();
                    s.set((n + 1, max.max(pause), total + pause));
                });
            }
            // Between slices of a GC that is still marking, Rust's reads of reflectors shade them
            // (while sweeping in slices too: later sweep groups still mark). While sweeping,
            // WeakRef treats dead objects that aren't finalized yet as gone.
            let in_progress = unsafe { js::jsapi::IsIncrementalGCInProgress(cx) };
            let sweeping = SWEEP_STARTED.with(Cell::get);
            incremental_gc::set_marking(in_progress && (!sweeping || incremental_sweep()));
            incremental_gc::set_sweeping(in_progress && sweeping);
        },
        GCProgress::GC_CYCLE_END => {
            IN_CYCLE.with(|c| c.set(false));
            incremental_gc::set_marking(false);
            incremental_gc::set_sweeping(false);
            SWEEP_STARTED.with(|s| s.set(false));
        },
    }
    if let Some(previous) = PREVIOUS_SLICE_CALLBACK.with(Cell::get) {
        unsafe { previous(cx, progress, desc) };
    }
}

/// Starts an incremental GC with a first slice.
fn start_incremental(cx: &mut JSContext, state: &State, kind: Kind) {
    let before = param(cx, JSGCParamKey::JSGC_BYTES);
    if config().finalize_stats {
        script_bindings::take_finalize_stats();
    }
    let started = Instant::now();
    SM_SLICES.with(|s| s.set(Default::default()));
    BLACK_REMARKS.with(|b| b.set(Default::default()));
    let budget = time_budget(config().slice);
    our_slice(|| unsafe {
        PrepareForFullGC(cx);
        StartIncrementalGC(cx, GCOptions::Normal, GCReason::FULL_GC_TIMER, &budget);
    });
    let gc = IncrementalGc {
        adopted: false,
        started,
        before,
        slices: 0,
        last_slice: started,
        max_slice: Duration::ZERO,
        total: Duration::ZERO,
    };
    after_slice(cx, state, kind, gc, started);
}

/// Runs the next slice of our incremental GC if one is due. Returns whether a GC is in progress
/// (then nothing else should be scheduled this turn).
fn incremental_step(cx: &mut JSContext, state: &State, kind: Kind, now: Instant) -> bool {
    let in_progress = unsafe { IsIncrementalGCInProgress(cx) };
    let Some(gc) = state.incremental.get() else {
        if in_progress && sm_slices() {
            // SpiderMonkey started one by itself (allocation trigger): drive it like ours.
            state.incremental.set(Some(IncrementalGc {
                adopted: true,
                started: now,
                before: param(cx, JSGCParamKey::JSGC_BYTES),
                slices: 0,
                last_slice: now,
                max_slice: Duration::ZERO,
                total: Duration::ZERO,
            }));
            return true;
        }
        return false;
    };
    if !in_progress {
        // SpiderMonkey finished it in a slice of its own.
        finish_log(cx, state, kind, gc, "finished by SpiderMonkey");
        return false;
    }
    if now - gc.last_slice < config().slice_gap {
        return true;
    }
    let started = Instant::now();
    black_remark_before_slice(unsafe { cx.raw_cx() });
    if started - gc.started > config().incremental_max {
        our_slice(|| unsafe { FinishIncrementalGC(cx, GCReason::FULL_GC_TIMER) });
        let mut gc = gc;
        gc.slices += 1;
        let pause = Instant::now() - started;
        gc.max_slice = gc.max_slice.max(pause);
        gc.total += pause;
        finish_log(cx, state, kind, gc, "over its time limit, finished");
        return true;
    }
    let budget = time_budget(config().slice);
    our_slice(|| unsafe { IncrementalGCSlice(cx, GCReason::FULL_GC_TIMER, &budget) });
    after_slice(cx, state, kind, gc, started);
    true
}

/// Runs a GC call of the scheduler's (its slices aren't counted as SpiderMonkey's).
fn our_slice(f: impl FnOnce()) {
    OUR_SLICE.with(|o| o.set(true));
    f();
    OUR_SLICE.with(|o| o.set(false));
}

/// After a slice: finishes the GC if sweeping started, else turns the expose barrier on.
fn after_slice(
    cx: &mut JSContext,
    state: &State,
    kind: Kind,
    mut gc: IncrementalGc,
    started: Instant,
) {
    if unsafe { IsIncrementalGCInProgress(cx) }
        && SWEEP_STARTED.with(Cell::get)
        && !incremental_sweep()
    {
        our_slice(|| unsafe { FinishIncrementalGC(cx, GCReason::FULL_GC_TIMER) });
    }
    let now = Instant::now();
    let pause = now - started;
    gc.slices += 1;
    gc.last_slice = now;
    gc.max_slice = gc.max_slice.max(pause);
    gc.total += pause;
    if unsafe { IsIncrementalGCInProgress(cx) } {
        state.incremental.set(Some(gc));
    } else {
        finish_log(cx, state, kind, gc, "done");
    }
}

/// Bookkeeping and the log line when our incremental GC is over.
fn finish_log(cx: &JSContext, state: &State, kind: Kind, gc: IncrementalGc, how: &str) {
    state.incremental.set(None);
    incremental_gc::set_marking(false);
    let now = Instant::now();
    let (sm_n, sm_max, sm_total) = SM_SLICES.with(|s| s.replace(Default::default()));
    let (br_n, br_edges, br_max, br_total) = BLACK_REMARKS.with(|b| b.replace(Default::default()));
    state.last_pause.set(gc.max_slice.max(sm_max));
    state
        .major_number
        .set(param(cx, JSGCParamKey::JSGC_MAJOR_GC_NUMBER));
    state.last_major.set(now);
    let after = param(cx, JSGCParamKey::JSGC_BYTES);
    state.bytes_after.set(after);
    let nursery = state.nursery_at.take();
    if config().log {
        let finalized = if config().finalize_stats {
            let (n, ns) = script_bindings::take_finalize_stats();
            format!(", finalized {} DOM objects in {:.2} ms", n, ns as f64 / 1e6)
        } else {
            String::new()
        };
        eprintln!(
            "[gc-sched] {} incremental GC{} {}: {} slices, max {:.2} ms, total {:.2} ms; SpiderMonkey's {} slices, max {:.2} ms, total {:.2} ms; black remark {}x, {} edges, max {:.2} ms, total {:.2} ms; over {} ms, {} -> {} KB{}{}",
            if kind == Kind::Worker {
                "worker"
            } else {
                "window"
            },
            if gc.adopted {
                " (started by SpiderMonkey)"
            } else {
                ""
            },
            how,
            gc.slices,
            gc.max_slice.as_secs_f64() * 1000.,
            gc.total.as_secs_f64() * 1000.,
            sm_n,
            sm_max.as_secs_f64() * 1000.,
            sm_total.as_secs_f64() * 1000.,
            br_n,
            br_edges,
            br_max.as_secs_f64() * 1000.,
            br_total.as_secs_f64() * 1000.,
            (now - gc.started).as_millis(),
            gc.before / 1024,
            after / 1024,
            nursery
                .map(|at| format!(
                    ", nursery {:.2} ms",
                    state.nursery_pause.get().as_secs_f64() * 1000.
                ))
                .unwrap_or_default(),
            finalized
        );
    }
    let _ = nursery;
}
