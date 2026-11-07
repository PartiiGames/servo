/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// Partii: the black remark of incremental_gc.rs. A callback tracer that shades every child it is
// handed with SpiderMonkey's incremental pre-barrier. Run from the mutator side (outside any GC
// slice) over the Rust fields of reflectors that are already marked, it pushes whatever they point
// to onto the black mark stack, so a Dom<T> moved into an already-scanned object is marked black
// before marking ends. mozjs has no way to make a JS::CallbackTracer from Rust, hence this file.

#include "jsapi.h"
#include "js/GCAPI.h"
#include "js/HeapAPI.h"
#include "js/TracingAPI.h"

namespace {

class BarrierTracer final : public JS::CallbackTracer {
 public:
  explicit BarrierTracer(JSRuntime* rt)
      : JS::CallbackTracer(rt, JS::TracerKind::Callback,
                           JS::TraceOptions(JS::WeakMapTraceAction::Skip,
                                            JS::WeakEdgeTraceAction::Skip)) {}

  size_t edges = 0;

  bool onChild(JS::GCCellPtr thing, const char* name) override {
    edges++;
    JS::IncrementalPreWriteBarrier(thing);
    return true;
  }
};

}  // namespace

extern "C" {

// A tracer for partii_barrier_tracer_finish to delete. The JSTracer* is what trace hooks take.
JSTracer* partii_barrier_tracer_new(JSContext* cx) {
  return new BarrierTracer(JS_GetRuntime(cx));
}

// Deletes the tracer; returns how many edges it shaded.
size_t partii_barrier_tracer_finish(JSTracer* trc) {
  auto* tracer = static_cast<BarrierTracer*>(static_cast<JS::CallbackTracer*>(trc));
  size_t edges = tracer->edges;
  delete tracer;
  return edges;
}

// sizeof and alignof JSTracer as these headers see it. The Rust side compares them with the
// bindgen layout of the mozjs_sys actually linked, so headers from a different SpiderMonkey
// configuration than the linked library turn the black remark off instead of corrupting memory.
void partii_barrier_tracer_layout(size_t* out) {
  out[0] = sizeof(JSTracer);
  out[1] = alignof(JSTracer);
}

}  // extern "C"
