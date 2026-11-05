/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://www.khronos.org/registry/webgl/specs/latest/1.0/webgl.idl
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::{DomRefCell, Ref};
use script_bindings::reflector::reflect_dom_object;
use script_bindings::weakref::WeakRef;
use servo_base::generic_channel::{GenericReceiver, TryReceiveError};
use servo_base::text::Utf8CodeUnits;
use servo_canvas_traits::webgl::{
    ActiveAttribInfo, ActiveUniformBlockInfo, ActiveUniformInfo, EmulatedBuiltinUniforms,
    ProgramLinkInfo, WebGLCommand,
    WebGLError, WebGLProgramId, WebGLResult, webgl_channel,
};

use crate::dom::bindings::codegen::Bindings::WebGL2RenderingContextBinding::WebGL2RenderingContextConstants as constants2;
use crate::dom::bindings::codegen::Bindings::WebGLRenderingContextBinding::WebGLRenderingContextConstants as constants;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::webgl::webglactiveinfo::WebGLActiveInfo;
use crate::dom::webgl::webglobject::WebGLObject;
use crate::dom::webgl::webglrenderingcontext::{Operation, WebGLRenderingContext};
use crate::dom::webgl::webglshader::{FragOutputKind, WebGLShader};
use crate::dom::webgl::webgluniformlocation::WebGLUniformLocation;
use crate::dom::webglrenderingcontext::capture_webgl_backtrace;

#[derive(JSTraceable, MallocSizeOf)]
struct DroppableWebGLProgram {
    #[no_trace]
    id: WebGLProgramId,
    context: WeakRef<WebGLRenderingContext>,
    fragment_shader: Option<WeakRef<WebGLShader>>,
    vertex_shader: Option<WeakRef<WebGLShader>>,
    marked_for_deletion: bool,
    is_in_use: bool,
}

impl DroppableWebGLProgram {
    fn new(id: WebGLProgramId, context: &WebGLRenderingContext) -> Self {
        Self {
            id,
            context: WeakRef::new(context),
            fragment_shader: None,
            vertex_shader: None,
            marked_for_deletion: Default::default(),
            is_in_use: Default::default(),
        }
    }
}

impl DroppableWebGLProgram {
    fn attach_shader<'a>(&mut self, shader: &'a WebGLShader) -> WebGLResult<&'a WebGLShader> {
        if self.is_deleted() || shader.is_deleted() {
            return Err(WebGLError::InvalidOperation);
        }
        let shader_slot = match shader.gl_type() {
            constants::FRAGMENT_SHADER => &mut self.fragment_shader,
            constants::VERTEX_SHADER => &mut self.vertex_shader,
            _ => {
                error!("detachShader: Unexpected shader type");
                return Err(WebGLError::InvalidValue);
            },
        };

        if shader_slot.is_some() {
            return Err(WebGLError::InvalidOperation);
        }

        *shader_slot = Some(WeakRef::new(shader));
        shader.increment_attached_counter();

        self.send_command(WebGLCommand::AttachShader(self.id, shader.id()));

        Ok(shader)
    }

    fn detach_shader<'a>(&mut self, shader: &'a WebGLShader) -> WebGLResult<&'a WebGLShader> {
        if self.is_deleted() {
            return Err(WebGLError::InvalidOperation);
        }
        let shader_slot = match shader.gl_type() {
            constants::FRAGMENT_SHADER => &mut self.fragment_shader,
            constants::VERTEX_SHADER => &mut self.vertex_shader,
            _ => return Err(WebGLError::InvalidValue),
        };

        match shader_slot {
            Some(attached_shader) => match attached_shader.root() {
                Some(root) => {
                    if root.id() != shader.id() {
                        return Err(WebGLError::InvalidOperation);
                    }
                },
                None => return Err(WebGLError::InvalidOperation),
            },
            None => return Err(WebGLError::InvalidOperation),
        }

        *shader_slot = None;
        shader.decrement_attached_counter();

        self.send_command(WebGLCommand::DetachShader(self.id, shader.id()));

        Ok(shader)
    }

    fn detach_shaders(&mut self) {
        if let Some(ref mut shader) = self.fragment_shader {
            if let Some(root) = shader.root() {
                root.decrement_attached_counter();
                self.send_command(WebGLCommand::DetachShader(self.id, root.id()));
            }
            self.fragment_shader = None;
        }
        if let Some(ref mut shader) = self.vertex_shader {
            if let Some(root) = shader.root() {
                root.decrement_attached_counter();
                self.send_command(WebGLCommand::DetachShader(self.id, root.id()));
            }
            self.vertex_shader = None;
        }
    }

    fn is_deleted(&self) -> bool {
        self.marked_for_deletion && !self.is_in_use
    }

    fn send_command(&self, command: WebGLCommand) {
        self.send_with_fallibility(command, Operation::Infallible);
    }

    fn send_with_fallibility(&self, command: WebGLCommand, fallibility: Operation) {
        if let Some(root) = self.context.root() {
            let result = root.sender().send(command, capture_webgl_backtrace());
            if matches!(fallibility, Operation::Infallible) {
                result.expect("Operation failed");
            }
        }
    }

    fn mark_for_deletion(&mut self, operation_fallibility: Operation) {
        if self.marked_for_deletion {
            return;
        }
        self.marked_for_deletion = true;
        self.send_with_fallibility(WebGLCommand::DeleteProgram(self.id), operation_fallibility);
        if self.is_deleted() {
            self.detach_shaders();
        }
    }

    fn in_use(&mut self, value: bool) {
        if self.is_in_use == value {
            return;
        }
        self.is_in_use = value;
        if self.is_deleted() {
            self.detach_shaders();
        }
    }
}

