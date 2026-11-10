/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://www.khronos.org/registry/webgl/specs/latest/1.0/webgl.idl
use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_weak_referenceable_dom_object;
use script_bindings::weakref::WeakRef;
use servo_base::generic_channel::{GenericReceiver, TryReceiveError};
use servo_canvas_traits::webgl::{
    GLLimits, ShaderCompileInfo, ShaderTranslation, WebGLCommand, WebGLError, WebGLResult,
    WebGLObjectNameKind, WebGLShaderId, WebGLVersion, webgl_channel,
};
#[cfg(feature = "webgl")]
use {
    crate::dom::webgl::extensions::WebGLExtensions,
    crate::dom::webgl::extensions::extfragdepth::EXTFragDepth,
    crate::dom::webgl::extensions::extshadertexturelod::EXTShaderTextureLod,
    crate::dom::webgl::extensions::oesstandardderivatives::OESStandardDerivatives,
    crate::dom::webgl::extensions::webglmultidraw::WEBGLMultiDraw,
    crate::dom::webgl::extensions::webglmultidrawinstancedbasevertexbaseinstance::WEBGLMultiDrawInstancedBaseVertexBaseInstance,
    crate::dom::webgl::shaderinterface::ShaderInterface,
    crate::dom::webgl::webglobject::WebGLObject,
    crate::dom::webgl::webglrenderingcontext::{Operation, WebGLRenderingContext},
    crate::dom::webglrenderingcontext::capture_webgl_backtrace,
};

use crate::dom::bindings::codegen::Bindings::WebGLRenderingContextBinding::WebGLRenderingContextConstants as constants;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;

/// Partii (gaps-5): what a translation depends on: the shader type, the source and the translator's settings
/// (ShaderTranslation, as its Debug text).
pub(crate) type CompileKey = (u32, String, String);

thread_local! {
    /// Partii (gaps-5): translations per script thread (see compile_cache_enabled).
    static COMPILE_CACHE: RefCell<std::collections::HashMap<Rc<CompileKey>, ShaderCompileInfo>> =
        RefCell::new(std::collections::HashMap::new());
}

/// Partii (gaps-5): compileShader of a source this script thread has had translated before, with the same
/// translator settings, knows its outcome at once (the translator is deterministic), so COMPILE_STATUS and
/// the info log don't wait for the WebGL thread to work through everything queued before the compile.
/// The WebGL thread still compiles the shader. Phaser asks COMPILE_STATUS right after each of its 62
/// compileShader calls a boot (Rocket to Heaven boots one after every death). `PARTII_WEBGL_COMPILE_CACHE=0`
/// turns it off.
fn compile_cache_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PARTII_WEBGL_COMPILE_CACHE").map_or(true, |v| v != "0"))
}

/// A compileShader the WebGL thread translates (ANGLE) and compiles. Its answer is read
/// lazily, so pages polling COMPLETION_STATUS_KHR (KHR_parallel_shader_compile) don't wait for
/// it, and linkProgram takes the job so the link sees this compile even if the shader is
/// recompiled before the link is read.
pub(crate) struct CompileJob {
    receiver: RefCell<Option<GenericReceiver<ShaderCompileInfo>>>,
    /// Partii (gaps-5): the translation's inputs (COMPILE_CACHE, and the program link cache's key).
    pub(crate) key: Rc<CompileKey>,
    outcome: OnceCell<CompileOutcome>,
    shader_type: u32,
    pub(crate) essl3: bool,
    /// See `parse_vertex_inputs`; read from the source at compileShader.
    pub(crate) vertex_inputs: Option<Box<[VertexInput]>>,
    /// The declarations the link compares between the stages (ShaderInterface::parse).
    pub(crate) interface: Option<ShaderInterface>,
}

pub(crate) struct CompileOutcome {
    pub(crate) succeeded: bool,
    pub(crate) info_log: String,
    /// See `parse_fragment_outputs`; None for vertex shaders or when they couldn't be read.
    pub(crate) fragment_outputs: Option<Box<[Option<FragOutputKind>]>>,
}

