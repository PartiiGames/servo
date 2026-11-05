/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii (perf stream, measurement builds only): `PARTII_FRAME_PROF=1` logs, every 2 s, the wall time
//! the script thread spent per second in update-the-rendering and in animation frame callbacks.

use std::cell::RefCell;
use std::time::{Duration, Instant};

pub(crate) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PARTII_FRAME_PROF").is_some())
}

#[derive(Default)]
struct Acc {
    since: Option<Instant>,
    utr: Duration,
    utr_n: u32,
    raf: Duration,
    raf_n: u32,
    raf_max: Duration,
}
thread_local!(static ACC: RefCell<Acc> = RefCell::default());
thread_local!(static LAST_RAF: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) });

pub(crate) struct Scope(Option<(Instant, bool)>);

impl Scope {
    /// `raf`: animation frame callbacks; otherwise update-the-rendering.
    pub(crate) fn new(raf: bool) -> Scope {
        Scope(enabled().then(|| (Instant::now(), raf)))
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let Some((start, raf)) = self.0 else {
            return;
        };
        let now = Instant::now();
        let d = now - start;
        if trace() {
            let start_wall = wall_ms() - d.as_secs_f64() * 1000.0;
            trace_span(if raf { "raf" } else { "update-the-rendering" }, start_wall, d);
            if raf {
                // A long frame: the gap since the previous animation frame callbacks began.
                let gap = LAST_RAF.replace(Some(start)).map(|last| start - last);
                if let Some(gap) = gap.filter(|g| g.as_millis() >= 40) {
                    eprintln!(
                        "partii-trace frame gap_end={start_wall:.1} gap_ms={:.1}",
                        gap.as_secs_f64() * 1000.0
                    );
                }
            }
        }
        ACC.with_borrow_mut(|a| {
            let since = *a.since.get_or_insert(start);
            if raf {
                a.raf += d;
                a.raf_n += 1;
                a.raf_max = a.raf_max.max(d);
            } else {
                a.utr += d;
                a.utr_n += 1;
            }
            let w = (now - since).as_secs_f64();
            if !raf && w >= 2.0 {
                let ms = |d: Duration| d.as_secs_f64() * 1000.0;
                eprintln!(
                    "partii-script-prof: utr_ms_s={:.1} utr_n_s={:.1} raf_ms_s={:.1} raf_n_s={:.1} raf_ms={:.2} raf_max_ms={:.2}",
                    ms(a.utr) / w,
                    a.utr_n as f64 / w,
                    ms(a.raf) / w,
                    a.raf_n as f64 / w,
                    if a.raf_n > 0 { ms(a.raf) / a.raf_n as f64 } else { 0.0 },
                    ms(a.raf_max),
                );
                *a = Acc::default();
                a.since = Some(now);
            }
        });
    }
}

/// `PARTII_FRAME_PROF=trace`: log each script task or rendering update of 8 ms or more, with a wall clock
/// shared by every thread (ms since the Unix epoch, mod 10^7), to line long frames up across threads.
pub(crate) fn trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PARTII_FRAME_PROF").is_ok_and(|v| v == "trace"))
}

pub(crate) fn wall_ms() -> f64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_micros() % 10_000_000_000) as f64 / 1000.0
}

pub(crate) fn trace_span(what: &str, start_wall_ms: f64, d: Duration) {
    let ms = d.as_secs_f64() * 1000.0;
    if ms >= 8.0 {
        eprintln!("partii-trace script {what} at={start_wall_ms:.1} ms={ms:.1}");
    }
}
