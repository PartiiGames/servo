/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// Partii console mode: WebAssembly modules compiled ahead of time to C (wasm2c) and linked into the
// runtime, for runtimes with no JIT (where SpiderMonkey has no WebAssembly). The embedder registers
// them; the runtime's `WebAssembly` stand-in (partii-runtime src/wasm-polyfill.js) uses this to
// instantiate a registered module when a page passes it the module's bytes.
[Exposed=(Window,Worker), Pref="dom_partii_hot_enabled"]
namespace PartiiWasm {
  // The registered module whose original .wasm these bytes (an ArrayBuffer or a view) are, or "".
  DOMString find(any bytes);
  // A module's imports as "module\tname\tsignature", in order; its exports as "name\tsignature"
  // (signature "m" for a memory). Signatures: "<ret>:<params>", letters i (i32), f (f32), d (f64), v (void).
  sequence<DOMString> imports(DOMString module);
  sequence<DOMString> exports(DOMString module);
  // A new instance. `imports` is an array with one function per import, in imports() order; it is
  // passed again to every call, since a module calls its imports only while one of its exports runs.
  [Throws] unsigned long instantiate(DOMString module, any imports);
  [Throws] unrestricted double call(unsigned long instance, unsigned long exportIndex, any imports, any... args);
  // The instance's memory export as an ArrayBuffer over the module's own bytes: `previous` is returned
  // while the memory hasn't moved or grown; otherwise it is detached and a new buffer is returned.
  [Throws] any memory(unsigned long instance, unsigned long exportIndex, any previous);
};
