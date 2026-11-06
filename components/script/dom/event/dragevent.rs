/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use euclid::Point2D;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::reflector::reflect_dom_object_with_proto;
use script_traits::MouseButtons;
use style_traits::CSSPixel;

use crate::dom::bindings::codegen::Bindings::DragEventBinding;
use crate::dom::bindings::codegen::Bindings::DragEventBinding::DragEventMethods;
use crate::dom::bindings::codegen::Bindings::MouseEventBinding::MouseEventMethods;
use crate::dom::bindings::error::Fallible;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::datatransfer::DataTransfer;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::mouseevent::MouseEvent;
use crate::dom::window::Window;

/// Partii: <https://html.spec.whatwg.org/multipage/#the-dragevent-interface>. Script can construct
/// one; the engine does not fire drag-and-drop events yet.
#[dom_struct]
pub(crate) struct DragEvent {
    mouseevent: MouseEvent,
    data_transfer: MutNullableDom<DataTransfer>,
}

impl DragEvent {
    fn new_inherited() -> DragEvent {
        DragEvent {
            mouseevent: MouseEvent::new_inherited(),
            data_transfer: MutNullableDom::new(None),
        }
    }
}

impl DragEventMethods<crate::DomTypeHolder> for DragEvent {
    /// <https://html.spec.whatwg.org/multipage/#dom-dragevent-constructor>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        event_type: DOMString,
        init: &DragEventBinding::DragEventInit,
    ) -> Fallible<DomRoot<DragEvent>> {
        let mouse_init = &init.parent;
        let event_init = &mouse_init.parent.parent.parent;
        let scroll_offset = window.scroll_offset();
        let page_point = Point2D::<f64, CSSPixel>::new(
            scroll_offset.x as f64 + *mouse_init.clientX,
            scroll_offset.y as f64 + *mouse_init.clientY,
        );
        let event =
            reflect_dom_object_with_proto(cx, Box::new(DragEvent::new_inherited()), window, proto);
        event.upcast::<MouseEvent>().initialize_mouse_event(
            event_type.into(),
            EventBubbles::from(event_init.bubbles),
            EventCancelable::from(event_init.cancelable),
            mouse_init.parent.parent.view.as_deref(),
            mouse_init.parent.parent.detail,
            Point2D::new(*mouse_init.screenX, *mouse_init.screenY),
            Point2D::new(*mouse_init.clientX, *mouse_init.clientY),
            page_point,
            mouse_init.parent.modifiers(),
            mouse_init.button.into(),
            MouseButtons::from_bits_retain(mouse_init.buttons),
            mouse_init.relatedTarget.as_deref(),
            None,
        );
        event.upcast::<Event>().set_composed(event_init.composed);
        event
            .upcast::<MouseEvent>()
            .set_movement((*mouse_init.movementX, *mouse_init.movementY));
        event.data_transfer.set(init.dataTransfer.as_deref());
        Ok(event)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-dragevent-datatransfer>
    fn GetDataTransfer(&self) -> Option<DomRoot<DataTransfer>> {
        self.data_transfer.get()
    }

    /// <https://dom.spec.whatwg.org/#dom-event-istrusted>
    fn IsTrusted(&self) -> bool {
        self.mouseevent.IsTrusted()
    }
}