impl Drop for DroppableWebGLProgram {
    fn drop(&mut self) {
        self.in_use(false);
        self.mark_for_deletion(Operation::Fallible);
    }
}

#[dom_struct]
pub(crate) struct WebGLProgram {
    webgl_object: WebGLObject,
    link_called: Cell<bool>,
    linked: Cell<bool>,
    link_generation: Cell<u64>,
    fragment_shader: MutNullableDom<WebGLShader>,
    vertex_shader: MutNullableDom<WebGLShader>,
    #[no_trace]
    active_attribs: DomRefCell<Box<[ActiveAttribInfo]>>,
    #[no_trace]
    active_uniforms: DomRefCell<Box<[ActiveUniformInfo]>>,
    #[no_trace]
    active_uniform_blocks: DomRefCell<Box<[ActiveUniformBlockInfo]>>,
    /// UNIFORM_BLOCK_BINDING of each active uniform block (0 after a link).
    uniform_block_bindings: DomRefCell<Box<[u32]>>,
    transform_feedback_varyings_length: Cell<i32>,
    transform_feedback_mode: Cell<i32>,
    transform_feedback_varying_bytes: DomRefCell<Box<[u32]>>,
    /// The WebGL thread's answer to the last linkProgram, until something needs it. Links are
    /// resolved lazily so that pages polling COMPLETION_STATUS_KHR (KHR_parallel_shader_compile)
    /// don't block on the link; everything that reads link results resolves it first.
    #[no_trace]
    #[ignore_malloc_size_of = "channel"]
    pending_link: RefCell<Option<GenericReceiver<ProgramLinkInfo>>>,
    /// Texture units set on sampler uniforms (uniform location -> unit), for the WebGL 2
    /// draw-time sampler type check. Unset samplers use unit 0.
    sampler_units: DomRefCell<HashMap<i32, i32>>,
    /// The fragment shader's outputs by location at the last link (see
    /// `WebGLShader::fragment_outputs`), for the WebGL 2 draw buffer check.
    fragment_outputs: DomRefCell<Option<Box<[Option<FragOutputKind>]>>>,
    /// Locations of the translator's gl_DrawID / gl_BaseVertex / gl_BaseInstance uniforms.
    #[no_trace]
    emulated_builtins: Cell<EmulatedBuiltinUniforms>,
    /// Driver indices of uniforms hidden from content (sorted), see `ProgramLinkInfo`.
    hidden_uniform_indices: DomRefCell<Box<[u32]>>,
    droppable: RefCell<DroppableWebGLProgram>,
}

impl WebGLProgram {
    fn new_inherited(context: &WebGLRenderingContext, id: WebGLProgramId) -> Self {
        Self {
            webgl_object: WebGLObject::new_inherited(context),
            link_called: Default::default(),
            linked: Default::default(),
            link_generation: Default::default(),
            fragment_shader: Default::default(),
            vertex_shader: Default::default(),
            active_attribs: DomRefCell::new(vec![].into()),
            active_uniforms: DomRefCell::new(vec![].into()),
            active_uniform_blocks: DomRefCell::new(vec![].into()),
            uniform_block_bindings: DomRefCell::new(vec![].into()),
            transform_feedback_varyings_length: Default::default(),
            transform_feedback_mode: Default::default(),
            transform_feedback_varying_bytes: DomRefCell::new(vec![].into()),
            pending_link: RefCell::new(None),
            sampler_units: Default::default(),
            fragment_outputs: Default::default(),
            emulated_builtins: Default::default(),
            hidden_uniform_indices: DomRefCell::new(vec![].into()),
            droppable: RefCell::new(DroppableWebGLProgram::new(id, context)),
        }
    }

