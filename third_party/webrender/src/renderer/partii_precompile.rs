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

use crate::device::{Device, UploadMethod, WarmEntry, WarmScratch};
use crate::renderer::{shade::Shaders, vertex};
use crate::renderer::init::WebRenderOptions;
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Makes and makes current a GL context on the calling (background) thread for the precompile:
/// the same GL API as the renderer's (so the same shader sources). The box keeps the context
/// alive until the thread is done.
pub type PartiiPrecompileContext =
    Box<dyn FnOnce() -> Option<(Rc<dyn gleam::gl::Gl>, Box<dyn std::any::Any>)> + Send>;

const RUNNING: u8 = 0;
const DONE: u8 = 1;
const FAILED: u8 = 2;

fn log_ms() -> Option<f64> {
    static MS: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
    *MS.get_or_init(|| std::env::var("PARTII_WR_PRECOMPILE_LOG").ok().and_then(|v| v.parse().ok()))
}

pub(crate) fn enabled() -> bool {
    std::env::var("PARTII_WR_PRECOMPILE").as_deref() != Ok("0")
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
    pending: VecDeque<WarmEntry>,
    scratch: Option<WarmScratch>,
    budget: Duration,
    total: usize,
    replayed: usize,
    frames: usize,
    max_frame_ms: f64,
    started: Instant,
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
        let thread_state = state.clone();
        let thread_entries = entries.clone();
        let spawned = std::thread::Builder::new()
            .name("WRPrecompile".to_owned())
            .spawn(move || {
                let ok = compile(thread_entries, make_context, config);
                thread_state.store(if ok { DONE } else { FAILED }, Ordering::Release);
            });
        if spawned.is_err() {
            return None;
        }
        let budget_ms: u64 = std::env::var("PARTII_WR_PRECOMPILE_FRAME_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
        Some(PartiiPrecompile {
            state,
            total: entries.len(),
            pending: entries.into(),
            scratch: Some(WarmScratch::default()),
            budget: Duration::from_millis(budget_ms),
            replayed: 0,
            frames: 0,
            max_frame_ms: 0.0,
            started: Instant::now(),
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
        match self.state.load(Ordering::Acquire) {
            RUNNING => return true,
            FAILED => {
                eprintln!("partii-wr-precompile: the background context failed; nothing replayed");
                return false;
            },
            _ => {},
        }
        let t0 = Instant::now();
        let mut scratch = self.scratch.take().unwrap_or_default();
        let mut drawn = false;
        while !self.pending.is_empty() {
            if drawn && t0.elapsed() >= self.budget {
                break;
            }
            let entry = self.pending.pop_front().unwrap();
            let t_entry = Instant::now();
            let mut bind_ms = 0.0;
            if let Some(kind) = shaders.partii_bind(&entry.program, device) {
                bind_ms = t_entry.elapsed().as_secs_f64() * 1000.0;
                device.partii_warm_draw(&entry, &vaos[kind], &mut scratch);
                self.replayed += 1;
            }
            drawn = true;
            // Measurement: PARTII_WR_PRECOMPILE_LOG=<ms> logs each in-renderer replay of <ms> or more.
            if let Some(min) = log_ms() {
                let ms = t_entry.elapsed().as_secs_f64() * 1000.0;
                if ms >= min {
                    eprintln!(
                        "partii-wr-precompile: slow replay {ms:.1} ms (bind {bind_ms:.1}) at {:.0} ms: {}",
                        self.started.elapsed().as_secs_f64() * 1000.0,
                        entry.to_line(),
                    );
                }
            }
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.frames += 1;
        self.max_frame_ms = self.max_frame_ms.max(ms);
        if self.pending.is_empty() {
            device.partii_warm_finish(scratch);
            eprintln!(
                "partii-wr-precompile: replayed {} of {} states in {} frames (longest {:.1} ms), done {:.0} ms after start",
                self.replayed,
                self.total,
                self.frames,
                self.max_frame_ms,
                self.started.elapsed().as_secs_f64() * 1000.0,
            );
            return false;
        }
        device.partii_warm_restore();
        self.scratch = Some(scratch);
        true
    }
}

/// The background thread: a device and WebRender's programs on its own context, every entry drawn
/// once so Mesa compiles (and caches on disk) its variants.
fn compile(
    entries: Vec<WarmEntry>,
    make_context: PartiiPrecompileContext,
    config: PrecompileConfig,
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
        let t_entry = Instant::now();
        if let Some(kind) = shaders.partii_bind(&entry.program, &mut device) {
            device.partii_warm_draw(entry, &vaos[kind], &mut scratch);
            drawn += 1;
        }
        if let Some(min) = log_ms() {
            let ms = t_entry.elapsed().as_secs_f64() * 1000.0;
            if ms >= min {
                eprintln!("partii-wr-precompile: background {ms:.1} ms: {}", entry.to_line());
            }
        }
    }
    device.partii_warm_finish(scratch);
    eprintln!(
        "partii-wr-precompile: {drawn} of {} states compiled on a background context in {:.0} ms",
        entries.len(),
        t0.elapsed().as_secs_f64() * 1000.0,
    );
    // Measurement (gaps-7): PARTII_WR_PRECOMPILE_SETTLE_MS keeps the context (and Mesa's disk-cache
    // writer) alive that long before the renderer replays, to tell late cache writes from misses.
    if let Some(ms) = std::env::var("PARTII_WR_PRECOMPILE_SETTLE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        std::thread::sleep(Duration::from_millis(ms));
    }
    vaos.deinit(&mut device);
    shaders.deinit(&mut device);
    device.end_frame();
    drop(device);
    drop(keep);
    true
}
