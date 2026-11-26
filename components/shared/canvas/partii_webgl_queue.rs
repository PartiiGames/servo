/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii (perf, wglq): a bound on the WebGL command queue. The script thread sends WebGLCommands to the WebGL
//! thread over an unbounded channel and never waits for fire-and-forget calls, so a page that issues GL faster
//! than the WebGL thread runs it built a backlog of seconds (harness/perf/wglcalls: 12 s queued after a 2 s
//! uniform+draw loop on llvmpipe): latency for every later frame and memory for every queued command.
//!
//! One gauge per process (the in-process WebGL mode runs one WebGL thread): the script side adds one before
//! each WebGLCommand it sends, the WebGL thread takes one off after handling it. When `PARTII_WEBGL_QUEUE_MAX`
//! (default 4096; 0 = off) commands are pending, the sender waits *before* enqueueing until the queue is down
//! to half. Waiting before the send (not after) matters: BufferData / BufferSubData send their bytes on a second
//! channel right after the command, and the WebGL thread blocks on those bytes when it reaches the command.
//!
//! It never hangs: the wait is in short slices, re-checks each time, and gives up after `GIVE_UP` (logged as a
//! `partii-webgl-frame: queue give-up` line on stderr, which the farm reports count). The gauge is only armed by a
//! WebGL thread in this process: a multi-process content process (whose WebGL thread lives elsewhere) never
//! waits. When the WebGL thread exits (or unwinds) it closes the gauge and wakes every waiter.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long one wait slice is before the sender re-checks.
pub const SLICE: Duration = Duration::from_millis(50);
/// After this long the sender sends anyway (and logs it).
pub const GIVE_UP: Duration = Duration::from_secs(2);
/// The default bound when PARTII_WEBGL_QUEUE_MAX is unset.
pub const DEFAULT_MAX: usize = 4096;

/// Pending WebGLCommands between the script thread(s) and the WebGL thread.
pub struct QueueGauge {
    pending: AtomicUsize,
    max: AtomicUsize,
    armed: AtomicBool,
    closed: AtomicBool,
    waiters: AtomicUsize,
    giveups: AtomicU64,
    waits: AtomicU64,
    lock: Mutex<()>,
    cv: Condvar,
}

/// What a wait for space did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Wait {
    /// No wait was needed (off, not armed, closed, or below the bound).
    No,
    /// Waited until the queue was down to half.
    Waited,
    /// The WebGL thread went away (closed) while waiting.
    Closed,
    /// Gave up after GIVE_UP; the command is sent anyway.
    GaveUp,
}

impl Default for QueueGauge {
    fn default() -> Self {
        Self::new()
    }
}

impl QueueGauge {
    pub const fn new() -> Self {
        QueueGauge {
            pending: AtomicUsize::new(0),
            max: AtomicUsize::new(0),
            armed: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            waiters: AtomicUsize::new(0),
            giveups: AtomicU64::new(0),
            waits: AtomicU64::new(0),
            lock: Mutex::new(()),
            cv: Condvar::new(),
        }
    }

    /// The WebGL thread, when it starts: the bound is `max` (0 = off).
    pub fn arm(&self, max: usize) {
        self.pending.store(0, Ordering::SeqCst);
        self.max.store(max, Ordering::SeqCst);
        self.closed.store(false, Ordering::SeqCst);
        self.armed.store(max > 0, Ordering::SeqCst);
    }

