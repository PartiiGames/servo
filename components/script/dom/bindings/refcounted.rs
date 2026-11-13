/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A generic, safe mechanism by which Promise objects can be pinned and transferred
//! between threads (or intra-thread for asynchronous events). See more information in
//! script_bindings::refcounted

use std::cell::RefCell;
use std::thread::{self, ThreadId};

use js::context::JSContext;
use js::conversions::ToJSValConvertible;
use js::jsapi::JSTracer;
use js::rust::Trace;
use rustc_hash::FxHashMap;
use script_bindings::error::Error;
pub(crate) use script_bindings::refcounted::Trusted;
use script_bindings::reflector::DomObject;

use crate::dom::promise::{Promise, RootedPromise, TracedPromise};
use crate::tasks::task::TaskOnce;

thread_local!(pub(super) static LIVE_PROMISE_REFERENCES: LivePromiseReferences =
    LivePromiseReferences {
        promise_table: RefCell::new(FxHashMap::default()),
    }
);

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct PromiseKey(*const Promise);

/// # Safety
/// PromiseKey is only ever used for comparisons between keys.
/// Its value is never read.
unsafe impl Send for PromiseKey {}

/// The set of live, pinned DOM objects that are currently prevented
/// from being garbage collected due to outstanding references.
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct LivePromiseReferences {
    // keyed on pointer to Rust DOM object
    promise_table: RefCell<FxHashMap<PromiseKey, TracedPromise>>,
}

impl LivePromiseReferences {
    pub(crate) fn destruct() {
        LIVE_PROMISE_REFERENCES.with(|live_references| {
            let _ = live_references.promise_table.take();
        });
    }

    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    fn store_promise(&self, promise: &RootedPromise) -> PromiseKey {
        // Since converting a RootedPromise to a TracedPromise allocates a new
        // underlying Rc<Promise>, we are guaranteed that there is no prior entry
        // in the hashtable for this particular promise object.
        let traced_promise = promise.to_traced();
        let key = PromiseKey(&raw const *traced_promise);
        let _exists = self
            .promise_table
            .borrow_mut()
            .insert(key, traced_promise)
            .is_some();
        debug_assert!(!_exists);
        key
    }
}

/// A safe wrapper around a DOM Promise object that can be shared among threads for use
/// in asynchronous operations. The underlying DOM object is guaranteed to live at least
/// as long as the last outstanding `TrustedPromise` instance. These values cannot be cloned,
/// only created from existing `Rc<Promise>` values.
pub struct TrustedPromise {
    dom_object: PromiseKey,
    owner_thread: ThreadId,
}

impl TrustedPromise {
    /// Create a new `TrustedPromise` instance from an existing DOM object. The object will
    /// be prevented from being GCed for the duration of the resulting `TrustedPromise` object's
    /// lifetime.
    #[track_caller]
    pub(crate) fn new(promise: &RootedPromise) -> TrustedPromise {
        partii_dropped_promises::release_pending();
        let site = std::panic::Location::caller();
        LIVE_PROMISE_REFERENCES.with(|live_references| {
            let ptr = live_references.store_promise(promise);
            partii_dropped_promises::note_site(ptr.0 as usize, site);
            TrustedPromise {
                dom_object: ptr,
                owner_thread: thread::current().id(),
            }
        })
    }

    /// Obtain a usable DOM Promise from a pinned `TrustedPromise` value. Fails if used on
    /// a different thread than the original value from which this `TrustedPromise` was
    /// obtained.
    pub(crate) fn root(self, cx: &JSContext) -> RootedPromise {
        partii_dropped_promises::release_pending();
        partii_dropped_promises::forget_site(self.dom_object.0 as usize);
        LIVE_PROMISE_REFERENCES.with(|live_references| {
            debug_assert_eq!(self.owner_thread, thread::current().id());
            live_references
                .promise_table
                .borrow_mut()
                .remove(&self.dom_object)
                .expect("Must always have an entry matching a TrustedPromise")
                .root(cx)
        })
    }

    /// A task which will reject the promise.
    pub(crate) fn reject_task(self, error: Error) -> impl TaskOnce {
        let this = self;
        task!(reject_promise: move |cx| {
            debug!("Rejecting promise.");
            this.root(cx).reject_error(cx, error);
        })
    }

    /// A task which will resolve the promise.
    pub(crate) fn resolve_task<T>(self, value: T) -> impl TaskOnce
    where
        T: ToJSValConvertible + Send,
    {
        let this = self;
        task!(resolve_promise: move |cx| {
            debug!("Resolving promise.");
            this.root(cx).resolve_native(cx, &value);
        })
    }
}

/// Partii: a `TrustedPromise` dropped without `root()` (its task was cancelled, its page exited, a
/// callback it was sent into went away) used to leave its entry in the table for good, rooting
/// the promise and, through it, its whole page. Dropping one now removes the entry: at once on the
/// owner thread, else at that thread's next `TrustedPromise::new`/`root`.
impl Drop for TrustedPromise {
    fn drop(&mut self) {
        let key = self.dom_object;
        if thread::current().id() == self.owner_thread {
            partii_dropped_promises::remove(key);
        } else {
            partii_dropped_promises::defer(self.owner_thread, key.0 as usize);
        }
    }
}

