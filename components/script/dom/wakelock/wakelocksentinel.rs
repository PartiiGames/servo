/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::reflect_dom_object_with_cx;

use crate::dom::bindings::codegen::Bindings::WakeLockBinding::{
    WakeLockSentinelMethods, WakeLockType,
};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::{Promise, RootedPromise};
use crate::dom::wakelock::WakeLock;

/// <https://w3c.github.io/screen-wake-lock/#the-wakelocksentinel-interface>
#[dom_struct]
pub(crate) struct WakeLockSentinel {
    eventtarget: EventTarget,
    released: Cell<bool>,
    type_: WakeLockType,
    /// The `navigator.wakeLock` that holds this sentinel in its active locks.
    wake_lock: Dom<WakeLock>,
}

impl WakeLockSentinel {
    pub(crate) fn new_inherited(type_: WakeLockType, wake_lock: &WakeLock) -> Self {
        Self {
            eventtarget: EventTarget::new_inherited(),
            released: Cell::new(false),
            type_,
            wake_lock: Dom::from_ref(wake_lock),
        }
    }

    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        global: &GlobalScope,
        type_: WakeLockType,
        wake_lock: &WakeLock,
    ) -> DomRoot<Self> {
        reflect_dom_object_with_cx(Box::new(Self::new_inherited(type_, wake_lock)), global, cx)
    }

    pub(crate) fn set_released(&self) {
        self.released.set(true);
    }
}

impl WakeLockSentinelMethods<crate::DomTypeHolder> for WakeLockSentinel {
    /// <https://w3c.github.io/screen-wake-lock/#dom-wakelocksentinel-released>
    fn Released(&self) -> bool {
        self.released.get()
    }

    /// <https://w3c.github.io/screen-wake-lock/#dom-wakelocksentinel-type>
    fn Type(&self) -> WakeLockType {
        self.type_
    }

    /// <https://w3c.github.io/screen-wake-lock/#the-release-method>
    fn Release(&self, cx: &mut JSContext) -> RootedPromise {
        // Step 1. If this's [[Released]] is false, then run release a wake lock with lock set to
        // this and type set to the value of this's type attribute.
        if !self.released.get() {
            self.wake_lock.release_a_wake_lock(cx, self);
        }
        // Step 2. Return a promise resolved with undefined.
        Promise::new_resolved(cx, &self.global(), ())
    }

    // <https://w3c.github.io/screen-wake-lock/#dom-wakelocksentinel-onrelease>
    event_handler!(release, GetOnrelease, SetOnrelease);
}