    pub(crate) fn maybe_new(
        cx: &mut JSContext,
        context: &WebGLRenderingContext,
    ) -> Option<DomRoot<Self>> {
        let (sender, receiver) = webgl_channel().unwrap();
        context.send_command(WebGLCommand::CreateProgram(sender));
        receiver
            .recv()
            .unwrap()
            .map(|id| WebGLProgram::new(cx, context, id))
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        context: &WebGLRenderingContext,
        id: WebGLProgramId,
    ) -> DomRoot<Self> {
        reflect_dom_object(
            cx,
            Box::new(WebGLProgram::new_inherited(context, id)),
            &*context.global(),
        )
    }
}

impl WebGLProgram {
    pub(crate) fn id(&self) -> WebGLProgramId {
        self.droppable.borrow().id
    }

    /// glDeleteProgram
    pub(crate) fn mark_for_deletion(&self, operation_fallibility: Operation) {
        if self.is_marked_for_deletion() {
            return;
        }
        self.set_marked_for_deletion(true);
        self.upcast().send_with_fallibility(
            WebGLCommand::DeleteProgram(self.id()),
            operation_fallibility,
        );
        if self.is_deleted() {
            self.detach_shaders();
        }
    }

    pub(crate) fn in_use(&self, value: bool) {
        if self.is_in_use() == value {
            return;
        }
        self.set_is_in_use(value);
        if self.is_deleted() {
            self.detach_shaders();
        }
    }

    fn detach_shaders(&self) {
        assert!(self.is_deleted());
        self.droppable.borrow_mut().detach_shaders();
        if self.fragment_shader.get().is_some() {
            self.fragment_shader.set(None);
        }
        if self.vertex_shader.get().is_some() {
            self.vertex_shader.set(None);
        }
    }

    pub(crate) fn is_in_use(&self) -> bool {
        self.droppable.borrow().is_in_use
    }

    pub(crate) fn is_marked_for_deletion(&self) -> bool {
        self.droppable.borrow().marked_for_deletion
    }

    pub(crate) fn is_deleted(&self) -> bool {
        self.is_marked_for_deletion() && !self.is_in_use()
    }

    pub(crate) fn is_linked(&self) -> bool {
        self.resolve_link();
        self.linked.get()
    }

    /// glLinkProgram
    pub(crate) fn link(&self) -> WebGLResult<()> {
        // A previous link still in flight is superseded; take its answer off the channel.
        self.resolve_link();
        self.sampler_units.borrow_mut().clear();
        self.linked.set(false);
        self.link_generation
            .set(self.link_generation.get().checked_add(1).unwrap());
        *self.active_attribs.borrow_mut() = Box::new([]);
        *self.active_uniforms.borrow_mut() = Box::new([]);
        *self.active_uniform_blocks.borrow_mut() = Box::new([]);
        *self.uniform_block_bindings.borrow_mut() = Box::new([]);
        *self.fragment_outputs.borrow_mut() = None;

        match self.fragment_shader.get() {
            Some(ref shader) if shader.successfully_compiled() => {
                *self.fragment_outputs.borrow_mut() = shader.fragment_outputs();
            },
            _ => return Ok(()), // callers use gl.LINK_STATUS to check link errors
        }

        match self.vertex_shader.get() {
            Some(ref shader) if shader.successfully_compiled() => {},
            _ => return Ok(()), // callers use gl.LINK_STATUS to check link errors
        }

        let (sender, receiver) = webgl_channel().unwrap();
        self.upcast()
            .send_command(WebGLCommand::LinkProgram(self.id(), sender));
        *self.pending_link.borrow_mut() = Some(receiver);
        Ok(())
    }

    /// Records the units given to a sampler uniform (array elements at consecutive locations).
    pub(crate) fn set_sampler_units(&self, location: i32, units: &[i32]) {
        let mut sampler_units = self.sampler_units.borrow_mut();
        for (i, &unit) in units.iter().enumerate() {
            sampler_units.insert(location + i as i32, unit);
        }
    }

