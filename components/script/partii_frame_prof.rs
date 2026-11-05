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
