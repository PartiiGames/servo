/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// Partii (not a web standard): hot code compiled ahead of time. A Partii game's Hot TS modules
// (*.hot.ts) become C on export for targets without a JIT (consoles); the embedder links that code and
// registers each exported function (script::partii_hot). The export's JS shim calls them here, so the
// game's code doesn't change. Typed arrays (shared or not) are passed as pointers to their bytes,
// numbers as the function's i32/u32/f64/bool parameters; the result comes back as a number.
[Exposed=(Window,Worker), Pref="dom_partii_hot_enabled"]
namespace PartiiHot {
  boolean has(DOMString name);
  [Throws] unrestricted double call(DOMString name, any... args);
};
