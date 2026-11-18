#!/usr/bin/env python3
# Partii: no traced std RefCell in script (fork commit "traced cells are DomRefCells").
#
# SpiderMonkey's parallel marking (on in the gc-fast defaults) runs trace hooks on GC helper threads, several at once,
# and Partii's remark traces again. A std RefCell's trace (mozjs's `impl Traceable for RefCell<T>`) calls borrow(): a
# non-atomic write to the borrow flag. Two threads tracing the same cell lose an update, the flag ends at -1 and the
# next borrow panics inside the GC (abort): rocket-to-heaven soaks f2650144 and 9247e7b4. DomRefCell's trace only reads
# the flag (borrow_for_gc_trace). So every traced cell must be a DomRefCell.
#
# This finds fields and tuple fields of JSTraceable structs and enums (#[derive(.. JSTraceable ..)] or #[dom_struct])
# whose type names std `RefCell<`, unless the field carries #[no_trace] or #[custom_trace]. It also flags a
# `.borrow()` / `.borrow_mut()` inside a hand-written `fn trace`. Run by the runtime's farm builds before cargo
# (harness/farm/gate.sh, tauri-runtime/tools/lx/export-job.sh); exit 1 lists each offender.
#   python3 python/partii/check_traced_refcell.py [components dir]
import os
import re
import sys

ROOT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "components")
DIRS = ("script", "script_bindings")
STD = re.compile(r"(?<![A-Za-z_])RefCell<")
ITEM = re.compile(r"\s*(pub(\([^)]*\))?\s+)?(struct|enum)\s+\w+")
TRACEABLE = re.compile(r"\s*#\[(dom_struct|derive\([^)]*\bJSTraceable\b)")
# Hand-written trace hooks that legitimately borrow on the script thread only (root tracing, sweeping), reviewed:
ALLOW_TRACE_BORROW = {
    ("script/dom/bindings/settings_stack.rs", "stack.borrow()"),  # a thread-local root, traced by its own thread
    ("script/dom/bindings/weakref.rs", "dom_objects.borrow_mut()"),  # sweep hook, not a trace
    ("script/dom/range/range.rs", "cell.borrow_mut()"),  # sweep hook, not a trace
}


def check_file(path, rel, out):
    lines = open(path, encoding="utf-8").read().split("\n")
    i = 0
    while i < len(lines):
        if not TRACEABLE.match(lines[i]):
            i += 1
            continue
        j = i
        while j < len(lines) and not ITEM.match(lines[j]):
            j += 1
        if j >= len(lines):
            break
        depth, k, attrs = 0, j, []
        while k < len(lines):
            t = lines[k]
            s = t.strip()
            if k > j and s.startswith("#["):
                attrs.append(s)
            elif k > j and s and not s.startswith("//"):
                code = s.split("//")[0]
                if STD.search(code) and not any("no_trace" in a or "custom_trace" in a for a in attrs):
                    out.append("%s:%d: traced std RefCell (make it a DomRefCell, or #[no_trace]): %s" % (rel, k + 1, s))
                if not code.rstrip().endswith(":"):  # a field whose type is on the next line keeps its attributes
                    attrs = []
            depth += t.count("{") + t.count("(") - t.count("}") - t.count(")")
            if k > j and depth <= 0 and (s.endswith("}") or s.endswith(");") or s.endswith("}"+";")):
                break
            if k == j and s.endswith(";"):
                break
            k += 1
        i = k + 1
    src = "\n".join(lines)
    for m in re.finditer(r"fn trace\s*(<[^>]*>)?\s*\([^)]*\)[^{;]*\{", src):
        a, d, b = m.end() - 1, 0, m.end() - 1
        while b < len(src):
            d += {"{": 1, "}": -1}.get(src[b], 0)
            if d == 0:
                break
            b += 1
        for c in re.finditer(r"([\w.]*)\.(borrow|borrow_mut|try_borrow|try_borrow_mut)\(\)", src[a:b]):
            what = c.group(0).split(".", 1)[-1] if c.group(1).startswith("self") else c.group(0)
            if any(rel == f and tok in c.group(0) for f, tok in ALLOW_TRACE_BORROW):
                continue
            ln = src[: a + c.start()].count("\n") + 1
            out.append("%s:%d: borrow in a trace hook (use borrow_for_gc_trace): %s" % (rel, ln, lines[ln - 1].strip()))


def main():
    out = []
    for d in DIRS:
        for dp, _, fs in os.walk(os.path.join(ROOT, d)):
            for f in sorted(fs):
                if f.endswith(".rs"):
                    p = os.path.join(dp, f)
                    check_file(p, os.path.relpath(p, ROOT), out)
    for o in out:
        print(o)
    print("check_traced_refcell: %d offender(s)" % len(out), file=sys.stderr)
    return 1 if out else 0


if __name__ == "__main__":
    sys.exit(main())
