/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://www.khronos.org/registry/webgl/specs/latest/1.0/webgl.idl
use dom_struct::dom_struct;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::{AssociatedMemory, Reflector};
use script_bindings::root::DomRoot;
use script_bindings::weakref::WeakRef;
use servo_canvas_traits::webgl::{WebGLCommand, WebGLContextId, WebGLMsgSender};

use crate::dom::bindings::codegen::Bindings::WebGLObjectBinding::WebGLObjectMethods;
use crate::dom::bindings::str::USVString;
use crate::dom::webgl::webglrenderingcontext::{DrawStateGen, WebGLRenderingContext};
use crate::dom::webglrenderingcontext::{Operation, capture_webgl_backtrace};

#[dom_struct]
pub(crate) struct WebGLObject {
    reflector_: Reflector<AssociatedMemory>,
    #[no_trace]
    webgl_sender: WebGLMsgSender,
    context: WeakRef<WebGLRenderingContext>,
    label: DomRefCell<USVString>,
    /// Partii (perf): the context's draw-state generation, moved on by this object's commands too.
    #[ignore_malloc_size_of = "A shared counter"]
    #[no_trace]
    draw_state_gen: DrawStateGen,
}

impl WebGLObject {
    pub(crate) fn new_inherited(context: &WebGLRenderingContext) -> WebGLObject {
        WebGLObject {
            reflector_: Reflector::new(),
            webgl_sender: context.sender().clone(),
            context: WeakRef::new(context),
            label: DomRefCell::new(USVString::default()),
            draw_state_gen: context.draw_state_gen().clone(),
        }
    }

    /// Get the [`WebGLRenderingContext`] that created this object.
    ///
    /// If `None` is returned the [`WebGLRenderingContext`] has already been garbage collected.
    pub(crate) fn context(&self) -> Option<DomRoot<WebGLRenderingContext>> {
        self.context.root()
    }

    #[inline]
    pub(crate) fn context_id(&self) -> WebGLContextId {
        self.webgl_sender.context_id()
    }

    #[inline]
    pub(crate) fn send_with_fallibility(&self, command: WebGLCommand, fallibility: Operation) {
        self.draw_state_gen.note(&command);
        let result = self.webgl_sender.send(command, capture_webgl_backtrace());
        if matches!(fallibility, Operation::Infallible) {
            result.expect("Operation failed");
        }
    }

    #[inline]
    pub(crate) fn send_command(&self, command: WebGLCommand) {
        self.send_with_fallibility(command, Operation::Infallible);
    }
}

/// Partii: the context that made an object, for the object's own GL commands (the deletes when
/// it is dropped among them). `root()` is None once that GL context is gone: a restored context
/// has a new one (WebGL 1.0 5.15.3), whose names would collide with this object's.
#[derive(JSTraceable, MallocSizeOf)]
pub(crate) struct WebGLObjectContext {
    context: WeakRef<WebGLRenderingContext>,
    #[no_trace]
    #[ignore_malloc_size_of = "An id"]
    context_id: WebGLContextId,
}

impl WebGLObjectContext {
    pub(crate) fn new(context: &WebGLRenderingContext) -> Self {
        Self {
            context: WeakRef::new(context),
            context_id: context.sender().context_id(),
        }
    }

    pub(crate) fn root(&self) -> Option<DomRoot<WebGLRenderingContext>> {
        self.context
            .root()
            .filter(|context| context.sender().context_id() == self.context_id)
    }
}

impl WebGLObjectMethods<crate::DomTypeHolder> for WebGLObject {
    /// <https://registry.khronos.org/webgl/specs/latest/1.0/#5.3>
    fn Label(&self) -> USVString {
        self.label.borrow().clone()
    }

    /// <https://registry.khronos.org/webgl/specs/latest/1.0/#5.3>
    fn SetLabel(&self, value: USVString) {
        *self.label.borrow_mut() = value;
    }
}