    /// The WebGL thread, when it exits or unwinds: nobody waits from now on, and every waiter wakes.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.armed.store(false, Ordering::SeqCst);
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.cv.notify_all();
    }

    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }
    pub fn giveups(&self) -> u64 {
        self.giveups.load(Ordering::Relaxed)
    }
    pub fn waits(&self) -> u64 {
        self.waits.load(Ordering::Relaxed)
    }

    /// Script side, before sending a command: wait (bounded) while the queue is full. `give_up` is GIVE_UP
    /// outside tests.
    #[inline]
    pub fn before_send(&self, give_up: Duration) -> Wait {
        if !self.armed.load(Ordering::Relaxed) {
            return Wait::No;
        }
        let max = self.max.load(Ordering::Relaxed);
        if self.pending.load(Ordering::Relaxed) < max {
            return Wait::No;
        }
        self.wait_slow(max, give_up)
    }

    #[cold]
    fn wait_slow(&self, max: usize, give_up: Duration) -> Wait {
        let low = max / 2;
        let t0 = Instant::now();
        self.waiters.fetch_add(1, Ordering::SeqCst);
        let mut g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let r = loop {
            if self.closed.load(Ordering::SeqCst) || !self.armed.load(Ordering::SeqCst) {
                break Wait::Closed;
            }
            if self.pending.load(Ordering::SeqCst) <= low {
                break Wait::Waited;
            }
            let spent = t0.elapsed();
            if spent >= give_up {
                break Wait::GaveUp;
            }
            g = self
                .cv
                .wait_timeout(g, SLICE.min(give_up - spent))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        };
        drop(g);
        self.waiters.fetch_sub(1, Ordering::SeqCst);
        self.waits.fetch_add(1, Ordering::Relaxed);
        if r == Wait::GaveUp {
            let n = self.giveups.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n % 100 == 0 {
                eprintln!(
                    "partii-webgl-frame: queue give-up #{n}: {} commands pending (bound {max}) after {:?}; sent anyway",
                    self.pending(),
                    t0.elapsed()
                );
            }
        }
        r
    }

    /// Script side: one command is being sent (call before the channel send, so the WebGL thread can't take it
    /// off first).
    #[inline]
    pub fn add(&self) {
        if self.armed.load(Ordering::Relaxed) {
            self.pending.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Script side: the send failed (receiver gone); undo `add`.
    #[inline]
    pub fn unadd(&self) {
        let _ = self
            .pending
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |p| p.checked_sub(1));
    }

    /// WebGL thread: one command handled (or dropped).
    #[inline]
    pub fn done(&self) {
        if !self.armed.load(Ordering::Relaxed) {
            return;
        }
        let before = self
            .pending
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |p| p.checked_sub(1))
            .unwrap_or(0);
        if self.waiters.load(Ordering::SeqCst) > 0 &&
            before.saturating_sub(1) <= self.max.load(Ordering::Relaxed) / 2
        {
            let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            self.cv.notify_all();
        }
    }
}

/// The process's gauge.
pub static GAUGE: QueueGauge = QueueGauge::new();

/// PARTII_WEBGL_QUEUE_MAX, or DEFAULT_MAX.
pub fn configured_max() -> usize {
    std::env::var("PARTII_WEBGL_QUEUE_MAX")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_MAX)
}

/// The WebGL thread holds one: closes the gauge when the thread exits or unwinds.
pub struct CloseOnDrop<'a>(pub &'a QueueGauge);

