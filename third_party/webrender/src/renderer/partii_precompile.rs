/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Partii (gaps-7): software-GL shader variants compiled off the frame path.
//!
//! The gaps-6 warm-up (device/partii_warm.rs) replays the draw states earlier runs recorded before
//! the first frame. That leaves a machine's first run (nothing recorded: every new screen's first
//! frame compiles its variants, 80-170 ms each, e.g. rocket's first GAME OVER at 1009 ms) and
//! states the start-up budget cut off. Replaying them all at start on a cold Mesa cache takes
//! seconds (211 farm states: 7302 ms), too long before the first frame.
//!
//! So, on a software GL: a background thread makes its own GL context (the embedder's
//! `partii_precompile_context`), builds WebRender's programs there and replays the pending states
//! (the built-in seed of states the farm's games draw, then any the start-up budget left). Mesa
//! keeps variants per context, but its disk cache is shared: once the thread is done, the
//! renderer replays the same states itself a few ms per frame, each now a cache hit (warm replay:
//! 211 states in 810 ms, ~4 ms each) instead of a compile inside a frame.
//!
//! `PARTII_WR_PRECOMPILE=0` turns it off; `PARTII_WR_PRECOMPILE_FRAME_MS` is the per-frame
//! budget of the in-renderer replay (default 4; one state at least per frame).
//!
//! Bounded (batch 4, 2026-11-24): on a cold Mesa cache some replayed states still compile in the renderer's
//! context (clips a2c52130: 120-148 ms single-state frames 10-14 s into 2d/bench/glitterfall, their 10 s-end
//! max 119-133 ms vs 52-54 without the precompile; a smaller per-frame budget can't help, one state is the
//! unit). So a replayed state that takes `PARTII_WR_PRECOMPILE_SLOW_MS` (default 16) or more ends that
//! frame's replay, and after `PARTII_WR_PRECOMPILE_SLOW_MAX` (default 2) such states the rest are dropped:
//! the cache is missing in this context, and replaying on only moves compiles into frames the page may never
//! need. `PARTII_WR_PRECOMPILE_SLOW_MAX=0`: no limit (the old replay).
//!
//! Default since batch 4 (clips a1743c72, cold, 3 reps): background compile only (fills Mesa's disk cache), no
//! in-renderer replay. Worst frame after 10 s: 2d 140 (old) / 34 (bounded) / 20 (no replay) / 19 (off) ms, bench
//! 128 / 37 / 18 / 17; glitterfall, gl-worker ~19 under all four; rocket's first GAME OVER ~200 under all four
//! (the replay's gain there didn't reproduce). `PARTII_WR_PRECOMPILE_REPLAY=1` turns the bounded replay back on.
//!
//! Deferred (app stream, 2026-11-24): on a cold Mesa cache the compile ran from WebRender's start through the
//! app's first contentful frame (8.4 s, 146 of 225 states, on a 2-CPU farm job) and slowed that frame (the
//! launcher's first paint 2514 vs 1824 ms without it). Now the thread waits until the first frame plus
//! `PARTII_WR_PRECOMPILE_DELAY_MS` (default 2000), or until the embedder says the app is usable
//! (`Renderer::partii_precompile_go`), whichever comes first, and compiles at idle priority (Linux
//! SCHED_IDLE, else nice 19). `PARTII_WR_PRECOMPILE_DEFER=0`: the old start (at once, normal priority).
//!
//! At shutdown a thread still compiling is stopped and waited for (`PartiiPrecompile::stop`, from
//! `Renderer::deinit` and on drop): it checks a flag between draws, releases its programs and drops
//! its context. Left running, it was still inside Mesa when the app tore GL down and exited, and
//! the first (cold) launch of the farm's play test exited 127 instead of 0 (gaps-7: brawlii-versus
//! and classic-3d, runs 2026-11-16T0504Z / 0837Z, once the second pass made a cold compile 14-26 s).