impl CompileJob {
    fn finish(&self, info: Option<ShaderCompileInfo>) -> CompileOutcome {
        let Some(info) = info else {
            // The context is gone.
            return CompileOutcome {
                succeeded: false,
                info_log: String::new(),
                fragment_outputs: None,
            };
        };
        if compile_cache_enabled() {
            COMPILE_CACHE.with(|cache| {
                let mut cache = cache.borrow_mut();
                if cache.len() >= 512 {
                    cache.clear();
                }
                cache.insert(self.key.clone(), info.clone());
            });
        }
        let fragment_outputs = match info.object_code {
            Some(ref object_code) if self.shader_type == constants::FRAGMENT_SHADER => {
                parse_fragment_outputs(object_code, self.essl3)
            },
            _ => None,
        };
        CompileOutcome {
            succeeded: info.object_code.is_some(),
            info_log: info.info_log,
            fragment_outputs,
        }
    }

    /// The outcome, waiting for it if needed.
    pub(crate) fn outcome(&self) -> &CompileOutcome {
        self.outcome.get_or_init(|| {
            let receiver = self.receiver.borrow_mut().take();
            self.finish(receiver.and_then(|receiver| receiver.recv().ok()))
        })
    }

    /// Whether the outcome is in, without waiting for it.
    pub(crate) fn completed(&self) -> bool {
        if self.outcome.get().is_some() {
            return true;
        }
        let result = match self.receiver.borrow().as_ref() {
            None => return true,
            Some(receiver) => receiver.try_recv(),
        };
        let info = match result {
            Ok(info) => Some(info),
            Err(TryReceiveError::Empty) => return false,
            Err(TryReceiveError::ReceiveError(_)) => None,
        };
        self.receiver.borrow_mut().take();
        let _ = self.outcome.set(self.finish(info));
        true
    }
}

#[derive(JSTraceable, MallocSizeOf)]
struct DroppableWebGLShader {
    context: WeakRef<WebGLRenderingContext>,
    #[no_trace]
    id: WebGLShaderId,
    marked_for_deletion: Cell<bool>,
}

impl DroppableWebGLShader {
    fn send_with_fallibility(&self, command: WebGLCommand, fallibility: Operation) {
        if let Some(root) = self.context.root() {
            let result = root.sender().send(command, capture_webgl_backtrace());
            if matches!(fallibility, Operation::Infallible) {
                result.expect("Operation failed");
            }
        }
    }

    fn mark_for_deletion(&self, operation_fallibility: Operation) {
        if !self.marked_for_deletion.get() {
            self.marked_for_deletion.set(true);
            self.send_with_fallibility(WebGLCommand::DeleteShader(self.id), operation_fallibility);
        }
    }
}

impl Drop for DroppableWebGLShader {
    fn drop(&mut self) {
        self.mark_for_deletion(Operation::Fallible);
    }
}

#[dom_struct]
pub(crate) struct WebGLShader {
    webgl_object: WebGLObject,
    gl_type: u32,
    source: DomRefCell<DOMString>,
    attached_counter: Cell<u32>,
    /// The last compileShader, if any.
    #[no_trace]
    #[ignore_malloc_size_of = "channel"]
    compile_job: DomRefCell<Option<Rc<CompileJob>>>,
    droppable: DroppableWebGLShader,
}

/// A vertex shader input as declared in the source.
#[derive(Clone, Debug)]
pub(crate) struct VertexInput {
    pub(crate) name: String,
    /// `layout(location = N)`, which overrides bindAttribLocation.
    pub(crate) location: Option<u32>,
    /// Locations it occupies (a matCxR takes C).
    pub(crate) slots: u32,
}

