/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The DOM side of incremental GC marking (`PARTII_GC_INCREMENTAL=1`; the driver is the script
//! crate's `runtime/gc_scheduler.rs`).
//!
//! SpiderMonkey's incremental marking is snapshot-at-the-beginning: everything reachable when the
//! GC starts gets marked, provided every edge the mutator removes while marking is in progress first
//! shades its target. JS edges and mozjs `Heap<T>` do that already. Servo's Rust edges (`Dom<T>`
//! inside `MutNullableDom`, `DomRefCell<Vec<..>>`, ...) don't, and a `Dom<T>` can also be moved
//! (`mem::take`, `swap`) from an unscanned DOM object into one already scanned. Two measures cover
//! them while the mutator runs between marking slices:
//! - **Expose barrier:** whenever Rust hands a DOM object's JS reflector out (`get_jsobject`,
//!   `rootable`) during marking, that object is shaded. So an object JS gets hold of through a Rust
//!   edge is marked even if the edge is removed afterwards.
//! - **Remark:** every live reflector is in a per-thread registry. When marking ends (from
//!   SpiderMonkey's gray-roots hook, inside the GC and before sweeping starts) the driver re-traces
//!   the Rust roots, and the Rust fields of every reflector that is already marked. That way an edge
//!   moved into an already-scanned object (or onto the Rust stack) is still found. Unmarked
//!   reflectors are skipped: tracing them would mark themselves through their own `reflector`
//!   field.
//!
//! By default sweeping isn't incremental (the driver finishes the GC as soon as sweeping starts).
//! With `PARTII_GC_INCREMENTAL_SWEEP=1` it is, and script can run while some zones are being swept.
//! A dead DOM object then still exists until its zone's finalizers run, and the only way Rust can
//! reach one is a `WeakRef`: `about_to_be_finalized` lets `WeakRef` treat it as already gone.

#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};

use js::jsapi::{Heap, JSObject, JSTracer};

/// Registry slot value of a reflector that isn't registered.
pub const UNREGISTERED: usize = usize::MAX;

thread_local! {
    /// Whether reflectors created on this thread are registered (set before the first is created).
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    /// Whether an incremental GC is marking and the mutator is running (between slices).
    static MARKING: Cell<bool> = const { Cell::new(false) };
    /// Whether an incremental GC is sweeping and the mutator is running (between slices).
    static SWEEPING: Cell<bool> = const { Cell::new(false) };
    /// Every registered reflector's object slot, and its registry index slot (for swap-remove).
    static REGISTRY: RefCell<Vec<(*const Heap<*mut JSObject>, *const Cell<usize>)>> =
        const { RefCell::new(Vec::new()) };
}

/// Registers this thread's reflectors from now on. Called when the thread's runtime is created,
/// before any DOM object exists on it.
pub fn enable() {
    ENABLED.with(|e| e.set(true));
}

/// Whether this thread registers its reflectors.
pub fn is_enabled() -> bool {
    ENABLED.with(Cell::get)
}

/// Turns the expose barrier on (between marking slices) or off (in a slice, and outside a GC).
pub fn set_marking(marking: bool) {
    MARKING.with(|m| m.set(marking));
}

/// Set while script runs between the slices of an incremental GC that is sweeping.
pub fn set_sweeping(sweeping: bool) {
    SWEEPING.with(|s| s.set(sweeping));
}

#[cfg(not(windows))]
unsafe extern "C" {
    /// `js::gc::IsAboutToBeFinalizedInternal<JSObject>`: unmarked in a zone that is being swept.
    #[cfg_attr(
        target_vendor = "apple",
        link_name = "\u{1}__ZN2js2gc28IsAboutToBeFinalizedInternalI8JSObjectEEbPT_"
    )]
    #[cfg_attr(
        not(target_vendor = "apple"),
        link_name = "\u{1}_ZN2js2gc28IsAboutToBeFinalizedInternalI8JSObjectEEbPT_"
    )]
    fn IsAboutToBeFinalized(obj: *mut JSObject) -> bool;
}

#[cfg(windows)]
unsafe fn IsAboutToBeFinalized(_: *mut JSObject) -> bool {
    false
}

