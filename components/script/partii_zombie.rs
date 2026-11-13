/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii (mem-2, measurement only): `PARTII_ZOMBIE_DIAG=<secs>` logs every discarded window
//! ("zombie": its pipeline exited, `clear_js_runtime` ran) from that moment until its `Window` is
//! freed, and every `<secs>` seconds, for each one still alive, the DOM objects of that window
//! that a `Trusted` or `TrustedPromise` keeps alive (by JS class; promises by the line that made
//! them under `PARTII_TRUSTED_CENSUS=1`). Lines on stderr:
//!   partii-zombie: cleared <pipeline> live=<n>
//!   partii-zombie: freed <pipeline> after <s> s
//!   partii-zombie: alive <pipeline> age=<s> trusted=<n> [Class=n ...] promises=<n> [site=n ...]

use std::cell::{Cell, RefCell};
use std::ffi::CStr;
use std::time::{Duration, Instant};

use js::rust::get_object_class;
use rustc_hash::FxHashMap;
use script_bindings::reflector::Reflector;

use crate::dom::bindings::refcounted::partii_live_promises;
use crate::dom::globalscope::GlobalScope;

fn every() -> Option<u64> {
    static E: std::sync::LazyLock<Option<u64>> = std::sync::LazyLock::new(|| {
        std::env::var("PARTII_ZOMBIE_DIAG")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|n: &u64| *n > 0)
    });
    *E
}

thread_local! {
    static ZOMBIES: RefCell<FxHashMap<usize, (String, Instant)>> = RefCell::new(FxHashMap::default());
    static NEXT: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// A field of `Window`: when the window is freed, its zombie entry goes (and says how long it lived).
#[derive(Default)]
pub(crate) struct DropNote(Cell<usize>);

impl Drop for DropNote {
    fn drop(&mut self) {
        let key = self.0.get();
        if key == 0 {
            return;
        }
        let _ = ZOMBIES.try_with(|z| {
            if let Some((p, t)) = z.borrow_mut().remove(&key) {
                eprintln!(
                    "partii-zombie: freed {p} after {:.1} s",
                    t.elapsed().as_secs_f64()
                );
            }
        });
    }
}

pub(crate) fn cleared(global: &GlobalScope, note: &DropNote) {
    if every().is_none() {
        return;
    }
    let key = global as *const GlobalScope as usize;
    note.0.set(key);
    let p = format!("{:?}", global.pipeline_id());
    let n = ZOMBIES.with(|z| {
        let mut z = z.borrow_mut();
        z.insert(key, (p.clone(), Instant::now()));
        z.len()
    });
    eprintln!("partii-zombie: cleared {p} live={n}");
}

#[expect(unsafe_code)]
pub(crate) fn maybe_log() {
    let Some(secs) = every() else { return };
    let now = Instant::now();
    if NEXT.with(|n| n.get()).is_some_and(|t| now < t) {
        return;
    }
    NEXT.with(|n| n.set(Some(now + Duration::from_secs(secs))));
    let zombies: FxHashMap<usize, (String, Instant)> = ZOMBIES.with(|z| z.borrow().clone());
    if zombies.is_empty() {
        return;
    }
    let global_key = |obj: *mut js::jsapi::JSObject| -> Option<usize> {
        if obj.is_null() {
            return None;
        }
        // Only a DOM global (a debugger or other non-DOM global would fail GlobalScope's unwrap).
        let flags = unsafe { (*get_object_class(js::jsapi::GetNonCCWObjectGlobal(obj))).flags };
        if flags & js::JSCLASS_IS_DOMJSCLASS == 0 || flags & js::JSCLASS_IS_GLOBAL == 0 {
            return None;
        }
        let g = unsafe { GlobalScope::from_object(obj) };
        Some(&*g as *const GlobalScope as usize)
    };
    let mut trusted: FxHashMap<usize, FxHashMap<String, usize>> = FxHashMap::default();
    for ptr in script_bindings::refcounted::partii_live_trusted() {
        let obj = unsafe { (*(ptr as *const Reflector)).get_jsobject_unbarriered() };
        let Some(k) = global_key(obj) else { continue };
        if !zombies.contains_key(&k) {
            continue;
        }
        let name = unsafe {
            let c = get_object_class(obj);
            if c.is_null() || (*c).name.is_null() {
                "?".to_string()
            } else {
                CStr::from_ptr((*c).name).to_string_lossy().into_owned()
            }
        };
        *trusted.entry(k).or_default().entry(name).or_default() += 1;
    }
    let mut promises: FxHashMap<usize, FxHashMap<String, usize>> = FxHashMap::default();
    for (obj, site) in partii_live_promises() {
        let Some(k) = global_key(obj) else { continue };
        if zombies.contains_key(&k) {
            *promises.entry(k).or_default().entry(site).or_default() += 1;
        }
    }
    let fmt = |m: Option<&FxHashMap<String, usize>>| -> (usize, String) {
        let Some(m) = m else { return (0, String::new()) };
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        (
            m.values().sum(),
            v.iter()
                .take(12)
                .map(|(k, n)| format!("{k}={n}"))
                .collect::<Vec<_>>()
                .join(" "),
        )
    };
    for (k, (p, t)) in &zombies {
        let (nt, st) = fmt(trusted.get(k));
        let (np, sp) = fmt(promises.get(k));
        eprintln!(
            "partii-zombie: alive {p} age={:.0} trusted={nt} [{st}] promises={np} [{sp}]",
            t.elapsed().as_secs_f64()
        );
    }
}