impl Drop for CloseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::thread;

    use super::*;

    fn gauge(max: usize) -> Arc<QueueGauge> {
        let g = Arc::new(QueueGauge::new());
        g.arm(max);
        g
    }

    /// A consumer thread that takes `n` commands off, one per `each`.
    fn consume(g: &Arc<QueueGauge>, n: usize, each: Duration) -> thread::JoinHandle<()> {
        let g = g.clone();
        thread::spawn(move || {
            for _ in 0..n {
                thread::sleep(each);
                g.done();
            }
        })
    }

    #[test]
    fn off_and_unarmed_never_wait() {
        let g = QueueGauge::new();
        for _ in 0..10_000 {
            assert_eq!(g.before_send(GIVE_UP), Wait::No);
            g.add();
        }
        let g = gauge(0);
        for _ in 0..10_000 {
            assert_eq!(g.before_send(GIVE_UP), Wait::No);
            g.add();
        }
        assert_eq!(g.pending(), 0);
    }

    /// 1. The bound holds and the sender resumes once the consumer drains to half.
    #[test]
    fn waits_at_bound_and_resumes_at_half() {
        let g = gauge(64);
        for _ in 0..64 {
            assert_eq!(g.before_send(GIVE_UP), Wait::No);
            g.add();
        }
        let c = consume(&g, 40, Duration::from_micros(200));
        let t0 = Instant::now();
        assert_eq!(g.before_send(GIVE_UP), Wait::Waited);
        assert!(g.pending() <= 32, "resumed at {}", g.pending());
        assert!(t0.elapsed() < Duration::from_secs(1));
        c.join().unwrap();
    }

    /// 2. Sync readbacks: a command whose reply the sender then waits for, issued with the queue full, gets its
    /// reply (FIFO; the wait is before the enqueue, so no reply is outstanding while the sender waits).
    #[test]
    fn sync_reply_with_full_queue() {
        let g = gauge(32);
        let (tx, rx) = crossbeam_channel::unbounded::<Option<crossbeam_channel::Sender<u32>>>();
        let gc = g.clone();
        let consumer = thread::spawn(move || {
            while let Ok(m) = rx.recv() {
                thread::sleep(Duration::from_micros(100));
                if let Some(reply) = m {
                    let _ = reply.send(7);
                }
                gc.done();
            }
        });
        for _ in 0..32 {
            g.before_send(GIVE_UP);
            g.add();
            tx.send(None).unwrap();
        }
        for _ in 0..5 {
            let (rtx, rrx) = crossbeam_channel::bounded(1);
            assert_ne!(g.before_send(GIVE_UP), Wait::GaveUp);
            g.add();
            tx.send(Some(rtx)).unwrap();
            assert_eq!(rrx.recv_timeout(Duration::from_secs(5)).unwrap(), 7);
        }
        drop(tx);
        consumer.join().unwrap();
    }

    /// 2b. BufferData's order: command, then its bytes on a second channel; the consumer blocks on the bytes.
    /// Waiting before the enqueue means the bytes always follow without a wait in between.
    #[test]
    fn command_then_bytes_never_deadlocks() {
        let g = gauge(8);
        let (tx, rx) = crossbeam_channel::unbounded::<crossbeam_channel::Receiver<u8>>();
        let gc = g.clone();
        let consumer = thread::spawn(move || {
            while let Ok(bytes) = rx.recv() {
                bytes.recv_timeout(Duration::from_secs(5)).expect("bytes never came");
                gc.done();
            }
        });
        let t0 = Instant::now();
        for _ in 0..2000 {
            assert_ne!(g.before_send(GIVE_UP), Wait::GaveUp);
            let (btx, brx) = crossbeam_channel::bounded(1);
            g.add();
            tx.send(brx).unwrap();
            btx.send(1).unwrap();
        }
        drop(tx);
        consumer.join().unwrap();
        assert!(t0.elapsed() < Duration::from_secs(10));
        assert_eq!(g.giveups(), 0);
    }

    /// 3. Context lost (and restored): the WebGL thread keeps taking commands for a lost context off (it drops
    /// them), so a parked sender wakes as the queue drains; a restored context's commands count the same way.
    #[test]
    fn lost_context_commands_still_drain() {
        let g = gauge(16);
        for _ in 0..16 {
            g.add();
        }
        // the "lost" context's commands are dropped fast
        let c = consume(&g, 16, Duration::from_micros(10));
        assert_eq!(g.before_send(GIVE_UP), Wait::Waited);
        c.join().unwrap();
        // restored: new commands, same gauge
        for _ in 0..8 {
            assert_eq!(g.before_send(GIVE_UP), Wait::No);
            g.add();
        }
        assert_eq!(g.pending(), 8);
    }

    /// 4. The WebGL thread torn down while the script waits: close() wakes it at once.
    #[test]
    fn teardown_wakes_waiter() {
        let g = gauge(4);
        for _ in 0..4 {
            g.add();
        }
        let gc = g.clone();
        let closer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            let _guard = CloseOnDrop(&gc);
        });
        let t0 = Instant::now();
        assert_eq!(g.before_send(GIVE_UP), Wait::Closed);
        assert!(t0.elapsed() < Duration::from_millis(500), "woke after {:?}", t0.elapsed());
        closer.join().unwrap();
        // after close nobody waits
        assert_eq!(g.before_send(GIVE_UP), Wait::No);
    }

    /// 4b. A WebGL thread that panics closes the gauge too (CloseOnDrop on unwind).
    #[test]
    fn panic_closes() {
        let g = gauge(2);
        g.add();
        g.add();
        let gc = g.clone();
        let r = thread::spawn(move || {
            let _guard = CloseOnDrop(&gc);
            panic!("webgl thread died");
        })
        .join();
        assert!(r.is_err());
        assert_eq!(g.before_send(GIVE_UP), Wait::No);
    }

    /// 5. A stalled consumer: the sender gives up after the cap and is counted.
    #[test]
    fn stalled_consumer_gives_up() {
        let g = gauge(4);
        for _ in 0..4 {
            g.add();
        }
        let t0 = Instant::now();
        assert_eq!(g.before_send(Duration::from_millis(200)), Wait::GaveUp);
        let spent = t0.elapsed();
        assert!(spent >= Duration::from_millis(200) && spent < Duration::from_secs(2), "{spent:?}");
        assert_eq!(g.giveups(), 1);
    }

    /// Many senders (several script threads) and one consumer: no lost wake-up, bound respected.
    #[test]
    fn many_senders_one_consumer() {
        let g = gauge(128);
        let (tx, rx) = crossbeam_channel::unbounded::<()>();
        let gc = g.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let consumer = thread::spawn(move || {
            let mut n = 0;
            while rx.recv().is_ok() {
                gc.done();
                n += 1;
            }
            n
        });
        let mut hs = vec![];
        for _ in 0..4 {
            let (g, tx) = (g.clone(), tx.clone());
            hs.push(thread::spawn(move || {
                for _ in 0..20_000 {
                    assert_ne!(g.before_send(GIVE_UP), Wait::GaveUp);
                    g.add();
                    tx.send(()).unwrap();
                }
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        stop.store(true, Ordering::SeqCst);
        drop(tx);
        assert_eq!(consumer.join().unwrap(), 80_000);
        assert_eq!(g.pending(), 0);
        assert_eq!(g.giveups(), 0);
    }
}