mod partii_dropped_promises {
    use std::cell::RefCell;
    use std::panic::Location;
    use std::sync::Mutex;
    use std::thread::{self, ThreadId};

    use rustc_hash::FxHashMap;

    use super::{LIVE_PROMISE_REFERENCES, PromiseKey};

    static DEFERRED: Mutex<Vec<(ThreadId, usize)>> = Mutex::new(Vec::new());

    thread_local! {
        /// Census (`PARTII_TRUSTED_CENSUS`): where each live entry was made.
        static SITES: RefCell<FxHashMap<usize, &'static Location<'static>>> = RefCell::new(FxHashMap::default());
    }

    fn census() -> bool {
        std::env::var_os("PARTII_TRUSTED_CENSUS").is_some()
    }

    pub(super) fn note_site(key: usize, site: &'static Location<'static>) {
        if census() {
            let _ = SITES.try_with(|s| s.borrow_mut().insert(key, site));
        }
    }

    pub(super) fn site_of(key: usize) -> String {
        SITES
            .try_with(|s| {
                s.borrow().get(&key).map(|l| {
                    let f: Vec<_> = l.file().rsplit('/').take(2).collect();
                    format!("{}/{}:{}", f.get(1).unwrap_or(&""), f[0], l.line())
                })
            })
            .ok()
            .flatten()
            .unwrap_or_else(|| "?".into())
    }

    pub(super) fn forget_site(key: usize) {
        if census() {
            let _ = SITES.try_with(|s| s.borrow_mut().remove(&key));
        }
    }

    pub(super) fn remove(key: PromiseKey) {
        forget_site(key.0 as usize);
        // Not while the table is being traced or changed; the entry stays (as before) then.
        let removed = LIVE_PROMISE_REFERENCES
            .try_with(|live| {
                live.promise_table
                    .try_borrow_mut()
                    .ok()
                    .and_then(|mut t| t.remove(&key))
            })
            .ok()
            .flatten();
        drop(removed);
    }

    pub(super) fn defer(owner: ThreadId, key: usize) {
        if let Ok(mut d) = DEFERRED.lock() {
            d.push((owner, key));
        }
    }

    pub(super) fn release_pending() {
        let me = thread::current().id();
        let mine: Vec<usize> = match DEFERRED.lock() {
            Ok(mut d) if !d.is_empty() => {
                let mut mine = Vec::new();
                d.retain(|(t, k)| {
                    if *t == me {
                        mine.push(*k);
                        false
                    } else {
                        true
                    }
                });
                mine
            },
            _ => return,
        };
        for k in mine {
            remove(PromiseKey(k as *const _));
        }
    }

    /// Census line: live table entries by where they were made.
    pub(super) fn print(total: usize) {
        let mut by: FxHashMap<String, usize> = FxHashMap::default();
        SITES.with(|s| {
            for l in s.borrow().values() {
                let f: Vec<_> = l.file().rsplit('/').take(2).collect();
                *by.entry(format!("{}/{}:{}", f.get(1).unwrap_or(&""), f[0], l.line()))
                    .or_default() += 1;
            }
        });
        let mut v: Vec<_> = by.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        let top: Vec<String> = v.iter().take(12).map(|(k, n)| format!("{k}={n}")).collect();
        eprintln!("partii-trusted-promises: live={total} {}", top.join(" "));
    }
}

fn partii_promise_census(total: usize) {
    use std::cell::Cell;
    thread_local!(static N: Cell<u64> = const { Cell::new(0) });
    let Some(every) = std::env::var("PARTII_TRUSTED_CENSUS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
    else {
        return;
    };
    let n = N.with(|c| {
        c.set(c.get() + 1);
        c.get()
    });
    if n % every == 1 || every == 1 {
        partii_dropped_promises::print(total);
    }
}

/// A JSTraceDataOp for tracing reflectors held in LIVE_REFERENCES
pub(crate) unsafe fn trace_refcounted_objects(tracer: *mut JSTracer) {
    trace!("tracing live refcounted promise references");
    LIVE_PROMISE_REFERENCES.with(|live_references| {
        let table = live_references.promise_table.borrow_mut();
        partii_promise_census(table.len());
        for promise in table.values() {
            unsafe {
                promise.reflector().trace(tracer);
            }
        }
    });
    trace!("tracing live refcounted references");
    unsafe {
        script_bindings::refcounted::trace_live_domreferences(tracer);
    }
}

/// Partii (mem-2, measurement): every live `TrustedPromise`'s promise object and, under
/// PARTII_TRUSTED_CENSUS, where it was made.
#[expect(unsafe_code)]
pub(crate) fn partii_live_promises() -> Vec<(*mut js::jsapi::JSObject, String)> {
    LIVE_PROMISE_REFERENCES.with(|live| {
        let Ok(t) = live.promise_table.try_borrow() else {
            return vec![];
        };
        t.keys()
            .map(|k| {
                let p = unsafe { &*k.0 };
                let site = partii_dropped_promises::site_of(k.0 as usize);
                (p.reflector().get_jsobject().get(), site)
            })
            .collect()
    })
}
