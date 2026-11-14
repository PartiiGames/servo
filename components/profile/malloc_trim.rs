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
//! since the last trim, at most every `PARTII_MALLOC_TRIM_SECS` (30) seconds, checking RSS every
//! 2 s. Only the arena being walked is locked, one at a time, but a thread allocating from it waits:
//! every 10 s (the first default) doubled rocket's frame gaps over 50 ms (3 interleaved 15-min reps on b92:
//! 49/87/88 vs 45/30/37 with trim off) and left 1-3 gaps over 100 ms in every rep; every 30 s is level
//! with off on gaps (42/37/36, p99 33-34 ms) and still holds RSS ~200 MB under it.
//!
//! RSS growth alone latches: a trim taken while the live heap is at a peak gives back little and
//! sets the baseline at that peak's RSS. When the peak's memory is freed a moment later it stays
//! resident in the arenas, RSS sits just under baseline + 48 MB, and no trim comes again. In the
//! rocket lx soak of 2026-11-14 (job 5a1f8be2) RSS stepped 780 -> 980 MB at minute 7 (live heap
//! 653 MB at that sample, 330-540 after) and stayed at 980-1010 MB to the end with no sawtooth:
//! play-median growth +25.1%. So the thread also watches the bytes in use inside the arenas
//! (mallinfo2 `uordblks`, every `PARTII_MALLOC_TRIM_PROBE_SECS` (6) s) and trims when
//! `PARTII_MALLOC_TRIM_FREED_MB` (48) of them have been freed since the highest point after the
//! last trim, at the same 30-s pace. `PARTII_MALLOC_TRIM_FREED_MB=0` is the RSS trigger alone.
//!
//! `PARTII_MALLOC_TRIM=0` turns it off; `PARTII_MALLOC_TRIM_LOG=1` prints each trim (RSS before
//! and after, how long it took, which trigger) to stderr.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[expect(unsafe_code)]
mod imp {
    use std::thread;
    use std::time::{Duration, Instant};

    unsafe extern "C" {
        fn malloc_trim(pad: libc::size_t) -> libc::c_int;
        fn mallinfo2() -> Mallinfo2;
    }

    /// glibc's `struct mallinfo2` (2.33+), as in system_reporter.rs.
    #[repr(C)]
    struct Mallinfo2 {
        _arena: usize,
        _ordblks: usize,
        _smblks: usize,
        _hblks: usize,
        _hblkhd: usize,
        _usmblks: usize,
        _fsmblks: usize,
        uordblks: usize,
        _fordblks: usize,
        _keepcost: usize,
    }

    /// Bytes in use inside the malloc arenas (not the mmapped big blocks: those go back to the
    /// OS when freed). Locks each arena in turn while it counts its free lists.
    fn arena_in_use() -> u64 {
        unsafe { mallinfo2() }.uordblks as u64
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
        let min_interval = Duration::from_secs(env_u64("PARTII_MALLOC_TRIM_SECS", 30));
        let log = std::env::var("PARTII_MALLOC_TRIM_LOG").is_ok_and(|v| v.trim() == "1");
        let freed_trigger = env_u64("PARTII_MALLOC_TRIM_FREED_MB", 48) << 20;
        let probe_every = env_u64("PARTII_MALLOC_TRIM_PROBE_SECS", 6).max(2) / 2;
        let _ = thread::Builder::new()
            .name("MallocTrim".to_owned())
            .spawn(move || {
                let Some(mut baseline) = rss_bytes() else {
                    return;
                };
                let mut last = Instant::now();
                // The most bytes in use in the arenas seen since the last trim (0 = not probed).
                let mut in_use_peak = 0u64;
                let mut tick = 0u64;
                loop {
                    thread::sleep(Duration::from_secs(2));
                    tick += 1;
                    let Some(rss) = rss_bytes() else {
                        return;
                    };
                    // RSS fell (a trim elsewhere, a big munmap): follow it down so the next
                    // growth is measured from there.
                    if rss < baseline {
                        baseline = rss;
                    }
                    let mut freed = 0;
                    if freed_trigger > 0 && tick % probe_every == 0 {
                        let in_use = arena_in_use();
                        in_use_peak = in_use_peak.max(in_use);
                        freed = in_use_peak - in_use;
                    }
                    let grew = rss - baseline >= grow;
                    let by_freed = freed_trigger > 0 && freed >= freed_trigger;
                    if !(grew || by_freed) || last.elapsed() < min_interval {
                        continue;
                    }
                    let t = Instant::now();
                    unsafe { malloc_trim(0) };
                    let took = t.elapsed();
                    last = Instant::now();
                    baseline = rss_bytes().unwrap_or(rss);
                    in_use_peak = if freed_trigger > 0 { arena_in_use() } else { 0 };
                    if log {
                        eprintln!(
                            "malloc-trim: rss {} -> {} MB in {:.1} ms ({}, {} MB freed in the arenas)",
                            rss >> 20,
                            baseline >> 20,
                            took.as_secs_f64() * 1000.0,
                            if grew { "rss grew" } else { "freed" },
                            freed >> 20
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
