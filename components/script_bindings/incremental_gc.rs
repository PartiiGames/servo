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

/// Whether the C++ barrier tracer was built (it needs SpiderMonkey's headers, which mozjs_sys's
/// prebuilt archive lacks). Without it `black_remark` does nothing and sweeping in slices is off.
/// It is also off when the shim's headers disagree with the linked SpiderMonkey on `JSTracer`'s
/// layout (headers from another configuration): checked once per process.
pub fn black_remark_available() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        if !cfg!(partii_gc_shim) {
            return false;
        }
        let mut layout = [0usize; 2];
        unsafe { partii_barrier_tracer_layout(layout.as_mut_ptr()) };
        let rust = [size_of::<JSTracer>(), align_of::<JSTracer>()];
        if layout != rust {
            eprintln!(
                "[gc-sched] black remark off: the shim's headers see JSTracer as {layout:?} (size, align), the linked SpiderMonkey as {rust:?}"
            );
        }
        layout == rust
    })
}

#[cfg(partii_gc_shim)]
unsafe extern "C" {
    fn partii_barrier_tracer_new(cx: *mut js::jsapi::JSContext) -> *mut JSTracer;
    fn partii_barrier_tracer_finish(trc: *mut JSTracer) -> usize;
    fn partii_barrier_tracer_layout(out: *mut usize);
}

#[cfg(not(partii_gc_shim))]
unsafe fn partii_barrier_tracer_layout(_: *mut usize) {}

#[cfg(not(partii_gc_shim))]
unsafe fn partii_barrier_tracer_new(_: *mut js::jsapi::JSContext) -> *mut JSTracer {
    std::ptr::null_mut()
}

#[cfg(not(partii_gc_shim))]
unsafe fn partii_barrier_tracer_finish(_: *mut JSTracer) -> usize {
    0
}

/// What a black remark did: reflectors re-traced, edges shaded.
#[derive(Clone, Copy, Default)]
pub struct BlackRemark {
    pub registered: usize,
    pub traced: usize,
    pub edges: usize,
}

/// The black remark, from the mutator side between slices of an incremental GC that is marking:
/// traces the Rust roots (`roots`) and the Rust fields of every registered reflector that is
/// already marked with a tracer that applies the incremental pre-barrier to each child. Whatever
/// a `Dom<T>` moved into an already-scanned object (or onto a Rust root) points to is then on the
/// black mark stack, and the next slice marks it before marking can end. Run before every slice,
/// SpiderMonkey's own included (its slice-budget callback), this covers the move case in black;
/// the gray-roots remark (`remark`) stays as the fallback for GCs finished outside the driver.
///
/// # Safety
/// `cx` is this thread's context; not inside a GC slice.
pub unsafe fn black_remark(
    cx: *mut js::jsapi::JSContext,
    roots: Option<(
        unsafe extern "C" fn(*mut JSTracer, *mut std::os::raw::c_void),
        *mut std::os::raw::c_void,
    )>,
) -> BlackRemark {
    unsafe {
        if !black_remark_available() {
            return BlackRemark::default();
        }
        let trc = partii_barrier_tracer_new(cx);
        if trc.is_null() {
            return BlackRemark::default();
        }
        if let Some((roots, data)) = roots {
            roots(trc, data);
        }
        let (registered, traced) = remark(trc);
        let edges = partii_barrier_tracer_finish(trc);
        BlackRemark {
            registered,
            traced,
            edges,
        }
    }
}

