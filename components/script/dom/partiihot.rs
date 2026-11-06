/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii: the `PartiiHot` namespace (PartiiHot.webidl, pref `dom_partii_hot_enabled`). A Partii game's
//! Hot TS modules are compiled ahead of time to C on export (platform/tsc, ptsc); a runtime without a
//! JIT (console mode) links that C and registers each exported function here before it starts Servo.
//! The export's JS shim then calls `PartiiHot.call("sand.stepAll", cells, w, h, stripes, step)`.
//!
//! The calling convention is one wrapper per function, generated with the C (partii-runtime
//! tools/nj/hot-glue.mjs): `void wrapper(const HotArg* args, HotArg* ret)`, plus a signature string
//! `"<ret>:<args>"` with one letter per value: `v` void (return only), `i` i32, `u` u32, `d` f64,
//! `b` bool, `p` pointer (a typed array or DataView, shared or not; its bytes are passed in place).
//! Hot TS has no strings, objects or exceptions, so nothing else crosses.

use std::collections::HashMap;
use std::ffi::{CString, c_void};
use std::ptr;
use std::sync::OnceLock;

use dom_struct::dom_struct;
use js::jsapi::{GetArrayBufferViewLengthAndData, JS_IsArrayBufferViewObject};
use js::rust::HandleValue;
use script_bindings::reflector::Reflector;

use crate::dom::bindings::codegen::Bindings::PartiiHotBinding::PartiiHotMethods;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::str::DOMString;
use crate::dom::globalscope::GlobalScope;

/// One argument or result of a hot function, as the generated C wrappers read and write it.
#[repr(C)]
#[derive(Clone, Copy)]
pub union HotArg {
    pub i: i32,
    pub u: u32,
    pub d: f64,
    pub b: bool,
    pub p: *mut c_void,
}

/// A generated wrapper: reads its arguments from `args`, writes its result (if any) to `ret`.
pub type HotWrapper = unsafe extern "C" fn(args: *const HotArg, ret: *mut HotArg);

/// One registered hot function.
#[derive(Clone, Copy)]
pub struct HotFunction {
    /// `"<ret>:<args>"`, e.g. `"v:piiii"` for `void sand_stepAll(uint8_t*, i32, i32, i32, i32)`.
    pub signature: &'static str,
    pub wrapper: HotWrapper,
}

static REGISTRY: OnceLock<HashMap<String, HotFunction>> = OnceLock::new();
static JS_THREAD_HOOK: OnceLock<fn()> = OnceLock::new();

/// Partii console sim: `hook` runs on every thread that is about to create a JS runtime (the script
/// thread, dedicated/shared/service workers, worklets), before the runtime exists. The embedder uses it
/// to lock JS threads out of executable memory (seccomp) while the GPU driver's threads stay free.
/// Set once, before any page loads; later calls are ignored (returns false).
pub fn set_js_thread_hook(hook: fn()) -> bool {
    JS_THREAD_HOOK.set(hook).is_ok()
}

pub(crate) fn run_js_thread_hook() {
    if let Some(hook) = JS_THREAD_HOOK.get() {
        hook();
    }
}

/// Registers the embedder's hot functions, by name (`"<module>.<function>"`). Call once, before any
/// page runs; later calls are ignored (returns false).
pub fn register_hot_functions(functions: Vec<(String, HotFunction)>) -> bool {
    REGISTRY.set(functions.into_iter().collect()).is_ok()
}

fn lookup(name: &str) -> Option<&'static HotFunction> {
    REGISTRY.get()?.get(name)
}

fn type_error(msg: String) -> Error {
    Error::Type(CString::new(msg).unwrap_or_default())
}

/// ECMAScript ToInt32 of a number (what `x | 0` gives in the dev JS).
fn to_int32(d: f64) -> i32 {
    if !d.is_finite() {
        return 0;
    }
    (d.trunc().rem_euclid(4294967296.0) as u64 as u32) as i32
}

#[dom_struct]
pub(crate) struct PartiiHot {
    reflector_: Reflector,
}

impl PartiiHotMethods<crate::DomTypeHolder> for PartiiHot {
    fn Has(_global: &GlobalScope, name: DOMString) -> bool {
        lookup(&name.str()).is_some()
    }

    #[expect(unsafe_code)]
    fn Call(_global: &GlobalScope, name: DOMString, args: Vec<HandleValue>) -> Fallible<f64> {
        let name = String::from(&*name.str());
        let Some(f) = lookup(&name) else {
            return Err(type_error(format!("PartiiHot: no hot function {name}")));
        };
        let (ret_kind, params) = f.signature.split_once(':').unwrap_or(("v", ""));
        if params.len() != args.len() {
            return Err(type_error(format!(
                "PartiiHot: {name} takes {} arguments, got {}",
                params.len(),
                args.len()
            )));
        }
        let mut native = [HotArg { d: 0.0 }; 16];
        if params.len() > native.len() {
            return Err(type_error(format!("PartiiHot: {name} has too many parameters")));
        }
        for (k, (kind, arg)) in params.bytes().zip(args.iter()).enumerate() {
            let v = arg.get();
            native[k] = match kind {
                b'p' => {
                    if !v.is_object() {
                        return Err(type_error(format!("PartiiHot: {name} argument {k} must be a typed array")));
                    }
                    let obj = v.to_object();
                    // SAFETY: obj is a live object held by the caller's arguments; no JS runs (and so
                    // no GC moves or detaches the buffer) until the native call returns.
                    if !unsafe { JS_IsArrayBufferViewObject(obj) } {
                        return Err(type_error(format!("PartiiHot: {name} argument {k} must be a typed array")));
                    }
                    let (mut len, mut shared, mut data) = (0usize, false, ptr::null_mut::<u8>());
                    unsafe { GetArrayBufferViewLengthAndData(obj, &mut len, &mut shared, &mut data) };
                    HotArg { p: data as *mut c_void }
                },
                b'i' | b'u' => {
                    let d = if v.is_int32() {
                        v.to_int32() as f64
                    } else if v.is_number() {
                        v.to_number()
                    } else if v.is_boolean() {
                        v.to_boolean() as i32 as f64
                    } else {
                        return Err(type_error(format!("PartiiHot: {name} argument {k} must be a number")));
                    };
                    HotArg { i: to_int32(d) }
                },
                b'd' => {
                    if !v.is_number() {
                        return Err(type_error(format!("PartiiHot: {name} argument {k} must be a number")));
                    }
                    HotArg { d: v.to_number() }
                },
                b'b' => HotArg {
                    b: (v.is_boolean() && v.to_boolean()) || (v.is_number() && v.to_number() != 0.0),
                },
                _ => return Err(type_error(format!("PartiiHot: {name} has a bad signature {}", f.signature))),
            };
        }
        let mut ret = HotArg { d: 0.0 };
        // SAFETY: the wrapper was generated with the signature it is registered under, and reads
        // exactly params.len() arguments.
        unsafe { (f.wrapper)(native.as_ptr(), &mut ret) };
        Ok(unsafe {
            match ret_kind {
                "i" => ret.i as f64,
                "u" => ret.u as f64,
                "d" => ret.d,
                "b" => ret.b as i32 as f64,
                _ => f64::NAN,
            }
        })
    }
}