/// Whether `obj` is dead and only waiting for its zone's sweep to finalize it (possible only while
/// an incremental GC sweeps in slices). One TLS load otherwise.
#[inline]
pub fn about_to_be_finalized(obj: *mut JSObject) -> bool {
    !obj.is_null() && SWEEPING.with(Cell::get) && unsafe { IsAboutToBeFinalized(obj) }
}

/// The expose barrier: shades `obj` if an incremental GC is marking.
#[inline]
pub fn expose(obj: *mut JSObject) {
    if !obj.is_null() && MARKING.with(Cell::get) {
        unsafe { js::jsapi::JS::IncrementalPreWriteBarrier(obj) }
    }
}

/// Adds a reflector to the registry (if enabled). `object` and `slot` must stay where they are
/// until `unregister(slot)`.
pub(crate) fn register(object: &Heap<*mut JSObject>, slot: &Cell<usize>) {
    if !is_enabled() || slot.get() != UNREGISTERED {
        return;
    }
    REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        slot.set(r.len());
        r.push((object as *const _, slot as *const _));
    });
}

/// Removes a reflector from the registry (a no-op if it isn't in it).
pub(crate) fn unregister(slot: &Cell<usize>) {
    let index = slot.get();
    if index == UNREGISTERED {
        return;
    }
    slot.set(UNREGISTERED);
    let _ = REGISTRY.try_with(|r| {
        let mut r = r.borrow_mut();
        r.swap_remove(index);
        if let Some(&(_, moved)) = r.get(index) {
            unsafe { (*moved).set(index) };
        }
    });
}

/// Number of registered reflectors on this thread.
pub fn registered() -> usize {
    REGISTRY.with(|r| r.borrow().len())
}

// SpiderMonkey's chunk layout (js/public/HeapAPI.h, SM 153), to read a tenured cell's mark bits.
const CHUNK_MASK: usize = (1 << 20) - 1;
const CHUNK_KIND_OFFSET: usize = 16;
const CHUNK_KIND_TENURED_ARENAS: u8 = 1;
const CHUNK_MARK_BITMAP_OFFSET: usize = 64;
const FIRST_ARENA_OFFSET: usize = (1 << 20) - 252 * 4096;
const CELL_BYTES_PER_MARK_BIT: usize = 8;
const FIRST_THING_ADJUSTMENT_WORDS: usize = FIRST_ARENA_OFFSET / CELL_BYTES_PER_MARK_BIT / 64;

/// Whether a GC thing is marked (black or gray) in the current GC. Things not in a tenured arena
/// chunk count as marked (the caller then traces them, which is only conservative).
///
/// # Safety
/// `obj` must be a live GC thing; only meaningful while a GC is in progress.
pub unsafe fn is_marked_any(obj: *mut JSObject) -> bool {
    let addr = obj as usize;
    let chunk = addr & !CHUNK_MASK;
    unsafe {
        if *((chunk + CHUNK_KIND_OFFSET) as *const u8) != CHUNK_KIND_TENURED_ARENAS {
            return true;
        }
        let offset = addr & CHUNK_MASK;
        if offset < FIRST_ARENA_OFFSET {
            return true;
        }
        let bit = offset / CELL_BYTES_PER_MARK_BIT;
        let word = |bit: usize| {
            let index = bit / 64 - FIRST_THING_ADJUSTMENT_WORDS;
            let w = *((chunk + CHUNK_MARK_BITMAP_OFFSET + index * 8) as *const usize);
            w & (1usize << (bit % 64)) != 0
        };
        // Bit 0: black; bit 1: gray or black.
        word(bit) || word(bit + 1)
    }
}

/// Re-traces the Rust fields of every registered reflector that is marked, with `trc` (the GC's
/// marking tracer). Returns (registered, re-traced).
///
/// # Safety
/// Only from inside a GC, with SpiderMonkey's marking tracer.
pub unsafe fn remark(trc: *mut JSTracer) -> (usize, usize) {
    let len = registered();
    let mut traced = 0;
    for i in 0..len {
        let Some((object, _)) = REGISTRY.with(|r| r.borrow().get(i).copied()) else {
            break;
        };
        let obj = unsafe { (*object).get() };
        if obj.is_null() || !unsafe { is_marked_any(obj) } {
            continue;
        }
        unsafe {
            let class = js::rust::get_object_class(obj);
            if class.is_null() || (*class).cOps.is_null() {
                continue;
            }
            if let Some(trace) = (*(*class).cOps).trace {
                trace(trc, obj);
                traced += 1;
            }
        }
    }
    (len, traced)
}