    /// The texture unit of a sampler uniform at `location` (0 until set).
    pub(crate) fn sampler_unit(&self, location: i32) -> u32 {
        self.sampler_units
            .borrow()
            .get(&location)
            .copied()
            .unwrap_or(0)
            .max(0) as u32
    }

    /// Waits for and applies the answer to a pending linkProgram, if there is one.
    fn resolve_link(&self) {
        let receiver = self.pending_link.borrow_mut().take();
        if let Some(receiver) = receiver &&
            let Ok(link_info) = receiver.recv()
        {
            self.apply_link_info(link_info);
        }
    }

    /// COMPLETION_STATUS_KHR: whether the last link has finished, without waiting for it.
    pub(crate) fn link_completed(&self) -> bool {
        let result = match self.pending_link.borrow().as_ref() {
            None => return true,
            Some(receiver) => receiver.try_recv(),
        };
        match result {
            Ok(link_info) => {
                self.pending_link.borrow_mut().take();
                self.apply_link_info(link_info);
                true
            },
            Err(TryReceiveError::Empty) => false,
            Err(TryReceiveError::ReceiveError(_)) => {
                self.pending_link.borrow_mut().take();
                true
            },
        }
    }

    fn apply_link_info(&self, link_info: ProgramLinkInfo) {
        {
            let mut used_locs = HashSet::new();
            let mut used_names = HashSet::new();
            for active_attrib in &*link_info.active_attribs {
                let Some(location) = active_attrib.location else {
                    continue;
                };
                let columns = match active_attrib.type_ {
                    constants::FLOAT_MAT2 => 2,
                    constants::FLOAT_MAT3 => 3,
                    constants::FLOAT_MAT4 => 4,
                    _ => 1,
                };
                assert!(used_names.insert(&*active_attrib.name));
                for column in 0..columns {
                    // https://www.khronos.org/registry/webgl/specs/latest/1.0/#6.31
                    if !used_locs.insert(location + column) {
                        return;
                    }
                }
            }
            for active_uniform in &*link_info.active_uniforms {
                // https://www.khronos.org/registry/webgl/specs/latest/1.0/#6.41
                if !used_names.insert(&*active_uniform.base_name) {
                    return;
                }
            }
        }

        self.linked.set(link_info.linked);
        self.link_called.set(true);
        self.transform_feedback_varyings_length
            .set(link_info.transform_feedback_length);
        self.transform_feedback_mode
            .set(link_info.transform_feedback_mode);
        *self.transform_feedback_varying_bytes.borrow_mut() =
            link_info.transform_feedback_varying_bytes;
        *self.active_attribs.borrow_mut() = link_info.active_attribs;
        *self.active_uniforms.borrow_mut() = link_info.active_uniforms;
        self.emulated_builtins.set(link_info.emulated_builtins);
        *self.hidden_uniform_indices.borrow_mut() = link_info.hidden_uniform_indices;
        *self.uniform_block_bindings.borrow_mut() =
            vec![0; link_info.active_uniform_blocks.len()].into();
        *self.active_uniform_blocks.borrow_mut() = link_info.active_uniform_blocks;
    }

    /// Locations of the uniforms emulating gl_DrawID, gl_BaseVertex and gl_BaseInstance.
    pub(crate) fn emulated_builtins(&self) -> EmulatedBuiltinUniforms {
        self.resolve_link();
        self.emulated_builtins.get()
    }

    /// A WebGL uniform index as the driver numbers it (the driver also counts hidden uniforms).
    fn uniform_index_to_gl(&self, index: u32) -> u32 {
        let mut index = index;
        for &hidden in &**self.hidden_uniform_indices.borrow() {
            if hidden <= index {
                index += 1;
            }
        }
        index
    }

    /// A driver uniform index as WebGL numbers it.
    fn uniform_index_from_gl(&self, index: u32) -> u32 {
        if index == constants2::INVALID_INDEX {
            return index;
        }
        let below = self
            .hidden_uniform_indices
            .borrow()
            .iter()
            .filter(|&&hidden| hidden < index)
            .count() as u32;
        index - below
    }

