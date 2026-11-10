/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii (stream mem-1): give freed malloc memory back to the OS.
//!
//! On Linux the runtime uses glibc malloc. glibc keeps freed memory in its arenas (one per busy
//! thread, dozens in Servo) and returns only what sits at the very top of an arena, so the
//! process RSS follows the high-water mark of every arena, not what is live. A game that churns
//! big buffers (Rocket to Heaven makes a new Phaser game and WebGL context on every death: GL
//! textures, image decodes, display lists) ratchets RSS up a little with every new peak while
//! the live heap stays flat: in the lx soak of 2026-11-10 (job 4c253af5) the live heap
//! (system-heap-allocated) stayed at ~480 MB from minute 4 to 19 while RSS went 1099 -> 1279 MB,
//! and ~500 MB of the RSS was free memory held by the arenas.
//!
//! `malloc_trim(0)` walks every arena and returns the whole pages inside free chunks
//! (MADV_DONTNEED), so a thread here calls it when RSS has grown by `PARTII_MALLOC_TRIM_MB` (48)
//! since the last trim, at most every `PARTII_MALLOC_TRIM_SECS` (10) seconds, checking RSS every
//! 2 s. Only the arena being walked is locked, one at a time.
//!
//! `PARTII_MALLOC_TRIM=0` turns it off; `PARTII_MALLOC_TRIM_LOG=1` prints each trim (RSS before
//! and after, how long it took) to stderr.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[expect(unsafe_code)]
mod imp {
    use std::thread;
    use std::time::{Duration, Instant};

    unsafe extern "C" {
        fn malloc_trim(pad: libc::size_t) -> libc::c_int;
    }

    fn env_u64(name: &str, default: u64) -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(default)
    }

    /// Resident set size of this process in bytes, from /proc/self/statm.
    fn rss_bytes() -> Option<u64> {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        Some(pages * if page > 0 { page as u64 } else { 4096 })
    }

    pub fn start() {
        if std::env::var("PARTII_MALLOC_TRIM").is_ok_and(|v| v.trim() == "0") {
            return;
        }
        let grow = env_u64("PARTII_MALLOC_TRIM_MB", 48) << 20;
        let min_interval = Duration::from_secs(env_u64("PARTII_MALLOC_TRIM_SECS", 10));
        let log = std::env::var("PARTII_MALLOC_TRIM_LOG").is_ok_and(|v| v.trim() == "1");
        let _ = thread::Builder::new()
            .name("MallocTrim".to_owned())
            .spawn(move || {
                let Some(mut baseline) = rss_bytes() else {
                    return;
                };
                let mut last = Instant::now();
                loop {
                    thread::sleep(Duration::from_secs(2));
                    let Some(rss) = rss_bytes() else {
                        return;
                    };
                    // RSS fell (a trim elsewhere, a big munmap): follow it down so the next
                    // growth is measured from there.
                    if rss < baseline {
                        baseline = rss;
                        continue;
                    }
                    if rss - baseline < grow || last.elapsed() < min_interval {
                        continue;
                    }
                    let t = Instant::now();
                    unsafe { malloc_trim(0) };
                    let took = t.elapsed();
                    last = Instant::now();
                    baseline = rss_bytes().unwrap_or(rss);
                    if log {
                        eprintln!(
                            "malloc-trim: rss {} -> {} MB in {:.1} ms",
                            rss >> 20,
                            baseline >> 20,
                            took.as_secs_f64() * 1000.0
                        );
                    }
                }
            });
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
mod imp {
    pub fn start() {}
}

/// Starts the trim thread (Linux with glibc only; once per process).
pub fn start() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(imp::start);
}
