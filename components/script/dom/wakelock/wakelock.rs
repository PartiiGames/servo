/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use embedder_traits::{AllowOrDeny, EmbedderMsg};
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_constellation_traits::ScriptToConstellationMessage;
use stylo_atoms::Atom;

use crate::conversions::Convert;
use crate::dom::bindings::codegen::Bindings::DocumentBinding::{
    DocumentMethods, DocumentVisibilityState,
};
use crate::dom::bindings::codegen::Bindings::WakeLockBinding::{
    WakeLockMethods, WakeLockSentinelMethods, WakeLockType,
};
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::error::Error;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::{Promise, RootedPromise};
use crate::dom::wakelock::wakelocksentinel::WakeLockSentinel;
use crate::routed_promise::{RoutedPromiseListener, callback_promise};

/// <https://w3c.github.io/screen-wake-lock/#the-wakelock-interface>
#[dom_struct]
pub(crate) struct WakeLock {
    reflector_: Reflector,
    type_: Cell<WakeLockType>,
    /// The document's [[ActiveLocks]] (one type, "screen", so one list).
    /// <https://w3c.github.io/screen-wake-lock/#dfn-activelocks>
    active_locks: DomRefCell<Vec<Dom<WakeLockSentinel>>>,
}

impl WakeLock {
    pub(crate) fn new_inherited() -> Self {
        Self {
            reflector_: Reflector::new(),
            type_: Cell::new(WakeLockType::Screen),
            active_locks: Default::default(),
        }
    }

    pub(crate) fn new(cx: &mut js::context::JSContext, global: &GlobalScope) -> DomRoot<Self> {
        reflect_dom_object_with_cx(Box::new(Self::new_inherited()), global, cx)
    }

    /// <https://w3c.github.io/screen-wake-lock/#dfn-release-a-wake-lock>
    pub(crate) fn release_a_wake_lock(&self, cx: &mut JSContext, lock: &WakeLockSentinel) {
        // Step 1. Let document be the result of calling GetAssociatedDocument. (This WakeLock.)
        // Step 2. If document.[[ActiveLocks]][type] does not contain lock, abort these steps.
        let position = self
            .active_locks
            .borrow()
            .iter()
            .position(|active| &**active == lock);
        let Some(position) = position else {
            return;
        };
        // Step 3. Remove lock from document.[[ActiveLocks]][type].
        self.active_locks.borrow_mut().remove(position);
        // Step 4. If document.[[ActiveLocks]][type] is empty, then run release a wake lock
        // (the platform lock). The constellation counts acquires across all documents and
        // releases the platform lock when the last one goes.
        self.global().as_window().send_to_constellation(
            ScriptToConstellationMessage::ReleaseWakeLock(lock.Type().convert()),
        );
        // Step 5. Set lock's [[Released]] to true.
        lock.set_released();
        // Step 6. Fire an event named "release" at lock.
        lock.upcast::<EventTarget>()
            .fire_event(cx, Atom::from("release"));
    }

    /// <https://w3c.github.io/screen-wake-lock/#handling-document-loss-of-visibility>
    /// Also used when the document is unloaded (its visibility becomes "hidden" then too).
    pub(crate) fn release_all(&self, cx: &mut JSContext) {
        let locks: Vec<DomRoot<WakeLockSentinel>> = self
            .active_locks
            .borrow()
            .iter()
            .map(|lock| lock.as_rooted())
            .collect();
        for lock in locks {
            self.release_a_wake_lock(cx, &lock);
        }
    }
}

impl WakeLockMethods<crate::DomTypeHolder> for WakeLock {
    /// <https://w3c.github.io/screen-wake-lock/#the-request-method>
    fn Request(&self, cx: &mut CurrentRealm, type_: WakeLockType) -> RootedPromise {
        let global = GlobalScope::from_current_realm(cx);
        let promise = Promise::new_in_realm(cx);

        // Step 1. Let document be this's relevant global object's associated Document.
        let document = global.as_window().Document();

        // Step 2. If document is not fully active, reject with NotAllowedError.
        if !document.is_fully_active() {
            promise.reject_error(cx, Error::NotAllowed(Some(
                        "Failed to execute 'request' on 'WakeLock': The requesting page is not fully active."
                        .to_string())));
            return promise;
        }

        // Step 3. If document's visibility state is "hidden", reject with NotAllowedError.
        if document.VisibilityState() == DocumentVisibilityState::Hidden {
            promise.reject_error(cx, Error::NotAllowed(Some(
                        "Failed to execute 'request' on 'WakeLock': The requesting page is not visible."
                        .to_string()
                        )));
            return promise;
        }

        // Step 4. Obtain permission for "screen-wake-lock".
        // <https://w3c.github.io/screen-wake-lock/#dfn-obtain-permission>
        let Some(webview_id) = global.webview_id() else {
            promise.reject_error(
                cx,
                Error::NotAllowed(Some("Unable to obtain WakeLock permission.".to_string())),
            );
            return promise;
        };

        self.type_.set(type_);
        let task_manager = global.task_manager();
        let task_source = task_manager.dom_manipulation_task_source();
        let callback = callback_promise(&promise, self, task_source);
        global.send_to_embedder(EmbedderMsg::RequestWakeLockPermission(
            webview_id,
            callback,
            self.type_.get().convert(),
        ));

        promise
    }
}

impl RoutedPromiseListener<crate::DomTypeHolder, AllowOrDeny> for WakeLock {
    /// <https://w3c.github.io/screen-wake-lock/#the-request-method>
    fn handle_response(&self, cx: &mut JSContext, response: AllowOrDeny, promise: &RootedPromise) {
        match response {
            // Step 7a. If permission is denied, reject with NotAllowedError.
            AllowOrDeny::Deny => {
                promise.reject_error(
                    cx,
                    Error::NotAllowed(Some(
                        "Failed to execute 'request' on 'WakeLock': Permission denied.".to_string(),
                    )),
                );
            },
            // Step 7b-7c. Acquire the lock and resolve with a WakeLockSentinel.
            AllowOrDeny::Allow => {
                let global = self.global();
                // Step 7.3.1. If document's visibility state is "hidden", reject promise with a
                // "NotAllowedError" DOMException.
                if global.as_window().Document().VisibilityState() ==
                    DocumentVisibilityState::Hidden
                {
                    promise.reject_error(
                        cx,
                        Error::NotAllowed(Some(
                            "Failed to execute 'request' on 'WakeLock': The requesting page is not visible."
                                .to_string(),
                        )),
                    );
                    return;
                }
                // Step 7.3.3. Acquire a wake lock (the platform lock).
                global.as_window().send_to_constellation(
                    ScriptToConstellationMessage::AcquireWakeLock(self.type_.get().convert()),
                );

                // Step 7.3.4-5. Let lock be a new WakeLockSentinel; append it to
                // document.[[ActiveLocks]][type]; resolve promise with lock.
                let sentinel = WakeLockSentinel::new(cx, &global, self.type_.get(), self);
                self.active_locks
                    .borrow_mut()
                    .push(Dom::from_ref(&*sentinel));
                promise.resolve_native(cx, &sentinel);
            },
        }
    }
}

impl Convert<embedder_traits::WakeLockType> for WakeLockType {
    fn convert(self) -> embedder_traits::WakeLockType {
        match self {
            WakeLockType::Screen => embedder_traits::WakeLockType::Screen,
        }
    }
}
