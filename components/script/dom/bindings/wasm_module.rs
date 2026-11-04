/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Structured clone of `WebAssembly.Module` within an agent cluster.
//!
//! SpiderMonkey leaves cloning a `WebAssembly.Module` to the embedder (`js/WasmModule.h`):
//! `JS::GetWasmModule(obj)` hands out a refcounted, thread-safe `JS::WasmModule`, the clone
//! buffer keeps that reference, and the reader calls its virtual `createObject(cx)` in the target
//! realm. Gecko does this in `StructuredCloneHolder`; here it rides in the in-process clone buffer
//! (`write_for_same_agent_cluster`), so a module only ever travels within this process, to a
//! window, a dedicated worker or an AudioWorklet (Emscripten pthread builds post their compiled
//! module to every worker).
//!
//! mozjs only has a bindgen declaration of `GetWasmModule` returning `RefPtr<WasmModule>` by
//! value, which doesn't match the C++ ABI (a class with a destructor is returned through a hidden
//! pointer), so it is called here with that pointer made explicit: the first argument on x86-64
//! (SysV and Windows), x8 on AArch64. The refcount (`js::AtomicRefCounted`, an atomic word after
//! the vtable pointer) is managed directly.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};

use js::jsapi::{HandleObject, JSContext as RawJSContext, JSObject};

/// The C++ object: `struct WasmModule : js::AtomicRefCounted<WasmModule>` with a virtual
/// destructor and two pure virtual methods.
#[repr(C)]
struct RawWasmModule {
    vtable: *const usize,
    refcount: AtomicUsize,
}

/// The vtable slot of `createObject(JSContext*) const`: after the complete-object and deleting
/// destructors on the Itanium ABI; after the scalar deleting destructor on MSVC.
#[cfg(not(target_env = "msvc"))]
const CREATE_OBJECT_SLOT: usize = 2;
#[cfg(target_env = "msvc")]
const CREATE_OBJECT_SLOT: usize = 1;

/// One reference to a `JS::WasmModule`. Send + Sync: the type is thread-safe by contract.
pub(crate) struct WasmModuleRef(NonNull<RawWasmModule>);

// SAFETY: `JS::WasmModule` is documented as thread-safe (refcount, destructor and methods).
unsafe impl Send for WasmModuleRef {}
unsafe impl Sync for WasmModuleRef {}

impl WasmModuleRef {
    /// The module behind a `WebAssembly.Module` object, or None if `obj` isn't one (or this
    /// architecture isn't supported).
    #[expect(unsafe_code)]
    pub(crate) fn from_object(obj: HandleObject) -> Option<WasmModuleRef> {
        unsafe {
            if !js::jsapi::JS::IsWasmModuleObject(obj) {
                return None;
            }
            let mut out: *mut RawWasmModule = std::ptr::null_mut();
            call_get_wasm_module(&mut out, obj)?;
            // `out` now holds the reference the returned RefPtr owned.
            NonNull::new(out).map(WasmModuleRef)
        }
    }

    /// A new `WebAssembly.Module` object for this module, in `cx`'s current realm.
    #[expect(unsafe_code)]
    pub(crate) fn create_object(&self, cx: *mut RawJSContext) -> *mut JSObject {
        unsafe {
            let vtable = self.0.as_ref().vtable;
            let create: extern "C" fn(*const RawWasmModule, *mut RawJSContext) -> *mut JSObject =
                std::mem::transmute(*vtable.add(CREATE_OBJECT_SLOT));
            create(self.0.as_ptr(), cx)
        }
    }
}

impl Clone for WasmModuleRef {
    #[expect(unsafe_code)]
    fn clone(&self) -> WasmModuleRef {
        unsafe { self.0.as_ref() }.refcount.fetch_add(1, Ordering::Relaxed);
        WasmModuleRef(self.0)
    }
}

impl Drop for WasmModuleRef {
    #[expect(unsafe_code)]
    fn drop(&mut self) {
        let previous = unsafe { self.0.as_ref() }
            .refcount
            .fetch_sub(1, Ordering::AcqRel);
        if previous == 1 {
            // The last reference: C++ would `js_delete` it, freeing through SpiderMonkey's
            // allocator, which can't be reached safely from here. This only happens when a clone
            // holding a module is dropped unread after every WebAssembly.Module object for it was
            // collected; the module is leaked rather than freed with the wrong allocator.
            log::warn!("Leaking a WebAssembly module whose last reference was an unread clone.");
        }
    }
}

/// Call `JS::GetWasmModule(obj)`, which returns `RefPtr<JS::WasmModule>` through a hidden
/// pointer, storing the raw pointer in `out`.
#[expect(unsafe_code)]
unsafe fn call_get_wasm_module(out: &mut *mut RawWasmModule, obj: HandleObject) -> Option<()> {
    // bindgen's declaration has the right symbol on every platform; only its signature is wrong,
    // so take its address and call it with the real one.
    let f = js::jsapi::JS::GetWasmModule as *const ();
    #[cfg(target_arch = "x86_64")]
    unsafe {
        // SysV and Windows x64: the hidden result pointer is the first argument.
        let f: extern "C" fn(*mut *mut RawWasmModule, HandleObject) = std::mem::transmute(f);
        f(out, obj);
        return Some(());
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        // AAPCS64: the hidden result pointer goes in x8; the Handle (one pointer) in x0.
        std::arch::asm!(
            "blr {f}",
            f = in(reg) f,
            in("x8") out as *mut *mut RawWasmModule,
            in("x0") obj.ptr,
            clobber_abi("C"),
        );
        return Some(());
    }
    #[allow(unreachable_code)]
    {
        let _ = (f, out, obj);
        None
    }
}
