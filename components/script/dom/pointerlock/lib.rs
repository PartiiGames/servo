/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pointer Lock 2.0 <https://w3c.github.io/pointerlock/>.
//!
//! A document holds the lock on one element. Getting it is a round trip to the embedder
//! (`EmbedderMsg::RequestPointerLock`), which grabs and hides the cursor and answers
//! through a callback, so the script thread never blocks. While the lock is held, mouse
//! events go to the lock target with frozen coordinates and `movementX/Y` deltas (see
//! `DocumentEventHandler`). The lock ends with `exitPointerLock()`, when the target leaves
//! the document, when the document unloads, on Esc, or when the embedder releases it
//! (`WebView::exit_pointer_lock`); each of those tells the embedder with
//! `EmbedderMsg::ReleasePointerLock`.

use embedder_traits::{AllowOrDeny, EmbedderMsg, PointerLockOptions as EmbedderPointerLockOptions};
use js::context::JSContext;
use js::realm::CurrentRealm;
use servo_base::generic_channel::GenericCallback;
use style::Atom;

use crate::dom::bindings::codegen::Bindings::ElementBinding::PointerLockOptions;
use crate::dom::bindings::error::Error;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::{Trusted, TrustedPromise};
use crate::dom::bindings::root::DomRoot;
use crate::dom::document::document::Document;
use crate::dom::element::Element;
use crate::dom::event::eventtarget::EventTarget;
use crate::dom::node::NodeTraits;
use crate::dom::promise::{Promise, RootedPromise};

impl Document {
    /// The element that holds the pointer lock in this document, if any.
    pub(crate) fn pointer_lock_element(&self) -> Option<DomRoot<Element>> {
        self.pointer_lock_state().0.get()
    }

    /// Whether the page asked for unadjusted (raw) movement with its current lock.
    pub(crate) fn pointer_lock_unadjusted_movement(&self) -> bool {
        self.pointer_lock_state().2.get()
    }

    /// Queue a task to fire `pointerlockchange` or `pointerlockerror` at this document.
    /// Neither event bubbles nor is cancelable.
    fn queue_pointer_lock_event(&self, name: &'static str) {
        let document = Trusted::new(self);
        self.window()
            .as_global_scope()
            .task_manager()
            .user_interaction_task_source()
            .queue(task!(fire_pointer_lock_event: move |cx| {
                document
                    .root()
                    .upcast::<EventTarget>()
                    .fire_event(cx, Atom::from(name));
            }));
    }

    /// Fail a request: fire `pointerlockerror` and reject the promise with `error`.
    fn fail_pointer_lock_request(&self, cx: &mut JSContext, promise: &RootedPromise, error: Error) {
        self.queue_pointer_lock_event("pointerlockerror");
        promise.reject_error(cx, error);
    }