/// Re-traces the Rust fields of every registered reflector that is marked, with `trc`. Returns
/// (registered, re-traced).
///
/// # Safety
/// While an incremental GC is in progress: inside it with SpiderMonkey's marking tracer (the gray
/// remark), or between slices with the barrier tracer (`black_remark`).
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
        JSGCParamKey, JSObject, JSTracer, OnNewGlobalHookOption, SliceBudget,
    };
    use js::realm::AutoRealm;
    use js::rooted;
    use js::rust::wrappers2::{
        FinishIncrementalGC, IncrementalGCSlice, IsIncrementalGCInProgress, JS_GC,
        JS_NewGlobalObject, JS_NewObject, JS_SetGCParameter, PrepareForFullGC, StartIncrementalGC,
    };
    use js::rust::{
        CompileOptionsWrapper, JSEngine, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS,
        evaluate_script,
    };

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
    static FILLER_OPS: JSClassOps = ops(finalize_filler);

    /// Fillers finalized so far (addresses), to tell which unreachable ones are still waiting.
    static FILLERS_FINALIZED: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

    unsafe extern "C" fn finalize_filler(_: *mut GCContext, obj: *mut JSObject) {
        FILLERS_FINALIZED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(obj as usize);
    }

    /// SpiderMonkey initializes once per process and can't be initialized again after shutdown,
    /// so these tests need a process each: run them with cargo-nextest
    /// (`cargo nextest run -p servo-script-bindings incremental_gc`). Each test inits the engine
    /// and drops its runtime, then the engine, before it returns (a full shutdown).
    fn engine() -> JSEngine {
        JSEngine::init()
            .expect("SpiderMonkey already initialized: run these tests with cargo-nextest")
    }

    // Foreground-finalized, like every DOM class (codegen.py).
    static LIVE_CLASS: JSClass = JSClass {
        name: c"IncrementalGcLive".as_ptr(),
        flags: JSCLASS_FOREGROUND_FINALIZE,
        cOps: &LIVE_OPS,
        spec: ptr::null(),
        ext: ptr::null(),
        oOps: ptr::null(),
    };
    static FILLER_CLASS: JSClass = JSClass {
        name: c"IncrementalGcFiller".as_ptr(),
        flags: JSCLASS_FOREGROUND_FINALIZE,
        cOps: &FILLER_OPS,
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
        let engine = engine();
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

            // Classes with a finalizer are allocated tenured, so these addresses are stable.
            rooted!(&in(cx) let live = JS_NewObject(cx, &LIVE_CLASS));
            // Unreachable fillers of the same alloc kind: foreground finalizing that kind then
            // spans many slices (the budget is stepped per arena), so some of them are still
            // waiting between two slices. Which arenas go first isn't ours to choose, so every
            // filler not finalized yet is checked, not just one object.
            FILLERS_FINALIZED.lock().unwrap().clear();
            let fillers: Vec<*mut JSObject> = (0..20_000)
                .map(|_| JS_NewObject(cx, &FILLER_CLASS))
                .collect();
            let dead_ptr = JS_NewObject(cx, &DEAD_CLASS);
            assert!(!DEAD_FINALIZED.load(Ordering::SeqCst));

            set_sweeping(true);
            assert!(!about_to_be_finalized(dead_ptr), "no GC in progress");
            set_sweeping(false);

            PrepareForFullGC(cx);
            StartIncrementalGC(cx, GCOptions::Normal, GCReason::API, &work_budget(1));
            let mut seen = false;
            let mut slices = 0;
            let mut partial_slices = 0;
            while IsIncrementalGCInProgress(cx) && slices < 1_000_000 {
                // Between slices, as the slice callback would set it.
                set_sweeping(true);
                assert!(!about_to_be_finalized(live.get()), "a reachable object");
                let finalized: std::collections::HashSet<usize> =
                    FILLERS_FINALIZED.lock().unwrap().iter().copied().collect();
                if !finalized.is_empty() && finalized.len() < fillers.len() {
                    partial_slices += 1;
                }
                let waiting = fillers
                    .iter()
                    .filter(|f| !finalized.contains(&(**f as usize)))
                    .any(|f| about_to_be_finalized(*f));
                let dead_waiting =
                    !DEAD_FINALIZED.load(Ordering::SeqCst) && about_to_be_finalized(dead_ptr);
                if waiting || dead_waiting {
                    seen = true;
                    break;
                }
                set_sweeping(false);
                // Small slices, so sweeping yields between them; WorkBudget(1) barely moves marking.
                IncrementalGCSlice(cx, GCReason::API, &work_budget(200));
                slices += 1;
            }
            set_sweeping(false);
            assert!(
                seen,
                "no unreachable object was reported dying between slices: {slices} slices ({partial_slices} with the fillers part-finalized), GC in progress {}, dead finalized {}, fillers finalized {}",
                IsIncrementalGCInProgress(cx),
                DEAD_FINALIZED.load(Ordering::SeqCst),
                FILLERS_FINALIZED.lock().unwrap().len()
            );
            assert!(!about_to_be_finalized(dead_ptr), "off outside sweeping");
            FinishIncrementalGC(cx, GCReason::API);
            assert!(DEAD_FINALIZED.load(Ordering::SeqCst));
            assert!(!LIVE_FINALIZED.load(Ordering::SeqCst));
            assert!(!live.get().is_null());
        }
    }

    // The move case: a Rust edge moved, without a barrier, out of an object marking hasn't
    // reached into one it has already scanned.
    thread_local! {
        static FIELD1: Box<Heap<*mut JSObject>> = Heap::boxed(ptr::null_mut());
        static FIELD2: Box<Heap<*mut JSObject>> = Heap::boxed(ptr::null_mut());
        static OWNER1: Box<Heap<*mut JSObject>> = Heap::boxed(ptr::null_mut());
        static OWNER2: Box<Heap<*mut JSObject>> = Heap::boxed(ptr::null_mut());
        static SLOT1: Box<Cell<usize>> = Box::new(Cell::new(UNREGISTERED));
        static SLOT2: Box<Cell<usize>> = Box::new(Cell::new(UNREGISTERED));
    }
    static CHILD_FINALIZED: AtomicBool = AtomicBool::new(false);

    unsafe fn trace_heap(trc: *mut JSTracer, heap: &Heap<*mut JSObject>) {
        unsafe { crate::trace::trace_object(trc, "field", heap) };
    }
    unsafe extern "C" fn trace_owner1(trc: *mut JSTracer, _: *mut JSObject) {
        FIELD1.with(|f| unsafe { trace_heap(trc, f) });
        OWNER1.with(|f| unsafe { trace_heap(trc, f) });
    }
    unsafe extern "C" fn trace_owner2(trc: *mut JSTracer, _: *mut JSObject) {
        FIELD2.with(|f| unsafe { trace_heap(trc, f) });
        OWNER2.with(|f| unsafe { trace_heap(trc, f) });
    }
    unsafe extern "C" fn finalize_child(_: *mut GCContext, _: *mut JSObject) {
        CHILD_FINALIZED.store(true, Ordering::SeqCst);
    }
    unsafe extern "C" fn finalize_nothing(_: *mut GCContext, _: *mut JSObject) {}

    const fn traced_ops(trace: unsafe extern "C" fn(*mut JSTracer, *mut JSObject)) -> JSClassOps {
        JSClassOps {
            trace: Some(trace),
            ..ops(finalize_nothing)
        }
    }
    static OWNER1_OPS: JSClassOps = traced_ops(trace_owner1);
    static OWNER2_OPS: JSClassOps = traced_ops(trace_owner2);
    static CHILD_OPS: JSClassOps = ops(finalize_child);
    static OWNER1_CLASS: JSClass = JSClass {
        name: c"IncrementalGcOwner1".as_ptr(),
        flags: JSCLASS_FOREGROUND_FINALIZE,
        cOps: &OWNER1_OPS,
        spec: ptr::null(),
        ext: ptr::null(),
        oOps: ptr::null(),
    };
    static OWNER2_CLASS: JSClass = JSClass {
        name: c"IncrementalGcOwner2".as_ptr(),
        flags: JSCLASS_FOREGROUND_FINALIZE,
        cOps: &OWNER2_OPS,
        spec: ptr::null(),
        ext: ptr::null(),
        oOps: ptr::null(),
    };
    static CHILD_CLASS: JSClass = JSClass {
        name: c"IncrementalGcChild".as_ptr(),
        flags: JSCLASS_FOREGROUND_FINALIZE,
        cOps: &CHILD_OPS,
        spec: ptr::null(),
        ext: ptr::null(),
        oOps: ptr::null(),
    };

    /// owner1 is rooted (scanned early); owner2 sits at the end of a long chain (scanned late)
    /// and holds the child. Mid-mark the child moves from owner2's Rust field to owner1's with no
    /// barrier. With the black remark between slices the child survives; without it (the control
    /// run) it is normally collected, which shows the test exercises the hazard.
    #[test]
    fn black_remark_catches_a_moved_edge() {
        if !cfg!(partii_gc_shim) {
            eprintln!(
                "black_remark_catches_a_moved_edge: skipped, built without the barrier tracer shim"
            );
            return;
        }
        // Built with the shim, the black remark must really run here (a unit-test target holds a
        // single from-source mozjs_sys, so the layout check has no reason to turn it off).
        assert!(
            black_remark_available(),
            "the shim was built but the JSTracer layout check turned the black remark off"
        );
        let engine = engine();
        let mut runtime = Runtime::new(engine.handle());
        let cx = runtime.cx();
        enable();
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

            let mut outcomes = vec![];
            for with_remark in [false, true] {
                // A child the previous round left alive (the control run's move need not lose
                // it) is garbage now; collect it before the flag is reset.
                JS_GC(cx, GCReason::API);
                CHILD_FINALIZED.store(false, Ordering::SeqCst);
                rooted!(&in(cx) let owner1 = JS_NewObject(cx, &OWNER1_CLASS));
                rooted!(&in(cx) let mut owner2 = JS_NewObject(cx, &OWNER2_CLASS));
                let child = JS_NewObject(cx, &CHILD_CLASS);
                OWNER1.with(|h| h.set(owner1.get()));
                OWNER2.with(|h| h.set(owner2.get()));
                FIELD2.with(|h| h.set(child));
                OWNER1.with(|h| SLOT1.with(|s| register(h, s)));
                OWNER2.with(|h| SLOT2.with(|s| register(h, s)));

                // owner2 at the end of a 20000-object chain, reachable only through it.
                rooted!(&in(cx) let o2 = js::jsval::ObjectValue(owner2.get()));
                assert!(js::rust::wrappers2::JS_SetProperty(
                    cx,
                    global.handle().into(),
                    c"o2".as_ptr(),
                    o2.handle().into()
                ));
                owner2.set(ptr::null_mut());
                rooted!(&in(cx) let mut head = js::jsval::UndefinedValue());
                let options = CompileOptionsWrapper::new(cx, c"chain".to_owned(), 1);
                assert!(evaluate_script(
                    cx,
                    global.handle(),
                    "var h = {}, c = h; for (var i = 0; i < 20000; i++) { c.n = {}; c = c.n; } c.o2 = o2; delete globalThis.o2; h",
                    head.handle_mut(),
                    options,
                )
                .is_ok());
                // Tenure everything (the traced fields follow).
                JS_GC(cx, GCReason::API);
                assert!(
                    !CHILD_FINALIZED.load(Ordering::SeqCst),
                    "the child is reachable before the GC"
                );

                PrepareForFullGC(cx);
                StartIncrementalGC(cx, GCOptions::Normal, GCReason::API, &work_budget(1));
                // A few slices in: owner1 marked, the chain (and so owner2) not reached yet.
                let owner2_ptr = OWNER2.with(|h| h.get());
                let mut slices = 0;
                while IsIncrementalGCInProgress(cx)
                    && !(is_marked_any(owner1.get()) && slices >= 50)
                    && slices < 100_000
                {
                    IncrementalGCSlice(cx, GCReason::API, &work_budget(100));
                    slices += 1;
                }
                assert!(
                    IsIncrementalGCInProgress(cx),
                    "the GC ended before the move ({slices} slices)"
                );
                let owner2_reached = is_marked_any(owner2_ptr);
                // The move, with no barrier on either side.
                let moved =
                    FIELD2.with(|h| std::mem::replace(&mut *h.get_unsafe(), ptr::null_mut()));
                FIELD1.with(|h| *h.get_unsafe() = moved);
                if with_remark {
                    let done = black_remark(cx.raw_cx(), None);
                    assert!(
                        done.traced >= 1 && done.edges >= 1,
                        "the black remark traced owner1"
                    );
                }
                FinishIncrementalGC(cx, GCReason::API);
                let died = CHILD_FINALIZED.load(Ordering::SeqCst);
                outcomes.push((with_remark, owner2_reached, died, slices));
                if died {
                    FIELD1.with(|h| *h.get_unsafe() = ptr::null_mut());
                }
                SLOT1.with(|s| unregister(s));
                SLOT2.with(|s| unregister(s));
                FIELD1.with(|h| *h.get_unsafe() = ptr::null_mut());
                OWNER1.with(|h| *h.get_unsafe() = ptr::null_mut());
                OWNER2.with(|h| *h.get_unsafe() = ptr::null_mut());
                head.set(js::jsval::UndefinedValue());
            }
            eprintln!(
                "black_remark_catches_a_moved_edge: (remark, owner2 reached before the move, child died, slices) = {outcomes:?}"
            );
            let (_, _, control_died, _) = outcomes[0];
            if !control_died {
                eprintln!(
                    "warning: the control run's child survived without the remark; the move wasn't a hazard this time"
                );
            }
            let (_, _, died, _) = outcomes[1];
            assert!(
                !died,
                "the child moved into an already-marked owner was collected despite the black remark"
            );
        }
    }
}