    pub(crate) fn active_attribs(&self) -> Ref<'_, [ActiveAttribInfo]> {
        self.resolve_link();
        Ref::map(self.active_attribs.borrow(), |attribs| &**attribs)
    }

    pub(crate) fn active_uniforms(&self) -> Ref<'_, [ActiveUniformInfo]> {
        self.resolve_link();
        Ref::map(self.active_uniforms.borrow(), |uniforms| &**uniforms)
    }

    pub(crate) fn active_uniform_blocks(&self) -> Ref<'_, [ActiveUniformBlockInfo]> {
        self.resolve_link();
        Ref::map(self.active_uniform_blocks.borrow(), |blocks| &**blocks)
    }

    /// The binding point of each active uniform block, by block index.
    pub(crate) fn uniform_block_bindings(&self) -> Ref<'_, [u32]> {
        self.resolve_link();
        Ref::map(self.uniform_block_bindings.borrow(), |bindings| &**bindings)
    }

    /// The fragment outputs by location as of the last link; None if they are unknown.
    pub(crate) fn fragment_outputs(&self) -> Ref<'_, Option<Box<[Option<FragOutputKind>]>>> {
        self.fragment_outputs.borrow()
    }

    /// glValidateProgram
    pub(crate) fn validate(&self) -> WebGLResult<()> {
        if self.is_deleted() {
            return Err(WebGLError::InvalidOperation);
        }
        self.upcast()
            .send_command(WebGLCommand::ValidateProgram(self.id()));
        Ok(())
    }

    /// glAttachShader
    pub(crate) fn attach_shader(&self, shader: &WebGLShader) -> WebGLResult<()> {
        match self.droppable.borrow_mut().attach_shader(shader) {
            Ok(shader) => {
                let shader_slot = match shader.gl_type() {
                    constants::FRAGMENT_SHADER => &self.fragment_shader,
                    constants::VERTEX_SHADER => &self.vertex_shader,
                    _ => {
                        error!("attach_shader: Unexpected shader type");
                        return Err(WebGLError::InvalidValue);
                    },
                };

                shader_slot.set(Some(shader));

                Ok(())
            },
            Err(e) => Err(e),
        }
    }

    /// glDetachShader
    pub(crate) fn detach_shader(&self, shader: &WebGLShader) -> WebGLResult<()> {
        match self.droppable.borrow_mut().detach_shader(shader) {
            Ok(shader) => {
                let shader_slot = match shader.gl_type() {
                    constants::FRAGMENT_SHADER => &self.fragment_shader,
                    constants::VERTEX_SHADER => &self.vertex_shader,
                    _ => {
                        error!("detach_shader: Unexpected shader type");
                        return Err(WebGLError::InvalidValue);
                    },
                };

                shader_slot.set(None);

                Ok(())
            },
            Err(e) => Err(e),
        }
    }

    /// glBindAttribLocation
    pub(crate) fn bind_attrib_location(&self, index: u32, name: DOMString) -> WebGLResult<()> {
        if self.is_deleted() {
            return Err(WebGLError::InvalidOperation);
        }

        if !validate_glsl_name(&name)? {
            return Ok(());
        }
        if name.starts_with_str("gl_") {
            return Err(WebGLError::InvalidOperation);
        }

        self.upcast().send_command(WebGLCommand::BindAttribLocation(
            self.id(),
            index,
            name.into(),
        ));
        Ok(())
    }

    pub(crate) fn get_active_uniform(
        &self,
        cx: &mut JSContext,
        index: u32,
    ) -> WebGLResult<DomRoot<WebGLActiveInfo>> {
        self.resolve_link();
        if self.is_deleted() {
            return Err(WebGLError::InvalidValue);
        }
        let uniforms = self.active_uniforms.borrow();
        let data = uniforms
            .get(index as usize)
            .ok_or(WebGLError::InvalidValue)?;
        Ok(WebGLActiveInfo::new(
            cx,
            &self.global(),
            data.size.unwrap_or(1),
            data.type_,
            data.name().into(),
        ))
    }

    /// glGetActiveAttrib
    pub(crate) fn get_active_attrib(
        &self,
        cx: &mut JSContext,
        index: u32,
    ) -> WebGLResult<DomRoot<WebGLActiveInfo>> {
        self.resolve_link();
        if self.is_deleted() {
            return Err(WebGLError::InvalidValue);
        }
        let attribs = self.active_attribs.borrow();
        let data = attribs
            .get(index as usize)
            .ok_or(WebGLError::InvalidValue)?;
        Ok(WebGLActiveInfo::new(
            cx,
            &self.global(),
            data.size,
            data.type_,
            data.name.clone().into(),
        ))
    }

    /// glGetAttribLocation
    pub(crate) fn get_attrib_location(&self, name: DOMString) -> WebGLResult<i32> {
        self.resolve_link();
        if !self.is_linked() || self.is_deleted() {
            return Err(WebGLError::InvalidOperation);
        }

        if !validate_glsl_name(&name)? {
            return Ok(-1);
        }
        if name.starts_with_str("gl_") {
            return Ok(-1);
        }

        let location = self
            .active_attribs
            .borrow()
            .iter()
            .find(|attrib| *attrib.name == name)
            .and_then(|attrib| attrib.location.map(|l| l as i32))
            .unwrap_or(-1);
        Ok(location)
    }

    /// glGetFragDataLocation
    pub(crate) fn get_frag_data_location(&self, name: DOMString) -> WebGLResult<i32> {
        self.resolve_link();
        if !self.is_linked() || self.is_deleted() {
            return Err(WebGLError::InvalidOperation);
        }

        if !validate_glsl_name(&name)? {
            return Ok(-1);
        }
        if name.starts_with_str("gl_") {
            return Ok(-1);
        }

        let (sender, receiver) = webgl_channel().unwrap();
        self.upcast()
            .send_command(WebGLCommand::GetFragDataLocation(
                self.id(),
                name.into(),
                sender,
            ));
        Ok(receiver.recv().unwrap())
    }

    /// glGetUniformLocation
    pub(crate) fn get_uniform_location(
        &self,
        cx: &mut JSContext,
        name: DOMString,
    ) -> WebGLResult<Option<DomRoot<WebGLUniformLocation>>> {
        self.resolve_link();
        if !self.is_linked() || self.is_deleted() {
            return Err(WebGLError::InvalidOperation);
        }

        if !validate_glsl_name(&name)? {
            return Ok(None);
        }
        if name.starts_with_str("gl_") {
            return Ok(None);
        }

        let (size, type_) = {
            let (base_name, array_index) = match parse_uniform_name(&name) {
                Some((name, index)) if index.is_none_or(|i| i >= 0) => (name, index),
                _ => return Ok(None),
            };

            let uniforms = self.active_uniforms.borrow();
            match uniforms
                .iter()
                .find(|attrib| *attrib.base_name == base_name)
            {
                Some(uniform) if array_index.is_none() || array_index < uniform.size => (
                    uniform
                        .size
                        .map(|size| size - array_index.unwrap_or_default()),
                    uniform.type_,
                ),
                _ => return Ok(None),
            }
        };

        let (sender, receiver) = webgl_channel().unwrap();
        self.upcast().send_command(WebGLCommand::GetUniformLocation(
            self.id(),
            name.into(),
            sender,
        ));
        let location = receiver.recv().unwrap();
        let context_id = self.upcast().context_id();

        Ok(Some(WebGLUniformLocation::new(
            cx,
            &self.global(),
            location,
            context_id,
            self.id(),
            self.link_generation.get(),
            size,
            type_,
        )))
    }

    pub(crate) fn get_uniform_block_index(&self, name: DOMString) -> WebGLResult<u32> {
        self.resolve_link();
        if !self.link_called.get() || self.is_deleted() {
            return Err(WebGLError::InvalidOperation);
        }

        if !validate_glsl_name(&name)? {
            return Ok(constants2::INVALID_INDEX);
        }

        let (sender, receiver) = webgl_channel().unwrap();
        self.upcast()
            .send_command(WebGLCommand::GetUniformBlockIndex(
                self.id(),
                name.into(),
                sender,
            ));
        Ok(receiver.recv().unwrap())
    }

    pub(crate) fn get_uniform_indices(&self, names: Vec<DOMString>) -> WebGLResult<Vec<u32>> {
        self.resolve_link();
        if !self.link_called.get() || self.is_deleted() {
            return Err(WebGLError::InvalidOperation);
        }

        let validation_errors = names.iter().map(validate_glsl_name).collect::<Vec<_>>();
        let first_validation_error = validation_errors.iter().find(|result| result.is_err());
        if let Some(error) = first_validation_error {
            return Err(error.unwrap_err());
        }

        let names = names
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>();

        let (sender, receiver) = webgl_channel().unwrap();
        self.upcast()
            .send_command(WebGLCommand::GetUniformIndices(self.id(), names, sender));
        Ok(receiver
            .recv()
            .unwrap()
            .into_iter()
            .map(|index| self.uniform_index_from_gl(index))
            .collect())
    }

    pub(crate) fn get_active_uniforms(
        &self,
        indices: Vec<u32>,
        pname: u32,
    ) -> WebGLResult<Vec<i32>> {
        self.resolve_link();
        if !self.is_linked() || self.is_deleted() {
            return Err(WebGLError::InvalidOperation);
        }

        match pname {
            constants2::UNIFORM_TYPE |
            constants2::UNIFORM_SIZE |
            constants2::UNIFORM_BLOCK_INDEX |
            constants2::UNIFORM_OFFSET |
            constants2::UNIFORM_ARRAY_STRIDE |
            constants2::UNIFORM_MATRIX_STRIDE |
            constants2::UNIFORM_IS_ROW_MAJOR => {},
            _ => return Err(WebGLError::InvalidEnum),
        }

        if indices.len() > self.active_uniforms.borrow().len() {
            return Err(WebGLError::InvalidValue);
        }

        let indices = indices
            .into_iter()
            .map(|index| self.uniform_index_to_gl(index))
            .collect();
        let (sender, receiver) = webgl_channel().unwrap();
        self.upcast().send_command(WebGLCommand::GetActiveUniforms(
            self.id(),
            indices,
            pname,
            sender,
        ));
        Ok(receiver.recv().unwrap())
    }

    pub(crate) fn get_active_uniform_block_parameter(
        &self,
        block_index: u32,
        pname: u32,
    ) -> WebGLResult<Vec<i32>> {
        self.resolve_link();
        if !self.link_called.get() || self.is_deleted() {
            return Err(WebGLError::InvalidOperation);
        }

        if block_index as usize >= self.active_uniform_blocks.borrow().len() {
            return Err(WebGLError::InvalidValue);
        }

        match pname {
            constants2::UNIFORM_BLOCK_BINDING |
            constants2::UNIFORM_BLOCK_DATA_SIZE |
            constants2::UNIFORM_BLOCK_ACTIVE_UNIFORMS |
            constants2::UNIFORM_BLOCK_ACTIVE_UNIFORM_INDICES |
            constants2::UNIFORM_BLOCK_REFERENCED_BY_VERTEX_SHADER |
            constants2::UNIFORM_BLOCK_REFERENCED_BY_FRAGMENT_SHADER => {},
            _ => return Err(WebGLError::InvalidEnum),
        }

        let (sender, receiver) = webgl_channel().unwrap();
        self.upcast()
            .send_command(WebGLCommand::GetActiveUniformBlockParameter(
                self.id(),
                block_index,
                pname,
                sender,
            ));
        let values = receiver.recv().unwrap();
        if pname != constants2::UNIFORM_BLOCK_ACTIVE_UNIFORM_INDICES {
            return Ok(values);
        }
        Ok(values
            .into_iter()
            .map(|index| self.uniform_index_from_gl(index as u32) as i32)
            .collect())
    }

    pub(crate) fn get_active_uniform_block_name(&self, block_index: u32) -> WebGLResult<String> {
        self.resolve_link();
        if !self.link_called.get() || self.is_deleted() {
            return Err(WebGLError::InvalidOperation);
        }

        if block_index as usize >= self.active_uniform_blocks.borrow().len() {
            return Err(WebGLError::InvalidValue);
        }

        let (sender, receiver) = webgl_channel().unwrap();
        self.upcast()
            .send_command(WebGLCommand::GetActiveUniformBlockName(
                self.id(),
                block_index,
                sender,
            ));
        Ok(receiver.recv().unwrap())
    }

    pub(crate) fn bind_uniform_block(
        &self,
        block_index: u32,
        block_binding: u32,
    ) -> WebGLResult<()> {
        self.resolve_link();
        if block_index as usize >= self.active_uniform_blocks.borrow().len() {
            return Err(WebGLError::InvalidValue);
        }

        if let Some(binding) = self
            .uniform_block_bindings
            .borrow_mut()
            .get_mut(block_index as usize)
        {
            *binding = block_binding;
        }

        self.upcast()
            .send_command(WebGLCommand::UniformBlockBinding(
                self.id(),
                block_index,
                block_binding,
            ));
        Ok(())
    }

    /// glGetProgramInfoLog
    pub(crate) fn get_info_log(&self) -> WebGLResult<String> {
        self.resolve_link();
        if self.is_deleted() {
            return Err(WebGLError::InvalidValue);
        }
        if self.link_called.get() {
            let shaders_compiled = match (self.fragment_shader.get(), self.vertex_shader.get()) {
                (Some(fs), Some(vs)) => fs.successfully_compiled() && vs.successfully_compiled(),
                _ => false,
            };
            if !shaders_compiled {
                return Ok("One or more shaders failed to compile".to_string());
            }
        }
        let (sender, receiver) = webgl_channel().unwrap();
        self.upcast()
            .send_command(WebGLCommand::GetProgramInfoLog(self.id(), sender));
        Ok(receiver.recv().unwrap())
    }

    pub(crate) fn attached_shaders(&self) -> WebGLResult<Vec<DomRoot<WebGLShader>>> {
        if self.is_marked_for_deletion() {
            return Err(WebGLError::InvalidValue);
        }
        Ok(
            match (self.vertex_shader.get(), self.fragment_shader.get()) {
                (Some(vertex_shader), Some(fragment_shader)) => {
                    vec![vertex_shader, fragment_shader]
                },
                (Some(shader), None) | (None, Some(shader)) => vec![shader],
                (None, None) => vec![],
            },
        )
    }

    pub(crate) fn link_generation(&self) -> u64 {
        self.link_generation.get()
    }

    pub(crate) fn transform_feedback_varyings_length(&self) -> i32 {
        self.resolve_link();
        self.transform_feedback_varyings_length.get()
    }

    pub(crate) fn transform_feedback_buffer_mode(&self) -> i32 {
        self.resolve_link();
        self.transform_feedback_mode.get()
    }

    /// Bytes per vertex written into each transform feedback buffer binding: one entry
    /// (the sum) for INTERLEAVED_ATTRIBS, one per varying for SEPARATE_ATTRIBS.
    pub(crate) fn transform_feedback_buffer_strides(&self) -> Vec<u64> {
        self.resolve_link();
        let bytes = self.transform_feedback_varying_bytes.borrow();
        if self.transform_feedback_mode.get() == constants2::INTERLEAVED_ATTRIBS as i32 {
            vec![bytes.iter().map(|b| *b as u64).sum()]
        } else {
            bytes.iter().map(|b| *b as u64).collect()
        }
    }

    fn set_marked_for_deletion(&self, value: bool) {
        self.droppable.borrow_mut().marked_for_deletion = value
    }

    fn set_is_in_use(&self, value: bool) {
        self.droppable.borrow_mut().is_in_use = value
    }
}