/// The inputs an ESSL 3.00 vertex shader declares at global scope, from its source (inactive
/// ones included: they still can't share a location, GLSL ES 3.00.6 12.46). None when the
/// source has preprocessor conditionals or macros, or a declaration isn't understood.
fn parse_vertex_inputs(source: &str) -> Option<Box<[VertexInput]>> {
    // Strip comments.
    let mut code = String::with_capacity(source.len());
    let mut rest = source;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("//") {
            rest = after.split_once('\n').map_or("", |(_, after)| after);
            code.push('\n');
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, after)| after);
            code.push(' ');
        } else {
            let c = rest.chars().next()?;
            code.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    let mut body = String::with_capacity(code.len());
    for line in code.lines() {
        let trimmed = line.trim_start();
        if let Some(directive) = trimmed.strip_prefix('#') {
            match directive.split_whitespace().next() {
                Some("version" | "extension" | "pragma") | None => continue,
                _ => return None,
            }
        }
        body.push_str(line);
        body.push('\n');
    }
    // Global statements: text at brace depth 0 up to a ';'. A '{' at depth 0 starts a
    // function body, struct or block, which drops what came before it.
    let mut statements = vec![];
    let mut current = String::new();
    let mut depth = 0u32;
    for c in body.chars() {
        match c {
            '{' => {
                if depth == 0 {
                    current.clear();
                }
                depth += 1;
            },
            '}' => depth = depth.checked_sub(1)?,
            ';' if depth == 0 => statements.push(std::mem::take(&mut current)),
            _ if depth == 0 => current.push(c),
            _ => {},
        }
    }
    let mut inputs = vec![];
    for statement in statements {
        let spaced = statement
            .replace('(', " ( ")
            .replace(')', " ) ")
            .replace(',', " , ")
            .replace('=', " = ")
            .replace('[', " [ ");
        let mut tokens = spaced.split_whitespace().peekable();
        let mut location = None;
        if tokens.peek() == Some(&"layout") {
            tokens.next();
            if tokens.next()? != "(" {
                return None;
            }
            loop {
                match tokens.next()? {
                    ")" => break,
                    "location" => {
                        if tokens.next()? != "=" {
                            return None;
                        }
                        location = Some(tokens.next()?.parse::<u32>().ok()?);
                    },
                    _ => {},
                }
            }
        }
        let mut is_input = false;
        let mut type_name = None;
        for token in tokens.by_ref() {
            match token {
                "in" => is_input = true,
                "highp" | "mediump" | "lowp" | "flat" | "smooth" | "centroid" | "invariant" => {},
                _ => {
                    type_name = Some(token);
                    break;
                },
            }
        }
        if !is_input {
            continue;
        }
        let slots = match type_name? {
            "float" | "int" | "uint" | "bool" | "vec2" | "vec3" | "vec4" | "ivec2" | "ivec3" |
            "ivec4" | "uvec2" | "uvec3" | "uvec4" | "bvec2" | "bvec3" | "bvec4" => 1,
            "mat2" | "mat2x2" | "mat2x3" | "mat2x4" => 2,
            "mat3" | "mat3x2" | "mat3x3" | "mat3x4" => 3,
            "mat4" | "mat4x2" | "mat4x3" | "mat4x4" => 4,
            _ => return None,
        };
        let mut declarator_count = 0;
        loop {
            let name = tokens.next()?;
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return None;
            }
            declarator_count += 1;
            inputs.push(VertexInput {
                name: name.to_owned(),
                location: location.filter(|_| declarator_count == 1),
                slots,
            });
            match tokens.next() {
                None => break,
                Some(",") => continue,
                Some(_) => return None,
            }
        }
    }
    Some(inputs.into())
}

/// What a fragment shader output holds, which WebGL 2 requires the colour buffer it writes
/// to match: float (float or fixed-point buffers), signed or unsigned integer.
#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf, PartialEq)]
pub(crate) enum FragOutputKind {
    Float,
    Int,
    Uint,
}

/// Whether a shader source starts with `#version 300 es` (after whitespace and comments).
fn is_essl3(source: &str) -> bool {
    let mut rest = source;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("//") {
            rest = after.split_once('\n').map_or("", |(_, after)| after);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, after)| after);
        } else {
            break;
        }
    }
    let Some(directive) = rest.strip_prefix('#') else {
        return false;
    };
    let mut words = directive.split_whitespace();
    words.next() == Some("version") && words.next() == Some("300")
}

