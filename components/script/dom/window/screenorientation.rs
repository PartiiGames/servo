/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `screen.orientation` for a desktop runtime: <https://w3c.github.io/screen-orientation/>.
//!
//! Desktop windows have no orientation sensor and can't be rotated by a page, so the
//! orientation is read from the window's aspect: a window at least as wide as it is tall is
//! `landscape-primary` (angle 0, the natural orientation), a taller one `portrait-primary`
//! (angle 90, as on a landscape-natural device). Resizing across that line fires `change`.
//! `lock()` resolves without doing anything (games call it and expect it to succeed) and
//! `unlock()` is a no-op.

use std::cell::Cell;

use content_security_policy::sandboxing_directive::SandboxingFlagSet;
use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::reflector::reflect_dom_object_with_cx;
use style::Atom;

use crate::dom::bindings::codegen::Bindings::DocumentBinding::{
    DocumentMethods, DocumentVisibilityState,
};
use crate::dom::bindings::codegen::Bindings::ScreenOrientationBinding::{
    OrientationLockType, OrientationType, ScreenOrientationMethods,
};
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::error::Error;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::eventtarget::EventTarget;
use crate::dom::promise::{Promise, RootedPromise};
use crate::dom::window::Window;

#[dom_struct]
pub(crate) struct ScreenOrientation {
    eventtarget: EventTarget,
    window: Dom<Window>,
    /// The type last reported, to fire `change` only when it changes.
    #[no_trace]
    last_type: Cell<Option<OrientationTypeKey>>,
}

/// `OrientationType` is a generated enum; this mirror is `Copy` + `PartialEq` for the cell.
#[derive(Clone, Copy, Debug, MallocSizeOf, PartialEq)]
enum OrientationTypeKey {
    Landscape,
    Portrait,
}

impl ScreenOrientation {
    fn new_inherited(window: &Window) -> ScreenOrientation {
        ScreenOrientation {
            eventtarget: EventTarget::new_inherited(),
            window: Dom::from_ref(window),
            last_type: Cell::new(None),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, window: &Window) -> DomRoot<ScreenOrientation> {
        let orientation = reflect_dom_object_with_cx(
            Box::new(ScreenOrientation::new_inherited(window)),
            window,
            cx,
        );
        orientation.last_type.set(Some(orientation.current()));
        orientation
    }

    /// The orientation from the window's aspect.
    fn current(&self) -> OrientationTypeKey {
        let (width, height) = (self.window.InnerWidth(), self.window.InnerHeight());
        if height > width {
            OrientationTypeKey::Portrait
        } else {
            OrientationTypeKey::Landscape
        }
    }

    /// <https://w3c.github.io/screen-orientation/#dfn-screen-orientation-change-steps>:
    /// the viewport changed; fire `change` if that crossed between landscape and portrait.
    pub(crate) fn viewport_changed(&self, cx: &mut JSContext) {
        let current = self.current();
        if self.last_type.replace(Some(current)) == Some(current) {
            return;
        }
        self.upcast::<EventTarget>()
            .fire_event(cx, Atom::from("change"));
    }
}

impl ScreenOrientationMethods<crate::DomTypeHolder> for ScreenOrientation {
    /// <https://w3c.github.io/screen-orientation/#dom-screenorientation-lock>
    fn Lock(&self, cx: &mut CurrentRealm, _orientation: OrientationLockType) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        let document = self.window.Document();

        // > If document is not fully active, return a promise rejected with an
        // > "InvalidStateError" DOMException.
        if !document.is_fully_active() {
            promise.reject_error(
                cx,
                Error::InvalidState(Some("The document is not fully active".into())),
            );
            return promise;
        }
        // > If document has the sandboxed orientation lock browsing context flag set, or
        // > its visibility state is "hidden", return a promise rejected with "SecurityError".
        if document.has_active_sandboxing_flag(
            SandboxingFlagSet::SANDBOXED_ORIENTATION_LOCK_BROWSING_CONTEXT_FLAG,
        ) || document.VisibilityState() == DocumentVisibilityState::Hidden
        {
            promise.reject_error(
                cx,
                Error::Security(Some("This document may not lock the orientation".into())),
            );
            return promise;
        }
        // A desktop window can't be rotated: accept the lock and change nothing.
        promise.resolve_native(cx, &());
        promise
    }

    /// <https://w3c.github.io/screen-orientation/#dom-screenorientation-unlock>
    fn Unlock(&self) {}

    /// <https://w3c.github.io/screen-orientation/#dom-screenorientation-type>
    fn Type(&self) -> OrientationType {
        match self.current() {
            OrientationTypeKey::Landscape => OrientationType::Landscape_primary,
            OrientationTypeKey::Portrait => OrientationType::Portrait_primary,
        }
    }

    /// <https://w3c.github.io/screen-orientation/#dom-screenorientation-angle>
    fn Angle(&self) -> u16 {
        match self.current() {
            OrientationTypeKey::Landscape => 0,
            OrientationTypeKey::Portrait => 90,
        }
    }

    // https://w3c.github.io/screen-orientation/#dom-screenorientation-onchange
    event_handler!(change, GetOnchange, SetOnchange);
}
