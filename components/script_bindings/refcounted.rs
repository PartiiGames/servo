/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A generic, safe mechanism by which DOM objects can be pinned and transferred
//! between threads (or intra-thread for asynchronous events). Akin to Gecko's
//! nsMainThreadPtrHandle, this uses thread-safe reference counting and ensures
//! that the actual SpiderMonkey GC integration occurs on the script thread via
//! weak refcounts. Ownership of a `Trusted<T>` object means the DOM object of
//! type T to which it points remains alive. Any other behaviour is undefined.
//! To guarantee the lifetime of a DOM object when performing asynchronous operations,
//! obtain a `Trusted<T>` from that object and pass it along with each operation.
//! A usable pointer to the original DOM object can be obtained on the script thread
//! from a `Trusted<T>` via the `root` method.
//!
//! The implementation of `Trusted<T>` is as follows:
//! The `Trusted<T>` object contains an atomic reference counted pointer to the Rust DOM object.
//! A hashtable resides in the script thread, keyed on the pointer.
//! The values in this hashtable are weak reference counts. When a `Trusted<T>` object is
//! created or cloned, the reference count is increased. When a `Trusted<T>` is dropped, the count
//! decreases. If the count hits zero, the weak reference is emptied, and is removed from
//! its hash table during the next GC. During GC, the entries of the hash table are counted
//! as JS roots.

use std::cell::RefCell;
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::{Arc, Weak};

use js::jsapi::JSTracer;
use rustc_hash::FxHashMap;

thread_local!(pub(super) static LIVE_DOM_REFERENCES: LiveDOMReferences =
    LiveDOMReferences {
        reflectable_table: RefCell::new(FxHashMap::default()),
    }
);

use crate::root::DomRoot;
use crate::trace::trace_reflector;
use crate::{DomObject, Reflector};

/// A pointer to a Rust DOM object that needs to be destroyed.
#[derive(MallocSizeOf)]
struct TrustedReference(
    #[ignore_malloc_size_of = "This is a shared reference."] *const libc::c_void,
);
unsafe impl Send for TrustedReference {}

impl TrustedReference {
    /// Creates a new TrustedReference from a pointer to a value that impements DOMObject.
    /// This is not enforced by the type system to reduce duplicated generic code,
    /// which is acceptable since this method is internal to this module.
    unsafe fn new(ptr: *const libc::c_void) -> TrustedReference {
        TrustedReference(ptr)
    }
}

/// A safe wrapper around a raw pointer to a DOM object that can be
/// shared among threads for use in asynchronous operations. The underlying
/// DOM object is guaranteed to live at least as long as the last outstanding
/// `Trusted<T>` instance.
#[cfg_attr(crown, crown::unrooted_must_root_lint::allow_unrooted_interior)]
#[derive(MallocSizeOf)]
pub struct Trusted<T: DomObject> {
    /// A pointer to the Rust DOM object of type T, but void to allow
    /// sending `Trusted<T>` between threads, regardless of T's sendability.
    #[conditional_malloc_size_of]
    refcount: Arc<TrustedReference>,
    #[ignore_malloc_size_of = "These are shared by all `Trusted` types."]
    owner_thread: *const LiveDOMReferences,
    phantom: PhantomData<T>,
}

impl<T: DomObject> std::fmt::Debug for Trusted<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.write_str("...")
    }
}

unsafe impl<T: DomObject> Send for Trusted<T> {}

impl<T: DomObject> Trusted<T> {
    /// Create a new `Trusted<T>` instance from an existing DOM pointer. The DOM object will
    /// be prevented from being GCed for the duration of the resulting `Trusted<T>` object's
    /// lifetime.
    #[track_caller]
    pub fn new(ptr: &T) -> Trusted<T> {
        partii_trusted_census::note_site(ptr as *const T as *const _);
        fn add_live_reference(
            ptr: *const libc::c_void,
        ) -> (Arc<TrustedReference>, *const LiveDOMReferences) {
            LIVE_DOM_REFERENCES.with(|live_references| {
                let refcount = unsafe { live_references.addref(ptr) };
                (refcount, live_references as *const _)
            })
        }

        let (refcount, owner_thread) = add_live_reference(ptr as *const T as *const _);
        Trusted {
            refcount,
            owner_thread,
            phantom: PhantomData,
        }
    }

