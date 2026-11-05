/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://www.khronos.org/registry/webgl/specs/latest/1.0/webgl.idl
use std::cell::Cell;
use std::os::raw::c_int;
use std::rc::Rc;
use std::sync::Once;

use dom_struct::dom_struct;
use js::context::JSContext;
use mozangle::shaders::{BuiltInResources, CompileOptions, Output, ShaderValidator};
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_weak_referenceable_dom_object;
use script_bindings::weakref::WeakRef;
use servo_canvas_traits::webgl::{
    GLLimits, GlType, WebGLCommand, WebGLError, WebGLResult, WebGLSLVersion, WebGLShaderId,
    WebGLVersion, webgl_channel,
};
#[cfg(feature = "webgl")]
use {
    crate::dom::webgl::extensions::WebGLExtensions,
    crate::dom::webgl::extensions::extfragdepth::EXTFragDepth,
    crate::dom::webgl::extensions::extshadertexturelod::EXTShaderTextureLod,
    crate::dom::webgl::extensions::oesstandardderivatives::OESStandardDerivatives,
    crate::dom::webgl::extensions::webglmultidraw::WEBGLMultiDraw,
    crate::dom::webgl::extensions::webglmultidrawinstancedbasevertexbaseinstance::WEBGLMultiDrawInstancedBaseVertexBaseInstance,
    crate::dom::webgl::webglobject::WebGLObject,
    crate::dom::webgl::webglrenderingcontext::{Operation, WebGLRenderingContext},
    crate::dom::webglrenderingcontext::capture_webgl_backtrace,
};

use crate::dom::bindings::codegen::Bindings::WebGLRenderingContextBinding::WebGLRenderingContextConstants as constants;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;

#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf, PartialEq)]
pub(crate) enum ShaderCompilationStatus {
    NotCompiled,
    Succeeded,
    Failed,
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
    info_log: DomRefCell<DOMString>,
    attached_counter: Cell<u32>,
    compilation_status: Cell<ShaderCompilationStatus>,
    /// A compiled fragment shader's outputs by location (None where nothing is declared), for
    /// the WebGL 2 draw check against the draw buffers. None when they couldn't be read.
    fragment_outputs: DomRefCell<Option<Box<[Option<FragOutputKind>]>>>,
    /// A compiled ESSL 3.00 vertex shader's declared inputs, active or not (see
    /// `parse_vertex_inputs`), for the link-time aliasing check. None when unknown.
    vertex_inputs: DomRefCell<Option<Box<[VertexInput]>>>,
    droppable: DroppableWebGLShader,
}

/// A vertex shader input as declared in the source.
#[derive(Clone, Debug, JSTraceable, MallocSizeOf)]
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

static GLSLANG_INITIALIZATION: Once = Once::new();

