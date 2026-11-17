/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii (gaps-3): an in-memory cache of compiled module scripts, per script thread.
//!
//! A page that loads the same module again in this event loop (a game that reloads itself, a second page of the
//! same origin) gets the module's SpiderMonkey stencil from here instead of parsing the source again: the module is
//! instantiated from the stencil, as Gecko does with its in-memory script cache. The cached stencil also collects the
//! functions each page compiles lazily (JS::StartCollectingDelazifications), so the next page instantiates those too
//! and doesn't compile them again when they first run.
//!
//! The key is the module's URL, the compile options that differ between our compiles (start line, introduction
//! type) and the full source text: a hit is the same text, so the same parse. `PARTII_STENCIL_CACHE=0` turns it off.

use std::cell::RefCell;
use std::ffi::CStr;
use std::ptr;

use js::jsapi::{
    CollectDelazificationsResult, DelazificationOption, EagerBaselineOption, InstantiateOptions,
    JSObject, ReadOnlyCompileOptions, Stencil, StencilRelease,
};
use js::realm::CurrentRealm;
use js::rust::transform_str_to_source_text;
use js::rust::wrappers2::{InstantiateModuleStencil, StartCollectingDelazifications1};

/// At most this many modules, and this much source text, per script thread (least recently used out first).
const MAX_ENTRIES: usize = 32;
const MAX_SOURCE_BYTES: usize = 64 << 20;

struct Entry {
    url: String,
    line: u32,
    intro: Option<&'static CStr>,
    source: Box<str>,
    stencil: *mut Stencil,
}

impl Drop for Entry {
    #[expect(unsafe_code)]
    fn drop(&mut self) {
        // The stencil is reference counted and holds no GC things: releasing it needs no JSContext.
        unsafe { StencilRelease(self.stencil) };
    }
}

thread_local!(static CACHE: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) });

pub(crate) fn enabled() -> bool {
    // Windows ARM64: MSVC returns `already_AddRefed<Stencil>` through a hidden pointer in x8, which a Rust
    // declaration can't express (see compile_to_stencil); no cache there until that call is done in asm or C++.
    if cfg!(all(windows, not(target_arch = "x86_64"))) {
        return false;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PARTII_STENCIL_CACHE").map_or(true, |v| v != "0"))
}

/// `JS::CompileModuleScriptToStencil`: the stencil (owned), or null with the error pending on `cx`.
///
/// It returns `already_AddRefed<Stencil>` by value. mfbt makes that class trivial on Unix so it comes back in a
/// register, which is what the bindgen declaration assumes. MSVC returns it through a hidden pointer instead (the
/// class has user-declared constructors and a private member), passed in RCX ahead of the arguments, so through the
/// bindgen declaration every argument lands one register off: the function read the compile options as the
/// JSContext and crashed (0xC0000005 in JSContext::stackLimitForCurrentPrincipal on every module worker, desk-1).
/// On Windows x64 it's called with the hidden pointer spelled out.
#[expect(unsafe_code)]
unsafe fn compile_to_stencil(
    cx: &mut CurrentRealm,
    options: *const ReadOnlyCompileOptions,
    text: &mut js::jsapi::SourceText<js::jsapi::mozilla::Utf8Unit>,
) -> *mut Stencil {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        type Msvc = unsafe extern "C" fn(
            *mut *mut Stencil,
            *mut js::jsapi::JSContext,
            *const ReadOnlyCompileOptions,
            *mut js::jsapi::SourceText<js::jsapi::mozilla::Utf8Unit>,
        ) -> *mut *mut Stencil;
        let f: Msvc =
            unsafe { std::mem::transmute(js::jsapi::CompileModuleScriptToStencil as *const ()) };
        let mut out: *mut Stencil = ptr::null_mut();
        unsafe { f(&mut out, cx.raw_cx(), options, text) };
        out
    }
    #[cfg(not(all(windows, target_arch = "x86_64")))]
    {
        unsafe { js::rust::wrappers2::CompileModuleScriptToStencil(cx, options, text) }.mRawPtr
    }
}

fn instantiate_options() -> InstantiateOptions {
    // The defaults: fill_compile_options sets none of these.
    InstantiateOptions {
        skipFilenameValidation: false,
        hideScriptFromDebugger: false,
        deferDebugMetadata: false,
        eagerDelazificationStrategy_: DelazificationOption::OnDemandOnly,
        eagerBaselineStrategy_: EagerBaselineOption::None,
    }
}

/// Compile (or instantiate from the cache) a module, as `CompileModule1` does: the module record, or null with the
/// parse error pending on `cx`.
#[expect(unsafe_code)]
pub(crate) unsafe fn compile_module(
    cx: &mut CurrentRealm,
    options: *const ReadOnlyCompileOptions,
    url: &str,
    line: u32,
    intro: Option<&'static CStr>,
    source: &str,
) -> *mut JSObject {
    let cached = CACHE.with_borrow_mut(|c| {
        let i = c.iter().position(|e| {
            e.url == url && e.line == line && e.intro == intro && *e.source == *source
        })?;
        // Most recently used last.
        let e = c.remove(i);
        let s = e.stencil;
        c.push(e);
        Some(s)
    });
    let (stencil, fresh) = match cached {
        Some(s) => (s, false),
        None => {
            let mut text = transform_str_to_source_text(source);
            let s = unsafe { compile_to_stencil(cx, options, &mut text) };
            if s.is_null() {
                return ptr::null_mut();
            }
            (s, true)
        },
    };
    let opts = instantiate_options();
    rooted!(&in(cx) let module = unsafe { InstantiateModuleStencil(cx, &opts, stencil, ptr::null_mut()) });
    if module.is_null() {
        if fresh {
            unsafe { StencilRelease(stencil) };
        }
        return ptr::null_mut();
    }
    // Lazily compiled functions of this page go into the (shared) stencil, for the next page.
    let mut result = CollectDelazificationsResult::NotSupported;
    if !unsafe { StartCollectingDelazifications1(cx, module.handle(), stencil, &mut result) } {
        unsafe { js::rust::wrappers2::JS_ClearPendingException(cx) };
    }
    if fresh {
        CACHE.with_borrow_mut(|c| {
            c.push(Entry {
                url: url.to_owned(),
                line,
                intro,
                source: source.into(),
                stencil,
            });
            while c.len() > MAX_ENTRIES ||
                (c.len() > 1 && c.iter().map(|e| e.source.len()).sum::<usize>() > MAX_SOURCE_BYTES)
            {
                c.remove(0);
            }
        });
    }
    module.get()
}