    /// <https://w3c.github.io/pointerlock/#dom-element-requestpointerlock>
    pub(crate) fn request_pointer_lock(
        &self,
        cx: &mut CurrentRealm,
        element: &Element,
        options: &PointerLockOptions,
    ) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);

        // > If this's shadow-including root is not the active document of a browsing context
        // > ... fire pointerlockerror and reject with a "WrongDocumentError" DOMException.
        if !self.is_fully_active()
            || !element.is_connected()
            || *element.owner_document() != *self
            || self.browsing_context().is_none()
        {
            self.fail_pointer_lock_request(
                cx,
                &promise,
                Error::WrongDocument(Some(
                    "The element is not in the active document of a browsing context".into(),
                )),
            );
            return promise;
        }

        // > If this's node document's active sandboxing flag set has the sandboxed pointer
        // > lock browsing context flag set (or the frame may not use pointer lock),
        // > reject with a "SecurityError" DOMException.
        // Document::allowed_to_use_pointer_lock (w7) checks the sandbox flag and the frame's
        // `allow="pointer-lock"` / `sandbox="allow-pointer-lock"`.
        if !self.allowed_to_use_pointer_lock() {
            self.fail_pointer_lock_request(
                cx,
                &promise,
                Error::Security(Some("This frame is not allowed to use pointer lock".into())),
            );
            return promise;
        }

        // > If the request was not started by an engagement gesture (transient activation),
        // > and the document does not already hold the lock, reject with "NotAllowedError".
        let current = self.pointer_lock_state().0.get();
        if current.is_none() && !self.window().has_transient_activation() {
            self.fail_pointer_lock_request(
                cx,
                &promise,
                Error::NotAllowed(Some(
                    "requestPointerLock() needs a user gesture (click or key press)".into(),
                )),
            );
            return promise;
        }

        // A request is already waiting for the embedder.
        if self.pointer_lock_state().1.get() {
            self.fail_pointer_lock_request(
                cx,
                &promise,
                Error::InvalidState(Some("A pointer lock request is already pending".into())),
            );
            return promise;
        }

        // The document already holds the lock: move it to this element (the cursor grab
        // stays as it is). Same element: still a pointerlockchange, as other engines do.
        if current.is_some() {
            self.pointer_lock_state().0.set(Some(element));
            self.pointer_lock_state().2.set(options.unadjustedMovement);
            self.queue_pointer_lock_event("pointerlockchange");
            promise.resolve_native(cx, &());
            return promise;
        }

        // Ask the embedder to grab the cursor; settle the promise from its answer.
        self.pointer_lock_state().1.set(true);
        let unadjusted = options.unadjustedMovement;
        let task_source = self
            .window()
            .as_global_scope()
            .task_manager()
            .user_interaction_task_source()
            .to_sendable();
        let mut pending = Some((
            Trusted::new(self),
            Trusted::new(element),
            TrustedPromise::new(&promise),
        ));
        let callback = GenericCallback::new(move |response| {
            let Some((document, element, promise)) = pending.take() else {
                return;
            };
            let response = response.unwrap_or(AllowOrDeny::Deny);
            task_source.queue(task!(pointer_lock_response: move |cx| {
                let promise = promise.root(cx);
                document.root().handle_pointer_lock_response(
                    cx,
                    &element.root(),
                    &promise,
                    unadjusted,
                    response,
                );
            }));
        })
        .expect("Could not create a pointer lock callback");
        self.send_to_embedder(EmbedderMsg::RequestPointerLock(
            self.webview_id(),
            EmbedderPointerLockOptions {
                unadjusted_movement: unadjusted,
            },
            callback,
        ));
        promise
    }

    /// The embedder answered a pointer lock request.
    fn handle_pointer_lock_response(
        &self,
        cx: &mut JSContext,
        element: &Element,
        promise: &RootedPromise,
        unadjusted: bool,
        response: AllowOrDeny,
    ) {
        self.pointer_lock_state().1.set(false);
        if response == AllowOrDeny::Deny {
            self.fail_pointer_lock_request(
                cx,
                promise,
                Error::NotAllowed(Some("Pointer lock was refused".into())),
            );
            return;
        }

        // The element or document went away while the embedder decided.
        if !self.is_fully_active() || !element.is_connected() || *element.owner_document() != *self
        {
            self.send_to_embedder(EmbedderMsg::ReleasePointerLock(self.webview_id()));
            self.fail_pointer_lock_request(
                cx,
                promise,
                Error::WrongDocument(Some(
                    "The element left the document before the pointer was locked".into(),
                )),
            );
            return;
        }

        self.pointer_lock_state().0.set(Some(element));
        self.pointer_lock_state().2.set(unadjusted);
        self.event_handler().note_pointer_locked();
        self.queue_pointer_lock_event("pointerlockchange");
        promise.resolve_native(cx, &());
    }

    /// <https://w3c.github.io/pointerlock/#dom-document-exitpointerlock>
    ///
    /// A no-op without a lock. Otherwise the lock ends at once, the embedder is told to
    /// release its cursor grab, and `pointerlockchange` follows in a task.
    pub(crate) fn exit_pointer_lock(&self) {
        if self.pointer_lock_state().0.take().is_none() {
            return;
        }
        self.pointer_lock_state().2.set(false);
        self.send_to_embedder(EmbedderMsg::ReleasePointerLock(self.webview_id()));
        self.queue_pointer_lock_event("pointerlockchange");
    }

    /// The embedder released the lock (Esc, focus loss, ...): `WebView::exit_pointer_lock`.
    pub(crate) fn exit_pointer_lock_from_embedder(&self, _cx: &mut JSContext) {
        self.exit_pointer_lock();
    }
}