use crate::device::{Device, UploadMethod, WarmEntry, WarmScratch};
use crate::renderer::{shade::Shaders, vertex};
use crate::renderer::init::WebRenderOptions;
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Makes and makes current a GL context on the calling (background) thread for the precompile:
/// the same GL API as the renderer's (so the same shader sources). The box keeps the context
/// alive until the thread is done.
pub type PartiiPrecompileContext =
    Box<dyn FnOnce() -> Option<(Rc<dyn gleam::gl::Gl>, Box<dyn std::any::Any>)> + Send>;

const RUNNING: u8 = 0;
const DONE: u8 = 1;
const FAILED: u8 = 2;
/// Stopped at shutdown before it finished: nothing to replay.
const STOPPED: u8 = 3;
/// How long shutdown waits for the thread to see the stop flag (one draw: a variant compile).
const STOP_WAIT: Duration = Duration::from_secs(3);

pub(crate) fn enabled() -> bool {
    std::env::var("PARTII_WR_PRECOMPILE").as_deref() != Ok("0")
}

fn deferred() -> bool {
    std::env::var("PARTII_WR_PRECOMPILE_DEFER").as_deref() != Ok("0")
}

/// When the background compile may start: the first frame's time (plus the delay), or the embedder's go.
#[derive(Default)]
struct Gate {
    first_frame: Option<Instant>,
    go: bool,
}

type SharedGate = Arc<(Mutex<Gate>, Condvar)>;

/// The thread's wait: until `go`, or the first frame plus `delay`, or the stop. False: stopped.
fn wait_gate(gate: &SharedGate, delay: Duration, stop: &AtomicBool) -> bool {
    let (lock, cv) = &**gate;
    let mut g = lock.lock().unwrap();
    loop {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        if g.go || g.first_frame.is_some_and(|t| t.elapsed() >= delay) {
            return true;
        }
        // Short waits: the stop flag isn't signalled through the condvar.
        let step = match g.first_frame {
            Some(t) => delay.saturating_sub(t.elapsed()).min(Duration::from_millis(50)),
            None => Duration::from_millis(50),
        };
        g = cv.wait_timeout(g, step.max(Duration::from_millis(1))).unwrap().0;
    }
}

/// The calling thread at idle priority: it runs on CPU no other thread of the machine wants.
#[cfg(target_os = "linux")]
fn idle_priority() -> &'static str {
    #[repr(C)]
    struct SchedParam {
        sched_priority: i32,
    }
    extern "C" {
        fn sched_setscheduler(pid: i32, policy: i32, param: *const SchedParam) -> i32;
        fn setpriority(which: i32, who: u32, prio: i32) -> i32;
    }
    const SCHED_IDLE: i32 = 5;
    const PRIO_PROCESS: i32 = 0;
    let p = SchedParam { sched_priority: 0 };
    // pid 0 / who 0: the calling thread (Linux schedules and nices threads one by one).
    if unsafe { sched_setscheduler(0, SCHED_IDLE, &p) } == 0 {
        "idle"
    } else if unsafe { setpriority(PRIO_PROCESS, 0, 19) } == 0 {
        "nice 19"
    } else {
        "normal"
    }
}

#[cfg(not(target_os = "linux"))]
fn idle_priority() -> &'static str {
    "normal"
}

/// What the background device is built with: the renderer's options that change shader sources or
/// the replay's scratch textures.
pub(crate) struct PrecompileConfig {
    pub resource_override_path: Option<PathBuf>,
    pub use_optimized_shaders: bool,
    pub allow_texture_storage_support: bool,
    pub allow_texture_swizzling: bool,
    pub surface_origin_is_top_left: bool,
    pub allow_dual_source_blending: bool,
    pub allow_advanced_blend_equation: bool,
    pub enable_dithering: bool,
    pub indexed_quads: Option<NonZeroUsize>,
}

