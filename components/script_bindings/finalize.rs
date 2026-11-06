/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Generic finalizer implementations for DOM binding implementations.

use std::any::type_name;
use std::cell::Cell;
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use js::jsapi::JSObject;
use js::rust::GCMethods;

use crate::DomObject;
use crate::codegen::PrototypeList::PROTO_OR_IFACE_LENGTH;
use crate::utils::{ProtoOrIfaceArray, get_proto_or_iface_array};
use crate::weakref::WeakReferenceable;

/// Drop the resources held by reserved slots of a global object
unsafe fn do_finalize_global(obj: *mut JSObject) {
    unsafe {
        let protolist = get_proto_or_iface_array(obj);
        let list = (*protolist).as_mut_ptr();
        for idx in 0..PROTO_OR_IFACE_LENGTH as isize {
            let entry = list.offset(idx);
            let value = *entry;
            <*mut JSObject>::post_barrier(entry, value, ptr::null_mut());
        }
        let _: Box<ProtoOrIfaceArray> = Box::from_raw(protolist);
    }
}

/// Whether DOM finalizers count themselves (for the GC scheduler's log, PARTII_GC_FINALIZE_STATS=1).
static FINALIZE_STATS: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// DOM objects finalized on this thread and the time their finalizers took, in ns.
    static FINALIZED: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
}

/// Turns the per-thread count of DOM finalizers on or off.
pub fn set_finalize_stats(enabled: bool) {
    FINALIZE_STATS.store(enabled, Ordering::Relaxed);
}

/// DOM objects finalized on this thread since the last call, and their finalizers' total time in
/// ns (both 0 unless `set_finalize_stats(true)`).
pub fn take_finalize_stats() -> (u64, u64) {
    FINALIZED.with(|f| f.replace((0, 0)))
}

/// Runs a finalizer, counting it when the stats are on.
#[inline]
fn counted(f: impl FnOnce()) {
    if !FINALIZE_STATS.load(Ordering::Relaxed) {
        return f();
    }
    let start = Instant::now();
    f();
    let ns = start.elapsed().as_nanos() as u64;
    FINALIZED.with(|c| {
        let (n, t) = c.get();
        c.set((n + 1, t + ns));
    });
}

/// # Safety
/// `this` must point to a valid, non-null instance of T.
pub(crate) unsafe fn finalize_common<T: DomObject>(this: *const T) {
    if !this.is_null() {
        // The pointer can be null if the object is the unforgeable holder of that interface.
        counted(|| {
            let this = unsafe { Box::from_raw(this as *mut T) };
            this.reflector().drop_memory(&*this);
        });
    }
    debug!("{} finalize: {:p}", type_name::<T>(), this);
}

/// # Safety
/// `obj` must point to a valid, non-null JS object.
/// `this` must point to a valid, non-null instance of T.
pub(crate) unsafe fn finalize_global<T: DomObject>(obj: *mut JSObject, this: *const T) {
    unsafe {
        do_finalize_global(obj);
        finalize_common::<T>(this);
    }
}

/// # Safety
/// `this` must point to a Rced valid, non-null instance of T.
pub(crate) unsafe fn finalize_weak_referenceable<T: WeakReferenceable>(this: *const T) {
    if !this.is_null() {
        // The pointer can be null if the object is the unforgeable holder of that interface.
        counted(|| {
            let this = unsafe { Rc::from_raw(this) };
            this.reflector().drop_memory(&*this);
        });
    }
    debug!("{} finalize: {:p}", type_name::<T>(), this);
}
