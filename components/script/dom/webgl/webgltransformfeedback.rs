/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::reflect_dom_object;
use servo_canvas_traits::webgl::{WebGLCommand, webgl_channel};

use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::webgl::webgl2renderingcontext::IndexedBinding;
use crate::dom::webgl::webglobject::{WebGLObject, WebGLObjectContext};
use crate::dom::webgl::webglrenderingcontext::{
    LOST_OBJECT_NAME, Operation, WebGLRenderingContext,
};
use crate::dom::webglrenderingcontext::capture_webgl_backtrace;

#[derive(JSTraceable, MallocSizeOf)]
struct DroppableWebGLTransformFeedback {
    context: WebGLObjectContext,
    id: u32,
    marked_for_deletion: Cell<bool>,
}

impl DroppableWebGLTransformFeedback {
    fn send_with_fallibility(&self, command: WebGLCommand, fallibility: Operation) {
        if let Some(root) = self.context.root() {
            let result = root.sender().send(command, capture_webgl_backtrace());
            if matches!(fallibility, Operation::Infallible) {
                result.expect("Operation failed");
            }
        }
    }

    fn delete(&self, operation_fallibility: Operation) {
        if self.is_valid() && self.id() != 0 {
            self.marked_for_deletion.set(true);
            self.send_with_fallibility(
                WebGLCommand::DeleteTransformFeedback(self.id()),
                operation_fallibility,
            );
        }
    }

    fn id(&self) -> u32 {
        self.id
    }

    fn is_valid(&self) -> bool {
        !self.marked_for_deletion.get()
    }
}

impl Drop for DroppableWebGLTransformFeedback {
    fn drop(&mut self) {
        self.delete(Operation::Fallible);
    }
}

/// Begin/pause state of a transform feedback object. The default transform
/// feedback object (GL name 0) has no DOM object, so its state lives in the
/// context; this struct is shared by both.
#[derive(JSTraceable, MallocSizeOf)]
pub(crate) struct TransformFeedbackState {
    /// The indexed TRANSFORM_FEEDBACK_BUFFER bindings, which are per-object state. The
    /// context keeps the bound object's bindings; these hold an object's bindings while
    /// another one is bound.
    saved_buffer_bindings: Box<[IndexedBinding]>,
    is_active: Cell<bool>,
    is_paused: Cell<bool>,
    primitive_mode: Cell<u32>,
    /// Vertices recorded since beginTransformFeedback, used for the
    /// buffer-space check in draws (ES 3.0 2.15.2).
    vertices_recorded: Cell<u64>,
}

impl TransformFeedbackState {
    pub(crate) fn new(context: &WebGLRenderingContext) -> Self {
        Self {
            saved_buffer_bindings: (0..context.limits().max_transform_feedback_separate_attribs)
                .map(|_| IndexedBinding::new())
                .collect::<Vec<_>>()
                .into(),
            is_active: Default::default(),
            is_paused: Default::default(),
            primitive_mode: Default::default(),
            vertices_recorded: Default::default(),
        }
    }

    pub(crate) fn saved_buffer_bindings(&self) -> &[IndexedBinding] {
        &self.saved_buffer_bindings
    }

    pub(crate) fn is_active(&self) -> bool {
        self.is_active.get()
    }

    /// Partii: a new object's state (a restored context's default transform feedback).
    pub(crate) fn reset(&self) {
        for binding in self.saved_buffer_bindings.iter() {
            binding.reset();
        }
        self.is_active.set(false);
        self.is_paused.set(false);
        self.primitive_mode.set(0);
        self.vertices_recorded.set(0);
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.is_paused.get()
    }

    pub(crate) fn is_active_unpaused(&self) -> bool {
        self.is_active.get() && !self.is_paused.get()
    }

    pub(crate) fn primitive_mode(&self) -> u32 {
        self.primitive_mode.get()
    }

    pub(crate) fn vertices_recorded(&self) -> u64 {
        self.vertices_recorded.get()
    }

    pub(crate) fn add_vertices_recorded(&self, count: u64) {
        self.vertices_recorded
            .set(self.vertices_recorded.get().saturating_add(count));
    }

    pub(crate) fn begin(&self, context: &WebGLRenderingContext, primitive_mode: u32) {
        if !self.is_active() {
            context.send_command(WebGLCommand::BeginTransformFeedback(primitive_mode));
            self.is_active.set(true);
            self.is_paused.set(false);
            self.primitive_mode.set(primitive_mode);
            self.vertices_recorded.set(0);
        }
    }

    pub(crate) fn end(&self, context: &WebGLRenderingContext) {
        if self.is_active() {
            if self.is_paused() {
                context.send_command(WebGLCommand::ResumeTransformFeedback());
            }
            context.send_command(WebGLCommand::EndTransformFeedback());
            self.is_active.set(false);
            self.is_paused.set(false);
        }
    }

    pub(crate) fn resume(&self, context: &WebGLRenderingContext) {
        if self.is_active() && self.is_paused() {
            context.send_command(WebGLCommand::ResumeTransformFeedback());
            self.is_paused.set(false);
        }
    }

    pub(crate) fn pause(&self, context: &WebGLRenderingContext) {
        if self.is_active() && !self.is_paused() {
            context.send_command(WebGLCommand::PauseTransformFeedback());
            self.is_paused.set(true);
        }
    }
}

#[dom_struct]
pub(crate) struct WebGLTransformFeedback {
    webgl_object: WebGLObject,
    has_been_bound: Cell<bool>,
    state: TransformFeedbackState,
    droppable: DroppableWebGLTransformFeedback,
}

impl WebGLTransformFeedback {
    fn new_inherited(context: &WebGLRenderingContext, id: u32) -> Self {
        Self {
            webgl_object: WebGLObject::new_inherited(context),
            has_been_bound: Cell::new(false),
            state: TransformFeedbackState::new(context),
            droppable: DroppableWebGLTransformFeedback {
                context: WebGLObjectContext::new(context),
                id,
                marked_for_deletion: Cell::new(false),
            },
        }
    }

    pub(crate) fn new(cx: &mut JSContext, context: &WebGLRenderingContext) -> DomRoot<Self> {
        // Partii: a lost context makes objects that name nothing.
        let id = if context.is_lost() {
            LOST_OBJECT_NAME.get()
        } else {
            let (sender, receiver) = webgl_channel().unwrap();
            context.send_command(WebGLCommand::CreateTransformFeedback(sender));
            receiver.recv().unwrap()
        };

        reflect_dom_object(
            cx,
            Box::new(WebGLTransformFeedback::new_inherited(context, id)),
            &*context.global(),
        )
    }
}

impl WebGLTransformFeedback {
    pub(crate) fn bind(&self, context: &WebGLRenderingContext, target: u32) {
        context.send_command(WebGLCommand::BindTransformFeedback(target, self.id()));
        self.has_been_bound.set(true);
    }

    pub(crate) fn state(&self) -> &TransformFeedbackState {
        &self.state
    }

    pub(crate) fn id(&self) -> u32 {
        self.droppable.id()
    }

    pub(crate) fn is_valid(&self) -> bool {
        self.droppable.is_valid()
    }

    pub(crate) fn is_active(&self) -> bool {
        self.state.is_active()
    }

    pub(crate) fn delete(&self, operation_fallibility: Operation) {
        self.droppable.delete(operation_fallibility);
    }
}