    /// Obtain a usable DOM pointer from a pinned `Trusted<T>` value. Fails if used on
    /// a different thread than the original value from which this `Trusted<T>` was
    /// obtained.
    pub fn root(&self) -> DomRoot<T> {
        fn validate(owner_thread: *const LiveDOMReferences) {
            assert!(
                LIVE_DOM_REFERENCES.with(|live_references| { owner_thread == live_references })
            );
        }
        validate(self.owner_thread);
        unsafe { DomRoot::from_ref(&*(self.refcount.0 as *const T)) }
    }
}

impl<T: DomObject> Clone for Trusted<T> {
    fn clone(&self) -> Trusted<T> {
        Trusted {
            refcount: self.refcount.clone(),
            owner_thread: self.owner_thread,
            phantom: PhantomData,
        }
    }
}

/// The set of live, pinned DOM objects that are currently prevented
/// from being garbage collected due to outstanding references.
pub struct LiveDOMReferences {
    // keyed on pointer to Rust DOM object
    reflectable_table: RefCell<FxHashMap<*const libc::c_void, Weak<TrustedReference>>>,
}

/// Trace all the references that are in Trusted and liive.
/// # Safety
/// tracer must point to a valid, non-null JS tracer.
pub unsafe fn trace_live_domreferences(tracer: *mut JSTracer) {
    LIVE_DOM_REFERENCES.with(|live_references| {
        let mut table = live_references.reflectable_table.borrow_mut();
        remove_nulls(&mut table);
        let census = partii_trusted_census::due();
        let mut by_class: FxHashMap<&'static str, usize> = FxHashMap::default();
        for obj in table.keys() {
            unsafe {
                let reflector = &*(*obj as *const Reflector);
                if census {
                    *by_class
                        .entry(partii_trusted_census::class_name(reflector))
                        .or_default() += 1;
                    partii_trusted_census::count_sites(*obj, reflector);
                }
                trace_reflector(tracer, "refcounted", reflector);
            }
        }
        if census {
            partii_trusted_census::print(table.len(), by_class);
            partii_trusted_census::print_sites(&table);
        }
    })
}

/// Partii (bd stream), diagnosis: `PARTII_TRUSTED_CENSUS=N` prints, at every Nth GC on each thread,
/// how many DOM objects `Trusted` handles keep alive, by class. A `Trusted` roots its object (and
/// with it, for a node, its document and window) until the handle is dropped, so a count that only
/// grows names what keeps discarded pages alive.
mod partii_trusted_census {
    use std::cell::{Cell, RefCell};
    use std::panic::Location;
    use std::sync::OnceLock;
    use std::sync::Weak;

    use rustc_hash::FxHashMap;

    use super::TrustedReference;

    use crate::reflector::Reflector;