#[cfg(all(test, not(windows)))]
mod tests {
    use std::cell::Cell;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};

    use js::jsapi::{
        GCContext, GCOptions, GCReason, Heap, JSCLASS_FOREGROUND_FINALIZE, JSClass, JSClassOps,
        JSGCParamKey, JSObject, OnNewGlobalHookOption, SliceBudget,
    };
    use js::realm::AutoRealm;
    use js::rooted;
    use js::rust::wrappers2::{
        FinishIncrementalGC, IncrementalGCSlice, IsIncrementalGCInProgress, JS_GC,
        JS_NewGlobalObject, JS_NewObject, JS_SetGCParameter, PrepareForFullGC, StartIncrementalGC,
    };
    use js::rust::{JSEngine, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS};

    use super::*;

    unsafe extern "C" {
        #[cfg_attr(
            target_vendor = "apple",
            link_name = "\u{1}__ZN2JS11SliceBudgetC1ENS_10WorkBudgetE"
        )]
        #[cfg_attr(
            not(target_vendor = "apple"),
            link_name = "\u{1}_ZN2JS11SliceBudgetC1ENS_10WorkBudgetE"
        )]
        fn SliceBudgetFromWork(this: *mut SliceBudget, work: i64);
    }

    fn work_budget(work: i64) -> SliceBudget {
        let mut budget = std::mem::MaybeUninit::<SliceBudget>::zeroed();
        unsafe {
            SliceBudgetFromWork(budget.as_mut_ptr(), work);
            budget.assume_init()
        }
    }

    static LIVE_FINALIZED: AtomicBool = AtomicBool::new(false);
    static DEAD_FINALIZED: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" fn finalize_live(_: *mut GCContext, _: *mut JSObject) {
        LIVE_FINALIZED.store(true, Ordering::SeqCst);
    }

    unsafe extern "C" fn finalize_dead(_: *mut GCContext, _: *mut JSObject) {
        DEAD_FINALIZED.store(true, Ordering::SeqCst);
    }

    const fn ops(finalize: unsafe extern "C" fn(*mut GCContext, *mut JSObject)) -> JSClassOps {
        JSClassOps {
            addProperty: None,
            delProperty: None,
            enumerate: None,
            newEnumerate: None,
            resolve: None,
            mayResolve: None,
            finalize: Some(finalize),
            call: None,
            construct: None,
            trace: None,
        }
    }

    static LIVE_OPS: JSClassOps = ops(finalize_live);
    static DEAD_OPS: JSClassOps = ops(finalize_dead);

    // Foreground-finalized, like every DOM class (codegen.py).
    static LIVE_CLASS: JSClass = JSClass {
        name: c"IncrementalGcLive".as_ptr(),
        flags: JSCLASS_FOREGROUND_FINALIZE,
        cOps: &LIVE_OPS,
        spec: ptr::null(),
        ext: ptr::null(),
        oOps: ptr::null(),
    };
    static DEAD_CLASS: JSClass = JSClass {
        name: c"IncrementalGcDead".as_ptr(),
        flags: JSCLASS_FOREGROUND_FINALIZE,
        cOps: &DEAD_OPS,
        spec: ptr::null(),
        ext: ptr::null(),
        oOps: ptr::null(),
    };

    /// Unregistering swaps the last entry into the hole and fixes that entry's index slot.
    #[test]
    fn registry_swap_remove_fixes_indices() {
        enable();
        let heaps: Vec<Box<Heap<*mut JSObject>>> =
            (0..4).map(|_| Box::new(Heap::default())).collect();
        let slots: Vec<Box<Cell<usize>>> =
            (0..4).map(|_| Box::new(Cell::new(UNREGISTERED))).collect();
        let base = registered();
        for i in 0..4 {
            register(&heaps[i], &slots[i]);
            assert_eq!(slots[i].get(), base + i);
        }
        // Registering twice is a no-op.
        register(&heaps[0], &slots[0]);
        assert_eq!(registered(), base + 4);

        unregister(&slots[1]);
        assert_eq!(slots[1].get(), UNREGISTERED);
        assert_eq!(registered(), base + 3);
        assert_eq!(
            slots[3].get(),
            base + 1,
            "the last entry moved into the hole"
        );
        let check = |i: usize| {
            REGISTRY.with(|r| {
                let r = r.borrow();
                let (heap, slot) = r[slots[i].get()];
                assert_eq!(heap, &*heaps[i] as *const _);
                assert_eq!(slot, &*slots[i] as *const _);
            })
        };
        [0, 2, 3].into_iter().for_each(check);

        // Removing the last entry, and removing twice.
        unregister(&slots[3]);
        unregister(&slots[3]);
        unregister(&slots[1]);
        assert_eq!(registered(), base + 2);
        [0, 2].into_iter().for_each(check);
        unregister(&slots[0]);
        unregister(&slots[2]);
        assert_eq!(registered(), base);
    }

    /// Off unless sweeping (one TLS load), and never for null.
    #[test]
    fn about_to_be_finalized_is_off_outside_sweeping() {
        set_sweeping(false);
        assert!(!about_to_be_finalized(ptr::null_mut()));
        set_sweeping(true);
        assert!(!about_to_be_finalized(ptr::null_mut()));
        set_sweeping(false);
    }

    /// An unreachable object is reported dying while its zone is swept, before its finalizer runs;
    /// a reachable one never is. (`WeakRef::root` returns None exactly when this is true.)
    #[test]
    fn about_to_be_finalized_while_sweeping_in_slices() {
        let engine = JSEngine::init().unwrap();
        let mut runtime = Runtime::new(engine.handle());
        let cx = runtime.cx();
        unsafe {
            rooted!(&in(cx) let global = JS_NewGlobalObject(
                cx,
                &SIMPLE_GLOBAL_CLASS,
                ptr::null_mut(),
                OnNewGlobalHookOption::FireOnNewGlobalHook,
                &*RealmOptions::default(),
            ));
            let mut realm = AutoRealm::new_from_handle(cx, global.handle());
            let cx = &mut realm;
            JS_SetGCParameter(cx, JSGCParamKey::JSGC_INCREMENTAL_GC_ENABLED, 1);

            rooted!(&in(cx) let live = JS_NewObject(cx, &LIVE_CLASS));
            rooted!(&in(cx) let dead = JS_NewObject(cx, &DEAD_CLASS));
            // Tenure both (the roots are updated to where they moved).
            JS_GC(cx, GCReason::API);
            let dead_ptr = dead.get();
            dead.set(ptr::null_mut());
            assert!(!DEAD_FINALIZED.load(Ordering::SeqCst));

            set_sweeping(true);
            assert!(!about_to_be_finalized(dead_ptr), "no GC in progress");
            set_sweeping(false);

            PrepareForFullGC(cx);
            StartIncrementalGC(cx, GCOptions::Normal, GCReason::API, &work_budget(1));
            let mut seen = false;
            let mut slices = 0;
            while IsIncrementalGCInProgress(cx) && slices < 1_000_000 {
                // Between slices, as the slice callback would set it.
                set_sweeping(true);
                assert!(!about_to_be_finalized(live.get()), "a reachable object");
                if DEAD_FINALIZED.load(Ordering::SeqCst) {
                    break;
                }
                if about_to_be_finalized(dead_ptr) {
                    seen = true;
                    break;
                }
                set_sweeping(false);
                IncrementalGCSlice(cx, GCReason::API, &work_budget(1));
                slices += 1;
            }
            set_sweeping(false);
            assert!(
                seen,
                "the unreachable object was finalized (or the GC ended) before it was reported dying, after {slices} slices"
            );
            assert!(!about_to_be_finalized(dead_ptr), "off outside sweeping");
            FinishIncrementalGC(cx, GCReason::API);
            assert!(DEAD_FINALIZED.load(Ordering::SeqCst));
            assert!(!LIVE_FINALIZED.load(Ordering::SeqCst));
            assert!(!live.get().is_null());
        }
    }
}
