/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::reflector::reflect_dom_object_with_proto;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::BlobEventBinding;
use crate::dom::bindings::codegen::Bindings::BlobEventBinding::BlobEventMethods;
use crate::dom::bindings::codegen::Bindings::EventBinding::Event_Binding::EventMethods;
use crate::dom::bindings::error::Fallible;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::blob::Blob;
use crate::dom::event::Event;
use crate::dom::globalscope::GlobalScope;
use crate::dom::window::Window;

/// <https://w3c.github.io/mediacapture-record/#blobevent-section>
#[dom_struct]
pub(crate) struct BlobEvent {
    event: Event,
    data: Dom<Blob>,
    timecode: f64,
}

impl BlobEvent {
    fn new_inherited(data: &Blob, timecode: f64) -> BlobEvent {
        BlobEvent {
            event: Event::new_inherited(),
            data: Dom::from_ref(data),
            timecode,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        type_: Atom,
        data: &Blob,
        timecode: f64,
    ) -> DomRoot<BlobEvent> {
        Self::new_with_proto(cx, global, None, type_, false, false, data, timecode)
    }

    #[expect(clippy::too_many_arguments)]
    fn new_with_proto(
        cx: &mut JSContext,
        global: &GlobalScope,
        proto: Option<HandleObject>,
        type_: Atom,
        bubbles: bool,
        cancelable: bool,
        data: &Blob,
        timecode: f64,
    ) -> DomRoot<BlobEvent> {
        let event = reflect_dom_object_with_proto(
            cx,
            Box::new(BlobEvent::new_inherited(data, timecode)),
            global,
            proto,
        );
        event.upcast::<Event>().init_event(type_, bubbles, cancelable);
        event
    }
}

impl BlobEventMethods<crate::DomTypeHolder> for BlobEvent {
    /// <https://w3c.github.io/mediacapture-record/#dom-blobevent-blobevent>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        type_: DOMString,
        init: &BlobEventBinding::BlobEventInit,
    ) -> Fallible<DomRoot<BlobEvent>> {
        Ok(BlobEvent::new_with_proto(
            cx,
            window.upcast(),
            proto,
            Atom::from(type_),
            init.parent.bubbles,
            init.parent.cancelable,
            &init.data,
            init.timecode.map_or(0.0, |t| *t),
        ))
    }

    /// <https://w3c.github.io/mediacapture-record/#dom-blobevent-data>
    fn Data(&self) -> DomRoot<Blob> {
        DomRoot::from_ref(&*self.data)
    }

    /// <https://w3c.github.io/mediacapture-record/#dom-blobevent-timecode>
    fn Timecode(&self) -> Finite<f64> {
        Finite::wrap(self.timecode)
    }

    /// <https://dom.spec.whatwg.org/#dom-event-istrusted>
    fn IsTrusted(&self) -> bool {
        self.event.IsTrusted()
    }
}