    fn every() -> Option<u64> {
        static N: OnceLock<Option<u64>> = OnceLock::new();
        *N.get_or_init(|| {
            std::env::var("PARTII_TRUSTED_CENSUS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .filter(|n| *n > 0)
        })
    }

    thread_local!(static GCS: Cell<u64> = const { Cell::new(0) });

    type Sites = Vec<&'static Location<'static>>;

    thread_local! {
        /// Where `Trusted::new` was called for each object (at most four sites an object).
        static SITES: RefCell<FxHashMap<*const libc::c_void, Sites>> = RefCell::new(FxHashMap::default());
        /// Live objects counted by class and creation sites, filled during a census.
        static BY_SITE: RefCell<FxHashMap<String, usize>> = RefCell::new(FxHashMap::default());
    }

    #[track_caller]
    pub(super) fn note_site(ptr: *const libc::c_void) {
        if every().is_none() {
            return;
        }
        let here = Location::caller();
        SITES.with(|s| {
            let mut s = s.borrow_mut();
            let v = s.entry(ptr).or_default();
            if v.len() < 4 && !v.iter().any(|l| l == &here) {
                v.push(here);
            }
        });
    }

    pub(super) unsafe fn count_sites(ptr: *const libc::c_void, reflector: &Reflector) {
        let class = unsafe { class_name(reflector) };
        let key = SITES.with(|s| {
            let s = s.borrow();
            let sites = s.get(&ptr).map(|v| {
                v.iter()
                    .map(|l| {
                        let f = l.file();
                        let f = f.rsplit('/').take(2).collect::<Vec<_>>();
                        format!("{}/{}:{}", f.get(1).unwrap_or(&""), f[0], l.line())
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            });
            format!("{class}@{}", sites.unwrap_or_else(|| "?".into()))
        });
        BY_SITE.with(|b| *b.borrow_mut().entry(key).or_default() += 1);
    }

    pub(super) fn print_sites(table: &FxHashMap<*const libc::c_void, Weak<TrustedReference>>) {
        // Forget the sites of objects no longer held.
        SITES.with(|s| s.borrow_mut().retain(|k, _| table.contains_key(k)));
        let mut v: Vec<(String, usize)> = BY_SITE.with(|b| b.borrow_mut().drain().collect());
        v.sort_by(|a, b| b.1.cmp(&a.1));
        for (k, n) in v.iter().take(12) {
            eprintln!(
                "partii-trusted-site: thread={} gc={} n={} {}",
                std::thread::current().name().unwrap_or("?"),
                GCS.with(|c| c.get()),
                n,
                k
            );
        }
    }

    pub(super) fn due() -> bool {
        let Some(n) = every() else { return false };
        GCS.with(|c| {
            c.set(c.get() + 1);
            c.get() % n == 1 || n == 1
        })
    }

    pub(super) unsafe fn class_name(reflector: &Reflector) -> &'static str {
        let obj = reflector.get_jsobject_unbarriered();
        if obj.is_null() {
            return "(null)";
        }
        let class = unsafe { js::rust::get_object_class(obj) };
        if class.is_null() || unsafe { (*class).name }.is_null() {
            return "(?)";
        }
        unsafe { std::ffi::CStr::from_ptr((*class).name) }
            .to_str()
            .unwrap_or("(?)")
    }

    pub(super) fn print(total: usize, by_class: FxHashMap<&'static str, usize>) {
        let mut v: Vec<_> = by_class.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        let top: Vec<String> = v.iter().take(12).map(|(c, n)| format!("{c}={n}")).collect();
        eprintln!(
            "partii-trusted-census: thread={} gc={} trusted={} {}",
            std::thread::current().name().unwrap_or("?"),
            GCS.with(|c| c.get()),
            total,
            top.join(" ")
        );
    }
}

impl LiveDOMReferences {
    pub fn destruct() {
        LIVE_DOM_REFERENCES.with(|live_references| {
            let _ = live_references.reflectable_table.take();
        });
    }

    /// ptr must be a pointer to a type that implements DOMObject.
    /// This is not enforced by the type system to reduce duplicated generic code,
    /// which is acceptable since this method is internal to this module.
    #[expect(clippy::arc_with_non_send_sync)]
    unsafe fn addref(&self, ptr: *const libc::c_void) -> Arc<TrustedReference> {
        let mut table = self.reflectable_table.borrow_mut();
        let capacity = table.capacity();
        let len = table.len();
        if (0 < capacity) && (capacity <= len) {
            trace!("growing refcounted references by {}", len);
            remove_nulls(&mut table);
            table.reserve(len);
        }
        match table.entry(ptr) {
            Occupied(mut entry) => match entry.get().upgrade() {
                Some(refcount) => refcount,
                None => {
                    let refcount = Arc::new(unsafe { TrustedReference::new(ptr) });
                    entry.insert(Arc::downgrade(&refcount));
                    refcount
                },
            },
            Vacant(entry) => {
                let refcount = Arc::new(unsafe { TrustedReference::new(ptr) });
                entry.insert(Arc::downgrade(&refcount));
                refcount
            },
        }
    }
}

/// Remove null entries from the live references table
fn remove_nulls<K: Eq + Hash + Clone, V>(table: &mut FxHashMap<K, Weak<V>>) {
    let to_remove: Vec<K> = table
        .iter()
        .filter(|&(_, value)| Weak::upgrade(value).is_none())
        .map(|(key, _)| key.clone())
        .collect();
    trace!("removing {} refcounted references", to_remove.len());
    for key in to_remove {
        table.remove(&key);
    }
}

unsafe impl<T: DomObject> crate::JSTraceable for Trusted<T> {
    #[inline]
    unsafe fn trace(&self, _: *mut JSTracer) {
        // Do nothing
    }
}
