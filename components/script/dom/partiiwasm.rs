/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii: the `PartiiWasm` namespace (PartiiWasm.webidl, pref `dom_partii_hot_enabled`). With the JIT
//! off SpiderMonkey has no WebAssembly, so a console export compiles the game's .wasm to C with wasm2c
//! and links it into the runtime; the embedder registers each module here (a C table generated next to
//! the wasm2c output by partii-runtime tools/nj/wasm-glue.mjs), and the runtime's `WebAssembly` stand-in
//! instantiates it when a page hands over the module's bytes.
//!
//! Calls go through the module's generated C: `call(inst, export, args, ret)` runs one export inside
//! wasm2c's trap handler and returns 0 or a trap code; the module's imports call back into
//! `partii_wasm_import`, which calls the page's JS function for that import. Imports only run while an
//! export runs, so the page passes its import functions with every call and nothing JS is kept here.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::{self, NonNull};
use std::sync::OnceLock;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::jsapi::{
    GetArrayBufferLengthAndData, GetArrayBufferViewLengthAndData, HandleValueArray,
    IsArrayBufferObject, JS_IsArrayBufferViewObject, JSContext as RawJSContext, JSObject, Value,
};
use js::jsval::{DoubleValue, Int32Value, ObjectValue, UndefinedValue};
use js::rust::wrappers2::{
    Call, DetachArrayBuffer, JS_GetElement, JS_IsExceptionPending,
    NewArrayBufferWithUserOwnedContents,
};
use js::rust::{HandleValue, IntoHandle, MutableHandleValue};
use script_bindings::reflector::Reflector;

use crate::dom::bindings::codegen::Bindings::PartiiWasmBinding::PartiiWasmMethods;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::str::DOMString;
use crate::dom::globalscope::GlobalScope;
use crate::dom::partiihot::HotArg;

/// A module's C table (tools/nj/wasm-glue.mjs writes one per module; the embedder passes pointers).
#[repr(C)]
pub struct WasmModuleTable {
    pub name: *const c_char,
    /// FNV-1a 64 of the original .wasm bytes, and their length: how a page's bytes are recognised.
    pub hash: u64,
    pub byte_len: u64,
    pub n_imports: u32,
    /// "module\tname\tsignature" per import.
    pub imports: *const *const c_char,
    pub n_exports: u32,
    /// "name\tsignature" per export ("m" for a memory).
    pub exports: *const *const c_char,
    /// A new instance (wasm2c instantiate, with this id handed to the imports), or null.
    pub instantiate: unsafe extern "C" fn(id: u32) -> *mut c_void,
    /// Runs export `index`: 0, or a wasm_rt trap code (an import that threw is WASM_RT_TRAP_UNREACHABLE
    /// with `import_failed` set by partii_wasm_import).
    pub call: unsafe extern "C" fn(
        inst: *mut c_void,
        index: u32,
        args: *const HotArg,
        ret: *mut HotArg,
    ) -> i32,
    /// Memory export `index`: its bytes and size.
    pub memory:
        unsafe extern "C" fn(inst: *mut c_void, index: u32, data: *mut *mut u8, size: *mut u64),
    /// Hands the module the function its imports call (`partii_wasm_import`).
    pub bind: unsafe extern "C" fn(import: ImportFn),
}

/// The callback a module's import wrappers call.
pub type ImportFn =
    unsafe extern "C" fn(id: u32, index: u32, args: *const HotArg, ret: *mut HotArg) -> bool;

// SAFETY: the tables are static, read-only data in the embedder's binary.
#[expect(unsafe_code)]
unsafe impl Send for WasmModuleTable {}
#[expect(unsafe_code)]
unsafe impl Sync for WasmModuleTable {}

static MODULES: OnceLock<HashMap<String, &'static WasmModuleTable>> = OnceLock::new();

/// Registers the embedder's AOT wasm modules. Call once, before any page runs; later calls are ignored.
#[expect(unsafe_code)]
pub fn register_wasm_modules(tables: Vec<&'static WasmModuleTable>) -> bool {
    if MODULES.get().is_some() {
        return false;
    }
    for t in &tables {
        // SAFETY: the generated bind only stores the pointer.
        unsafe { (t.bind)(partii_wasm_import) };
    }
    let map = tables
        .into_iter()
        // SAFETY: each name is a NUL-terminated static string.
        .map(|t| {
            (
                unsafe { CStr::from_ptr(t.name) }
                    .to_string_lossy()
                    .into_owned(),
                t,
            )
        })
        .collect();
    MODULES.set(map).is_ok()
}

