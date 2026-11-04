/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Render-quantum timing, for spotting audio glitches (`PARTII_AUDIO_STATS=1`).
//!
//! A real-time context renders ahead of the device by the sink's buffer (about 20 ms with the
//! native backend), so a single quantum that takes longer than that is an audible glitch, and
//! quanta that keep taking longer than their own duration (128 frames, 2.67 ms at 48 kHz) drain
//! the buffer. Every few seconds the render thread logs a histogram summary to stderr:
//!
//!     [audio-stats] 5.0s quanta=1875 mean=0.12ms p99=0.40ms max=3.10ms over_budget=1 over10ms=0 over20ms=0 idle_hook_max=2.9ms underruns=0 underrun_frames=0
//!
//! `underruns` come from the sink (device side): callbacks that found the FIFO short after the
//! stream first filled. They are the audible glitches.
//!
//! Plus totals since the context started when it closes. Embedders (the AudioWorklet's GC)
//! report their own work through [`note_idle_work`].

use std::cell::RefCell;
use std::time::{Duration, Instant};

/// 0.05 ms buckets up to 50 ms; the last bucket holds everything slower.
const BUCKET_US: u64 = 50;
const BUCKETS: usize = 1001;

pub(crate) struct RenderStats {
    enabled: bool,
    budget: Duration,
    window_start: Instant,
    window: Histogram,
    total: Histogram,
    idle_max: Duration,
    total_idle_max: Duration,
    /// From the sink: (events, missing frames) since the stream started.
    underruns: Option<(u64, u64)>,
    window_underruns: (u64, u64),
}

#[derive(Clone)]
struct Histogram {
    counts: Vec<u32>,
    n: u64,
    sum: Duration,
    max: Duration,
    over_budget: u64,
    over_10ms: u64,
    over_20ms: u64,
}

impl Histogram {
    fn new() -> Self {
        Histogram {
            counts: vec![0; BUCKETS],
            n: 0,
            sum: Duration::ZERO,
            max: Duration::ZERO,
            over_budget: 0,
            over_10ms: 0,
            over_20ms: 0,
        }
    }

    fn add(&mut self, d: Duration, budget: Duration) {
        let bucket = ((d.as_micros() as u64) / BUCKET_US).min(BUCKETS as u64 - 1) as usize;
        self.counts[bucket] += 1;
        self.n += 1;
        self.sum += d;
        self.max = self.max.max(d);
        self.over_budget += (d > budget) as u64;
        self.over_10ms += (d > Duration::from_millis(10)) as u64;
        self.over_20ms += (d > Duration::from_millis(20)) as u64;
    }

    fn percentile_ms(&self, p: f64) -> f64 {
        let target = ((self.n as f64) * p).ceil() as u64;
        let mut seen = 0;
        for (i, c) in self.counts.iter().enumerate() {
            seen += *c as u64;
            if seen >= target {
                return ((i as u64 + 1) * BUCKET_US) as f64 / 1000.;
            }
        }
        0.
    }

    fn line(&self, label: &str, idle_max: Duration) -> String {
        let ms = |d: Duration| d.as_secs_f64() * 1000.;
        format!(
            "[audio-stats] {label} quanta={} mean={:.3}ms p99={:.2}ms max={:.2}ms over_budget={} over10ms={} over20ms={} idle_hook_max={:.2}ms",
            self.n,
            if self.n > 0 { ms(self.sum) / self.n as f64 } else { 0. },
            self.percentile_ms(0.99),
            ms(self.max),
            self.over_budget,
            self.over_10ms,
            self.over_20ms,
            ms(idle_max),
        )
    }
}

thread_local! {
    static IDLE_WORK: RefCell<Duration> = const { RefCell::new(Duration::ZERO) };
}

/// Report time spent in an idle hook (e.g. a GC run while the sink was full).
pub fn note_idle_work(d: Duration) {
    IDLE_WORK.with(|w| {
        let mut w = w.borrow_mut();
        *w = (*w).max(d);
    });
}

impl RenderStats {
    pub(crate) fn new(sample_rate: f32, real_time: bool) -> Self {
        let enabled = real_time && std::env::var_os("PARTII_AUDIO_STATS").is_some_and(|v| v != "0");
        RenderStats {
            enabled,
            // Never panic here: a context may come up with a sample rate of 0 (or worse).
            budget: if enabled {
                Duration::try_from_secs_f64(128. / sample_rate as f64).unwrap_or(Duration::MAX)
            } else {
                Duration::ZERO
            },
            window_start: Instant::now(),
            window: Histogram::new(),
            total: Histogram::new(),
            idle_max: Duration::ZERO,
            total_idle_max: Duration::ZERO,
            underruns: None,
            window_underruns: (0, 0),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn record(&mut self, d: Duration, underruns: Option<(u64, u64)>) {
        if !self.enabled {
            return;
        }
        self.underruns = underruns;
        self.window.add(d, self.budget);
        self.total.add(d, self.budget);
        let idle = IDLE_WORK.with(|w| std::mem::take(&mut *w.borrow_mut()));
        self.idle_max = self.idle_max.max(idle);
        self.total_idle_max = self.total_idle_max.max(idle);
        let elapsed = self.window_start.elapsed();
        if elapsed >= Duration::from_secs(5) {
            let (events, frames) = self.underruns.unwrap_or_default();
            eprintln!(
                "{} underruns={} underrun_frames={}",
                self.window.line(&format!("{:.1}s", elapsed.as_secs_f64()), self.idle_max),
                events - self.window_underruns.0.min(events),
                frames - self.window_underruns.1.min(frames),
            );
            self.window_underruns = (events, frames);
            self.window = Histogram::new();
            self.idle_max = Duration::ZERO;
            self.window_start = Instant::now();
        }
    }

    pub(crate) fn finish(&self) {
        if self.enabled && self.total.n > 0 {
            let (events, frames) = self.underruns.unwrap_or_default();
            eprintln!(
                "{} underruns={events} underrun_frames={frames}",
                self.total.line("total", self.total_idle_max)
            );
        }
    }
}