/// The outputs of a fragment shader by location, read from the translator's object code, where
/// each global declaration is one line (`layout(location = 1) out vec4 _uc[3];`). ESSL 1.00
/// shaders write gl_FragColor or gl_FragData[0], a float at location 0. None when a declaration
/// isn't understood: the draw check is then skipped rather than risk rejecting good draws.
fn parse_fragment_outputs(object_code: &str, essl3: bool) -> Option<Box<[Option<FragOutputKind>]>> {
    if !essl3 {
        return Some(Box::new([Some(FragOutputKind::Float)]));
    }
    let mut declared = vec![];
    let mut depth = 0i32;
    for line in object_code.lines() {
        let global = depth == 0;
        depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
        let line = line.trim();
        if !global || line.starts_with('#') {
            continue;
        }
        let mut location = None;
        let mut secondary = false;
        let rest = match line.strip_prefix("layout") {
            Some(layout) => {
                let (args, rest) = layout.trim_start().strip_prefix('(')?.split_once(')')?;
                for arg in args.split(',') {
                    let (key, value) = match arg.split_once('=') {
                        Some((key, value)) => (key.trim(), Some(value.trim())),
                        None => (arg.trim(), None),
                    };
                    match key {
                        "location" => location = Some(value?.parse::<u32>().ok()?),
                        // EXT_blend_func_extended's second colour, which no draw buffer takes.
                        "index" => secondary = value? != "0",
                        _ => {},
                    }
                }
                rest.trim_start()
            },
            None => line,
        };
        let Some(declaration) = rest.strip_prefix("out ") else {
            continue;
        };
        let mut words = declaration
            .strip_suffix(';')?
            .split_whitespace()
            .filter(|word| !matches!(*word, "highp" | "mediump" | "lowp"));
        let kind = match words.next()? {
            "float" | "vec2" | "vec3" | "vec4" => FragOutputKind::Float,
            "int" | "ivec2" | "ivec3" | "ivec4" => FragOutputKind::Int,
            "uint" | "uvec2" | "uvec3" | "uvec4" => FragOutputKind::Uint,
            _ => return None,
        };
        let declarator: String = words.collect();
        if declarator.contains(',') {
            return None;
        }
        let count = match declarator.split_once('[') {
            Some((_, size)) => size.strip_suffix(']')?.parse::<u32>().ok()?,
            None => 1,
        };
        if !secondary {
            declared.push((location, count, kind));
        }
    }
    // A single output without a location is at location 0 (ESSL 3.00 4.3.8.2).
    if let [(location, _, _)] = declared.as_mut_slice() {
        location.get_or_insert(0);
    }
    let mut outputs = vec![];
    for (location, count, kind) in declared {
        let location = location?;
        for slot in location..location.checked_add(count)? {
            let slot = slot as usize;
            if slot >= 32 {
                return None;
            }
            if outputs.len() <= slot {
                outputs.resize(slot + 1, None);
            }
            if outputs[slot].replace(kind).is_some() {
                return None;
            }
        }
    }
    Some(outputs.into())
}

impl WebGLShader {
    fn new_inherited(context: &WebGLRenderingContext, id: WebGLShaderId, shader_type: u32) -> Self {
        Self {
            webgl_object: WebGLObject::new_inherited(context),
            gl_type: shader_type,
            source: Default::default(),
            attached_counter: Cell::new(0),
            compile_job: Default::default(),
            droppable: DroppableWebGLShader {
                context: WeakRef::new(context),
                id,
                marked_for_deletion: Cell::new(false),
            },
        }
    }

    pub(crate) fn maybe_new(
        cx: &mut JSContext,
        context: &WebGLRenderingContext,
        shader_type: u32,
    ) -> Option<DomRoot<Self>> {
        // Partii (gaps-5): a pooled name (take_object_name), not a round trip behind all the queued work.
        let kind = match shader_type {
            constants::VERTEX_SHADER => WebGLObjectNameKind::VertexShader,
            constants::FRAGMENT_SHADER => WebGLObjectNameKind::FragmentShader,
            _ => {
                let (sender, receiver) = webgl_channel().unwrap();
                context.send_command(WebGLCommand::CreateShader(shader_type, sender));
                return receiver
                    .recv()
                    .unwrap()
                    .map(|id| WebGLShader::new(cx, context, id, shader_type));
            },
        };
        let id = WebGLShaderId::new(context.take_object_name(kind)?);
        Some(WebGLShader::new(cx, context, id, shader_type))
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        context: &WebGLRenderingContext,
        id: WebGLShaderId,
        shader_type: u32,
    ) -> DomRoot<Self> {
        reflect_weak_referenceable_dom_object(
            cx,
            Rc::new(WebGLShader::new_inherited(context, id, shader_type)),
            &*context.global(),
        )
    }
}

impl WebGLShader {
    pub(crate) fn id(&self) -> WebGLShaderId {
        self.droppable.id
    }

    pub(crate) fn gl_type(&self) -> u32 {
        self.gl_type
    }