impl WebGLShader {
    fn new_inherited(context: &WebGLRenderingContext, id: WebGLShaderId, shader_type: u32) -> Self {
        GLSLANG_INITIALIZATION.call_once(|| ::mozangle::shaders::initialize().unwrap());
        Self {
            webgl_object: WebGLObject::new_inherited(context),
            gl_type: shader_type,
            source: Default::default(),
            info_log: Default::default(),
            attached_counter: Cell::new(0),
            compilation_status: Cell::new(ShaderCompilationStatus::NotCompiled),
            fragment_outputs: Default::default(),
            vertex_inputs: Default::default(),
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
        let (sender, receiver) = webgl_channel().unwrap();
        context.send_command(WebGLCommand::CreateShader(shader_type, sender));
        receiver
            .recv()
            .unwrap()
            .map(|id| WebGLShader::new(cx, context, id, shader_type))
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
        api_type: GlType,
        webgl_version: WebGLVersion,
        glsl_version: WebGLSLVersion,
        limits: &GLLimits,
        ext: &WebGLExtensions,
    ) -> WebGLResult<()> {
        if self.droppable.marked_for_deletion.get() && !self.is_attached() {
            return Err(WebGLError::InvalidValue);
        }
        if self.compilation_status.get() != ShaderCompilationStatus::NotCompiled {
            debug!("Compiling already compiled shader {}", self.id());
        }

        let source = self.source.borrow();

        let mut params = BuiltInResources {
            MaxVertexAttribs: limits.max_vertex_attribs as c_int,
            MaxVertexUniformVectors: limits.max_vertex_uniform_vectors as c_int,
            MaxVertexTextureImageUnits: limits.max_vertex_texture_image_units as c_int,
            MaxCombinedTextureImageUnits: limits.max_combined_texture_image_units as c_int,
            MaxTextureImageUnits: limits.max_texture_image_units as c_int,
            MaxFragmentUniformVectors: limits.max_fragment_uniform_vectors as c_int,

            MaxVertexOutputVectors: limits.max_vertex_output_vectors as c_int,
            MaxFragmentInputVectors: limits.max_fragment_input_vectors as c_int,
            MaxVaryingVectors: limits.max_varying_vectors as c_int,

            OES_standard_derivatives: ext.is_enabled::<OESStandardDerivatives>() as c_int,
            EXT_shader_texture_lod: ext.is_enabled::<EXTShaderTextureLod>() as c_int,
            EXT_frag_depth: ext.is_enabled::<EXTFragDepth>() as c_int,
            // gl_DrawID; the translator turns it into the uniform angle_DrawID (emulateGLDrawID).
            ANGLE_multi_draw: (ext.is_enabled::<WEBGLMultiDraw>() ||
                ext.is_enabled::<WEBGLMultiDrawInstancedBaseVertexBaseInstance>())
                as c_int,

            FragmentPrecisionHigh: 1,
            ..Default::default()
        };

        if webgl_version == WebGLVersion::WebGL2 {
            params.MinProgramTexelOffset = limits.min_program_texel_offset as c_int;
            params.MaxProgramTexelOffset = limits.max_program_texel_offset as c_int;
            params.MaxDrawBuffers = limits.max_draw_buffers as c_int;
        }

        let validator = match webgl_version {
            WebGLVersion::WebGL1 => {
                let output_format = if api_type == GlType::Gles {
                    Output::Essl
                } else {
                    Output::Glsl
                };
                ShaderValidator::for_webgl(self.gl_type, output_format, &params).unwrap()
            },
            WebGLVersion::WebGL2 => {
                let output_format = if api_type == GlType::Gles {
                    Output::Essl
                } else {
                    match (glsl_version.major, glsl_version.minor) {
                        (1, 30) => Output::Glsl130,
                        (1, 40) => Output::Glsl140,
                        (1, 50) => Output::Glsl150Core,
                        (3, 30) => Output::Glsl330Core,
                        (4, 0) => Output::Glsl400Core,
                        (4, 10) => Output::Glsl410Core,
                        (4, 20) => Output::Glsl420Core,
                        (4, 30) => Output::Glsl430Core,
                        (4, 40) => Output::Glsl440Core,
                        (4, _) => Output::Glsl450Core,
                        _ => Output::Glsl140,
                    }
                };
                ShaderValidator::for_webgl2(self.gl_type, output_format, &params).unwrap()
            },
        };

        // Replicating
        // https://searchfox.org/mozilla-esr115/rev/f1fb0868dc63b89ccf9eea157960d1ec27fb55a2/dom/canvas/WebGLShaderValidator.cpp#29
        let mut options = CompileOptions::mozangle();
        options.set_variables(1);
        options.set_enforcePackingRestrictions(1);
        options.set_objectCode(1);
        options.set_initGLPosition(1);
        options.set_initializeUninitializedLocals(1);
        options.set_initOutputVariables(1);

        options.set_limitExpressionComplexity(1);
        options.set_limitCallStackDepth(1);
        // gl_DrawID as a uniform the draw calls set (WEBGL_multi_draw).
        options.set_emulateGLDrawID(1);

        if cfg!(target_os = "macos") {
            options.set_removeInvariantAndCentroidForESSL3(1);

            // Work around https://bugs.webkit.org/show_bug.cgi?id=124684,
            // https://chromium.googlesource.com/angle/angle/+/5e70cf9d0b1bb
            options.set_unfoldShortCircuit(1);
            // Work around that Mac drivers handle struct scopes incorrectly.
            options.set_regenerateStructNames(1);
            // TODO: Only apply this workaround to Intel hardware
            // Work around that Intel drivers on Mac OSX handle for-loop incorrectly.
            options.set_addAndTrueToLoopCondition(1);
            options.set_rewriteTexelFetchOffsetToTexelFetch(1);
        } else {
            // We want to do this everywhere, but to do this on Mac, we need
            // to do it only on Mac OSX > 10.6 as this causes the shader
            // compiler in 10.6 to crash
            options.set_clampIndirectArrayBounds(1);
        }

        match validator.compile(&[&source.str()], options) {
            Ok(()) => {
                let translated_source = validator.object_code();
                debug!("Shader translated: {}", translated_source);
                *self.fragment_outputs.borrow_mut() = if self.gl_type == constants::FRAGMENT_SHADER
                {
                    parse_fragment_outputs(&translated_source, is_essl3(&source.str()))
                } else {
                    None
                };
                *self.vertex_inputs.borrow_mut() =
                    if self.gl_type == constants::VERTEX_SHADER && is_essl3(&source.str()) {
                        parse_vertex_inputs(&source.str())
                    } else {
                        None
                    };
                // NOTE: At this point we should be pretty sure that the compilation in the paint thread
                // will succeed.
                // It could be interesting to retrieve the info log from the paint thread though
                self.upcast()
                    .send_command(WebGLCommand::CompileShader(self.id(), translated_source));
                self.compilation_status
                    .set(ShaderCompilationStatus::Succeeded);
            },
            Err(error) => {
                *self.fragment_outputs.borrow_mut() = None;
                *self.vertex_inputs.borrow_mut() = None;
                self.compilation_status.set(ShaderCompilationStatus::Failed);
                debug!("Shader {} compilation failed: {}", self.id(), error);
            },
        }

        *self.info_log.borrow_mut() = validator.info_log().into();

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
        self.info_log.borrow().clone()
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
        self.compilation_status.get() == ShaderCompilationStatus::Succeeded
    }

    /// The last successful compile's fragment outputs by location (see
    /// `parse_fragment_outputs`); None for vertex shaders or when they couldn't be read.
    pub(crate) fn vertex_inputs(&self) -> Option<Box<[VertexInput]>> {
        self.vertex_inputs.borrow().clone()
    }

    pub(crate) fn fragment_outputs(&self) -> Option<Box<[Option<FragOutputKind>]>> {
        self.fragment_outputs.borrow().clone()
    }
}