fn validate_glsl_name(name: &DOMString) -> WebGLResult<bool> {
    if name.is_empty() {
        return Ok(false);
    }
    if name.len_utf8() > MAX_UNIFORM_AND_ATTRIBUTE_LEN {
        return Err(WebGLError::InvalidValue);
    }
    for c in name.str().chars() {
        validate_glsl_char(c)?;
    }
    if name.starts_with_str("webgl_") || name.starts_with_str("_webgl_") {
        return Err(WebGLError::InvalidOperation);
    }
    Ok(true)
}

fn validate_glsl_char(c: char) -> WebGLResult<()> {
    match c {
        'a'..='z' |
        'A'..='Z' |
        '0'..='9' |
        ' ' |
        '\t' |
        '\u{11}' |
        '\u{12}' |
        '\r' |
        '\n' |
        '_' |
        '.' |
        '+' |
        '-' |
        '/' |
        '*' |
        '%' |
        '<' |
        '>' |
        '[' |
        ']' |
        '(' |
        ')' |
        '{' |
        '}' |
        '^' |
        '|' |
        '&' |
        '~' |
        '=' |
        '!' |
        ':' |
        ';' |
        ',' |
        '?' => Ok(()),
        _ => Err(WebGLError::InvalidValue),
    }
}

fn parse_uniform_name(name: &DOMString) -> Option<(String, Option<i32>)> {
    let name = name.str();
    if !name.ends_with(']') {
        return Some((String::from(name), None));
    }
    let bracket_pos = name[..name.len() - 1].rfind('[')?;
    let index = name[(bracket_pos + 1)..(name.len() - 1)]
        .parse::<i32>()
        .ok()?;
    Some((String::from(&name[..bracket_pos]), Some(index)))
}

/// <https://registry.khronos.org/webgl/specs/latest/1.0/#6.20>
/// > WebGL requires support of tokens up to 256 characters in length.
/// > Shaders containing tokens longer than 256 characters must fail to compile.
///
/// FIXME: how is "character" defined here? Should this be `Utf32CodeUnits` instead?
pub(crate) const MAX_UNIFORM_AND_ATTRIBUTE_LEN: Utf8CodeUnits = Utf8CodeUnits(256);