static WORKLET_PRELUDE: OnceLock<String> = OnceLock::new();

/// Script the embedder runs in every AudioWorkletGlobalScope and WorkerGlobalScope (dedicated, shared)
/// before their scripts (console mode: the `WebAssembly` stand-in over PartiiWasm). Call once, before any
/// page runs; later calls are ignored.
pub fn set_worklet_prelude(js: String) -> bool {
    WORKLET_PRELUDE.set(js).is_ok()
}

/// The worklet prelude, when the embedder set one and `dom_partii_hot_enabled` is on.
pub(crate) fn worklet_prelude() -> Option<&'static str> {
    if !servo_config::pref!(dom_partii_hot_enabled) {
        return None;
    }
    WORKLET_PRELUDE.get().map(String::as_str)
}

fn module(name: &str) -> Option<&'static WasmModuleTable> {
    MODULES.get()?.get(name).copied()
}

#[expect(unsafe_code)]
fn strings(p: *const *const c_char, n: u32) -> Vec<String> {
    // SAFETY: the table holds n NUL-terminated static strings.
    (0..n as usize)
        .map(|k| {
            unsafe { CStr::from_ptr(*p.add(k)) }
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn signature(entry: &str) -> &str {
    entry.rsplit('\t').next().unwrap_or("")
}

struct Instance {
    table: &'static WasmModuleTable,
    ptr: *mut c_void,
}

/// The page's view of one export call in progress: the context and the import functions (a rooted
/// value: the caller's argument). A stack, since an import may call back into the module.
struct Frame {
    cx: *mut RawJSContext,
    imports: *const Value,
    import_failed: bool,
}

thread_local! {
    static INSTANCES: RefCell<Vec<Instance>> = const { RefCell::new(Vec::new()) };
    static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

fn type_error(msg: String) -> Error {
    Error::Type(CString::new(msg).unwrap_or_default())
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The bytes of an ArrayBuffer or ArrayBuffer view (copied: the caller may run JS afterwards).
#[expect(unsafe_code)]
fn buffer_bytes(v: HandleValue) -> Option<Vec<u8>> {
    if !v.is_object() {
        return None;
    }
    let obj = v.to_object();
    let (mut len, mut shared, mut data) = (0usize, false, ptr::null_mut::<u8>());
    // SAFETY: obj is live (held by the caller's arguments); no JS runs while we read.
    unsafe {
        if JS_IsArrayBufferViewObject(obj) {
            GetArrayBufferViewLengthAndData(obj, &mut len, &mut shared, &mut data);
        } else if IsArrayBufferObject(obj) {
            GetArrayBufferLengthAndData(obj, &mut len, &mut shared, &mut data);
        } else {
            return None;
        }
        if data.is_null() {
            return Some(Vec::new());
        }
        Some(std::slice::from_raw_parts(data, len).to_vec())
    }
}

fn to_int32(d: f64) -> i32 {
    if !d.is_finite() {
        return 0;
    }
    (d.trunc().rem_euclid(4294967296.0) as u64 as u32) as i32
}

fn number(v: &Value) -> f64 {
    if v.is_int32() {
        v.to_int32() as f64
    } else if v.is_number() {
        v.to_number()
    } else if v.is_boolean() {
        v.to_boolean() as i32 as f64
    } else {
        f64::NAN
    }
}

fn to_arg(kind: u8, v: &Value) -> HotArg {
    let d = number(v);
    match kind {
        b'i' => HotArg { i: to_int32(d) },
        b'f' => HotArg {
            d: (d as f32) as f64,
        },
        _ => HotArg { d },
    }
}

/// Called by a module's generated import wrappers: runs import `index` of instance `id` with `args`
/// (the import's signature says how many and what), writes its result to `ret`. Returns false if the
/// JS threw (the exception stays pending; the wrapper then traps out of the module).
#[expect(unsafe_code)]
unsafe extern "C" fn partii_wasm_import(
    id: u32,
    index: u32,
    args: *const HotArg,
    ret: *mut HotArg,
) -> bool {
    let Some((cx_ptr, imports)) = FRAMES.with(|f| f.borrow().last().map(|f| (f.cx, f.imports)))
    else {
        return false;
    };
    let Some(table) = INSTANCES.with(|i| i.borrow().get(id as usize).map(|i| i.table)) else {
        return false;
    };
    let entries = strings(table.imports, table.n_imports);
    let Some(entry) = entries.get(index as usize) else {
        return false;
    };
    let (ret_kind, params) = signature(entry).split_once(':').unwrap_or(("v", ""));
    // SAFETY: we are inside PartiiWasm.call on this thread, whose JSContext is suspended in the C call
    // below it; this wrapper is the only one in use until we return.
    let mut cx = unsafe { JSContext::from_ptr(NonNull::new_unchecked(cx_ptr)) };
    let cx = &mut cx;
    // SAFETY: `imports` points at the caller's rooted argument for the whole call.
    let imports = unsafe { HandleValue::from_marked_location(imports) };
    if !imports.is_object() {
        return false;
    }
    rooted!(&in(cx) let obj = imports.to_object());
    rooted!(&in(cx) let mut f = UndefinedValue());
    if !unsafe { JS_GetElement(cx, obj.handle(), index, f.handle_mut()) } {
        return false;
    }
    rooted_vec!(let mut argv);
    for (k, kind) in params.bytes().enumerate() {
        // SAFETY: the wrapper passes exactly params.len() arguments.
        let a = unsafe { *args.add(k) };
        argv.push(unsafe {
            match kind {
                b'i' => Int32Value(a.i),
                _ => DoubleValue(a.d),
            }
        });
    }
    let argv = HandleValueArray::from(&argv);
    rooted!(&in(cx) let mut rval = UndefinedValue());
    if !unsafe {
        Call(
            cx,
            HandleValue::undefined(),
            f.handle(),
            &argv,
            rval.handle_mut(),
        )
    } {
        FRAMES.with(|f| {
            if let Some(top) = f.borrow_mut().last_mut() {
                top.import_failed = true;
            }
        });
        return false;
    }
    if !ret.is_null() && ret_kind != "v" {
        // SAFETY: ret points at the wrapper's result slot.
        unsafe { *ret = to_arg(ret_kind.as_bytes()[0], &rval.get()) };
    }
    true
}

const TRAPS: [&str; 12] = [
    "no trap",
    "out of bounds memory access",
    "integer overflow",
    "integer divide by zero",
    "invalid conversion to integer",
    "unreachable executed",
    "indirect call to null",
    "indirect call signature mismatch",
    "uncaught exception",
    "unaligned atomic",
    "call stack exhausted",
    "trap",
];

#[dom_struct]
pub(crate) struct PartiiWasm {
    reflector_: Reflector,
}

impl PartiiWasmMethods<crate::DomTypeHolder> for PartiiWasm {
    fn Find(_global: &GlobalScope, bytes: HandleValue) -> DOMString {
        let Some(b) = buffer_bytes(bytes) else {
            return DOMString::new();
        };
        let h = fnv1a64(&b);
        let found = MODULES.get().and_then(|m| {
            m.iter()
                .find(|(_, t)| t.hash == h && t.byte_len == b.len() as u64)
                .map(|(n, _)| n.clone())
        });
        DOMString::from(found.unwrap_or_default())
    }

    fn Imports(_global: &GlobalScope, name: DOMString) -> Vec<DOMString> {
        module(&name.str())
            .map(|t| {
                strings(t.imports, t.n_imports)
                    .into_iter()
                    .map(DOMString::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn Exports(_global: &GlobalScope, name: DOMString) -> Vec<DOMString> {
        module(&name.str())
            .map(|t| {
                strings(t.exports, t.n_exports)
                    .into_iter()
                    .map(DOMString::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[expect(unsafe_code)]
    fn Instantiate(
        cx: &mut JSContext,
        _global: &GlobalScope,
        name: DOMString,
        imports: HandleValue,
    ) -> Fallible<u32> {
        let name = String::from(&*name.str());
        let Some(table) = module(&name) else {
            return Err(type_error(format!("PartiiWasm: no module {name}")));
        };
        let id = INSTANCES.with(|i| i.borrow().len()) as u32;
        // The start function (if any) may call imports.
        FRAMES.with(|f| {
            f.borrow_mut().push(Frame {
                cx: unsafe { cx.raw_cx() },
                imports: imports.into_handle().ptr,
                import_failed: false,
            })
        });
        // SAFETY: the generated instantiate takes the id it hands to its import wrappers.
        let ptr = unsafe { (table.instantiate)(id) };
        let failed = FRAMES
            .with(|f| f.borrow_mut().pop())
            .is_some_and(|f| f.import_failed);
        if failed {
            return Err(Error::JSFailed);
        }
        if ptr.is_null() {
            return Err(type_error(format!(
                "PartiiWasm: {name} failed to instantiate"
            )));
        }
        INSTANCES.with(|i| i.borrow_mut().push(Instance { table, ptr }));
        Ok(id)
    }

    #[expect(unsafe_code)]
    fn Call(
        cx: &mut JSContext,
        _global: &GlobalScope,
        instance: u32,
        export_index: u32,
        imports: HandleValue,
        args: Vec<HandleValue>,
    ) -> Fallible<f64> {
        let Some((table, ptr)) =
            INSTANCES.with(|i| i.borrow().get(instance as usize).map(|i| (i.table, i.ptr)))
        else {
            return Err(type_error(format!("PartiiWasm: no instance {instance}")));
        };
        let exports = strings(table.exports, table.n_exports);
        let Some(entry) = exports.get(export_index as usize) else {
            return Err(type_error(format!("PartiiWasm: no export {export_index}")));
        };
        let (ret_kind, params) = signature(entry).split_once(':').unwrap_or(("v", ""));
        let mut native = [HotArg { d: 0.0 }; 32];
        if params.len() > native.len() {
            return Err(type_error(format!(
                "PartiiWasm: {entry} has too many parameters"
            )));
        }
        for (k, kind) in params.bytes().enumerate() {
            let v = args.get(k).map(|a| a.get()).unwrap_or(UndefinedValue());
            native[k] = to_arg(kind, &v);
        }
        let mut ret = HotArg { d: 0.0 };
        FRAMES.with(|f| {
            f.borrow_mut().push(Frame {
                cx: unsafe { cx.raw_cx() },
                imports: imports.into_handle().ptr,
                import_failed: false,
            })
        });
        // SAFETY: the generated call reads params.len() arguments for this export.
        let trap = unsafe { (table.call)(ptr, export_index, native.as_ptr(), &mut ret) };
        let import_failed = FRAMES
            .with(|f| f.borrow_mut().pop())
            .is_some_and(|f| f.import_failed);
        if import_failed && unsafe { JS_IsExceptionPending(cx) } {
            return Err(Error::JSFailed);
        }
        if trap != 0 {
            let what = TRAPS.get(trap as usize).copied().unwrap_or("trap");
            return Err(Error::Type(
                CString::new(format!("wasm trap: {what}")).unwrap_or_default(),
            ));
        }
        Ok(unsafe {
            match ret_kind {
                "i" => ret.i as f64,
                "f" | "d" => ret.d,
                _ => f64::NAN,
            }
        })
    }

    #[expect(unsafe_code)]
    fn Memory(
        cx: &mut JSContext,
        _global: &GlobalScope,
        instance: u32,
        export_index: u32,
        previous: HandleValue,
        mut rval: MutableHandleValue,
    ) -> Fallible<()> {
        let Some((table, ptr)) =
            INSTANCES.with(|i| i.borrow().get(instance as usize).map(|i| (i.table, i.ptr)))
        else {
            return Err(type_error(format!("PartiiWasm: no instance {instance}")));
        };
        let (mut data, mut size) = (ptr::null_mut::<u8>(), 0u64);
        // SAFETY: the generated accessor writes the memory's current bytes and size.
        unsafe { (table.memory)(ptr, export_index, &mut data, &mut size) };
        if previous.is_object() {
            let obj = previous.to_object();
            let (mut len, mut shared, mut pdata) = (0usize, false, ptr::null_mut::<u8>());
            // SAFETY: obj is live (the caller's argument).
            if unsafe { IsArrayBufferObject(obj) } {
                unsafe { GetArrayBufferLengthAndData(obj, &mut len, &mut shared, &mut pdata) };
                if pdata == data && len as u64 == size {
                    rval.set(ObjectValue(obj));
                    return Ok(());
                }
                // The memory grew or moved: the old buffer must not keep pointing at freed bytes.
                rooted!(&in(cx) let old = obj);
                unsafe { DetachArrayBuffer(cx, old.handle()) };
            }
        }
        // SAFETY: the bytes are the instance's memory, which lives as long as the instance (instances are
        // never freed); a grow detaches this buffer (above) before anyone reads moved memory through it.
        let ab: *mut JSObject =
            unsafe { NewArrayBufferWithUserOwnedContents(cx, size as usize, data as *mut c_void) };
        if ab.is_null() {
            return Err(Error::JSFailed);
        }
        rval.set(ObjectValue(ab));
        Ok(())
    }
}