    /// glCompileShader
    pub(crate) fn compile(
        &self,
        webgl_version: WebGLVersion,
        limits: &GLLimits,
        ext: &WebGLExtensions,
    ) -> WebGLResult<()> {
        if self.droppable.marked_for_deletion.get() && !self.is_attached() {
            return Err(WebGLError::InvalidValue);
        }

        let source: String = (*self.source.borrow().str()).to_owned();
        let webgl2 = webgl_version == WebGLVersion::WebGL2;
        let mut translation = ShaderTranslation {
            shader_type: self.gl_type,
            webgl2,
            max_vertex_attribs: limits.max_vertex_attribs as i32,
            max_vertex_uniform_vectors: limits.max_vertex_uniform_vectors as i32,
            max_vertex_texture_image_units: limits.max_vertex_texture_image_units as i32,
            max_combined_texture_image_units: limits.max_combined_texture_image_units as i32,
            max_texture_image_units: limits.max_texture_image_units as i32,
            max_fragment_uniform_vectors: limits.max_fragment_uniform_vectors as i32,
            max_vertex_output_vectors: limits.max_vertex_output_vectors as i32,
            max_fragment_input_vectors: limits.max_fragment_input_vectors as i32,
            max_varying_vectors: limits.max_varying_vectors as i32,
            oes_standard_derivatives: ext.is_enabled::<OESStandardDerivatives>(),
            ext_shader_texture_lod: ext.is_enabled::<EXTShaderTextureLod>(),
            ext_frag_depth: ext.is_enabled::<EXTFragDepth>(),
            angle_multi_draw: ext.is_enabled::<WEBGLMultiDraw>() ||
                ext.is_enabled::<WEBGLMultiDrawInstancedBaseVertexBaseInstance>(),
            ..Default::default()
        };
        if webgl2 {
            translation.min_program_texel_offset = limits.min_program_texel_offset as i32;
            translation.max_program_texel_offset = limits.max_program_texel_offset as i32;
            translation.max_draw_buffers = limits.max_draw_buffers as i32;
        }

        let essl3 = is_essl3(&source);
        let vertex_inputs = if self.gl_type == constants::VERTEX_SHADER && essl3 {
            parse_vertex_inputs(&source)
        } else {
            None
        };
        let interface = ShaderInterface::parse(&source, self.gl_type == constants::VERTEX_SHADER);
        let key = Rc::new((self.gl_type, source.clone(), format!("{translation:?}")));
        let cached = if compile_cache_enabled() {
            COMPILE_CACHE.with(|cache| cache.borrow().get(&key).cloned())
        } else {
            None
        };
        let (sender, receiver) = webgl_channel().unwrap();
        self.upcast().send_command(WebGLCommand::CompileShader(
            self.id(),
            source,
            translation,
            sender,
        ));
        let job = Rc::new(CompileJob {
            receiver: RefCell::new(Some(receiver)),
            key,
            outcome: OnceCell::new(),
            shader_type: self.gl_type,
            essl3,
            vertex_inputs,
            interface,
        });
        if let Some(info) = cached {
            // The WebGL thread's answer (the same) is never read.
            let _ = job.outcome.set(job.finish(Some(info)));
        }
        *self.compile_job.borrow_mut() = Some(job);
        Ok(())
    }

    /// Mark this shader as deleted (if it wasn't previously)
    /// and delete it as if calling glDeleteShader.
    /// Currently does not check if shader is attached
    pub(crate) fn mark_for_deletion(&self, operation_fallibility: Operation) {
        self.droppable.mark_for_deletion(operation_fallibility);
    }

    pub(crate) fn is_marked_for_deletion(&self) -> bool {
        self.droppable.marked_for_deletion.get()
    }

    pub(crate) fn is_deleted(&self) -> bool {
        self.droppable.marked_for_deletion.get() && !self.is_attached()
    }

    pub(crate) fn is_attached(&self) -> bool {
        self.attached_counter.get() > 0
    }

    pub(crate) fn increment_attached_counter(&self) {
        self.attached_counter.set(self.attached_counter.get() + 1);
    }

    pub(crate) fn decrement_attached_counter(&self) {
        assert!(self.attached_counter.get() > 0);
        self.attached_counter.set(self.attached_counter.get() - 1);
    }

    /// glGetShaderInfoLog
    pub(crate) fn info_log(&self) -> DOMString {
        match self.compile_job() {
            Some(job) => job.outcome().info_log.clone().into(),
            None => DOMString::new(),
        }
    }

    /// Get the shader source
    pub(crate) fn source(&self) -> DOMString {
        self.source.borrow().clone()
    }

    /// glShaderSource
    pub(crate) fn set_source(&self, source: DOMString) {
        *self.source.borrow_mut() = source;
    }

    pub(crate) fn successfully_compiled(&self) -> bool {
        self.compile_job()
            .is_some_and(|job| job.outcome().succeeded)
    }

    /// COMPLETION_STATUS_KHR: whether the last compileShader has finished, without waiting.
    pub(crate) fn compile_completed(&self) -> bool {
        self.compile_job().is_none_or(|job| job.completed())
    }

    /// The last compileShader, for linkProgram.
    pub(crate) fn compile_job(&self) -> Option<Rc<CompileJob>> {
        self.compile_job.borrow().clone()
    }
}