/// The renderer's side: the states to replay once the background thread is done.
pub(crate) struct PartiiPrecompile {
    state: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pending: VecDeque<WarmEntry>,
    scratch: Option<WarmScratch>,
    budget: Duration,
    gate: SharedGate,
    total: usize,
    replayed: usize,
    frames: usize,
    max_frame_ms: f64,
    started: Instant,
    /// False (the default): background compile only. `PARTII_WR_PRECOMPILE_REPLAY=1`: the bounded replay.
    replay: bool,
    /// A replayed state this slow or slower counts as a compile in the renderer's context.
    slow_ms: f64,
    /// Slow states after which the rest are dropped (0: no limit).
    slow_max: usize,
    slow: usize,
    dropped: usize,
}

fn env_num<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

impl PartiiPrecompile {
    /// Starts the background compile of `entries`.
    pub(crate) fn start(
        entries: Vec<WarmEntry>,
        make_context: PartiiPrecompileContext,
        config: PrecompileConfig,
    ) -> Option<Self> {
        if entries.is_empty() {
            return None;
        }
        let state = Arc::new(AtomicU8::new(RUNNING));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = state.clone();
        let thread_stop = stop.clone();
        let thread_entries = entries.clone();
        let gate: SharedGate = Arc::default();
        let thread_gate = gate.clone();
        let defer = deferred();
        let delay = Duration::from_millis(
            std::env::var("PARTII_WR_PRECOMPILE_DELAY_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2000),
        );
        let spawned = std::thread::Builder::new()
            .name("WRPrecompile".to_owned())
            .spawn(move || {
                let t0 = Instant::now();
                if defer {
                    if !wait_gate(&thread_gate, delay, &thread_stop) {
                        thread_state.store(STOPPED, Ordering::Release);
                        return;
                    }
                    let prio = idle_priority();
                    eprintln!(
                        "partii-wr-precompile: starting {:.0} ms after WebRender ({prio} priority)",
                        t0.elapsed().as_secs_f64() * 1000.0
                    );
                }
                let ok = compile(thread_entries, make_context, config, &thread_stop);
                let end = if thread_stop.load(Ordering::Acquire) {
                    STOPPED
                } else if ok {
                    DONE
                } else {
                    FAILED
                };
                thread_state.store(end, Ordering::Release);
            });
        let Ok(thread) = spawned else {
            return None;
        };
        let budget_ms: u64 = std::env::var("PARTII_WR_PRECOMPILE_FRAME_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
        Some(PartiiPrecompile {
            state,
            stop,
            thread: Some(thread),
            total: entries.len(),
            pending: entries.into(),
            scratch: Some(WarmScratch::default()),
            budget: Duration::from_millis(budget_ms),
            gate,
            replayed: 0,
            frames: 0,
            max_frame_ms: 0.0,
            started: Instant::now(),
            replay: std::env::var("PARTII_WR_PRECOMPILE_REPLAY").as_deref() == Ok("1"),
            slow_ms: env_num("PARTII_WR_PRECOMPILE_SLOW_MS", 16.0),
            slow_max: env_num("PARTII_WR_PRECOMPILE_SLOW_MAX", 2),
            slow: 0,
            dropped: 0,
        })
    }

    /// Called inside a frame, before WebRender draws: replays pending states within the budget once
    /// the background compile is done. Returns false when there's nothing more to do.
    pub(crate) fn on_frame(
        &mut self,
        device: &mut Device,
        shaders: &mut Shaders,
        vaos: &vertex::RendererVAOs,
    ) -> bool {
        {
            // This frame's start: the deferred compile counts its delay from the first one.
            let (lock, cv) = &*self.gate;
            let mut g = lock.lock().unwrap();
            if g.first_frame.is_none() {
                g.first_frame = Some(Instant::now());
                cv.notify_all();
            }
        }
        match self.state.load(Ordering::Acquire) {
            RUNNING => return true,
            STOPPED => return false,
            FAILED => {
                eprintln!("partii-wr-precompile: the background context failed; nothing replayed");
                return false;
            },
            _ => {},
        }
        if !self.replay {
            eprintln!(
                "partii-wr-precompile: background compile done {:.0} ms after start; in-renderer replay off (default)",
                self.started.elapsed().as_secs_f64() * 1000.0
            );
            // Nothing was drawn with the scratch: no GL objects to free.
            self.scratch = None;
            return false;
        }
        let t0 = Instant::now();
        let mut scratch = self.scratch.take().unwrap_or_default();
        let mut drawn = false;
        while !self.pending.is_empty() {
            if drawn && t0.elapsed() >= self.budget {
                break;
            }
            let entry = self.pending.pop_front().unwrap();
            let s0 = Instant::now();
            if let Some(kind) = shaders.partii_bind(&entry.program, device) {
                device.partii_warm_draw(&entry, &vaos[kind], &mut scratch);
                self.replayed += 1;
            }
            drawn = true;
            let state_ms = s0.elapsed().as_secs_f64() * 1000.0;
            if state_ms >= self.slow_ms {
                // A compile in this context: no more replay this frame.
                self.slow += 1;
                if self.slow_max > 0 && self.slow >= self.slow_max {
                    self.dropped = self.pending.len();
                    self.pending.clear();
                    eprintln!(
                        "partii-wr-precompile: {} replays took {:.0} ms or more (the last {:.1} ms): the renderer's \
                         context misses the cache, so the other {} states are dropped",
                        self.slow, self.slow_ms, state_ms, self.dropped
                    );
                }
                break;
            }
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.frames += 1;
        self.max_frame_ms = self.max_frame_ms.max(ms);
        if self.pending.is_empty() {
            device.partii_warm_finish(scratch);
            eprintln!(
                "partii-wr-precompile: replayed {} of {} states in {} frames (longest {:.1} ms, {} slow, {} dropped), done {:.0} ms after start",
                self.replayed,
                self.total,
                self.frames,
                self.max_frame_ms,
                self.slow,
                self.dropped,
                self.started.elapsed().as_secs_f64() * 1000.0,
            );
            return false;
        }
        device.partii_warm_restore();
        self.scratch = Some(scratch);
        true
    }
}

impl PartiiPrecompile {
    /// The embedder's "the app is usable": a deferred compile starts now instead of at its delay.
    pub(crate) fn go(&self) {
        let (lock, cv) = &*self.gate;
        lock.lock().unwrap().go = true;
        cv.notify_all();
    }

    /// Shutdown: tells a thread still compiling to stop after its current draw and waits (at most
    /// `STOP_WAIT`) for it to release its programs and context, so it is out of GL before the
    /// renderer and the embedder tear GL down. Idempotent; a finished thread is just joined.
    pub(crate) fn stop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.stop.store(true, Ordering::Release);
        let t0 = Instant::now();
        while self.state.load(Ordering::Acquire) == RUNNING && t0.elapsed() < STOP_WAIT {
            std::thread::sleep(Duration::from_millis(5));
        }
        if self.state.load(Ordering::Acquire) == RUNNING {
            eprintln!(
                "partii-wr-precompile: still compiling {:.0} ms after the stop; not waiting",
                t0.elapsed().as_secs_f64() * 1000.0
            );
            return;
        }
        let _ = thread.join();
        if self.state.load(Ordering::Acquire) == STOPPED {
            eprintln!(
                "partii-wr-precompile: stopped at shutdown ({:.0} ms after start, waited {:.0} ms)",
                self.started.elapsed().as_secs_f64() * 1000.0,
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}

impl Drop for PartiiPrecompile {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The background thread: a device and WebRender's programs on its own context, every entry drawn
/// once so Mesa compiles (and caches on disk) its variants. `stop` (shutdown) ends it between draws.
fn compile(
    entries: Vec<WarmEntry>,
    make_context: PartiiPrecompileContext,
    config: PrecompileConfig,
    stop: &AtomicBool,
) -> bool {
    let t0 = Instant::now();
    let Some((gl, keep)) = make_context() else {
        return false;
    };
    let gl_type = gl.get_type();
    let mut device = Device::new(
        gl,
        None,
        config.resource_override_path.clone(),
        config.use_optimized_shaders,
        UploadMethod::Immediate,
        0,
        None,
        config.allow_texture_storage_support,
        config.allow_texture_swizzling,
        None,
        config.surface_origin_is_top_left,
        false,
    );
    let options = WebRenderOptions {
        resource_override_path: config.resource_override_path,
        use_optimized_shaders: config.use_optimized_shaders,
        allow_dual_source_blending: config.allow_dual_source_blending,
        allow_advanced_blend_equation: config.allow_advanced_blend_equation,
        enable_dithering: config.enable_dithering,
        ..Default::default()
    };
    device.begin_frame();
    let mut shaders = match Shaders::new(&mut device, gl_type, &options) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("partii-wr-precompile: shaders failed on the background context: {e:?}");
            device.end_frame();
            drop(device);
            drop(keep);
            return false;
        },
    };
    let vaos = vertex::RendererVAOs::new(&mut device, config.indexed_quads);
    let mut scratch = WarmScratch::default();
    let mut drawn = 0;
    for entry in &entries {
        if stop.load(Ordering::Acquire) {
            break;
        }
        if let Some(kind) = shaders.partii_bind(&entry.program, &mut device) {
            device.partii_warm_draw(entry, &vaos[kind], &mut scratch);
            drawn += 1;
        }
    }
    if !stop.load(Ordering::Acquire) {
        second_pass(&mut device, gl_type, &options, &entries, &vaos, &mut scratch, stop);
    }
    device.partii_warm_finish(scratch);
    eprintln!(
        "partii-wr-precompile: {drawn} of {} states compiled on a background context in {:.0} ms",
        entries.len(),
        t0.elapsed().as_secs_f64() * 1000.0,
    );
    vaos.deinit(&mut device);
    shaders.deinit(&mut device);
    device.end_frame();
    drop(device);
    drop(keep);
    true
}

/// Mesa's llvmpipe keys its disk cache on the variant key plus the shader's NIR, and building a
/// variant runs gallivm's prepasses on that NIR in place (out of SSA, locals to registers:
/// lp_build_nir_soa, for the draw module's vertex shaders as for fragment shaders). So in each
/// context a program's first variant is keyed on the NIR as linked and every later one on the
/// rewritten NIR. The renderer has usually drawn a program with its real textures (one vertex
/// variant) before it replays the program's first seed state with the scratch textures (another
/// vertex key), which it then builds on rewritten NIR; the loop above built that key as the
/// program's first variant, on the NIR as linked, so the renderer missed: a 70-100 ms vertex
/// compile per program (gaps-7: 8-9 of the ~10 slow replays of a cold run, GALLIVM_DEBUG=perf).
/// Later states of a program share its vertex key and hit. So, on a second set of program objects
/// (programs link lazily): each program's first state with other sampler state first (another
/// vertex and fragment key, which rewrites the NIR), then the first state itself.
/// `PARTII_WR_PRECOMPILE_SECOND=0` skips it.
fn second_pass(
    device: &mut Device,
    gl_type: gleam::gl::GlType,
    options: &WebRenderOptions,
    entries: &[WarmEntry],
    vaos: &vertex::RendererVAOs,
    scratch: &mut WarmScratch,
    stop: &AtomicBool,
) {
    if std::env::var("PARTII_WR_PRECOMPILE_SECOND").as_deref() == Ok("0") {
        return;
    }
    let t0 = Instant::now();
    let mut firsts: Vec<&WarmEntry> = Vec::new();
    for e in entries {
        if !firsts.iter().any(|f| f.program == e.program) {
            firsts.push(e);
        }
    }
    let mut shaders = match Shaders::new(device, gl_type, options) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("partii-wr-precompile: second program set failed: {e:?}");
            return;
        },
    };
    let mut drawn = 0;
    for &first in &firsts {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let Some(kind) = shaders.partii_bind(&first.program, device) else {
            continue;
        };
        device.partii_warm_draw(&first.partii_primer(), &vaos[kind], scratch);
        device.partii_warm_draw(first, &vaos[kind], scratch);
        drawn += 1;
    }
    shaders.deinit(device);
    eprintln!(
        "partii-wr-precompile: second pass: {drawn} of {} programs' first states as later variants in {:.0} ms",
        firsts.len(),
        t0.elapsed().as_secs_f64() * 1000.0,
    )
}
