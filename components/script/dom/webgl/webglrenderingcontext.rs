/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::{cmp, ptr};

#[cfg(feature = "webgl_backtrace")]
use backtrace::Backtrace;
use bitflags::bitflags;
use dom_struct::dom_struct;
use euclid::default::{Point2D, Rect, Size2D};
use js::context::{JSContext, NoGC};
use js::conversions::ToJSValConvertible;
use js::jsapi::{JSObject, JSTracer, Type};
use js::jsval::{BooleanValue, DoubleValue, Int32Value, NullValue, ObjectValue, UInt32Value};
use js::rust::{CustomAutoRooterGuard, MutableHandleObject, MutableHandleValue};
use js::typedarray::{ArrayBufferView, Float32, Int32, TypedArrayElementCreator, Uint32};
use pixels::{self, Alpha, PixelFormat, Snapshot, SnapshotPixelFormat};
use script_bindings::cell::{DomRefCell, Ref, RefMut};
use script_bindings::reflector::{
    AssociatedMemory, Reflector, reflect_weak_referenceable_dom_object,
};
use serde::{Deserialize, Serialize};
use servo_base::generic_channel::{GenericReceiver, GenericSharedMemory};
use servo_base::{Epoch, generic_channel};
use servo_canvas_traits::webgl::WebGLError::*;
use servo_canvas_traits::webgl::{
    AlphaTreatment, CompressedTexData, GLContextAttributes, GLLimits, GlType, Parameter,
    SizedDataType, TexDataType, TexFormat, TexParameter, WebGLCommand, WebGLCommandBacktrace,
    WebGLContextId, WebGLError, WebGLFramebufferBindingRequest, WebGLMsg, WebGLMsgSender,
    WebGLObjectNameKind, WebGLProgramId, WebGLResult, WebGLVersion, WebGLVertexArrayId,
    YAxisTreatment,
    webgl_channel,
};
use servo_config::pref;
use webrender_api::ImageKey;

use crate::canvas_context::{CanvasContext, HTMLCanvasElementOrOffscreenCanvas};
#[cfg(feature = "webxr")]
use crate::dom::RootedPromise;
use crate::dom::bindings::buffer_source::{
    array_buffer_view_bytes, array_buffer_view_bytes_mut, create_buffer_source,
    get_buffer_source_slice, typed_array_to_vec,
};
use crate::dom::bindings::codegen::Bindings::ANGLEInstancedArraysBinding::ANGLEInstancedArraysConstants;
use crate::dom::bindings::codegen::Bindings::EXTBlendMinmaxBinding::EXTBlendMinmaxConstants;
use crate::dom::bindings::codegen::Bindings::KHRParallelShaderCompileBinding::KHRParallelShaderCompileConstants;
use crate::dom::bindings::codegen::Bindings::OESVertexArrayObjectBinding::OESVertexArrayObjectConstants;
use crate::dom::bindings::codegen::Bindings::WebGL2RenderingContextBinding::WebGL2RenderingContextConstants;
use crate::dom::bindings::codegen::Bindings::WebGLRenderingContextBinding::{
    TexImageSource, WebGLContextAttributes, WebGLRenderingContextConstants as constants,
    WebGLRenderingContextMethods,
};
use crate::dom::bindings::codegen::UnionTypes::{
    Float32ArrayOrUnrestrictedFloatSequence,
    HTMLCanvasElementOrOffscreenCanvas as RootedHTMLCanvasElementOrOffscreenCanvas,
    Int32ArrayOrLongSequence, MaybeSharedArrayBufferViewOrMaybeSharedArrayBuffer,
};
use crate::dom::bindings::conversions::DerivedFrom;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{DomOnceCell, DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::trace::JSTraceable;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::globalscope::GlobalScope;
use crate::dom::node::NodeTraits;
#[cfg(feature = "webxr")]
use crate::dom::promise::Promise;
use crate::dom::webgl::extensions::WebGLExtensions;
use crate::dom::webgl::validations::WebGLValidator;
use crate::dom::webgl::validations::tex_image_2d::{
    CommonCompressedTexImage2DValidatorResult, CommonTexImage2DValidator,
    CommonTexImage2DValidatorResult, CompressedTexImage2DValidator,
    CompressedTexSubImage2DValidator, TexImage2DValidator, TexImage2DValidatorResult,
};
use crate::dom::webgl::validations::types::TexImageTarget;
use crate::dom::webgl::vertexarrayobject::VertexAttribData;
use crate::dom::webgl::webgl2renderingcontext::WebGL2RenderingContext;
use crate::dom::webgl::webglactiveinfo::WebGLActiveInfo;
use crate::dom::webgl::webglbuffer::WebGLBuffer;
use crate::dom::webgl::webglcontextevent::WebGLContextEvent;
use crate::dom::webgl::webglframebuffer::{
    AttachedImage, CompleteForRendering, WebGLFramebuffer, WebGLFramebufferAttachmentRoot,
};
use crate::dom::webgl::webglobject::WebGLObject;
use crate::dom::webgl::webglprogram::WebGLProgram;
use crate::dom::webgl::webglrenderbuffer::WebGLRenderbuffer;
use crate::dom::webgl::webglshader::WebGLShader;
use crate::dom::webgl::webglshaderprecisionformat::WebGLShaderPrecisionFormat;
use crate::dom::webgl::webgltexture::{TexParameterValue, WebGLTexture};
use crate::dom::webgl::webgluniformlocation::WebGLUniformLocation;
use crate::dom::webgl::webglvertexarrayobject::WebGLVertexArrayObject;
use crate::dom::webgl::webglvertexarrayobjectoes::WebGLVertexArrayObjectOES;
use crate::dom::window::Window;

fn has_invalid_blend_constants(arg1: u32, arg2: u32) -> bool {
    match (arg1, arg2) {
        (constants::CONSTANT_COLOR, constants::CONSTANT_ALPHA) => true,
        (constants::ONE_MINUS_CONSTANT_COLOR, constants::ONE_MINUS_CONSTANT_ALPHA) => true,
        (constants::ONE_MINUS_CONSTANT_COLOR, constants::CONSTANT_ALPHA) => true,
        (constants::CONSTANT_COLOR, constants::ONE_MINUS_CONSTANT_ALPHA) => true,
        (_, _) => false,
    }
}

/// Whether `factor` is a blend factor of core WebGL (the SRC1_* ones need
/// WEBGL_blend_func_extended, which isn't exposed). SRC_ALPHA_SATURATE is a source factor only,
/// except in WebGL 2 (ES 3.0).
fn is_blend_factor(factor: u32, is_dest: bool, webgl2: bool) -> bool {
    match factor {
        constants::ZERO |
        constants::ONE |
        constants::SRC_COLOR |
        constants::ONE_MINUS_SRC_COLOR |
        constants::DST_COLOR |
        constants::ONE_MINUS_DST_COLOR |
        constants::SRC_ALPHA |
        constants::ONE_MINUS_SRC_ALPHA |
        constants::DST_ALPHA |
        constants::ONE_MINUS_DST_ALPHA |
        constants::CONSTANT_COLOR |
        constants::ONE_MINUS_CONSTANT_COLOR |
        constants::CONSTANT_ALPHA |
        constants::ONE_MINUS_CONSTANT_ALPHA => true,
        constants::SRC_ALPHA_SATURATE => !is_dest || webgl2,
        _ => false,
    }
}

pub(crate) fn uniform_get<T, F>(triple: (&WebGLRenderingContext, WebGLProgramId, i32), f: F) -> T
where
    F: FnOnce(WebGLProgramId, i32, generic_channel::GenericSender<T>) -> WebGLCommand,
    T: for<'de> Deserialize<'de> + Serialize,
{
    let (sender, receiver) = webgl_channel().unwrap();
    triple.0.send_command(f(triple.1, triple.2, sender));
    receiver.recv().unwrap()
}

pub(crate) fn uniform_typed<T>(
    cx: &mut JSContext,
    value: &[T::Element],
    mut retval: MutableHandleValue,
) where
    T: TypedArrayElementCreator + 'static,
{
    rooted!(&in(cx) let mut rval = ptr::null_mut::<JSObject>());
    create_buffer_source::<T>(cx, value, rval.handle_mut()).unwrap();
    retval.set(ObjectValue(rval.get()));
}

/// Set of bitflags for texture unpacking (texImage2d, etc...)
#[derive(Clone, Copy, JSTraceable, MallocSizeOf)]
pub(crate) struct TextureUnpacking(u8);

bitflags! {
    impl TextureUnpacking: u8 {
        const FLIP_Y_AXIS = 0x01;
        const PREMULTIPLY_ALPHA = 0x02;
        const CONVERT_COLORSPACE = 0x04;
    }
}

#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf)]
pub(crate) enum VertexAttrib {
    Float(f32, f32, f32, f32),
    Int(i32, i32, i32, i32),
    Uint(u32, u32, u32, u32),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Operation {
    Fallible,
    Infallible,
}

#[derive(JSTraceable, MallocSizeOf)]
struct DroppableWebGLRenderingContext {
    #[no_trace]
    webgl_sender: WebGLMsgSender,
}

impl Drop for DroppableWebGLRenderingContext {
    fn drop(&mut self) {
        LIVE_CONTEXTS.with(|live| live.set(live.get().saturating_sub(1)));
        let _ = self.webgl_sender.send_remove();
    }
}

/// Live WebGL contexts made by this script thread, and the count at which the next context
/// creation collects garbage first. A context is a whole GL context in the WebGL thread (on
/// llvmpipe, megabytes each) but reports only its drawing buffer to the GC, so a page that makes
/// many small throwaway canvases (the WebGPU CTS copy_contents_from_gl_context_canvas: thousands)
/// never triggers a GC and holds them all: ~25 GB in a minute. Browsers bound this by losing the
/// oldest context; until context loss exists here, a GC past the threshold frees the unreachable
/// ones (foreground finalizers send RemoveContext at once). The threshold doubles over the
/// contexts still live after it, so a page that really keeps many pays one GC per doubling.
const MIN_CONTEXTS_BEFORE_GC: usize = 16;
thread_local! {
    static LIVE_CONTEXTS: Cell<usize> = const { Cell::new(0) };
    static NEXT_GC_AT: Cell<usize> = const { Cell::new(MIN_CONTEXTS_BEFORE_GC) };
}

#[expect(unsafe_code)]
fn collect_dead_contexts_if_many(cx: &mut JSContext) {
    if LIVE_CONTEXTS.with(Cell::get) < NEXT_GC_AT.with(Cell::get) {
        return;
    }
    unsafe { js::rust::wrappers2::JS_GC(cx, js::jsapi::GCReason::API) };
    let live = LIVE_CONTEXTS.with(Cell::get);
    NEXT_GC_AT.with(|next| next.set((live * 2).max(MIN_CONTEXTS_BEFORE_GC)));
}

#[dom_struct]
pub(crate) struct WebGLRenderingContext {
    reflector_: Reflector<AssociatedMemory>,
    #[no_trace]
    webgl_version: WebGLVersion,
    #[ignore_malloc_size_of = "Defined in surfman"]
    #[no_trace]
    limits: GLLimits,
    canvas: HTMLCanvasElementOrOffscreenCanvas,
    #[ignore_malloc_size_of = "Defined in servo_canvas_traits"]
    #[no_trace]
    last_error: Cell<Option<WebGLError>>,
    texture_packing_alignment: Cell<u8>,
    texture_unpacking_settings: Cell<TextureUnpacking>,
    // TODO(nox): Should be Cell<u8>.
    texture_unpacking_alignment: Cell<u32>,
    bound_draw_framebuffer: MutNullableDom<WebGLFramebuffer>,
    // TODO(mmatyas): This was introduced in WebGL2, but listed here because it's used by
    // Textures and Renderbuffers, but such WebGLObjects have access only to the GL1 context.
    bound_read_framebuffer: MutNullableDom<WebGLFramebuffer>,
    bound_renderbuffer: MutNullableDom<WebGLRenderbuffer>,
    bound_buffer_array: MutNullableDom<WebGLBuffer>,
    current_program: MutNullableDom<WebGLProgram>,
    /// The WebGL 2 context wrapping this one, for extensions whose draws need the WebGL 2
    /// checks (WEBGL_multi_draw). None for WebGL 1.
    webgl2: MutNullableDom<WebGL2RenderingContext>,
    current_vertex_attribs: DomRefCell<Box<[VertexAttrib]>>,
    #[ignore_malloc_size_of = "Because it's small"]
    current_scissor: Cell<(i32, i32, u32, u32)>,
    #[ignore_malloc_size_of = "Because it's small"]
    current_clear_color: Cell<(f32, f32, f32, f32)>,
    #[no_trace]
    size: Cell<Size2D<u32>>,
    /// Partii (perf): object names reserved on the WebGL thread, handed out by create{Buffer, Framebuffer,
    /// Renderbuffer, Texture} without a round trip each (take_object_name).
    #[ignore_malloc_size_of = "Channels and a few names"]
    object_names: DomRefCell<ObjectNamePools>,
    /// Partii (perf): the draw-state generation (DrawStateGen), shared with every object of this
    /// context.
    #[ignore_malloc_size_of = "A shared counter"]
    #[no_trace]
    draw_state_gen: DrawStateGen,
    extension_manager: WebGLExtensions,
    capabilities: Capabilities,
    /// Front and back stencil reference and masks, for the draw-time check that they agree
    /// (WebGL 1.0 6.11).
    #[ignore_malloc_size_of = "Because it's small"]
    #[no_trace]
    stencil_state: Cell<StencilState>,
    /// Whether the default framebuffer was asked for a stencil buffer.
    default_framebuffer_stencil: bool,
    default_vao: DomOnceCell<WebGLVertexArrayObjectOES>,
    current_vao: MutNullableDom<WebGLVertexArrayObjectOES>,
    default_vao_webgl2: DomOnceCell<WebGLVertexArrayObject>,
    current_vao_webgl2: MutNullableDom<WebGLVertexArrayObject>,
    textures: Textures,
    #[no_trace]
    api_type: GlType,
    droppable: DroppableWebGLRenderingContext,
}

impl WebGLRenderingContext {
    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    pub(crate) fn new_inherited(
        global: &GlobalScope,
        canvas: HTMLCanvasElementOrOffscreenCanvas,
        webgl_version: WebGLVersion,
        size: Size2D<u32>,
        attrs: GLContextAttributes,
    ) -> Result<WebGLRenderingContext, String> {
        if pref!(webgl_testing_context_creation_error) {
            return Err("WebGL context creation error forced by pref `webgl.testing.context_creation_error`".into());
        }

        // A Window, or a worker drawing into an OffscreenCanvas.
        let webgl_chan = match global.webgl_chan() {
            Some(chan) => chan,
            None => return Err("WebGL initialization failed early on".into()),
        };
        let Some(webview_id) = global.webview_id() else {
            return Err("WebGL needs a Window or a dedicated or shared worker".into());
        };

        let default_framebuffer_stencil = attrs.stencil;
        let (sender, receiver) = webgl_channel().unwrap();
        webgl_chan
            .send(WebGLMsg::CreateContext(
                webview_id.into(),
                webgl_version,
                size,
                attrs,
                sender,
            ))
            .unwrap();
        let result = receiver.recv().unwrap();

        result.map(|ctx_data| {
            let max_combined_texture_image_units = ctx_data.limits.max_combined_texture_image_units;
            let max_vertex_attribs = ctx_data.limits.max_vertex_attribs as usize;
            Self {
                reflector_: Reflector::new(),
                webgl_version,
                limits: ctx_data.limits,
                canvas,
                last_error: Cell::new(None),
                texture_packing_alignment: Cell::new(4),
                texture_unpacking_settings: Cell::new(TextureUnpacking::CONVERT_COLORSPACE),
                texture_unpacking_alignment: Cell::new(4),
                bound_draw_framebuffer: MutNullableDom::new(None),
                bound_read_framebuffer: MutNullableDom::new(None),
                bound_buffer_array: MutNullableDom::new(None),
                bound_renderbuffer: MutNullableDom::new(None),
                current_program: MutNullableDom::new(None),
                webgl2: MutNullableDom::new(None),
                current_vertex_attribs: DomRefCell::new(
                    vec![VertexAttrib::Float(0f32, 0f32, 0f32, 1f32); max_vertex_attribs].into(),
                ),
                current_scissor: Cell::new((0, 0, size.width, size.height)),
                // FIXME(#21718) The backend is allowed to choose a size smaller than
                // what was requested
                size: Cell::new(size),
                current_clear_color: Cell::new((0.0, 0.0, 0.0, 0.0)),
                extension_manager: WebGLExtensions::new(
                    webgl_version,
                    ctx_data.api_type,
                    ctx_data.glsl_version,
                ),
                object_names: Default::default(),
                draw_state_gen: Default::default(),
                capabilities: Default::default(),
                stencil_state: Default::default(),
                default_framebuffer_stencil,
                default_vao: Default::default(),
                current_vao: Default::default(),
                default_vao_webgl2: Default::default(),
                current_vao_webgl2: Default::default(),
                textures: Textures::new(max_combined_texture_image_units),
                api_type: ctx_data.api_type,
                droppable: DroppableWebGLRenderingContext {
                    webgl_sender: {
                        LIVE_CONTEXTS.with(|live| live.set(live.get() + 1));
                        ctx_data.sender
                    },
                },
            }
        })
    }

    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        canvas: &RootedHTMLCanvasElementOrOffscreenCanvas,
        webgl_version: WebGLVersion,
        size: Size2D<u32>,
        attrs: GLContextAttributes,
    ) -> Option<DomRoot<WebGLRenderingContext>> {
        collect_dead_contexts_if_many(cx);
        match WebGLRenderingContext::new_inherited(
            global,
            HTMLCanvasElementOrOffscreenCanvas::from(canvas),
            webgl_version,
            size,
            attrs,
        ) {
            Ok(ctx) => Some(reflect_weak_referenceable_dom_object(
                cx,
                Rc::new(ctx),
                global,
            )),
            Err(msg) => {
                error!("Couldn't create WebGLRenderingContext: {}", msg);
                let event = WebGLContextEvent::new(
                    cx,
                    global,
                    atom!("webglcontextcreationerror"),
                    EventBubbles::DoesNotBubble,
                    EventCancelable::Cancelable,
                    DOMString::from(msg),
                );
                match canvas {
                    RootedHTMLCanvasElementOrOffscreenCanvas::HTMLCanvasElement(canvas) => {
                        event.upcast::<Event>().fire(cx, canvas.upcast());
                    },
                    RootedHTMLCanvasElementOrOffscreenCanvas::OffscreenCanvas(canvas) => {
                        event.upcast::<Event>().fire(cx, canvas.upcast());
                    },
                }
                None
            },
        }
    }

    pub(crate) fn set_image_key(&self, image_key: ImageKey) {
        self.droppable.webgl_sender.set_image_key(image_key);
    }

    pub(crate) fn update_rendering(&self, canvas_epoch: Epoch) -> bool {
        if !self.onscreen() {
            return false;
        }
        self.swap_buffers(Some(canvas_epoch))
    }

    /// Presents the drawing buffer: the WebGL thread swaps it to the front and, if this
    /// context has an image key, updates that image (tagged `canvas_epoch`). This works on
    /// any thread; an OffscreenCanvas uses it to update its placeholder canvas element.
    pub(crate) fn swap_buffers(&self, canvas_epoch: Option<Epoch>) -> bool {
        let Some(webgl_chan) = self.global().webgl_chan() else {
            return false;
        };
        webgl_chan
            .send(WebGLMsg::SwapBuffers(
                vec![self.context_id()],
                canvas_epoch,
                0, /* time */
            ))
            .is_ok()
    }

    pub(crate) fn webgl_version(&self) -> WebGLVersion {
        self.webgl_version
    }

    pub(crate) fn limits(&self) -> &GLLimits {
        &self.limits
    }

    pub(crate) fn texture_unpacking_alignment(&self) -> u32 {
        self.texture_unpacking_alignment.get()
    }

    pub(crate) fn bound_renderbuffer(&self) -> Option<DomRoot<WebGLRenderbuffer>> {
        self.bound_renderbuffer.get()
    }

    pub(crate) fn bound_draw_framebuffer(&self) -> Option<DomRoot<WebGLFramebuffer>> {
        self.bound_draw_framebuffer.get()
    }

    pub(crate) fn current_vao(&self, cx: &mut JSContext) -> DomRoot<WebGLVertexArrayObjectOES> {
        self.current_vao.or_init(|| {
            DomRoot::from_ref(
                self.default_vao
                    .init_once(|| WebGLVertexArrayObjectOES::new(cx, self, None)),
            )
        })
    }

    pub(crate) fn current_vao_webgl2(&self, cx: &mut JSContext) -> DomRoot<WebGLVertexArrayObject> {
        self.current_vao_webgl2.or_init(|| {
            DomRoot::from_ref(
                self.default_vao_webgl2
                    .init_once(|| WebGLVertexArrayObject::new(cx, self, None)),
            )
        })
    }

    /// Whether the bound WebGL 2 vertex array binds `buffer` (without creating the default
    /// vertex array, which binds nothing until it exists).
    pub(crate) fn current_vao_webgl2_binds_buffer(&self, buffer: &WebGLBuffer) -> bool {
        match self.current_vao_webgl2.get() {
            Some(vao) => vao.binds_buffer(buffer),
            None => self
                .default_vao_webgl2
                .get()
                .is_some_and(|vao| vao.binds_buffer(buffer)),
        }
    }

    pub(crate) fn current_vertex_attribs(&self) -> RefMut<'_, Box<[VertexAttrib]>> {
        self.current_vertex_attribs.borrow_mut()
    }

    #[inline]
    pub(crate) fn sender(&self) -> &WebGLMsgSender {
        &self.droppable.webgl_sender
    }

    #[inline]
    pub(crate) fn send_with_fallibility(&self, command: WebGLCommand, fallibility: Operation) {
        self.draw_state_gen.note(&command);
        let result = self
            .droppable
            .webgl_sender
            .send(command, capture_webgl_backtrace());
        if matches!(fallibility, Operation::Infallible) {
            result.expect("Operation failed");
        }
    }

    pub(crate) fn draw_state_gen(&self) -> &DrawStateGen {
        &self.draw_state_gen
    }

    /// Partii (perf): whether the current program's draw-time state checks (the ones that read no
    /// draw argument) passed with the bound vertex array and nothing that could change their outcome
    /// sent since (DrawStateGen). Texture and sampler bindings don't move the generation: each
    /// texture unit holds the binding sequence number of its last change (Textures), and a record
    /// made before a change to a unit the program samples asks for the sampler check alone
    /// (WebGL 2) or for every check (WebGL 1, whose feedback-loop check reads the bindings).
    #[inline]
    pub(crate) fn draw_state_cached(&self, cx: &mut JSContext) -> DrawStateCache {
        if !draw_cache_enabled() {
            return DrawStateCache::Miss;
        }
        let Some(program) = self.current_program.get() else {
            return DrawStateCache::Miss;
        };
        let generation = self.draw_state_gen.get();
        if self.webgl_version() == WebGLVersion::WebGL1 {
            let vao = self.current_vao(cx);
            let Some(validated_at) = vao.draw_validated(generation, program.id()) else {
                return DrawStateCache::Miss;
            };
            let sequence = self.textures.binding_sequence();
            if validated_at != sequence {
                if program
                    .draw_sampler_units()
                    .iter()
                    .any(|&unit| self.textures.unit_changed_since(unit, validated_at))
                {
                    return DrawStateCache::Miss;
                }
                vao.set_draw_validated(generation, program.id(), sequence);
            }
            return DrawStateCache::Hit;
        }
        let vao = self.current_vao_webgl2(cx);
        let Some(validated_at) = vao.draw_validated(generation, program.id()) else {
            return DrawStateCache::Miss;
        };
        let sequence = self.textures.binding_sequence();
        if validated_at == sequence {
            return DrawStateCache::Hit;
        }
        if program
            .draw_sampler_units()
            .iter()
            .any(|&unit| self.textures.unit_changed_since(unit, validated_at))
        {
            return DrawStateCache::Samplers;
        }
        // Only units this program doesn't sample changed: still valid, from now on too.
        vao.set_draw_validated(generation, program.id(), sequence);
        DrawStateCache::Hit
    }

    /// Partii (perf): records that every draw-time state check passed for `program` with the bound
    /// vertex array (see draw_state_cached). Not while the draw framebuffer belongs to an XR
    /// session, whose completeness depends on the frame.
    pub(crate) fn mark_draw_state_valid(&self, cx: &mut JSContext, program: &WebGLProgram) {
        if !draw_cache_enabled() ||
            self.bound_draw_framebuffer
                .get()
                .is_some_and(|fb| fb.is_in_xr_session())
        {
            return;
        }
        let generation = self.draw_state_gen.get();
        let sequence = self.textures.binding_sequence();
        match self.webgl_version() {
            WebGLVersion::WebGL1 => {
                self.current_vao(cx)
                    .set_draw_validated(generation, program.id(), sequence)
            },
            WebGLVersion::WebGL2 => {
                self.current_vao_webgl2(cx)
                    .set_draw_validated(generation, program.id(), sequence)
            },
        }
    }

    #[inline]
    /// Partii (perf): a new object name of `kind` from the reserved batch. createTexture and friends used to
    /// wait for a reply from the WebGL thread each, behind everything queued before them (Phaser's boot: ~200
    /// such waits). Names come in batches of object_name_batch() (the same glGen* calls), the next batch asked
    /// for when half the current one is used, so a create waits only when a whole batch went in between.
    /// None when the WebGL thread has none to give (as the Create* commands answer None).
    pub(crate) fn take_object_name(&self, kind: WebGLObjectNameKind) -> Option<NonZeroU32> {
        let batch = object_name_batch();
        let mut pools = self.object_names.borrow_mut();
        let pool = &mut pools.0[kind as usize];
        if pool.names.is_empty() {
            match pool.pending.take() {
                Some(receiver) => pool.names.extend(receiver.recv().unwrap_or_default()),
                None => {
                    let (sender, receiver) = webgl_channel().unwrap();
                    self.send_command(WebGLCommand::GenObjectNames(kind, batch, sender));
                    pool.names.extend(receiver.recv().unwrap_or_default());
                },
            }
        }
        let name = pool.names.pop_front().and_then(NonZeroU32::new)?;
        if batch > 1 && pool.names.len() < batch as usize / 2 && pool.pending.is_none() {
            let (sender, receiver) = webgl_channel().unwrap();
            self.send_command(WebGLCommand::GenObjectNames(kind, batch, sender));
            pool.pending = Some(receiver);
        }
        Some(name)
    }

    pub(crate) fn send_command(&self, command: WebGLCommand) {
        self.send_with_fallibility(command, Operation::Infallible);
    }

    pub(crate) fn send_command_ignored(&self, command: WebGLCommand) {
        self.send_with_fallibility(command, Operation::Fallible);
    }

    pub(crate) fn webgl_error(&self, err: WebGLError) {
        // TODO(emilio): Add useful debug messages to this
        warn!(
            "WebGL error: {:?}, previous error was {:?}",
            err,
            self.last_error.get()
        );

        // If an error has been detected no further errors must be
        // recorded until `getError` has been called
        if self.last_error.get().is_none() {
            self.last_error.set(Some(err));
        }
    }

    // Helper function for validating framebuffer completeness in
    // calls touching the framebuffer.  From the GLES 2.0.25 spec,
    // page 119:
    //
    //    "Effects of Framebuffer Completeness on Framebuffer
    //     Operations
    //
    //     If the currently bound framebuffer is not framebuffer
    //     complete, then it is an error to attempt to use the
    //     framebuffer for writing or reading. This means that
    //     rendering commands such as DrawArrays and DrawElements, as
    //     well as commands that read the framebuffer such as
    //     ReadPixels and CopyTexSubImage, will generate the error
    //     INVALID_FRAMEBUFFER_OPERATION if called while the
    //     framebuffer is not framebuffer complete."
    //
    // The WebGL spec mentions a couple more operations that trigger
    // this: clear() and getParameter(IMPLEMENTATION_COLOR_READ_*).
    pub(crate) fn set_webgl2(&self, webgl2: &WebGL2RenderingContext) {
        self.webgl2.set(Some(webgl2));
    }

    pub(crate) fn webgl2(&self) -> Option<DomRoot<WebGL2RenderingContext>> {
        self.webgl2.get()
    }

    pub(crate) fn validate_framebuffer(&self) -> WebGLResult<()> {
        match self.bound_draw_framebuffer.get() {
            Some(fb) => match fb.check_status_for_rendering() {
                CompleteForRendering::Complete => Ok(()),
                CompleteForRendering::Incomplete => Err(InvalidFramebufferOperation),
                CompleteForRendering::MissingColorAttachment => Err(InvalidOperation),
            },
            None => Ok(()),
        }
    }

    /// WebGL 1.0 6.11 (and WebGL 2.0 5.x): with the stencil test on and a stencil buffer to draw
    /// into, the front and back stencil reference, value mask and write mask must agree in the
    /// buffer's bits, else the draw is INVALID_OPERATION. A draw-state check: its inputs change
    /// by enable/disable and framebuffer commands (off the DrawStateGen allowlist) or by a
    /// stencil command that flips the agreement (set_stencil_state moves the generation).
    pub(crate) fn validate_stencil_for_draw(&self) -> WebGLResult<()> {
        if !self.stencil_state.get().mismatched() ||
            !self
                .capabilities
                .is_enabled(constants::STENCIL_TEST)
                .unwrap_or(false)
        {
            return Ok(());
        }
        let has_stencil = match self.bound_draw_framebuffer.get() {
            Some(fb) => fb
                .attachment_internal_format(constants::STENCIL_ATTACHMENT)
                .or_else(|| fb.attachment_internal_format(constants::DEPTH_STENCIL_ATTACHMENT))
                .is_some(),
            None => self.default_framebuffer_stencil,
        };
        if has_stencil {
            return Err(InvalidOperation);
        }
        Ok(())
    }

    /// WebGL 1.0 6.26: a draw that samples a texture level the draw framebuffer renders into (at
    /// any attachment, whatever drawBuffers says) is a feedback loop. WebGL 2 checks this with
    /// the sampler types (validate_sampler_types_for_draw). A draw-state check: sampler units,
    /// filters and attachments change by commands off the allowlist; texture bindings through
    /// Textures::note_unit_changed, for the units recorded here (draw_sampler_units).
    fn validate_feedback_loops_webgl1(&self) -> WebGLResult<()> {
        if self.webgl_version() != WebGLVersion::WebGL1 {
            return Ok(());
        }
        let Some(program) = self.current_program() else {
            return Ok(());
        };
        // Partii (perf): the units the samplers read, for the draw-state cache's per-unit test
        // (draw_state_cached): a bindTexture moves no generation, only its unit's sequence.
        {
            let uniforms = program.active_uniforms();
            let mut units = program.draw_sampler_units_mut();
            units.clear();
            for uniform in uniforms.iter() {
                if !matches!(
                    uniform.type_,
                    constants::SAMPLER_2D | constants::SAMPLER_CUBE
                ) {
                    continue;
                }
                let Some(location) = uniform.location else {
                    continue;
                };
                for element in 0..uniform.size.unwrap_or(1).max(1) {
                    units.push(program.sampler_unit(location + element));
                }
            }
        }
        let Some(framebuffer) = self.bound_draw_framebuffer.get() else {
            return Ok(());
        };
        for uniform in program.active_uniforms().iter() {
            let target = match uniform.type_ {
                constants::SAMPLER_2D => constants::TEXTURE_2D,
                constants::SAMPLER_CUBE => constants::TEXTURE_CUBE_MAP,
                _ => continue,
            };
            let Some(location) = uniform.location else {
                continue;
            };
            for element in 0..uniform.size.unwrap_or(1).max(1) {
                let unit = program.sampler_unit(location + element);
                let Some(texture) = self.textures.texture_at(unit, target) else {
                    continue;
                };
                let Some((first, last)) = texture.sampled_levels() else {
                    continue;
                };
                if framebuffer
                    .texture_attachment_levels(&texture)
                    .into_iter()
                    .any(|level| level >= 0 && (first..=last).contains(&(level as u32)))
                {
                    return Err(InvalidOperation);
                }
            }
        }
        Ok(())
    }

    /// Applies a stencil func/mask command to the tracked front/back state. The commands stay on
    /// the DrawStateGen allowlist (games change them every few draws); the generation moves only
    /// when the front/back agreement that validate_stencil_for_draw reads changes.
    fn set_stencil_state(&self, face: u32, update: impl Fn(&mut StencilFace)) {
        let mut state = self.stencil_state.get();
        let before = state.mismatched();
        if face != constants::BACK {
            update(&mut state.front);
        }
        if face != constants::FRONT {
            update(&mut state.back);
        }
        self.stencil_state.set(state);
        if state.mismatched() != before {
            self.draw_state_gen.bump();
        }
    }

    /// WebGL 2 copies read the read framebuffer's read buffer: with nothing attached there (or
    /// READ_BUFFER NONE), GL ES 3.0 makes the copy INVALID_OPERATION. WebGL 1 framebuffers
    /// without colour are already rejected by `validate_framebuffer`.
    pub(crate) fn validate_read_buffer_image(&self) -> WebGLResult<()> {
        if self.webgl_version() != WebGLVersion::WebGL2 {
            return Ok(());
        }
        match self.bound_read_framebuffer.get() {
            Some(fb) if fb.attachment(fb.read_buffer()).is_none() => Err(InvalidOperation),
            _ => Ok(()),
        }
    }

    pub(crate) fn validate_ownership<T>(&self, object: &T) -> WebGLResult<()>
    where
        T: DerivedFrom<WebGLObject>,
    {
        let Some(context) = object.upcast().context() else {
            return Err(WebGLError::InvalidOperation);
        };
        if self != &*context {
            return Err(WebGLError::InvalidOperation);
        }
        Ok(())
    }

    pub(crate) fn with_location<F>(&self, location: Option<&WebGLUniformLocation>, f: F)
    where
        F: FnOnce(&WebGLUniformLocation) -> WebGLResult<()>,
    {
        let location = match location {
            Some(loc) => loc,
            None => return,
        };
        match self.current_program.get() {
            Some(ref program)
                if program.id() == location.program_id() &&
                    program.link_generation() == location.link_generation() => {},
            _ => return self.webgl_error(InvalidOperation),
        }
        handle_potential_webgl_error!(self, f(location));
    }

    pub(crate) fn textures(&self) -> &Textures {
        &self.textures
    }

    fn tex_parameter(&self, target: u32, param: u32, value: TexParameterValue) {
        let texture_slot = handle_potential_webgl_error!(
            self,
            self.textures
                .active_texture_slot(target, self.webgl_version()),
            return
        );
        let texture =
            handle_potential_webgl_error!(self, texture_slot.get().ok_or(InvalidOperation), return);

        if !self
            .extension_manager
            .is_get_tex_parameter_name_enabled(param)
        {
            return self.webgl_error(InvalidEnum);
        }

        handle_potential_webgl_error!(self, texture.tex_parameter(param, value), return);
    }

    fn vertex_attrib(&self, cx: &mut JSContext, indx: u32, x: f32, y: f32, z: f32, w: f32) {
        if indx >= self.limits.max_vertex_attribs {
            return self.webgl_error(InvalidValue);
        }

        match self.webgl_version() {
            WebGLVersion::WebGL1 => self
                .current_vao(cx)
                .set_vertex_attrib_type(indx, constants::FLOAT),
            WebGLVersion::WebGL2 => self
                .current_vao_webgl2(cx)
                .set_vertex_attrib_type(indx, constants::FLOAT),
        };
        self.current_vertex_attribs.borrow_mut()[indx as usize] = VertexAttrib::Float(x, y, z, w);

        self.send_command(WebGLCommand::VertexAttrib(indx, x, y, z, w));
    }

    /// Whether a copy into `texture`'s image at `target` and `level` would read that image (the
    /// read framebuffer's read buffer): a feedback loop, INVALID_OPERATION (WebGL 1.0 6.26,
    /// GL ES 3.0 4.4.3).
    fn copy_reads_image(&self, texture: &WebGLTexture, target: TexImageTarget, level: u32) -> bool {
        let face = match target {
            TexImageTarget::Texture2D => 0,
            target => target.as_gl_constant() as i32,
        };
        self.copy_reads_layer(texture, level, face)
    }

    /// Copies (GL ES 3.0 3.8.5, GL ES 2.0 3.7.2): whether the read framebuffer's read buffer can
    /// be copied into an image of `destination` format. `sub_image` (copyTexSub*) checks
    /// component types and presence only, not sized bit counts, in WebGL 1 and 2; a WebGL 1
    /// copyTexImage2D has its own rule and always passes here.
    pub(crate) fn copy_format_compatible(&self, destination: TexFormat, sub_image: bool) -> bool {
        let webgl1 = self.webgl_version() == WebGLVersion::WebGL1;
        if webgl1 && !sub_image {
            return true;
        }
        // WebGL 1 keeps its one framebuffer binding in the draw slot.
        let read_framebuffer = if webgl1 {
            self.bound_draw_framebuffer.get()
        } else {
            self.bound_read_framebuffer.get()
        };
        let source = match read_framebuffer {
            Some(fb) => fb
                .attachment_internal_format(fb.read_buffer())
                .and_then(TexFormat::from_gl_constant),
            None => Some(if self.GetContextAttributes().unwrap().alpha {
                TexFormat::RGBA8
            } else {
                TexFormat::RGB8
            }),
        };
        source.is_some_and(|source| {
            if sub_image {
                copy_tex_sub_image_compatible(source, destination)
            } else {
                copy_tex_image_compatible(source, destination)
            }
        })
    }

    /// `copy_reads_image` for a cube face target or a 3D / array layer (`face_or_layer`, as
    /// framebuffer attachments record it).
    pub(crate) fn copy_reads_layer(
        &self,
        texture: &WebGLTexture,
        level: u32,
        face_or_layer: i32,
    ) -> bool {
        // WebGL 1 has one framebuffer binding, kept in the draw slot.
        let fb = match self.webgl_version() {
            WebGLVersion::WebGL1 => self.bound_draw_framebuffer.get(),
            WebGLVersion::WebGL2 => self.bound_read_framebuffer.get(),
        };
        let Some(fb) = fb else {
            return false;
        };
        fb.attached_image(fb.read_buffer()) ==
            Some(AttachedImage::Texture(
                texture.id(),
                level as i32,
                face_or_layer,
            ))
    }

    /// The size of the framebuffer copies and readPixels read from.
    pub(crate) fn read_framebuffer_size(&self) -> (i32, i32) {
        // WebGL 1 has one framebuffer binding, kept in the draw slot.
        let fb = match self.webgl_version() {
            WebGLVersion::WebGL1 => self.bound_draw_framebuffer.get(),
            WebGLVersion::WebGL2 => self.bound_read_framebuffer.get(),
        };
        match fb {
            Some(fb) => fb.size().unwrap_or((0, 0)),
            None => (self.DrawingBufferWidth(), self.DrawingBufferHeight()),
        }
    }

    pub(crate) fn get_current_framebuffer_size(&self) -> Option<(i32, i32)> {
        match self.bound_draw_framebuffer.get() {
            Some(fb) => fb.size(),

            // The window system framebuffer is bound
            None => Some((self.DrawingBufferWidth(), self.DrawingBufferHeight())),
        }
    }

    pub(crate) fn get_texture_packing_alignment(&self) -> u8 {
        self.texture_packing_alignment.get()
    }

    pub(crate) fn get_current_unpack_state(
        &self,
        premultiplied: Alpha,
    ) -> (Option<AlphaTreatment>, YAxisTreatment) {
        let settings = self.texture_unpacking_settings.get();
        let dest_premultiplied = settings.contains(TextureUnpacking::PREMULTIPLY_ALPHA);

        let alpha_treatment = match (premultiplied, dest_premultiplied) {
            (Alpha::Premultiplied, false) => Some(AlphaTreatment::Unmultiply),
            (Alpha::NotPremultiplied, true) => Some(AlphaTreatment::Premultiply),
            _ => None,
        };

        let y_axis_treatment = if settings.contains(TextureUnpacking::FLIP_Y_AXIS) {
            YAxisTreatment::Flipped
        } else {
            YAxisTreatment::AsIs
        };

        (alpha_treatment, y_axis_treatment)
    }

    /// WebGL 1: the sampler bindings of the current program whose texture is incomplete
    /// because it is float / half-float without OES_texture_(half_)float_linear yet filters
    /// with LINEAR (OES_texture_float_linear), or because it is non-power-of-two sized and
    /// repeats or mipmaps (`WebGLTexture::is_incomplete_npot`; desktop GL would sample it).
    /// Such a texture samples as opaque black.
    fn incomplete_sampler_bindings(&self) -> Vec<(u32, u32, DomRoot<WebGLTexture>)> {
        let mut bindings = Vec::new();
        if self.webgl_version() != WebGLVersion::WebGL1 {
            return bindings;
        }
        let Some(program) = self.current_program() else {
            return bindings;
        };
        for uniform in program.active_uniforms().iter() {
            let target = match uniform.type_ {
                constants::SAMPLER_2D => constants::TEXTURE_2D,
                constants::SAMPLER_CUBE => constants::TEXTURE_CUBE_MAP,
                _ => continue,
            };
            let Some(location) = uniform.location else {
                continue;
            };
            for element in 0..uniform.size.unwrap_or(1).max(1) {
                let unit = program.sampler_unit(location + element);
                let Some(texture) = self.textures.texture_at(unit, target) else {
                    continue;
                };
                let filterable = texture
                    .image_info_at_face(0, 0)
                    .and_then(|info| info.data_type())
                    .is_none_or(|data_type| {
                        self.extension_manager
                            .is_filterable(data_type.as_gl_constant())
                    });
                let incomplete = (!filterable && texture.is_using_linear_filtering()) ||
                    texture.is_incomplete_npot();
                if incomplete && !bindings.iter().any(|(u, t, _)| *u == unit && *t == target) {
                    bindings.push((unit, target, texture));
                }
            }
        }
        bindings
    }

    /// Sends a draw command, with incomplete textures (see `incomplete_sampler_bindings`)
    /// swapped for a black one for the duration of the draw.
    fn send_draw_command(&self, command: WebGLCommand) {
        let bindings = self.incomplete_sampler_bindings();
        for (unit, target, _) in &bindings {
            self.send_command(WebGLCommand::ActiveTexture(constants::TEXTURE0 + unit));
            self.send_command(WebGLCommand::BindIncompleteTexture(*target));
        }
        self.send_command(command);
        if bindings.is_empty() {
            return;
        }
        for (unit, target, texture) in &bindings {
            self.send_command(WebGLCommand::ActiveTexture(constants::TEXTURE0 + unit));
            self.send_command(WebGLCommand::BindTexture(*target, Some(texture.id())));
        }
        self.send_command(WebGLCommand::ActiveTexture(
            self.textures.active_unit_enum(),
        ));
    }

    fn validate_stencil_actions(&self, action: u32) -> bool {
        matches!(
            action,
            0 | constants::KEEP |
                constants::REPLACE |
                constants::INCR |
                constants::DECR |
                constants::INVERT |
                constants::INCR_WRAP |
                constants::DECR_WRAP
        )
    }

    pub(crate) fn get_image_pixels(
        &self,
        no_gc: &NoGC,
        source: TexImageSource,
    ) -> Fallible<Option<TexPixels>> {
        Ok(Some(match source {
            TexImageSource::ImageBitmap(bitmap) => {
                if !bitmap.origin_is_clean() {
                    return Err(Error::Security(None));
                }

                let Some(snapshot) = bitmap.bitmap_data().clone() else {
                    return Ok(None);
                };

                let snapshot = snapshot.to_shared();
                let size = snapshot.size().cast();
                let format = match snapshot.format() {
                    SnapshotPixelFormat::RGBA => PixelFormat::RGBA8,
                    SnapshotPixelFormat::BGRA => PixelFormat::BGRA8,
                };

                // If the TexImageSource is an ImageBitmap, the values of
                // UNPACK_FLIP_Y, UNPACK_PREMULTIPLY_ALPHA, and
                // UNPACK_COLORSPACE_CONVERSION are to be ignored.
                // Set alpha and y_axis treatment parameters such that no
                // conversions will be made.
                // <https://registry.khronos.org/webgl/specs/latest/1.0/#6.10>
                TexPixels::new(
                    snapshot.shared_memory(),
                    size,
                    format,
                    None,
                    YAxisTreatment::AsIs,
                )
            },
            TexImageSource::ImageData(image_data) => {
                let (alpha_treatment, y_axis_treatment) =
                    self.get_current_unpack_state(Alpha::NotPremultiplied);

                TexPixels::new(
                    image_data.to_shared_memory(no_gc),
                    image_data.get_size(),
                    PixelFormat::RGBA8,
                    alpha_treatment,
                    y_axis_treatment,
                )
            },
            TexImageSource::HTMLImageElement(image) => {
                let document = match self.canvas {
                    HTMLCanvasElementOrOffscreenCanvas::HTMLCanvasElement(ref canvas) => {
                        canvas.owner_document()
                    },
                    HTMLCanvasElementOrOffscreenCanvas::OffscreenCanvas(ref _canvas) => {
                        // TODO: Support retrieving image pixels here for OffscreenCanvas
                        return Ok(None);
                    },
                };
                if !image.same_origin(&document.origin()) {
                    return Err(Error::Security(None));
                }

                // Decoded images are stored premultiplied, which loses the colour under
                // zero alpha. Without UNPACK_PREMULTIPLY_ALPHA, upload a straight-alpha
                // decode instead.
                if !self
                    .texture_unpacking_settings
                    .get()
                    .contains(TextureUnpacking::PREMULTIPLY_ALPHA)
                {
                    if let Some((bytes, size)) = image.get_unpremultiplied_raster_image_data() {
                        let (alpha_treatment, y_axis_treatment) =
                            self.get_current_unpack_state(Alpha::NotPremultiplied);
                        return Ok(Some(TexPixels::new(
                            GenericSharedMemory::from_vec(bytes),
                            size,
                            PixelFormat::RGBA8,
                            alpha_treatment,
                            y_axis_treatment,
                        )));
                    }
                }

                // Vector (SVG) images are rasterized at their natural size, as in other
                // engines (see https://github.com/KhronosGroup/WebGL/issues/1503).
                let Some(snapshot) = image.get_raster_image_data() else {
                    return Ok(None);
                };

                let snapshot = snapshot.to_shared();
                let size = snapshot.size().cast();
                let format: PixelFormat = match snapshot.format() {
                    SnapshotPixelFormat::RGBA => PixelFormat::RGBA8,
                    SnapshotPixelFormat::BGRA => PixelFormat::BGRA8,
                };

                let (alpha_treatment, y_axis_treatment) =
                    self.get_current_unpack_state(snapshot.alpha_mode().alpha());

                TexPixels::new(
                    snapshot.shared_memory(),
                    size,
                    format,
                    alpha_treatment,
                    y_axis_treatment,
                )
            },
            // TODO(emilio): Getting canvas data is implemented in CanvasRenderingContext2D,
            // but we need to refactor it moving it to `HTMLCanvasElement` and support
            // WebGLContext (probably via GetPixels()).
            TexImageSource::HTMLCanvasElement(canvas) => {
                if !canvas.origin_is_clean() {
                    return Err(Error::Security(None));
                }

                let Some(snapshot) = canvas.get_image_data() else {
                    return Ok(None);
                };

                let snapshot = snapshot.to_shared();
                let size = snapshot.size().cast();
                let format = match snapshot.format() {
                    SnapshotPixelFormat::RGBA => PixelFormat::RGBA8,
                    SnapshotPixelFormat::BGRA => PixelFormat::BGRA8,
                };

                let (alpha_treatment, y_axis_treatment) =
                    self.get_current_unpack_state(snapshot.alpha_mode().alpha());

                TexPixels::new(
                    snapshot.shared_memory(),
                    size,
                    format,
                    alpha_treatment,
                    y_axis_treatment,
                )
            },
            // An OffscreenCanvas with a 2D, bitmaprenderer or WebGL context (e.g. text drawn
            // off-screen for a texture), as in every other engine.
            TexImageSource::OffscreenCanvas(canvas) => {
                if !canvas.origin_is_clean() {
                    return Err(Error::Security(None));
                }

                let Some(snapshot) = canvas.get_image_data() else {
                    return Ok(None);
                };

                let snapshot = snapshot.to_shared();
                let size = snapshot.size().cast();
                let format = match snapshot.format() {
                    SnapshotPixelFormat::RGBA => PixelFormat::RGBA8,
                    SnapshotPixelFormat::BGRA => PixelFormat::BGRA8,
                };

                let (alpha_treatment, y_axis_treatment) =
                    self.get_current_unpack_state(snapshot.alpha_mode().alpha());

                TexPixels::new(
                    snapshot.shared_memory(),
                    size,
                    format,
                    alpha_treatment,
                    y_axis_treatment,
                )
            },
            TexImageSource::HTMLVideoElement(video) => {
                if !video.origin_is_clean() {
                    return Err(Error::Security(None));
                }

                let Some(snapshot) = video.get_current_frame_data() else {
                    return Ok(None);
                };

                let snapshot = snapshot.to_shared();
                let size = snapshot.size().cast();
                let format: PixelFormat = match snapshot.format() {
                    SnapshotPixelFormat::RGBA => PixelFormat::RGBA8,
                    SnapshotPixelFormat::BGRA => PixelFormat::BGRA8,
                };

                let (alpha_treatment, y_axis_treatment) =
                    self.get_current_unpack_state(snapshot.alpha_mode().alpha());

                TexPixels::new(
                    snapshot.shared_memory(),
                    size,
                    format,
                    alpha_treatment,
                    y_axis_treatment,
                )
            },
        }))
    }

    // TODO(emilio): Move this logic to a validator.
    pub(crate) fn validate_tex_image_2d_data(
        &self,
        width: u32,
        height: u32,
        format: TexFormat,
        data_type: TexDataType,
        unpacking_alignment: u32,
        data: Option<&ArrayBufferView>,
    ) -> Result<u32, ()> {
        let element_size = data_type.element_size();
        let components_per_element = data_type.components_per_element();
        let components = format.components();

        // If data is non-null, the type of pixels must match the type of the
        // data to be read.
        // If it is UNSIGNED_BYTE, a Uint8Array must be supplied;
        // if it is UNSIGNED_SHORT_5_6_5, UNSIGNED_SHORT_4_4_4_4,
        // or UNSIGNED_SHORT_5_5_5_1, a Uint16Array must be supplied.
        // or FLOAT, a Float32Array must be supplied.
        // If the types do not match, an INVALID_OPERATION error is generated.
        // FLOAT_32_UNSIGNED_INT_24_8_REV takes no client data at all (WebGL 2.0 5.14.8: only
        // null pixels), whatever the view's type.
        let data_type_matches = data.as_ref().is_none_or(|buffer| {
            data_type != TexDataType::Float32UnsignedInt248Rev &&
                Some(data_type.sized_data_type()) ==
                    array_buffer_type_to_sized_type(buffer.get_array_type()) &&
                data_type.required_webgl_version() <= self.webgl_version()
        });

        if !data_type_matches {
            self.webgl_error(InvalidOperation);
            return Err(());
        }

        // NOTE: width and height are positive or zero due to validate()
        if height == 0 {
            Ok(0)
        } else {
            // We need to be careful here to not count unpack
            // alignment at the end of the image, otherwise (for
            // example) passing a single byte for uploading a 1x1
            // GL_ALPHA/GL_UNSIGNED_BYTE texture would throw an error.
            let cpp = element_size * components / components_per_element;
            let stride = (width * cpp + unpacking_alignment - 1) & !(unpacking_alignment - 1);
            Ok(stride * (height - 1) + width * cpp)
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn tex_image_2d(
        &self,
        texture: &WebGLTexture,
        target: TexImageTarget,
        data_type: TexDataType,
        internal_format: TexFormat,
        format: TexFormat,
        level: u32,
        _border: u32,
        unpacking_alignment: u32,
        size: Size2D<u32>,
        source: TexSource,
    ) {
        // TexImage2D depth is always equal to 1. Record the internal format (sized in WebGL 2,
        // e.g. RGBA16F), not the transfer format: completeness, readPixels and blitFramebuffer
        // look at it.
        handle_potential_webgl_error!(
            self,
            texture.initialize(
                target,
                size.width,
                size.height,
                1,
                internal_format,
                level,
                Some(data_type)
            )
        );

        let internal_format = self
            .extension_manager
            .get_effective_tex_internal_format(internal_format, data_type.as_gl_constant());

        let effective_data_type = self
            .extension_manager
            .effective_type(data_type.as_gl_constant());

        match source {
            TexSource::Pixels(pixels) => {
                // TODO(emilio): convert colorspace if requested.
                self.send_command(WebGLCommand::TexImage2D {
                    target: target.as_gl_constant(),
                    level,
                    internal_format,
                    size,
                    format,
                    data_type,
                    effective_data_type,
                    unpacking_alignment,
                    alpha_treatment: pixels.alpha_treatment,
                    y_axis_treatment: pixels.y_axis_treatment,
                    pixel_format: pixels.pixel_format,
                    data: pixels.data.into(),
                });
            },
            TexSource::BufferOffset(offset) => {
                self.send_command(WebGLCommand::TexImage2DPBO {
                    target: target.as_gl_constant(),
                    level,
                    internal_format,
                    size,
                    format,
                    effective_data_type,
                    unpacking_alignment,
                    offset,
                });
            },
        }

        if let Some(fb) = self.bound_draw_framebuffer.get() {
            fb.invalidate_texture(texture);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn tex_sub_image_2d(
        &self,
        texture: DomRoot<WebGLTexture>,
        target: TexImageTarget,
        level: u32,
        xoffset: i32,
        yoffset: i32,
        format: TexFormat,
        data_type: TexDataType,
        unpacking_alignment: u32,
        pixels: TexPixels,
    ) {
        let size = pixels.size();
        self.tex_sub_image_2d_from_source(
            texture,
            target,
            level,
            xoffset,
            yoffset,
            size,
            format,
            data_type,
            unpacking_alignment,
            TexSource::Pixels(pixels),
        )
    }

    /// texSubImage2D from client pixels or, in WebGL 2, from an offset into the
    /// bound PIXEL_UNPACK_BUFFER.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn tex_sub_image_2d_from_source(
        &self,
        texture: DomRoot<WebGLTexture>,
        target: TexImageTarget,
        level: u32,
        xoffset: i32,
        yoffset: i32,
        size: Size2D<u32>,
        format: TexFormat,
        data_type: TexDataType,
        unpacking_alignment: u32,
        source: TexSource,
    ) {
        // We have already validated level
        let image_info = match texture.image_info_for_target(&target, level) {
            Some(info) => info,
            None => return self.webgl_error(InvalidOperation),
        };

        // GL_INVALID_VALUE is generated if:
        //   - xoffset or yoffset is less than 0
        //   - x offset plus the width is greater than the texture width
        //   - y offset plus the height is greater than the texture height
        if xoffset < 0 ||
            (xoffset as u32 + size.width) > image_info.width() ||
            yoffset < 0 ||
            (yoffset as u32 + size.height) > image_info.height()
        {
            return self.webgl_error(InvalidValue);
        }

        // The unsized format must be compatible with the sized internal format
        debug_assert!(!format.is_sized());
        if format != image_info.internal_format().to_unsized() {
            return self.webgl_error(InvalidOperation);
        }

        // See https://www.khronos.org/registry/webgl/specs/latest/2.0/#5.1.6
        if self.webgl_version() == WebGLVersion::WebGL1 &&
            data_type != image_info.data_type().unwrap()
        {
            return self.webgl_error(InvalidOperation);
        }

        let effective_data_type = self
            .extension_manager
            .effective_type(data_type.as_gl_constant());

        match source {
            // TODO(emilio): convert colorspace if requested.
            TexSource::Pixels(pixels) => self.send_command(WebGLCommand::TexSubImage2D {
                target: target.as_gl_constant(),
                level,
                xoffset,
                yoffset,
                size,
                format,
                data_type,
                effective_data_type,
                unpacking_alignment,
                alpha_treatment: pixels.alpha_treatment,
                y_axis_treatment: pixels.y_axis_treatment,
                pixel_format: pixels.pixel_format,
                data: pixels.data.into(),
            }),
            TexSource::BufferOffset(offset) => self.send_command(WebGLCommand::TexSubImage2DPBO {
                target: target.as_gl_constant(),
                level,
                xoffset,
                yoffset,
                size,
                format,
                effective_data_type,
                unpacking_alignment,
                offset,
            }),
        }
    }

    fn get_gl_extensions(&self) -> String {
        let (sender, receiver) = webgl_channel().unwrap();
        self.send_command(WebGLCommand::GetExtensions(sender));
        receiver.recv().unwrap()
    }

    /// The bound vertex array's draw check (WebGL 1.0 6.2, 6.6), for `required_len` vertices (0:
    /// none counted) and `primcount` instances from `base_instance`.
    fn validate_vao_for_draw_base_instance(
        &self,
        cx: &mut JSContext,
        required_len: u32,
        primcount: i32,
        base_instance: u32,
        program: &WebGLProgram,
    ) -> WebGLResult<()> {
        match self.webgl_version() {
            WebGLVersion::WebGL1 => self.current_vao(cx).validate_for_draw(
                required_len,
                primcount as u32,
                &program.active_attribs(),
            ),
            WebGLVersion::WebGL2 => self.current_vao_webgl2(cx).validate_for_draw_base_instance(
                required_len,
                primcount as u32,
                base_instance,
                &program.active_attribs(),
            ),
        }
    }

    // https://www.khronos.org/registry/webgl/extensions/ANGLE_instanced_arrays/
    pub(crate) fn draw_arrays_instanced(
        &self,
        cx: &mut JSContext,
        mode: u32,
        first: i32,
        count: i32,
        primcount: i32,
    ) -> WebGLResult<()> {
        self.draw_arrays_instanced_base_instance(cx, mode, first, count, primcount, 0)
    }

    /// drawArraysInstanced whose instanced attributes start at `base_instance`
    /// (WEBGL_draw_instanced_base_vertex_base_instance, WebGL 2 only; 0 elsewhere).
    pub(crate) fn draw_arrays_instanced_base_instance(
        &self,
        cx: &mut JSContext,
        mode: u32,
        first: i32,
        count: i32,
        primcount: i32,
        base_instance: u32,
    ) -> WebGLResult<()> {
        // WebGL 1 (the only caller that isn't WebGL 2's checked draws) answers Hit or Miss.
        let cached = self.draw_state_cached(cx) == DrawStateCache::Hit;
        self.draw_arrays_instanced_base_instance_checked(
            cx,
            mode,
            first,
            count,
            primcount,
            base_instance,
            cached,
        )
    }

    /// `draw_arrays_instanced_base_instance`, told whether the draw-state checks are `cached`
    /// (draw_state_cached, which WebGL 2 draws read once for all their checks).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_arrays_instanced_base_instance_checked(
        &self,
        cx: &mut JSContext,
        mode: u32,
        first: i32,
        count: i32,
        primcount: i32,
        base_instance: u32,
        cached: bool,
    ) -> WebGLResult<()> {
        match mode {
            constants::POINTS |
            constants::LINE_STRIP |
            constants::LINE_LOOP |
            constants::LINES |
            constants::TRIANGLE_STRIP |
            constants::TRIANGLE_FAN |
            constants::TRIANGLES => {},
            _ => {
                return Err(InvalidEnum);
            },
        }
        if first < 0 || count < 0 || primcount < 0 {
            return Err(InvalidValue);
        }

        let current_program = self.current_program.get().ok_or(InvalidOperation)?;
        // A failed relink invalidates the program in use (WebGL 1.0 5.14.9). Checked on every
        // draw, cached or not (the LinkProgram also moves the draw-state generation on).
        if !current_program.is_linked() {
            return Err(InvalidOperation);
        }

        // first + count can pass i32::MAX (vertex-id draws at first = INT32_MAX): count in u32,
        // where two non-negative i32s always fit; attribute arrays still bound it below.
        let required_len = if count > 0 {
            first as u32 + count as u32
        } else {
            0
        };

        // With no vertices to count, the attribute check reads no draw argument (it is then part of
        // the cached state checks).
        if !cached || required_len > 0 {
            self.validate_vao_for_draw_base_instance(
                cx,
                required_len,
                primcount,
                base_instance,
                &current_program,
            )?;
        }
        if !cached {
            self.validate_framebuffer()?;
            self.validate_stencil_for_draw()?;
            self.validate_feedback_loops_webgl1()?;
            self.mark_draw_state_valid(cx, &current_program);
        }

        if count == 0 || primcount == 0 {
            return Ok(());
        }

        self.send_draw_command(if base_instance != 0 {
            WebGLCommand::DrawArraysInstancedBaseInstance {
                mode,
                first,
                count,
                primcount,
                base_instance,
            }
        } else if primcount == 1 {
            WebGLCommand::DrawArrays { mode, first, count }
        } else {
            WebGLCommand::DrawArraysInstanced {
                mode,
                first,
                count,
                primcount,
            }
        });
        self.mark_as_dirty();
        Ok(())
    }

    // https://www.khronos.org/registry/webgl/extensions/ANGLE_instanced_arrays/
    pub(crate) fn draw_elements_instanced(
        &self,
        cx: &mut JSContext,
        mode: u32,
        count: i32,
        type_: u32,
        offset: i64,
        primcount: i32,
    ) -> WebGLResult<()> {
        self.draw_elements_instanced_base_vertex_base_instance(
            cx, mode, count, type_, offset, primcount, 0, 0,
        )
    }

    /// drawElementsInstanced with a base vertex added to each index and instanced attributes
    /// starting at `base_instance` (WEBGL_draw_instanced_base_vertex_base_instance, WebGL 2 only).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_elements_instanced_base_vertex_base_instance(
        &self,
        cx: &mut JSContext,
        mode: u32,
        count: i32,
        type_: u32,
        offset: i64,
        primcount: i32,
        base_vertex: i32,
        base_instance: u32,
    ) -> WebGLResult<()> {
        let cached = self.draw_state_cached(cx) == DrawStateCache::Hit;
        self.draw_elements_instanced_base_vertex_base_instance_checked(
            cx,
            mode,
            count,
            type_,
            offset,
            primcount,
            base_vertex,
            base_instance,
            cached,
        )
    }

    /// `draw_elements_instanced_base_vertex_base_instance`, told whether the draw-state checks are
    /// `cached` (draw_state_cached, which WebGL 2 draws read once for all their checks).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_elements_instanced_base_vertex_base_instance_checked(
        &self,
        cx: &mut JSContext,
        mode: u32,
        count: i32,
        type_: u32,
        offset: i64,
        primcount: i32,
        base_vertex: i32,
        base_instance: u32,
        cached: bool,
    ) -> WebGLResult<()> {
        match mode {
            constants::POINTS |
            constants::LINE_STRIP |
            constants::LINE_LOOP |
            constants::LINES |
            constants::TRIANGLE_STRIP |
            constants::TRIANGLE_FAN |
            constants::TRIANGLES => {},
            _ => {
                return Err(InvalidEnum);
            },
        }
        if count < 0 || offset < 0 || primcount < 0 {
            return Err(InvalidValue);
        }
        let type_size = match type_ {
            constants::UNSIGNED_BYTE => 1,
            constants::UNSIGNED_SHORT => 2,
            constants::UNSIGNED_INT => match self.webgl_version() {
                WebGLVersion::WebGL1 if self.extension_manager.is_element_index_uint_enabled() => 4,
                WebGLVersion::WebGL2 => 4,
                _ => return Err(InvalidEnum),
            },
            _ => return Err(InvalidEnum),
        };
        if offset % type_size != 0 {
            return Err(InvalidOperation);
        }

        let current_program = self.current_program.get().ok_or(InvalidOperation)?;
        // A failed relink invalidates the program in use (WebGL 1.0 5.14.9). Checked on every
        // draw, cached or not (the LinkProgram also moves the draw-state generation on).
        if !current_program.is_linked() {
            return Err(InvalidOperation);
        }
        let array_buffer = match self.webgl_version() {
            WebGLVersion::WebGL1 => self.current_vao(cx).element_array_buffer().get(),
            WebGLVersion::WebGL2 => self.current_vao_webgl2(cx).element_array_buffer().get(),
        }
        .ok_or(InvalidOperation)?;

        if count > 0 && primcount > 0 {
            // This operation cannot overflow in u64 and we know all those values are nonnegative.
            let val = offset as u64 + (count as u64 * type_size as u64);
            if val > array_buffer.capacity() as u64 {
                return Err(InvalidOperation);
            }
        }

        // TODO(nox): Pass the correct number of vertices required.
        // With no vertex count, the attribute check reads no draw argument: it is one of the
        // cached state checks.
        if !cached {
            self.validate_vao_for_draw_base_instance(cx, 0, primcount, base_instance, &current_program)?;
            self.validate_framebuffer()?;
            self.validate_stencil_for_draw()?;
            self.validate_feedback_loops_webgl1()?;
            self.mark_draw_state_valid(cx, &current_program);
        }

        if count == 0 || primcount == 0 {
            return Ok(());
        }
        // Instanced attributes read draw arguments (primcount, base_instance), so they're
        // checked on every draw, cached or not.
        let active_attribs = current_program.active_attribs();
        match self.webgl_version() {
            WebGLVersion::WebGL1 => self.current_vao(cx).validate_instanced_attribs(
                primcount as u32,
                0,
                &active_attribs,
            ),
            WebGLVersion::WebGL2 => self.current_vao_webgl2(cx).validate_instanced_attribs(
                primcount as u32,
                base_instance,
                &active_attribs,
            ),
        }?;

        let offset = offset as u32;
        self.send_draw_command(if base_vertex != 0 || base_instance != 0 {
            WebGLCommand::DrawElementsInstancedBaseVertexBaseInstance {
                mode,
                count,
                type_,
                offset,
                primcount,
                base_vertex,
                base_instance,
            }
        } else if primcount == 1 {
            WebGLCommand::DrawElements {
                mode,
                count,
                type_,
                offset,
            }
        } else {
            WebGLCommand::DrawElementsInstanced {
                mode,
                count,
                type_,
                offset,
                primcount,
            }
        });
        self.mark_as_dirty();
        Ok(())
    }

    pub(crate) fn vertex_attrib_divisor(&self, cx: &mut JSContext, index: u32, divisor: u32) {
        if index >= self.limits.max_vertex_attribs {
            return self.webgl_error(InvalidValue);
        }

        match self.webgl_version() {
            WebGLVersion::WebGL1 => self.current_vao(cx).vertex_attrib_divisor(index, divisor),
            WebGLVersion::WebGL2 => self
                .current_vao_webgl2(cx)
                .vertex_attrib_divisor(index, divisor),
        };
        self.send_command(WebGLCommand::VertexAttribDivisor { index, divisor });
    }

    pub(crate) fn array_buffer(&self) -> Option<DomRoot<WebGLBuffer>> {
        self.bound_buffer_array.get()
    }

    pub(crate) fn array_buffer_slot(&self) -> &MutNullableDom<WebGLBuffer> {
        &self.bound_buffer_array
    }

    pub(crate) fn bound_buffer(
        &self,
        cx: &mut JSContext,
        target: u32,
    ) -> WebGLResult<Option<DomRoot<WebGLBuffer>>> {
        match target {
            constants::ARRAY_BUFFER => Ok(self.bound_buffer_array.get()),
            constants::ELEMENT_ARRAY_BUFFER => {
                Ok(self.current_vao(cx).element_array_buffer().get())
            },
            _ => Err(WebGLError::InvalidEnum),
        }
    }

    pub(crate) fn buffer_usage(&self, usage: u32) -> WebGLResult<u32> {
        match usage {
            constants::STREAM_DRAW | constants::STATIC_DRAW | constants::DYNAMIC_DRAW => Ok(usage),
            _ => Err(WebGLError::InvalidEnum),
        }
    }

    pub(crate) fn create_vertex_array(
        &self,
        cx: &mut JSContext,
    ) -> Option<DomRoot<WebGLVertexArrayObjectOES>> {
        // Partii (perf): a pooled name, as createTexture takes (take_object_name), rather than a
        // round trip to the WebGL thread per createVertexArray.
        if !vao_pool_enabled() {
            let (sender, receiver) = webgl_channel().unwrap();
            self.send_command(WebGLCommand::CreateVertexArray(sender));
            return receiver
                .recv()
                .unwrap()
                .map(|id| WebGLVertexArrayObjectOES::new(cx, self, Some(id)));
        }
        let id = WebGLVertexArrayId::new(self.take_object_name(WebGLObjectNameKind::VertexArray)?);
        Some(WebGLVertexArrayObjectOES::new(cx, self, Some(id)))
    }

    pub(crate) fn create_vertex_array_webgl2(
        &self,
        cx: &mut JSContext,
    ) -> Option<DomRoot<WebGLVertexArrayObject>> {
        // Partii (perf): a pooled name, as in create_vertex_array.
        if !vao_pool_enabled() {
            let (sender, receiver) = webgl_channel().unwrap();
            self.send_command(WebGLCommand::CreateVertexArray(sender));
            return receiver
                .recv()
                .unwrap()
                .map(|id| WebGLVertexArrayObject::new(cx, self, Some(id)));
        }
        let id = WebGLVertexArrayId::new(self.take_object_name(WebGLObjectNameKind::VertexArray)?);
        Some(WebGLVertexArrayObject::new(cx, self, Some(id)))
    }

    pub(crate) fn delete_vertex_array(
        &self,
        cx: &mut JSContext,
        vao: Option<&WebGLVertexArrayObjectOES>,
    ) {
        if let Some(vao) = vao {
            handle_potential_webgl_error!(self, self.validate_ownership(vao), return);
            // The default vertex array has no id and should never be passed around.
            assert!(vao.id().is_some());
            if vao.is_deleted() {
                return;
            }
            if vao == &*self.current_vao(cx) {
                // Setting it to None will make self.current_vao() reset it to the default one
                // next time it is called.
                self.current_vao.set(None);
                self.send_command(WebGLCommand::BindVertexArray(None));
            }
            vao.delete(Operation::Infallible);
        }
    }

    pub(crate) fn delete_vertex_array_webgl2(
        &self,
        cx: &mut JSContext,
        vao: Option<&WebGLVertexArrayObject>,
    ) {
        if let Some(vao) = vao {
            handle_potential_webgl_error!(self, self.validate_ownership(vao), return);
            // The default vertex array has no id and should never be passed around.
            assert!(vao.id().is_some());
            if vao.is_deleted() {
                return;
            }
            if vao == &*self.current_vao_webgl2(cx) {
                // Setting it to None will make self.current_vao() reset it to the default one
                // next time it is called.
                self.current_vao_webgl2.set(None);
                self.send_command(WebGLCommand::BindVertexArray(None));
            }
            vao.delete(Operation::Infallible);
        }
    }

    pub(crate) fn is_vertex_array(&self, vao: Option<&WebGLVertexArrayObjectOES>) -> bool {
        vao.is_some_and(|vao| {
            // The default vertex array has no id and should never be passed around.
            assert!(vao.id().is_some());
            self.validate_ownership(vao).is_ok() && vao.ever_bound() && !vao.is_deleted()
        })
    }

    pub(crate) fn is_vertex_array_webgl2(&self, vao: Option<&WebGLVertexArrayObject>) -> bool {
        vao.is_some_and(|vao| {
            // The default vertex array has no id and should never be passed around.
            assert!(vao.id().is_some());
            self.validate_ownership(vao).is_ok() && vao.ever_bound() && !vao.is_deleted()
        })
    }

    pub(crate) fn bind_vertex_array(&self, vao: Option<&WebGLVertexArrayObjectOES>) {
        if let Some(vao) = vao {
            // The default vertex array has no id and should never be passed around.
            assert!(vao.id().is_some());
            handle_potential_webgl_error!(self, self.validate_ownership(vao), return);
            if vao.is_deleted() {
                return self.webgl_error(InvalidOperation);
            }
            vao.set_ever_bound();
        }
        self.send_command(WebGLCommand::BindVertexArray(vao.and_then(|vao| vao.id())));
        // Setting it to None will make self.current_vao() reset it to the default one
        // next time it is called.
        self.current_vao.set(vao);
    }

    pub(crate) fn bind_vertex_array_webgl2(&self, vao: Option<&WebGLVertexArrayObject>) {
        if let Some(vao) = vao {
            // The default vertex array has no id and should never be passed around.
            assert!(vao.id().is_some());
            handle_potential_webgl_error!(self, self.validate_ownership(vao), return);
            if vao.is_deleted() {
                return self.webgl_error(InvalidOperation);
            }
            vao.set_ever_bound();
        }
        self.send_command(WebGLCommand::BindVertexArray(vao.and_then(|vao| vao.id())));
        // Setting it to None will make self.current_vao() reset it to the default one
        // next time it is called.
        self.current_vao_webgl2.set(vao);
    }

    fn validate_blend_mode(&self, mode: u32) -> WebGLResult<()> {
        match mode {
            constants::FUNC_ADD | constants::FUNC_SUBTRACT | constants::FUNC_REVERSE_SUBTRACT => {
                Ok(())
            },
            EXTBlendMinmaxConstants::MIN_EXT | EXTBlendMinmaxConstants::MAX_EXT
                if self.extension_manager.is_blend_minmax_enabled() =>
            {
                Ok(())
            },
            _ => Err(InvalidEnum),
        }
    }

    pub(crate) fn initialize_framebuffer(&self, clear_bits: u32) {
        if clear_bits == 0 {
            return;
        }
        self.send_command(WebGLCommand::InitializeFramebuffer {
            color: clear_bits & constants::COLOR_BUFFER_BIT != 0,
            depth: clear_bits & constants::DEPTH_BUFFER_BIT != 0,
            stencil: clear_bits & constants::STENCIL_BUFFER_BIT != 0,
        });
    }

    pub(crate) fn extension_manager(&self) -> &WebGLExtensions {
        &self.extension_manager
    }

    pub(crate) fn buffer_data(
        &self,
        no_gc: &NoGC,
        target: u32,
        data: Option<MaybeSharedArrayBufferViewOrMaybeSharedArrayBuffer>,
        usage: u32,
        bound_buffer: Option<DomRoot<WebGLBuffer>>,
    ) {
        let data = handle_potential_webgl_error!(self, data.ok_or(InvalidValue), return);
        let bound_buffer =
            handle_potential_webgl_error!(self, bound_buffer.ok_or(InvalidOperation), return);

        let data = get_buffer_source_slice(&data, no_gc);
        handle_potential_webgl_error!(self, bound_buffer.buffer_data(target, data, usage));
    }

    pub(crate) fn buffer_data_(
        &self,
        target: u32,
        size: i64,
        usage: u32,
        bound_buffer: Option<DomRoot<WebGLBuffer>>,
    ) {
        let bound_buffer =
            handle_potential_webgl_error!(self, bound_buffer.ok_or(InvalidOperation), return);

        if size < 0 {
            return self.webgl_error(InvalidValue);
        }

        // FIXME: Allocating a buffer based on user-requested size is
        // not great, but we don't have a fallible allocation to try.
        let data = vec![0u8; size as usize];
        handle_potential_webgl_error!(self, bound_buffer.buffer_data(target, &data, usage));
    }

    pub(crate) fn buffer_sub_data(
        &self,
        no_gc: &NoGC,
        target: u32,
        offset: i64,
        data: MaybeSharedArrayBufferViewOrMaybeSharedArrayBuffer,
        bound_buffer: Option<DomRoot<WebGLBuffer>>,
    ) {
        let bound_buffer =
            handle_potential_webgl_error!(self, bound_buffer.ok_or(InvalidOperation), return);

        if offset < 0 {
            return self.webgl_error(InvalidValue);
        }

        let data = get_buffer_source_slice(&data, no_gc);
        if (offset as u64) + data.len() as u64 > bound_buffer.capacity() as u64 {
            return self.webgl_error(InvalidValue);
        }
        let (sender, receiver) = generic_channel::channel().unwrap();
        self.send_command(WebGLCommand::BufferSubData(
            target,
            offset as isize,
            receiver,
        ));
        let buffer = GenericSharedMemory::from_bytes(data);
        sender.send(buffer).unwrap();
    }

    pub(crate) fn bind_buffer_maybe(
        &self,
        slot: &MutNullableDom<WebGLBuffer>,
        target: u32,
        buffer: Option<&WebGLBuffer>,
    ) {
        if let Some(buffer) = buffer {
            handle_potential_webgl_error!(self, self.validate_ownership(buffer), return);

            if buffer.is_marked_for_deletion() {
                return self.webgl_error(InvalidOperation);
            }
            handle_potential_webgl_error!(self, buffer.set_target_maybe(target), return);
            buffer.increment_attached_counter();
        }

        self.send_command(WebGLCommand::BindBuffer(target, buffer.map(|b| b.id())));
        if let Some(old) = slot.get() {
            old.decrement_attached_counter(Operation::Infallible);
        }

        slot.set(buffer);
    }

    pub(crate) fn current_program(&self) -> Option<DomRoot<WebGLProgram>> {
        self.current_program.get()
    }

    pub(crate) fn uniform_check_program(
        &self,
        program: &WebGLProgram,
        location: &WebGLUniformLocation,
    ) -> WebGLResult<()> {
        self.validate_ownership(program)?;

        if program.is_deleted() ||
            !program.is_linked() ||
            self.context_id() != location.context_id() ||
            program.id() != location.program_id() ||
            program.link_generation() != location.link_generation()
        {
            return Err(InvalidOperation);
        }

        Ok(())
    }

    fn uniform_vec_section_int(
        &self,
        vec: Int32ArrayOrLongSequence,
        offset: u32,
        length: u32,
        uniform_size: usize,
        uniform_location: &WebGLUniformLocation,
    ) -> WebGLResult<Vec<i32>> {
        let vec = match vec {
            Int32ArrayOrLongSequence::Int32Array(v) => typed_array_to_vec(&*v),
            Int32ArrayOrLongSequence::LongSequence(v) => v,
        };
        self.uniform_vec_section::<i32>(vec, offset, length, uniform_size, uniform_location)
    }

    fn uniform_vec_section_float(
        &self,
        vec: Float32ArrayOrUnrestrictedFloatSequence,
        offset: u32,
        length: u32,
        uniform_size: usize,
        uniform_location: &WebGLUniformLocation,
    ) -> WebGLResult<Vec<f32>> {
        let vec = match vec {
            Float32ArrayOrUnrestrictedFloatSequence::Float32Array(v) => typed_array_to_vec(&*v),
            Float32ArrayOrUnrestrictedFloatSequence::UnrestrictedFloatSequence(v) => v,
        };
        self.uniform_vec_section::<f32>(vec, offset, length, uniform_size, uniform_location)
    }

    pub(crate) fn uniform_vec_section<T: Clone>(
        &self,
        vec: Vec<T>,
        offset: u32,
        length: u32,
        uniform_size: usize,
        uniform_location: &WebGLUniformLocation,
    ) -> WebGLResult<Vec<T>> {
        let offset = offset as usize;
        if offset > vec.len() {
            return Err(InvalidValue);
        }

        let length = if length > 0 {
            length as usize
        } else {
            vec.len() - offset
        };
        if offset + length > vec.len() {
            return Err(InvalidValue);
        }

        let vec = if offset == 0 && length == vec.len() {
            vec
        } else {
            vec[offset..offset + length].to_vec()
        };

        if vec.len() < uniform_size || vec.len() % uniform_size != 0 {
            return Err(InvalidValue);
        }
        if uniform_location.size().is_none() && vec.len() != uniform_size {
            return Err(InvalidOperation);
        }

        Ok(vec)
    }

    pub(crate) fn uniform_matrix_section(
        &self,
        vec: Float32ArrayOrUnrestrictedFloatSequence,
        offset: u32,
        length: u32,
        transpose: bool,
        uniform_size: usize,
        uniform_location: &WebGLUniformLocation,
    ) -> WebGLResult<Vec<f32>> {
        let vec = match vec {
            Float32ArrayOrUnrestrictedFloatSequence::Float32Array(v) => typed_array_to_vec(&*v),
            Float32ArrayOrUnrestrictedFloatSequence::UnrestrictedFloatSequence(v) => v,
        };
        if transpose {
            return Err(InvalidValue);
        }
        self.uniform_vec_section::<f32>(vec, offset, length, uniform_size, uniform_location)
    }

    pub(crate) fn get_draw_framebuffer_slot(&self) -> &MutNullableDom<WebGLFramebuffer> {
        &self.bound_draw_framebuffer
    }

    pub(crate) fn get_read_framebuffer_slot(&self) -> &MutNullableDom<WebGLFramebuffer> {
        &self.bound_read_framebuffer
    }

    pub(crate) fn validate_new_framebuffer_binding(
        &self,
        framebuffer: Option<&WebGLFramebuffer>,
    ) -> WebGLResult<()> {
        if let Some(fb) = framebuffer {
            self.validate_ownership(fb)?;
            if fb.is_deleted() {
                // From the WebGL spec:
                //
                //     "An attempt to bind a deleted framebuffer will
                //      generate an INVALID_OPERATION error, and the
                //      current binding will remain untouched."
                return Err(InvalidOperation);
            }
        }
        Ok(())
    }

    pub(crate) fn bind_framebuffer_to(
        &self,
        target: u32,
        framebuffer: Option<&WebGLFramebuffer>,
        slot: &MutNullableDom<WebGLFramebuffer>,
    ) {
        match framebuffer {
            Some(framebuffer) => framebuffer.bind(target),
            None => {
                // Bind the default framebuffer
                let cmd =
                    WebGLCommand::BindFramebuffer(target, WebGLFramebufferBindingRequest::Default);
                self.send_command(cmd);
            },
        }
        slot.set(framebuffer);
    }

    pub(crate) fn renderbuffer_storage(
        &self,
        target: u32,
        samples: i32,
        internal_format: u32,
        width: i32,
        height: i32,
    ) {
        if target != constants::RENDERBUFFER {
            return self.webgl_error(InvalidEnum);
        }

        let max = self.limits.max_renderbuffer_size;

        if samples < 0 || width < 0 || width as u32 > max || height < 0 || height as u32 > max {
            return self.webgl_error(InvalidValue);
        }

        let rb = handle_potential_webgl_error!(
            self,
            self.bound_renderbuffer.get().ok_or(InvalidOperation),
            return
        );
        handle_potential_webgl_error!(
            self,
            rb.storage(self.api_type, samples, internal_format, width, height)
        );
        if let Some(fb) = self.bound_draw_framebuffer.get() {
            fb.invalidate_renderbuffer(&rb);
        }

        // FIXME: https://github.com/servo/servo/issues/13710
    }

    pub(crate) fn valid_color_attachment_enum(&self, attachment: u32) -> bool {
        let last_slot = constants::COLOR_ATTACHMENT0 + self.limits().max_color_attachments - 1;
        constants::COLOR_ATTACHMENT0 <= attachment && attachment <= last_slot
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn compressed_tex_image_2d(
        &self,
        target: u32,
        level: i32,
        internal_format: u32,
        width: i32,
        height: i32,
        border: i32,
        data: CompressedSource,
    ) {
        let validator = CompressedTexImage2DValidator::new(
            self,
            target,
            level,
            width,
            height,
            border,
            internal_format,
            data.len(),
        );
        let CommonCompressedTexImage2DValidatorResult {
            texture,
            target,
            level,
            width,
            height,
            compression,
        } = match validator.validate() {
            Ok(result) => result,
            Err(_) => return,
        };

        if texture.is_immutable() {
            return self.webgl_error(InvalidOperation);
        }

        let size = Size2D::new(width, height);
        let data = data.into_command_data();

        handle_potential_webgl_error!(
            self,
            texture.initialize(
                target,
                size.width,
                size.height,
                1,
                compression.format,
                level,
                Some(TexDataType::UnsignedByte)
            )
        );

        self.send_command(WebGLCommand::CompressedTexImage2D {
            target: target.as_gl_constant(),
            level,
            internal_format,
            size: Size2D::new(width, height),
            data,
        });

        if let Some(fb) = self.bound_draw_framebuffer.get() {
            fb.invalidate_texture(&texture);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn compressed_tex_sub_image_2d(
        &self,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        data: CompressedSource,
    ) {
        let validator = CompressedTexSubImage2DValidator::new(
            self,
            target,
            level,
            xoffset,
            yoffset,
            width,
            height,
            format,
            data.len(),
        );
        let CommonCompressedTexImage2DValidatorResult {
            texture: _,
            target,
            level,
            width,
            height,
            ..
        } = match validator.validate() {
            Ok(result) => result,
            Err(_) => return,
        };

        self.send_command(WebGLCommand::CompressedTexSubImage2D {
            target: target.as_gl_constant(),
            level: level as i32,
            xoffset,
            yoffset,
            size: Size2D::new(width, height),
            format,
            data: data.into_command_data(),
        });
    }

    pub(crate) fn uniform1iv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Int32ArrayOrLongSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL | constants::INT => {},
                sampler if is_sampler_type(sampler) => {},
                _ => return Err(InvalidOperation),
            }

            let val = self.uniform_vec_section_int(val, src_offset, src_length, 1, location)?;

            if is_sampler_type(location.type_()) &&
                let Some(program) = self.current_program.get()
            {
                program.set_sampler_units(location.id(), &val);
            }
            match location.type_() {
                sampler if is_sampler_type(sampler) => {
                    for &v in val
                        .iter()
                        .take(cmp::min(location.size().unwrap_or(1) as usize, val.len()))
                    {
                        if v < 0 || v as u32 >= self.limits.max_combined_texture_image_units {
                            return Err(InvalidValue);
                        }
                    }
                },
                _ => {},
            }
            self.send_command(WebGLCommand::Uniform1iv(location.id(), val));
            Ok(())
        });
    }

    pub(crate) fn uniform1fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL | constants::FLOAT => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.uniform_vec_section_float(val, src_offset, src_length, 1, location)?;
            self.send_command(WebGLCommand::Uniform1fv(location.id(), val));
            Ok(())
        });
    }

    pub(crate) fn uniform2fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC2 | constants::FLOAT_VEC2 => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.uniform_vec_section_float(val, src_offset, src_length, 2, location)?;
            self.send_command(WebGLCommand::Uniform2fv(location.id(), val));
            Ok(())
        });
    }

    pub(crate) fn uniform2iv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Int32ArrayOrLongSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC2 | constants::INT_VEC2 => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.uniform_vec_section_int(val, src_offset, src_length, 2, location)?;
            self.send_command(WebGLCommand::Uniform2iv(location.id(), val));
            Ok(())
        });
    }

    pub(crate) fn uniform3fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC3 | constants::FLOAT_VEC3 => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.uniform_vec_section_float(val, src_offset, src_length, 3, location)?;
            self.send_command(WebGLCommand::Uniform3fv(location.id(), val));
            Ok(())
        });
    }

    pub(crate) fn uniform3iv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Int32ArrayOrLongSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC3 | constants::INT_VEC3 => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.uniform_vec_section_int(val, src_offset, src_length, 3, location)?;
            self.send_command(WebGLCommand::Uniform3iv(location.id(), val));
            Ok(())
        });
    }

    pub(crate) fn uniform4iv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Int32ArrayOrLongSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC4 | constants::INT_VEC4 => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.uniform_vec_section_int(val, src_offset, src_length, 4, location)?;
            self.send_command(WebGLCommand::Uniform4iv(location.id(), val));
            Ok(())
        });
    }

    pub(crate) fn uniform4fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC4 | constants::FLOAT_VEC4 => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.uniform_vec_section_float(val, src_offset, src_length, 4, location)?;
            self.send_command(WebGLCommand::Uniform4fv(location.id(), val));
            Ok(())
        });
    }

    pub(crate) fn uniform_matrix_2fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        transpose: bool,
        val: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::FLOAT_MAT2 => {},
                _ => return Err(InvalidOperation),
            }
            let val =
                self.uniform_matrix_section(val, src_offset, src_length, transpose, 4, location)?;
            self.send_command(WebGLCommand::UniformMatrix2fv(location.id(), val));
            Ok(())
        });
    }

    pub(crate) fn uniform_matrix_3fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        transpose: bool,
        val: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::FLOAT_MAT3 => {},
                _ => return Err(InvalidOperation),
            }
            let val =
                self.uniform_matrix_section(val, src_offset, src_length, transpose, 9, location)?;
            self.send_command(WebGLCommand::UniformMatrix3fv(location.id(), val));
            Ok(())
        });
    }

    pub(crate) fn uniform_matrix_4fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        transpose: bool,
        val: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::FLOAT_MAT4 => {},
                _ => return Err(InvalidOperation),
            }
            let val =
                self.uniform_matrix_section(val, src_offset, src_length, transpose, 16, location)?;
            self.send_command(WebGLCommand::UniformMatrix4fv(location.id(), val));
            Ok(())
        });
    }

    pub(crate) fn get_buffer_param(
        &self,
        buffer: Option<DomRoot<WebGLBuffer>>,
        parameter: u32,
        mut retval: MutableHandleValue,
    ) {
        let buffer = handle_potential_webgl_error!(
            self,
            buffer.ok_or(InvalidOperation),
            return retval.set(NullValue())
        );

        retval.set(match parameter {
            constants::BUFFER_SIZE => Int32Value(buffer.capacity() as i32),
            constants::BUFFER_USAGE => Int32Value(buffer.usage() as i32),
            _ => {
                self.webgl_error(InvalidEnum);
                NullValue()
            },
        })
    }
}

impl CanvasContext for WebGLRenderingContext {
    type ID = WebGLContextId;

    fn context_id(&self) -> Self::ID {
        self.droppable.webgl_sender.context_id()
    }

    fn canvas(&self) -> Option<RootedHTMLCanvasElementOrOffscreenCanvas> {
        Some(RootedHTMLCanvasElementOrOffscreenCanvas::from(&self.canvas))
    }

    fn resize(&self) {
        let size = self.size().cast();
        // Setting width or height to the size the drawing buffer already has changes nothing:
        // contents, depth and stencil stay (render-after-resize-test's no-op resizes).
        if self.size.get() == size {
            return;
        }
        let (sender, receiver) = webgl_channel().unwrap();
        self.droppable
            .webgl_sender
            .send_resize(size, sender)
            .unwrap();
        // FIXME(#21718) The backend is allowed to choose a size smaller than
        // what was requested
        self.size.set(size);
        self.reflector_
            .update_memory_size(self, size.cast::<usize>().area() * 4);

        if let Err(msg) = receiver.recv().unwrap() {
            error!("Error resizing WebGLContext: {}", msg);
            return;
        };

        // ClearColor needs to be restored because after a resize the GLContext is recreated
        // and the framebuffer is cleared using the default black transparent color.
        let color = self.current_clear_color.get();
        self.send_command(WebGLCommand::ClearColor(color.0, color.1, color.2, color.3));

        // WebGL Spec: Scissor rect must not change if the canvas is resized.
        // See: webgl/conformance-1.0.3/conformance/rendering/gl-scissor-canvas-dimensions.html
        // NativeContext handling library changes the scissor after a resize, so we need to reset the
        // default scissor when the canvas was created or the last scissor that the user set.
        let rect = self.current_scissor.get();
        self.send_command(WebGLCommand::Scissor(rect.0, rect.1, rect.2, rect.3));

        // Bound texture must not change when the canvas is resized.
        // Right now surfman generates a new FBO and the bound texture is changed
        // in order to create a new render to texture attachment.
        // Send a command to re-bind the TEXTURE_2D, if any.
        if let Some(texture) = self
            .textures
            .active_texture_slot(constants::TEXTURE_2D, self.webgl_version())
            .unwrap()
            .get()
        {
            self.send_command(WebGLCommand::BindTexture(
                constants::TEXTURE_2D,
                Some(texture.id()),
            ));
        }

        // Bound framebuffer must not change when the canvas is resized.
        // Right now surfman generates a new FBO on resize.
        // Send a command to re-bind the framebuffer, if any.
        if let Some(fbo) = self.bound_draw_framebuffer.get() {
            let id = WebGLFramebufferBindingRequest::Explicit(fbo.id());
            self.send_command(WebGLCommand::BindFramebuffer(constants::FRAMEBUFFER, id));
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-offscreencanvas-transfertoimagebitmap>
    /// step 4: give the context a new, cleared drawing buffer. A swap does that: the WebGL
    /// thread clears the new back buffer (unless `preserveDrawingBuffer` is set, which
    /// keeps its contents, as a swap with that attribute always does).
    fn reset_bitmap(&self) {
        self.swap_buffers(None);
    }

    // Used by HTMLCanvasElement.toDataURL
    //
    // This emits errors quite liberally, but the spec says that this operation
    // can fail and that it is UB what happens in that case.
    //
    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#2.2>
    fn get_image_data(&self) -> Option<Snapshot> {
        handle_potential_webgl_error!(self, self.validate_framebuffer(), return None);
        let mut size = self.size().cast();

        let (fb_width, fb_height) = handle_potential_webgl_error!(
            self,
            self.get_current_framebuffer_size().ok_or(InvalidOperation),
            return None
        );
        size.width = cmp::min(size.width, fb_width as u32);
        size.height = cmp::min(size.height, fb_height as u32);

        let (sender, receiver) = generic_channel::channel().unwrap();
        self.send_command(WebGLCommand::ReadPixels(
            Rect::from_size(size),
            constants::RGBA,
            constants::UNSIGNED_BYTE,
            sender,
        ));
        let (data, alpha_mode) = receiver.recv().unwrap();
        // glReadPixels returns rows bottom-up; a snapshot of the canvas (toDataURL,
        // drawImage, createImageBitmap, transferToImageBitmap) is top-down.
        let mut data = data.to_vec();
        pixels::flip_y_rgba8_image_inplace(size, &mut data);
        Some(Snapshot::from_vec(
            size.cast(),
            SnapshotPixelFormat::RGBA,
            alpha_mode,
            data,
        ))
    }

    fn mark_as_dirty(&self) {
        // If we have a bound framebuffer, then don't mark the canvas as dirty.
        if self.bound_draw_framebuffer.get().is_some() {
            return;
        }

        // Dirtying the canvas is unnecessary if we're actively displaying immersive
        // XR content right now.
        if self
            .global()
            .downcast::<Window>()
            .is_some_and(|window| window.in_immersive_xr_session())
        {
            return;
        }

        self.canvas.mark_as_dirty();
    }
}

#[cfg(not(feature = "webgl_backtrace"))]
#[inline]
pub(crate) fn capture_webgl_backtrace() -> WebGLCommandBacktrace {
    WebGLCommandBacktrace {}
}

#[cfg(feature = "webgl_backtrace")]
#[cfg_attr(feature = "webgl_backtrace", expect(unsafe_code))]
pub(crate) fn capture_webgl_backtrace() -> WebGLCommandBacktrace {
    let bt = Backtrace::new();
    unsafe {
        // TODO: https://github.com/servo/servo/issues/40600
        #[expect(unsafe_code)]
        let mut cx = unsafe { script_bindings::script_runtime::temp_cx() };
        capture_stack!(&in(cx) let stack);
        WebGLCommandBacktrace {
            backtrace: format!("{:?}", bt),
            js_backtrace: stack.and_then(|s| s.as_string(None, js::jsapi::StackFormat::Default)),
        }
    }
}

impl WebGLRenderingContextMethods<crate::DomTypeHolder> for WebGLRenderingContext {
    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.1>
    fn Canvas(&self) -> RootedHTMLCanvasElementOrOffscreenCanvas {
        RootedHTMLCanvasElementOrOffscreenCanvas::from(&self.canvas)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.11>
    fn Flush(&self) {
        self.send_command(WebGLCommand::Flush);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.11>
    fn Finish(&self) {
        let (sender, receiver) = webgl_channel().unwrap();
        self.send_command(WebGLCommand::Finish(sender));
        receiver.recv().unwrap()
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.1>
    fn DrawingBufferWidth(&self) -> i32 {
        let (sender, receiver) = webgl_channel().unwrap();
        self.send_command(WebGLCommand::DrawingBufferWidth(sender));
        receiver.recv().unwrap()
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.1>
    fn DrawingBufferHeight(&self) -> i32 {
        let (sender, receiver) = webgl_channel().unwrap();
        self.send_command(WebGLCommand::DrawingBufferHeight(sender));
        receiver.recv().unwrap()
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.5>
    fn GetBufferParameter(
        &self,
        cx: &mut JSContext,
        target: u32,
        parameter: u32,
        mut retval: MutableHandleValue,
    ) {
        let buffer = handle_potential_webgl_error!(
            self,
            self.bound_buffer(cx, target),
            return retval.set(NullValue())
        );
        self.get_buffer_param(buffer, parameter, retval)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn GetParameter(&self, cx: &mut JSContext, parameter: u32, mut retval: MutableHandleValue) {
        if !self
            .extension_manager
            .is_get_parameter_name_enabled(parameter)
        {
            self.webgl_error(WebGLError::InvalidEnum);
            return retval.set(NullValue());
        }

        match parameter {
            constants::ARRAY_BUFFER_BINDING => {
                self.bound_buffer_array.get().to_jsval(cx, retval);
                return;
            },
            constants::CURRENT_PROGRAM => {
                self.current_program.get().to_jsval(cx, retval);
                return;
            },
            constants::ELEMENT_ARRAY_BUFFER_BINDING => {
                let buffer = self.current_vao(cx).element_array_buffer().get();
                buffer.to_jsval(cx, retval);
                return;
            },
            constants::FRAMEBUFFER_BINDING => {
                self.bound_draw_framebuffer.get().to_jsval(cx, retval);
                return;
            },
            constants::RENDERBUFFER_BINDING => {
                self.bound_renderbuffer.get().to_jsval(cx, retval);
                return;
            },
            constants::TEXTURE_BINDING_2D => {
                let texture = self
                    .textures
                    .active_texture_slot(constants::TEXTURE_2D, self.webgl_version())
                    .unwrap()
                    .get();
                texture.to_jsval(cx, retval);
                return;
            },
            WebGL2RenderingContextConstants::TEXTURE_BINDING_2D_ARRAY => {
                let texture = self
                    .textures
                    .active_texture_slot(
                        WebGL2RenderingContextConstants::TEXTURE_2D_ARRAY,
                        self.webgl_version(),
                    )
                    .unwrap()
                    .get();
                texture.to_jsval(cx, retval);
                return;
            },
            WebGL2RenderingContextConstants::TEXTURE_BINDING_3D => {
                let texture = self
                    .textures
                    .active_texture_slot(
                        WebGL2RenderingContextConstants::TEXTURE_3D,
                        self.webgl_version(),
                    )
                    .unwrap()
                    .get();
                texture.to_jsval(cx, retval);
                return;
            },
            constants::TEXTURE_BINDING_CUBE_MAP => {
                let texture = self
                    .textures
                    .active_texture_slot(constants::TEXTURE_CUBE_MAP, self.webgl_version())
                    .unwrap()
                    .get();
                texture.to_jsval(cx, retval);
                return;
            },
            OESVertexArrayObjectConstants::VERTEX_ARRAY_BINDING_OES => {
                let vao = self.current_vao.get().filter(|vao| vao.id().is_some());
                vao.to_jsval(cx, retval);
                return;
            },
            // In readPixels we currently support RGBA/UBYTE only.  If
            // we wanted to support other formats, we could ask the
            // driver, but we would need to check for
            // GL_OES_read_format support (assuming an underlying GLES
            // driver. Desktop is happy to format convert for us).
            constants::IMPLEMENTATION_COLOR_READ_FORMAT => {
                if self.validate_framebuffer().is_err() {
                    self.webgl_error(InvalidOperation);
                    return retval.set(NullValue());
                }
                return retval.set(Int32Value(constants::RGBA as i32));
            },
            constants::IMPLEMENTATION_COLOR_READ_TYPE => {
                if self.validate_framebuffer().is_err() {
                    self.webgl_error(InvalidOperation);
                    return retval.set(NullValue());
                }
                return retval.set(Int32Value(constants::UNSIGNED_BYTE as i32));
            },
            constants::COMPRESSED_TEXTURE_FORMATS => {
                let format_ids = self.extension_manager.get_tex_compression_ids();

                rooted!(&in(cx) let mut rval = ptr::null_mut::<JSObject>());
                create_buffer_source::<Uint32>(cx, &format_ids, rval.handle_mut()).unwrap();
                return retval.set(ObjectValue(rval.get()));
            },
            constants::VERSION => {
                "WebGL 1.0".to_jsval(cx, retval);
                return;
            },
            constants::RENDERER | constants::VENDOR => {
                "Mozilla/Servo".to_jsval(cx, retval);
                return;
            },
            constants::SHADING_LANGUAGE_VERSION => {
                "WebGL GLSL ES 1.0".to_jsval(cx, retval);
                return;
            },
            constants::UNPACK_FLIP_Y_WEBGL => {
                let unpack = self.texture_unpacking_settings.get();
                retval.set(BooleanValue(unpack.contains(TextureUnpacking::FLIP_Y_AXIS)));
                return;
            },
            constants::UNPACK_PREMULTIPLY_ALPHA_WEBGL => {
                let unpack = self.texture_unpacking_settings.get();
                retval.set(BooleanValue(
                    unpack.contains(TextureUnpacking::PREMULTIPLY_ALPHA),
                ));
                return;
            },
            constants::PACK_ALIGNMENT => {
                retval.set(UInt32Value(self.texture_packing_alignment.get() as u32));
                return;
            },
            constants::UNPACK_ALIGNMENT => {
                retval.set(UInt32Value(self.texture_unpacking_alignment.get()));
                return;
            },
            constants::UNPACK_COLORSPACE_CONVERSION_WEBGL => {
                let unpack = self.texture_unpacking_settings.get();
                retval.set(UInt32Value(
                    if unpack.contains(TextureUnpacking::CONVERT_COLORSPACE) {
                        constants::BROWSER_DEFAULT_WEBGL
                    } else {
                        constants::NONE
                    },
                ));
                return;
            },
            _ => {},
        }

        // Handle any MAX_ parameters by retrieving the limits that were stored
        // when this context was created.
        let limit = match parameter {
            constants::MAX_VERTEX_ATTRIBS => Some(self.limits.max_vertex_attribs),
            constants::MAX_TEXTURE_SIZE => Some(self.limits.max_tex_size),
            constants::MAX_CUBE_MAP_TEXTURE_SIZE => Some(self.limits.max_cube_map_tex_size),
            constants::MAX_COMBINED_TEXTURE_IMAGE_UNITS => {
                Some(self.limits.max_combined_texture_image_units)
            },
            constants::MAX_FRAGMENT_UNIFORM_VECTORS => {
                Some(self.limits.max_fragment_uniform_vectors)
            },
            constants::MAX_RENDERBUFFER_SIZE => Some(self.limits.max_renderbuffer_size),
            constants::MAX_TEXTURE_IMAGE_UNITS => Some(self.limits.max_texture_image_units),
            constants::MAX_VARYING_VECTORS => Some(self.limits.max_varying_vectors),
            constants::MAX_VERTEX_TEXTURE_IMAGE_UNITS => {
                Some(self.limits.max_vertex_texture_image_units)
            },
            constants::MAX_VERTEX_UNIFORM_VECTORS => Some(self.limits.max_vertex_uniform_vectors),
            _ => None,
        };
        if let Some(limit) = limit {
            retval.set(UInt32Value(limit));
            return;
        }

        if let Ok(value) = self.capabilities.is_enabled(parameter) {
            retval.set(BooleanValue(value));
            return;
        }

        match handle_potential_webgl_error!(
            self,
            Parameter::from_u32(parameter),
            return retval.set(NullValue())
        ) {
            Parameter::Bool(param) => {
                let (sender, receiver) = webgl_channel().unwrap();
                self.send_command(WebGLCommand::GetParameterBool(param, sender));
                retval.set(BooleanValue(receiver.recv().unwrap()))
            },
            Parameter::Bool4(param) => {
                let (sender, receiver) = webgl_channel().unwrap();
                self.send_command(WebGLCommand::GetParameterBool4(param, sender));
                receiver.recv().unwrap().to_jsval(cx, retval);
            },
            Parameter::Int(param) => {
                let (sender, receiver) = webgl_channel().unwrap();
                self.send_command(WebGLCommand::GetParameterInt(param, sender));
                retval.set(Int32Value(receiver.recv().unwrap()))
            },
            Parameter::UInt(param) => {
                let (sender, receiver) = webgl_channel().unwrap();
                self.send_command(WebGLCommand::GetParameterUInt(param, sender));
                retval.set(UInt32Value(receiver.recv().unwrap()))
            },
            Parameter::Int2(param) => {
                let (sender, receiver) = webgl_channel().unwrap();
                self.send_command(WebGLCommand::GetParameterInt2(param, sender));
                rooted!(&in(cx) let mut rval = ptr::null_mut::<JSObject>());
                create_buffer_source::<Int32>(cx, &receiver.recv().unwrap(), rval.handle_mut())
                    .unwrap();
                retval.set(ObjectValue(rval.get()))
            },
            Parameter::Int4(param) => {
                let (sender, receiver) = webgl_channel().unwrap();
                self.send_command(WebGLCommand::GetParameterInt4(param, sender));
                rooted!(&in(cx) let mut rval = ptr::null_mut::<JSObject>());
                create_buffer_source::<Int32>(cx, &receiver.recv().unwrap(), rval.handle_mut())
                    .unwrap();
                retval.set(ObjectValue(rval.get()))
            },
            Parameter::Float(param) => {
                let (sender, receiver) = webgl_channel().unwrap();
                self.send_command(WebGLCommand::GetParameterFloat(param, sender));
                retval.set(DoubleValue(receiver.recv().unwrap() as f64))
            },
            Parameter::Float2(param) => {
                let (sender, receiver) = webgl_channel().unwrap();
                self.send_command(WebGLCommand::GetParameterFloat2(param, sender));
                rooted!(&in(cx) let mut rval = ptr::null_mut::<JSObject>());
                create_buffer_source::<Float32>(cx, &receiver.recv().unwrap(), rval.handle_mut())
                    .unwrap();
                retval.set(ObjectValue(rval.get()))
            },
            Parameter::Float4(param) => {
                let (sender, receiver) = webgl_channel().unwrap();
                self.send_command(WebGLCommand::GetParameterFloat4(param, sender));
                rooted!(&in(cx) let mut rval = ptr::null_mut::<JSObject>());
                create_buffer_source::<Float32>(cx, &receiver.recv().unwrap(), rval.handle_mut())
                    .unwrap();
                retval.set(ObjectValue(rval.get()))
            },
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn GetTexParameter(
        &self,
        _cx: &mut JSContext,
        target: u32,
        pname: u32,
        mut retval: MutableHandleValue,
    ) {
        let texture_slot = handle_potential_webgl_error!(
            self,
            self.textures
                .active_texture_slot(target, self.webgl_version()),
            return retval.set(NullValue())
        );
        let texture = handle_potential_webgl_error!(
            self,
            texture_slot.get().ok_or(InvalidOperation),
            return retval.set(NullValue())
        );

        if !self
            .extension_manager
            .is_get_tex_parameter_name_enabled(pname)
        {
            self.webgl_error(InvalidEnum);
            return retval.set(NullValue());
        }

        match pname {
            constants::TEXTURE_MAG_FILTER => return retval.set(UInt32Value(texture.mag_filter())),
            constants::TEXTURE_MIN_FILTER => return retval.set(UInt32Value(texture.min_filter())),
            _ => {},
        }

        let texparam = handle_potential_webgl_error!(
            self,
            TexParameter::from_u32(pname),
            return retval.set(NullValue())
        );
        if self.webgl_version() < texparam.required_webgl_version() {
            self.webgl_error(InvalidEnum);
            return retval.set(NullValue());
        }

        if let Some(value) = texture.maybe_get_tex_parameter(texparam) {
            match value {
                TexParameterValue::Float(v) => retval.set(DoubleValue(v as f64)),
                TexParameterValue::Int(v) => retval.set(Int32Value(v)),
                TexParameterValue::Bool(v) => retval.set(BooleanValue(v)),
            }
            return;
        }

        match texparam {
            TexParameter::Float(param) => {
                let (sender, receiver) = webgl_channel().unwrap();
                self.send_command(WebGLCommand::GetTexParameterFloat(target, param, sender));
                retval.set(DoubleValue(receiver.recv().unwrap() as f64))
            },
            TexParameter::Int(param) => {
                let (sender, receiver) = webgl_channel().unwrap();
                self.send_command(WebGLCommand::GetTexParameterInt(target, param, sender));
                retval.set(Int32Value(receiver.recv().unwrap()))
            },
            TexParameter::Bool(param) => {
                let (sender, receiver) = webgl_channel().unwrap();
                self.send_command(WebGLCommand::GetTexParameterBool(target, param, sender));
                retval.set(BooleanValue(receiver.recv().unwrap()))
            },
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn GetError(&self) -> u32 {
        let error_code = if let Some(error) = self.last_error.get() {
            match error {
                WebGLError::InvalidEnum => constants::INVALID_ENUM,
                WebGLError::InvalidFramebufferOperation => constants::INVALID_FRAMEBUFFER_OPERATION,
                WebGLError::InvalidValue => constants::INVALID_VALUE,
                WebGLError::InvalidOperation => constants::INVALID_OPERATION,
                WebGLError::OutOfMemory => constants::OUT_OF_MEMORY,
                WebGLError::ContextLost => constants::CONTEXT_LOST_WEBGL,
            }
        } else {
            constants::NO_ERROR
        };
        self.last_error.set(None);
        error_code
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.2>
    fn GetContextAttributes(&self) -> Option<WebGLContextAttributes> {
        let (sender, receiver) = webgl_channel().unwrap();

        // If the send does not succeed, assume context lost
        let backtrace = capture_webgl_backtrace();
        if self
            .droppable
            .webgl_sender
            .send(WebGLCommand::GetContextAttributes(sender), backtrace)
            .is_err()
        {
            return None;
        }

        let attrs = receiver.recv().unwrap();

        Some(WebGLContextAttributes {
            alpha: attrs.alpha,
            antialias: attrs.antialias,
            depth: attrs.depth,
            failIfMajorPerformanceCaveat: false,
            preferLowPowerToHighPerformance: false,
            premultipliedAlpha: attrs.premultiplied_alpha,
            preserveDrawingBuffer: attrs.preserve_drawing_buffer,
            stencil: attrs.stencil,
        })
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.13>
    fn IsContextLost(&self) -> bool {
        false
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.14>
    fn GetSupportedExtensions(&self) -> Option<Vec<DOMString>> {
        self.extension_manager
            .init_once(|| self.get_gl_extensions());
        let extensions = self.extension_manager.get_supported_extensions();
        Some(
            extensions
                .iter()
                .map(|name| DOMString::from(*name))
                .collect(),
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.14>
    fn GetExtension(
        &self,
        cx: &mut js::context::JSContext,
        name: DOMString,
        rval: MutableHandleObject,
    ) {
        self.extension_manager
            .init_once(|| self.get_gl_extensions());
        self.extension_manager
            .get_or_init_extension(cx, &name, self, rval);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn ActiveTexture(&self, texture: u32) {
        handle_potential_webgl_error!(self, self.textures.set_active_unit_enum(texture), return);
        self.send_command(WebGLCommand::ActiveTexture(texture));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn BlendColor(&self, r: f32, g: f32, b: f32, a: f32) {
        // WebGL 1 clamps the blend colour to [0, 1] unless a float colour buffer extension is
        // enabled (WEBGL_color_buffer_float, EXT_color_buffer_half_float); desktop GL doesn't.
        let clamp = self.webgl_version() == WebGLVersion::WebGL1 &&
            !self.extension_manager.is_blend_color_unclamped();
        let [r, g, b, a] = [r, g, b, a].map(|v| if clamp { v.clamp(0., 1.) } else { v });
        self.send_command(WebGLCommand::BlendColor(r, g, b, a));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn BlendEquation(&self, mode: u32) {
        handle_potential_webgl_error!(self, self.validate_blend_mode(mode), return);
        self.send_command(WebGLCommand::BlendEquation(mode))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn BlendEquationSeparate(&self, mode_rgb: u32, mode_alpha: u32) {
        handle_potential_webgl_error!(self, self.validate_blend_mode(mode_rgb), return);
        handle_potential_webgl_error!(self, self.validate_blend_mode(mode_alpha), return);
        self.send_command(WebGLCommand::BlendEquationSeparate(mode_rgb, mode_alpha));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn BlendFunc(&self, src_factor: u32, dest_factor: u32) {
        // From the WebGL 1.0 spec, 6.13: Viewport Depth Range:
        //
        //     A call to blendFunc will generate an INVALID_OPERATION error if one of the two
        //     factors is set to CONSTANT_COLOR or ONE_MINUS_CONSTANT_COLOR and the other to
        //     CONSTANT_ALPHA or ONE_MINUS_CONSTANT_ALPHA.
        let webgl2 = self.webgl_version() == WebGLVersion::WebGL2;
        if !is_blend_factor(src_factor, false, webgl2) ||
            !is_blend_factor(dest_factor, true, webgl2)
        {
            return self.webgl_error(InvalidEnum);
        }
        if has_invalid_blend_constants(src_factor, dest_factor) {
            return self.webgl_error(InvalidOperation);
        }
        if has_invalid_blend_constants(dest_factor, src_factor) {
            return self.webgl_error(InvalidOperation);
        }

        self.send_command(WebGLCommand::BlendFunc(src_factor, dest_factor));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn BlendFuncSeparate(&self, src_rgb: u32, dest_rgb: u32, src_alpha: u32, dest_alpha: u32) {
        // From the WebGL 1.0 spec, 6.13: Viewport Depth Range:
        //
        //     A call to blendFuncSeparate will generate an INVALID_OPERATION error if srcRGB is
        //     set to CONSTANT_COLOR or ONE_MINUS_CONSTANT_COLOR and dstRGB is set to
        //     CONSTANT_ALPHA or ONE_MINUS_CONSTANT_ALPHA or vice versa.
        let webgl2 = self.webgl_version() == WebGLVersion::WebGL2;
        if !is_blend_factor(src_rgb, false, webgl2) ||
            !is_blend_factor(dest_rgb, true, webgl2) ||
            !is_blend_factor(src_alpha, false, webgl2) ||
            !is_blend_factor(dest_alpha, true, webgl2)
        {
            return self.webgl_error(InvalidEnum);
        }
        if has_invalid_blend_constants(src_rgb, dest_rgb) {
            return self.webgl_error(InvalidOperation);
        }
        if has_invalid_blend_constants(dest_rgb, src_rgb) {
            return self.webgl_error(InvalidOperation);
        }

        self.send_command(WebGLCommand::BlendFuncSeparate(
            src_rgb, dest_rgb, src_alpha, dest_alpha,
        ));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn AttachShader(&self, program: &WebGLProgram, shader: &WebGLShader) {
        handle_potential_webgl_error!(self, self.validate_ownership(program), return);
        handle_potential_webgl_error!(self, self.validate_ownership(shader), return);
        handle_potential_webgl_error!(self, program.attach_shader(shader));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn DetachShader(&self, program: &WebGLProgram, shader: &WebGLShader) {
        handle_potential_webgl_error!(self, self.validate_ownership(program), return);
        handle_potential_webgl_error!(self, self.validate_ownership(shader), return);
        handle_potential_webgl_error!(self, program.detach_shader(shader));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn BindAttribLocation(&self, program: &WebGLProgram, index: u32, name: DOMString) {
        handle_potential_webgl_error!(self, self.validate_ownership(program), return);
        handle_potential_webgl_error!(self, program.bind_attrib_location(index, name));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.5>
    fn BindBuffer(&self, cx: &mut JSContext, target: u32, buffer: Option<&WebGLBuffer>) {
        let current_vao;
        let slot = match target {
            constants::ARRAY_BUFFER => &self.bound_buffer_array,
            constants::ELEMENT_ARRAY_BUFFER => {
                current_vao = self.current_vao(cx);
                current_vao.element_array_buffer()
            },
            _ => return self.webgl_error(InvalidEnum),
        };
        self.bind_buffer_maybe(slot, target, buffer);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.6>
    fn BindFramebuffer(&self, target: u32, framebuffer: Option<&WebGLFramebuffer>) {
        handle_potential_webgl_error!(
            self,
            self.validate_new_framebuffer_binding(framebuffer),
            return
        );

        if target != constants::FRAMEBUFFER {
            return self.webgl_error(InvalidEnum);
        }

        self.bind_framebuffer_to(target, framebuffer, &self.bound_draw_framebuffer)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.7>
    fn BindRenderbuffer(&self, target: u32, renderbuffer: Option<&WebGLRenderbuffer>) {
        if let Some(rb) = renderbuffer {
            handle_potential_webgl_error!(self, self.validate_ownership(rb), return);
        }

        if target != constants::RENDERBUFFER {
            return self.webgl_error(InvalidEnum);
        }

        match renderbuffer {
            // Implementations differ on what to do in the deleted
            // case: Chromium currently unbinds, and Gecko silently
            // returns.  The conformance tests don't cover this case.
            Some(renderbuffer) if !renderbuffer.is_deleted() => {
                self.bound_renderbuffer.set(Some(renderbuffer));
                renderbuffer.bind(target);
            },
            _ => {
                if renderbuffer.is_some() {
                    self.webgl_error(InvalidOperation);
                }

                self.bound_renderbuffer.set(None);
                // Unbind the currently bound renderbuffer
                self.send_command(WebGLCommand::BindRenderbuffer(target, None));
            },
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn BindTexture(&self, target: u32, texture: Option<&WebGLTexture>) {
        if let Some(texture) = texture {
            handle_potential_webgl_error!(self, self.validate_ownership(texture), return);
        }

        let texture_slot = handle_potential_webgl_error!(
            self,
            self.textures
                .active_texture_slot(target, self.webgl_version()),
            return
        );

        if let Some(texture) = texture {
            handle_potential_webgl_error!(self, texture.bind(target), return);
        } else {
            self.send_command(WebGLCommand::BindTexture(target, None));
        }
        // Partii (perf): bindTexture doesn't move the draw-state generation; the unit's binding
        // sequence number does the same job for the sampler check (draw_state_cached).
        if texture_slot.get().as_deref().map(|t| t as *const WebGLTexture) !=
            texture.map(|t| t as *const WebGLTexture)
        {
            self.textures.note_active_unit_changed();
        }
        texture_slot.set(texture);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn GenerateMipmap(&self, target: u32) {
        let texture_slot = handle_potential_webgl_error!(
            self,
            self.textures
                .active_texture_slot(target, self.webgl_version()),
            return
        );
        let texture =
            handle_potential_webgl_error!(self, texture_slot.get().ok_or(InvalidOperation), return);
        handle_potential_webgl_error!(self, texture.generate_mipmap());
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.5>
    fn BufferData_(
        &self,
        cx: &mut JSContext,
        target: u32,
        data: Option<MaybeSharedArrayBufferViewOrMaybeSharedArrayBuffer>,
        usage: u32,
    ) {
        let usage = handle_potential_webgl_error!(self, self.buffer_usage(usage), return);
        let bound_buffer =
            handle_potential_webgl_error!(self, self.bound_buffer(cx, target), return);
        self.buffer_data(cx.no_gc(), target, data, usage, bound_buffer)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.5>
    fn BufferData(&self, cx: &mut JSContext, target: u32, size: i64, usage: u32) {
        let usage = handle_potential_webgl_error!(self, self.buffer_usage(usage), return);
        let bound_buffer =
            handle_potential_webgl_error!(self, self.bound_buffer(cx, target), return);
        self.buffer_data_(target, size, usage, bound_buffer)
    }

    // https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.5
    fn BufferSubData(
        &self,
        cx: &mut JSContext,
        target: u32,
        offset: i64,
        data: MaybeSharedArrayBufferViewOrMaybeSharedArrayBuffer,
    ) {
        let bound_buffer =
            handle_potential_webgl_error!(self, self.bound_buffer(cx, target), return);
        self.buffer_sub_data(cx.no_gc(), target, offset, data, bound_buffer)
    }

    // https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8
    fn CompressedTexImage2D(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        internal_format: u32,
        width: i32,
        height: i32,
        border: i32,
        data: CustomAutoRooterGuard<ArrayBufferView>,
    ) {
        let data = array_buffer_view_bytes(&data, no_gc);
        self.compressed_tex_image_2d(
            target,
            level,
            internal_format,
            width,
            height,
            border,
            CompressedSource::Bytes(data),
        )
    }

    // https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8
    fn CompressedTexSubImage2D(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        data: CustomAutoRooterGuard<ArrayBufferView>,
    ) {
        let data = array_buffer_view_bytes(&data, no_gc);
        self.compressed_tex_sub_image_2d(
            target,
            level,
            xoffset,
            yoffset,
            width,
            height,
            format,
            CompressedSource::Bytes(data),
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn CopyTexImage2D(
        &self,
        target: u32,
        level: i32,
        internal_format: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        border: i32,
    ) {
        handle_potential_webgl_error!(self, self.validate_framebuffer(), return);
        handle_potential_webgl_error!(self, self.validate_read_buffer_image(), return);

        let validator = CommonTexImage2DValidator::new(
            self,
            target,
            level,
            internal_format,
            width,
            height,
            border,
        );
        let CommonTexImage2DValidatorResult {
            texture,
            target,
            level,
            internal_format,
            width,
            height,
            border: _,
        } = match validator.validate() {
            Ok(result) => result,
            Err(_) => return,
        };

        if texture.is_immutable() {
            return self.webgl_error(InvalidOperation);
        }
        if self.copy_reads_image(&texture, target, level) {
            return self.webgl_error(InvalidOperation);
        }

        // WebGL 2: the GL ES 3.0 rules, against the read framebuffer's read buffer.
        if !self.copy_format_compatible(internal_format, false) {
            return self.webgl_error(InvalidOperation);
        }

        // (WebGL 2 was checked above; the comparison below is WebGL 1's.)
        let framebuffer_format = match self.bound_draw_framebuffer.get() {
            _ if self.webgl_version() == WebGLVersion::WebGL2 => Some(internal_format),
            Some(fb) => match fb.attachment(constants::COLOR_ATTACHMENT0) {
                Some(WebGLFramebufferAttachmentRoot::Renderbuffer(rb)) => {
                    TexFormat::from_gl_constant(rb.internal_format())
                },
                Some(WebGLFramebufferAttachmentRoot::Texture(texture)) => texture
                    .image_info_for_target(&target, 0)
                    .map(|info| info.internal_format()),
                None => None,
            },
            None => {
                let attrs = self.GetContextAttributes().unwrap();
                Some(if attrs.alpha {
                    TexFormat::RGBA
                } else {
                    TexFormat::RGB
                })
            },
        };

        let framebuffer_format = match framebuffer_format {
            Some(f) => f,
            None => {
                self.webgl_error(InvalidOperation);
                return;
            },
        };

        // Compare base formats: attachments may have sized formats (RGBA8, from texStorage2D
        // or renderbufferStorage) where copyTexImage2D asks for an unsized one.
        match (
            framebuffer_format.to_unsized(),
            internal_format.to_unsized(),
        ) {
            (a, b) if a == b => (),
            (TexFormat::RGBA, TexFormat::RGB) => (),
            (TexFormat::RGBA, TexFormat::Alpha) => (),
            (TexFormat::RGBA, TexFormat::Luminance) => (),
            (TexFormat::RGBA, TexFormat::LuminanceAlpha) => (),
            (TexFormat::RGB, TexFormat::Luminance) => (),
            _ => {
                self.webgl_error(InvalidOperation);
                return;
            },
        }

        // NB: TexImage2D depth is always equal to 1
        handle_potential_webgl_error!(
            self,
            texture.initialize(target, width, height, 1, internal_format, level, None)
        );

        let msg = WebGLCommand::CopyTexImage2D {
            target: target.as_gl_constant(),
            level: level as i32,
            internal_format: internal_format.as_gl_constant(),
            x,
            y,
            width: width as i32,
            height: height as i32,
            read_size: self.read_framebuffer_size(),
        };

        self.send_command(msg);

        if let Some(framebuffer) = self.bound_draw_framebuffer.get() {
            framebuffer.invalidate_texture(&texture);
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn CopyTexSubImage2D(
        &self,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) {
        handle_potential_webgl_error!(self, self.validate_framebuffer(), return);
        handle_potential_webgl_error!(self, self.validate_read_buffer_image(), return);

        // NB: We use a dummy (valid) format and border in order to reuse the
        // common validations, but this should have its own validator.
        let validator = CommonTexImage2DValidator::new(
            self,
            target,
            level,
            TexFormat::RGBA.as_gl_constant(),
            width,
            height,
            0,
        )
        .sub_image();
        let CommonTexImage2DValidatorResult {
            texture,
            target,
            level,
            width,
            height,
            ..
        } = match validator.validate() {
            Ok(result) => result,
            Err(_) => return,
        };

        let image_info = match texture.image_info_for_target(&target, level) {
            Some(info) => info,
            None => return self.webgl_error(InvalidOperation),
        };

        // GL_INVALID_VALUE is generated if:
        //   - xoffset or yoffset is less than 0
        //   - x offset plus the width is greater than the texture width
        //   - y offset plus the height is greater than the texture height
        if xoffset < 0 ||
            (xoffset as u32 + width) > image_info.width() ||
            yoffset < 0 ||
            (yoffset as u32 + height) > image_info.height()
        {
            self.webgl_error(InvalidValue);
            return;
        }
        if self.copy_reads_image(&texture, target, level) {
            return self.webgl_error(InvalidOperation);
        }
        if !self.copy_format_compatible(image_info.internal_format(), true) {
            return self.webgl_error(InvalidOperation);
        }

        // Pixels outside the read framebuffer are left alone (WebGL 1.0 5.14.8; GL leaves
        // them undefined), so only the part inside is copied.
        let Some((xoffset, yoffset, x, y, width, height)) = clip_copy_to_read_framebuffer(
            self.read_framebuffer_size(),
            xoffset,
            yoffset,
            x,
            y,
            width as i32,
            height as i32,
        ) else {
            return;
        };
        let msg = WebGLCommand::CopyTexSubImage2D(
            target.as_gl_constant(),
            level as i32,
            xoffset,
            yoffset,
            x,
            y,
            width,
            height,
        );

        self.send_command(msg);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.11>
    fn Clear(&self, mask: u32) {
        handle_potential_webgl_error!(self, self.validate_framebuffer(), return);
        if mask &
            !(constants::DEPTH_BUFFER_BIT |
                constants::STENCIL_BUFFER_BIT |
                constants::COLOR_BUFFER_BIT) !=
            0
        {
            return self.webgl_error(InvalidValue);
        }

        self.send_command(WebGLCommand::Clear(mask));
        self.mark_as_dirty();
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn ClearColor(&self, red: f32, green: f32, blue: f32, alpha: f32) {
        self.current_clear_color.set((red, green, blue, alpha));
        self.send_command(WebGLCommand::ClearColor(red, green, blue, alpha));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn ClearDepth(&self, depth: f32) {
        self.send_command(WebGLCommand::ClearDepth(depth))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn ClearStencil(&self, stencil: i32) {
        self.send_command(WebGLCommand::ClearStencil(stencil))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn ColorMask(&self, r: bool, g: bool, b: bool, a: bool) {
        self.send_command(WebGLCommand::ColorMask(r, g, b, a))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn CullFace(&self, mode: u32) {
        match mode {
            constants::FRONT | constants::BACK | constants::FRONT_AND_BACK => {
                self.send_command(WebGLCommand::CullFace(mode))
            },
            _ => self.webgl_error(InvalidEnum),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn FrontFace(&self, mode: u32) {
        match mode {
            constants::CW | constants::CCW => self.send_command(WebGLCommand::FrontFace(mode)),
            _ => self.webgl_error(InvalidEnum),
        }
    }
    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn DepthFunc(&self, func: u32) {
        match func {
            constants::NEVER |
            constants::LESS |
            constants::EQUAL |
            constants::LEQUAL |
            constants::GREATER |
            constants::NOTEQUAL |
            constants::GEQUAL |
            constants::ALWAYS => self.send_command(WebGLCommand::DepthFunc(func)),
            _ => self.webgl_error(InvalidEnum),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn DepthMask(&self, flag: bool) {
        self.send_command(WebGLCommand::DepthMask(flag))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn DepthRange(&self, near: f32, far: f32) {
        // https://www.khronos.org/registry/webgl/specs/latest/1.0/#VIEWPORT_DEPTH_RANGE
        if near > far {
            return self.webgl_error(InvalidOperation);
        }
        self.send_command(WebGLCommand::DepthRange(near, far))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn Enable(&self, cap: u32) {
        if handle_potential_webgl_error!(self, self.capabilities.set(cap, true), return) {
            self.send_command(WebGLCommand::Enable(cap));
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn Disable(&self, cap: u32) {
        if handle_potential_webgl_error!(self, self.capabilities.set(cap, false), return) {
            self.send_command(WebGLCommand::Disable(cap));
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn CompileShader(&self, shader: &WebGLShader) {
        handle_potential_webgl_error!(self, self.validate_ownership(shader), return);
        handle_potential_webgl_error!(
            self,
            shader.compile(self.webgl_version, &self.limits, &self.extension_manager)
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.5>
    fn CreateBuffer(&self, cx: &mut JSContext) -> Option<DomRoot<WebGLBuffer>> {
        WebGLBuffer::maybe_new(cx, self)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.6>
    fn CreateFramebuffer(&self, cx: &mut JSContext) -> Option<DomRoot<WebGLFramebuffer>> {
        WebGLFramebuffer::maybe_new(cx, self)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.7>
    fn CreateRenderbuffer(&self, cx: &mut JSContext) -> Option<DomRoot<WebGLRenderbuffer>> {
        WebGLRenderbuffer::maybe_new(cx, self)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn CreateTexture(&self, cx: &mut JSContext) -> Option<DomRoot<WebGLTexture>> {
        WebGLTexture::maybe_new(cx, self)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn CreateProgram(&self, cx: &mut JSContext) -> Option<DomRoot<WebGLProgram>> {
        WebGLProgram::maybe_new(cx, self)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn CreateShader(&self, cx: &mut JSContext, shader_type: u32) -> Option<DomRoot<WebGLShader>> {
        match shader_type {
            constants::VERTEX_SHADER | constants::FRAGMENT_SHADER => {},
            _ => {
                self.webgl_error(InvalidEnum);
                return None;
            },
        }
        WebGLShader::maybe_new(cx, self, shader_type)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.5>
    fn DeleteBuffer(&self, cx: &mut JSContext, buffer: Option<&WebGLBuffer>) {
        let buffer = match buffer {
            Some(buffer) => buffer,
            None => return,
        };
        handle_potential_webgl_error!(self, self.validate_ownership(buffer), return);
        if buffer.is_marked_for_deletion() {
            return;
        }
        self.current_vao(cx).unbind_buffer(buffer);
        if self.bound_buffer_array.get().is_some_and(|b| buffer == &*b) {
            self.bound_buffer_array.set(None);
            buffer.decrement_attached_counter(Operation::Infallible);
        }
        buffer.mark_for_deletion(Operation::Infallible);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.6>
    fn DeleteFramebuffer(&self, framebuffer: Option<&WebGLFramebuffer>) {
        if let Some(framebuffer) = framebuffer {
            // https://immersive-web.github.io/webxr/#opaque-framebuffer
            // Can opaque framebuffers be deleted?
            // https://github.com/immersive-web/webxr/issues/855
            handle_potential_webgl_error!(self, framebuffer.validate_transparent(), return);
            handle_potential_webgl_error!(self, self.validate_ownership(framebuffer), return);
            if let Some(bound_object) = self.bound_draw_framebuffer.get() &&
                bound_object.id() == framebuffer.id()
            {
                self.bound_draw_framebuffer.set(None);
                self.send_command(WebGLCommand::BindFramebuffer(
                    framebuffer.target().unwrap(),
                    WebGLFramebufferBindingRequest::Default,
                ));
            }
            // WebGL 2 binds READ_FRAMEBUFFER separately: deleting the read framebuffer reverts
            // it to the default one too (a dangling read binding made readPixels check the
            // deleted framebuffer's formats).
            if let Some(bound_object) = self.bound_read_framebuffer.get() &&
                bound_object.id() == framebuffer.id()
            {
                self.bound_read_framebuffer.set(None);
                if self.webgl_version() == WebGLVersion::WebGL2 {
                    self.send_command(WebGLCommand::BindFramebuffer(
                        WebGL2RenderingContextConstants::READ_FRAMEBUFFER,
                        WebGLFramebufferBindingRequest::Default,
                    ));
                }
            }
            framebuffer.delete(Operation::Infallible)
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.7>
    fn DeleteRenderbuffer(&self, renderbuffer: Option<&WebGLRenderbuffer>) {
        if let Some(renderbuffer) = renderbuffer {
            handle_potential_webgl_error!(self, self.validate_ownership(renderbuffer), return);
            if let Some(bound_object) = self.bound_renderbuffer.get() &&
                bound_object.id() == renderbuffer.id()
            {
                self.bound_renderbuffer.set(None);
                self.send_command(WebGLCommand::BindRenderbuffer(
                    constants::RENDERBUFFER,
                    None,
                ));
            }
            renderbuffer.delete(Operation::Infallible)
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn DeleteTexture(&self, texture: Option<&WebGLTexture>) {
        if let Some(texture) = texture {
            handle_potential_webgl_error!(self, self.validate_ownership(texture), return);

            // From the GLES 2.0.25 spec, page 85:
            //
            //     "If a texture that is currently bound to one of the targets
            //      TEXTURE_2D, or TEXTURE_CUBE_MAP is deleted, it is as though
            //      BindTexture had been executed with the same target and texture
            //      zero."
            //
            // The same texture may be bound to multiple texture units.
            let mut active_unit_enum = self.textures.active_unit_enum();
            for (unit_enum, slot) in self.textures.iter() {
                if let Some(target) = slot.unbind(texture) {
                    if unit_enum != active_unit_enum {
                        self.send_command(WebGLCommand::ActiveTexture(unit_enum));
                        active_unit_enum = unit_enum;
                    }
                    self.send_command(WebGLCommand::BindTexture(target, None));
                }
            }

            // Restore bound texture unit if it has been changed.
            if active_unit_enum != self.textures.active_unit_enum() {
                self.send_command(WebGLCommand::ActiveTexture(
                    self.textures.active_unit_enum(),
                ));
            }

            texture.delete(Operation::Infallible)
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn DeleteProgram(&self, program: Option<&WebGLProgram>) {
        if let Some(program) = program {
            handle_potential_webgl_error!(self, self.validate_ownership(program), return);
            program.mark_for_deletion(Operation::Infallible)
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn DeleteShader(&self, shader: Option<&WebGLShader>) {
        if let Some(shader) = shader {
            handle_potential_webgl_error!(self, self.validate_ownership(shader), return);
            shader.mark_for_deletion(Operation::Infallible)
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.11>
    fn DrawArrays(&self, cx: &mut JSContext, mode: u32, first: i32, count: i32) {
        handle_potential_webgl_error!(self, self.draw_arrays_instanced(cx, mode, first, count, 1));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.11>
    fn DrawElements(&self, cx: &mut JSContext, mode: u32, count: i32, type_: u32, offset: i64) {
        handle_potential_webgl_error!(
            self,
            self.draw_elements_instanced(cx, mode, count, type_, offset, 1)
        );
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn EnableVertexAttribArray(&self, cx: &mut JSContext, attrib_id: u32) {
        if attrib_id >= self.limits.max_vertex_attribs {
            return self.webgl_error(InvalidValue);
        }
        match self.webgl_version() {
            WebGLVersion::WebGL1 => self
                .current_vao(cx)
                .enabled_vertex_attrib_array(attrib_id, true),
            WebGLVersion::WebGL2 => self
                .current_vao_webgl2(cx)
                .enabled_vertex_attrib_array(attrib_id, true),
        };
        self.send_command(WebGLCommand::EnableVertexAttribArray(attrib_id));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn DisableVertexAttribArray(&self, cx: &mut JSContext, attrib_id: u32) {
        if attrib_id >= self.limits.max_vertex_attribs {
            return self.webgl_error(InvalidValue);
        }
        match self.webgl_version() {
            WebGLVersion::WebGL1 => self
                .current_vao(cx)
                .enabled_vertex_attrib_array(attrib_id, false),
            WebGLVersion::WebGL2 => self
                .current_vao_webgl2(cx)
                .enabled_vertex_attrib_array(attrib_id, false),
        };
        self.send_command(WebGLCommand::DisableVertexAttribArray(attrib_id));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn GetActiveUniform(
        &self,
        cx: &mut JSContext,
        program: &WebGLProgram,
        index: u32,
    ) -> Option<DomRoot<WebGLActiveInfo>> {
        handle_potential_webgl_error!(self, self.validate_ownership(program), return None);
        match program.get_active_uniform(cx, index) {
            Ok(ret) => Some(ret),
            Err(e) => {
                self.webgl_error(e);
                None
            },
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn GetActiveAttrib(
        &self,
        cx: &mut JSContext,
        program: &WebGLProgram,
        index: u32,
    ) -> Option<DomRoot<WebGLActiveInfo>> {
        handle_potential_webgl_error!(self, self.validate_ownership(program), return None);
        handle_potential_webgl_error!(self, program.get_active_attrib(cx, index).map(Some), None)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn GetAttribLocation(&self, program: &WebGLProgram, name: DOMString) -> i32 {
        handle_potential_webgl_error!(self, self.validate_ownership(program), return -1);
        handle_potential_webgl_error!(self, program.get_attrib_location(name), -1)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.6>
    fn GetFramebufferAttachmentParameter(
        &self,
        cx: &mut JSContext,
        target: u32,
        attachment: u32,
        pname: u32,
        mut retval: MutableHandleValue,
    ) {
        // Check if currently bound framebuffer is non-zero as per spec.
        if let Some(fb) = self.bound_draw_framebuffer.get() {
            // Opaque framebuffers cannot have their attachments inspected
            // https://immersive-web.github.io/webxr/#opaque-framebuffer
            handle_potential_webgl_error!(
                self,
                fb.validate_transparent(),
                return retval.set(NullValue())
            );
        } else {
            self.webgl_error(InvalidOperation);
            return retval.set(NullValue());
        }

        // Note: commented out stuff is for the WebGL2 standard.
        let target_matches = match target {
            // constants::READ_FRAMEBUFFER |
            // constants::DRAW_FRAMEBUFFER => true,
            constants::FRAMEBUFFER => true,
            _ => false,
        };
        let attachment_matches = match attachment {
            // constants::MAX_COLOR_ATTACHMENTS ... gl::COLOR_ATTACHMENT0 |
            // constants::BACK |
            constants::COLOR_ATTACHMENT0 |
            constants::DEPTH_STENCIL_ATTACHMENT |
            constants::DEPTH_ATTACHMENT |
            constants::STENCIL_ATTACHMENT => true,
            _ => false,
        };
        // FRAMEBUFFER_ATTACHMENT_COMPONENT_TYPE_EXT (EXT_color_buffer_half_float,
        // WEBGL_color_buffer_float).
        const COMPONENT_TYPE: u32 = 0x8211;
        let component_type_query = pname == COMPONENT_TYPE &&
            self.extension_manager
                .is_attachment_component_type_query_enabled();
        let pname_matches = match pname {
            // constants::FRAMEBUFFER_ATTACHMENT_ALPHA_SIZE |
            // constants::FRAMEBUFFER_ATTACHMENT_BLUE_SIZE |
            // constants::FRAMEBUFFER_ATTACHMENT_COLOR_ENCODING |
            // constants::FRAMEBUFFER_ATTACHMENT_COMPONENT_TYPE |
            // constants::FRAMEBUFFER_ATTACHMENT_DEPTH_SIZE |
            // constants::FRAMEBUFFER_ATTACHMENT_GREEN_SIZE |
            // constants::FRAMEBUFFER_ATTACHMENT_RED_SIZE |
            // constants::FRAMEBUFFER_ATTACHMENT_STENCIL_SIZE |
            // constants::FRAMEBUFFER_ATTACHMENT_TEXTURE_LAYER |
            constants::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME |
            constants::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE |
            constants::FRAMEBUFFER_ATTACHMENT_TEXTURE_CUBE_MAP_FACE |
            constants::FRAMEBUFFER_ATTACHMENT_TEXTURE_LEVEL => true,
            _ => component_type_query,
        };

        let bound_attachment_matches = match self
            .bound_draw_framebuffer
            .get()
            .unwrap()
            .attachment(attachment)
        {
            Some(attachment_root) => match attachment_root {
                WebGLFramebufferAttachmentRoot::Renderbuffer(_) => {
                    component_type_query ||
                        matches!(
                            pname,
                            constants::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE |
                                constants::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME
                        )
                },
                WebGLFramebufferAttachmentRoot::Texture(_) => {
                    component_type_query ||
                        matches!(
                            pname,
                            constants::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE |
                                constants::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME |
                                constants::FRAMEBUFFER_ATTACHMENT_TEXTURE_LEVEL |
                                constants::FRAMEBUFFER_ATTACHMENT_TEXTURE_CUBE_MAP_FACE
                        )
                },
            },
            _ => matches!(pname, constants::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE),
        };

        // A depth-stencil image has two component types (ES 3.0 6.1.13).
        if target_matches &&
            component_type_query &&
            attachment == constants::DEPTH_STENCIL_ATTACHMENT
        {
            self.webgl_error(InvalidOperation);
            return retval.set(NullValue());
        }

        if !target_matches || !attachment_matches || !pname_matches || !bound_attachment_matches {
            self.webgl_error(InvalidEnum);
            return retval.set(NullValue());
        }

        // From the GLES2 spec:
        //
        //     If the value of FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE is NONE,
        //     then querying any other pname will generate INVALID_ENUM.
        //
        // otherwise, return `WebGLRenderbuffer` or `WebGLTexture` dom object
        if pname == constants::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME {
            // if fb is None, an INVALID_OPERATION is returned
            // at the beggining of the function, so `.unwrap()` will never panic
            let fb = self.bound_draw_framebuffer.get().unwrap();
            if let Some(webgl_attachment) = fb.attachment(attachment) {
                match webgl_attachment {
                    WebGLFramebufferAttachmentRoot::Renderbuffer(rb) => {
                        rb.to_jsval(cx, retval);
                        return;
                    },
                    WebGLFramebufferAttachmentRoot::Texture(texture) => {
                        texture.to_jsval(cx, retval);
                        return;
                    },
                }
            }
            self.webgl_error(InvalidEnum);
            return retval.set(NullValue());
        }

        let (sender, receiver) = webgl_channel().unwrap();
        self.send_command(WebGLCommand::GetFramebufferAttachmentParameter(
            target, attachment, pname, sender,
        ));

        retval.set(Int32Value(receiver.recv().unwrap()))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.7>
    fn GetRenderbufferParameter(
        &self,
        _cx: &mut JSContext,
        target: u32,
        pname: u32,
        mut retval: MutableHandleValue,
    ) {
        // We do not check to see if the renderbuffer came from an opaque framebuffer
        // https://github.com/immersive-web/webxr/issues/862
        let target_matches = target == constants::RENDERBUFFER;

        let pname_matches = matches!(
            pname,
            constants::RENDERBUFFER_WIDTH |
                constants::RENDERBUFFER_HEIGHT |
                constants::RENDERBUFFER_INTERNAL_FORMAT |
                constants::RENDERBUFFER_RED_SIZE |
                constants::RENDERBUFFER_GREEN_SIZE |
                constants::RENDERBUFFER_BLUE_SIZE |
                constants::RENDERBUFFER_ALPHA_SIZE |
                constants::RENDERBUFFER_DEPTH_SIZE |
                constants::RENDERBUFFER_STENCIL_SIZE
        );

        if !target_matches || !pname_matches {
            self.webgl_error(InvalidEnum);
            return retval.set(NullValue());
        }

        if self.bound_renderbuffer.get().is_none() {
            self.webgl_error(InvalidOperation);
            return retval.set(NullValue());
        }

        let result = if pname == constants::RENDERBUFFER_INTERNAL_FORMAT {
            let rb = self.bound_renderbuffer.get().unwrap();
            rb.internal_format() as i32
        } else {
            let (sender, receiver) = webgl_channel().unwrap();
            self.send_command(WebGLCommand::GetRenderbufferParameter(
                target, pname, sender,
            ));
            receiver.recv().unwrap()
        };

        retval.set(Int32Value(result))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetProgramInfoLog(&self, program: &WebGLProgram) -> Option<DOMString> {
        handle_potential_webgl_error!(self, self.validate_ownership(program), return None);
        match program.get_info_log() {
            Ok(value) => Some(DOMString::from(value)),
            Err(e) => {
                self.webgl_error(e);
                None
            },
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetProgramParameter(
        &self,
        _cx: &mut JSContext,
        program: &WebGLProgram,
        param: u32,
        mut retval: MutableHandleValue,
    ) {
        handle_potential_webgl_error!(
            self,
            self.validate_ownership(program),
            return retval.set(NullValue())
        );
        if program.is_deleted() {
            self.webgl_error(InvalidOperation);
            return retval.set(NullValue());
        }
        retval.set(match param {
            constants::DELETE_STATUS => BooleanValue(program.is_marked_for_deletion()),
            constants::LINK_STATUS => BooleanValue(program.is_linked()),
            constants::VALIDATE_STATUS => {
                // FIXME(nox): This could be cached on the DOM side when we call validateProgram
                // but I'm not sure when the value should be reset.
                let (sender, receiver) = webgl_channel().unwrap();
                self.send_command(WebGLCommand::GetProgramValidateStatus(program.id(), sender));
                BooleanValue(receiver.recv().unwrap())
            },
            constants::ATTACHED_SHADERS => {
                // FIXME(nox): This allocates a vector and roots a couple of shaders for nothing.
                Int32Value(
                    program
                        .attached_shaders()
                        .map(|shaders| shaders.len() as i32)
                        .unwrap_or(0),
                )
            },
            constants::ACTIVE_ATTRIBUTES => Int32Value(program.active_attribs().len() as i32),
            constants::ACTIVE_UNIFORMS => Int32Value(program.active_uniforms().len() as i32),
            KHRParallelShaderCompileConstants::COMPLETION_STATUS_KHR
                if self.extension_manager.is_parallel_shader_compile_enabled() =>
            {
                BooleanValue(program.link_completed())
            },
            _ => {
                self.webgl_error(InvalidEnum);
                NullValue()
            },
        })
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetShaderInfoLog(&self, shader: &WebGLShader) -> Option<DOMString> {
        handle_potential_webgl_error!(self, self.validate_ownership(shader), return None);
        Some(shader.info_log())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetShaderParameter(
        &self,
        _cx: &mut JSContext,
        shader: &WebGLShader,
        param: u32,
        mut retval: MutableHandleValue,
    ) {
        handle_potential_webgl_error!(
            self,
            self.validate_ownership(shader),
            return retval.set(NullValue())
        );
        if shader.is_deleted() {
            self.webgl_error(InvalidValue);
            return retval.set(NullValue());
        }
        retval.set(match param {
            constants::DELETE_STATUS => BooleanValue(shader.is_marked_for_deletion()),
            constants::COMPILE_STATUS => BooleanValue(shader.successfully_compiled()),
            constants::SHADER_TYPE => UInt32Value(shader.gl_type()),
            KHRParallelShaderCompileConstants::COMPLETION_STATUS_KHR
                if self.extension_manager.is_parallel_shader_compile_enabled() =>
            {
                BooleanValue(shader.compile_completed())
            },
            _ => {
                self.webgl_error(InvalidEnum);
                NullValue()
            },
        })
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetShaderPrecisionFormat(
        &self,
        cx: &mut JSContext,
        shader_type: u32,
        precision_type: u32,
    ) -> Option<DomRoot<WebGLShaderPrecisionFormat>> {
        match shader_type {
            constants::FRAGMENT_SHADER | constants::VERTEX_SHADER => (),
            _ => {
                self.webgl_error(InvalidEnum);
                return None;
            },
        }

        match precision_type {
            constants::LOW_FLOAT |
            constants::MEDIUM_FLOAT |
            constants::HIGH_FLOAT |
            constants::LOW_INT |
            constants::MEDIUM_INT |
            constants::HIGH_INT => (),
            _ => {
                self.webgl_error(InvalidEnum);
                return None;
            },
        }

        let (sender, receiver) = webgl_channel().unwrap();
        self.send_command(WebGLCommand::GetShaderPrecisionFormat(
            shader_type,
            precision_type,
            sender,
        ));

        let (range_min, range_max, precision) = receiver.recv().unwrap();
        Some(WebGLShaderPrecisionFormat::new(
            cx,
            &self.global(),
            range_min,
            range_max,
            precision,
        ))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn GetUniformLocation(
        &self,
        cx: &mut JSContext,
        program: &WebGLProgram,
        name: DOMString,
    ) -> Option<DomRoot<WebGLUniformLocation>> {
        handle_potential_webgl_error!(self, self.validate_ownership(program), return None);
        handle_potential_webgl_error!(self, program.get_uniform_location(cx, name), None)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetVertexAttrib(
        &self,
        cx: &mut JSContext,
        index: u32,
        param: u32,
        mut retval: MutableHandleValue,
    ) {
        let mut get_attrib = |cx: &mut JSContext, data: Ref<'_, VertexAttribData>| {
            if param == constants::CURRENT_VERTEX_ATTRIB {
                let attrib = self.current_vertex_attribs.borrow()[index as usize];
                match attrib {
                    VertexAttrib::Float(x, y, z, w) => {
                        let value = [x, y, z, w];
                        rooted!(&in(cx) let mut result = ptr::null_mut::<JSObject>());
                        create_buffer_source::<Float32>(cx, &value, result.handle_mut()).unwrap();
                        return retval.set(ObjectValue(result.get()));
                    },
                    VertexAttrib::Int(x, y, z, w) => {
                        let value = [x, y, z, w];
                        rooted!(&in(cx) let mut result = ptr::null_mut::<JSObject>());
                        create_buffer_source::<Int32>(cx, &value, result.handle_mut()).unwrap();
                        return retval.set(ObjectValue(result.get()));
                    },
                    VertexAttrib::Uint(x, y, z, w) => {
                        let value = [x, y, z, w];
                        rooted!(&in(cx) let mut result = ptr::null_mut::<JSObject>());
                        create_buffer_source::<Uint32>(cx, &value, result.handle_mut()).unwrap();
                        return retval.set(ObjectValue(result.get()));
                    },
                };
            }
            if !self
                .extension_manager
                .is_get_vertex_attrib_name_enabled(param)
            {
                self.webgl_error(WebGLError::InvalidEnum);
                return retval.set(NullValue());
            }

            match param {
                constants::VERTEX_ATTRIB_ARRAY_ENABLED => {
                    retval.set(BooleanValue(data.enabled_as_array))
                },
                constants::VERTEX_ATTRIB_ARRAY_SIZE => retval.set(Int32Value(data.size as i32)),
                constants::VERTEX_ATTRIB_ARRAY_TYPE => retval.set(Int32Value(data.type_ as i32)),
                constants::VERTEX_ATTRIB_ARRAY_NORMALIZED => {
                    retval.set(BooleanValue(data.normalized))
                },
                constants::VERTEX_ATTRIB_ARRAY_STRIDE => retval.set(Int32Value(data.stride as i32)),
                constants::VERTEX_ATTRIB_ARRAY_BUFFER_BINDING => {
                    if let Some(buffer) = data.buffer() {
                        buffer.to_jsval(cx, retval.reborrow());
                    } else {
                        retval.set(NullValue());
                    }
                },
                ANGLEInstancedArraysConstants::VERTEX_ATTRIB_ARRAY_DIVISOR_ANGLE => {
                    retval.set(UInt32Value(data.divisor))
                },
                WebGL2RenderingContextConstants::VERTEX_ATTRIB_ARRAY_INTEGER
                    if self.webgl_version() == WebGLVersion::WebGL2 =>
                {
                    retval.set(BooleanValue(data.integer))
                },
                _ => {
                    self.webgl_error(InvalidEnum);
                    retval.set(NullValue())
                },
            }
        };

        match self.webgl_version() {
            WebGLVersion::WebGL1 => {
                let current_vao = self.current_vao(cx);
                let data = handle_potential_webgl_error!(
                    self,
                    current_vao.get_vertex_attrib(index).ok_or(InvalidValue),
                    return retval.set(NullValue())
                );
                get_attrib(cx, data)
            },
            WebGLVersion::WebGL2 => {
                let current_vao = self.current_vao_webgl2(cx);
                let data = handle_potential_webgl_error!(
                    self,
                    current_vao.get_vertex_attrib(index).ok_or(InvalidValue),
                    return retval.set(NullValue())
                );
                get_attrib(cx, data)
            },
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn GetVertexAttribOffset(&self, cx: &mut JSContext, index: u32, pname: u32) -> i64 {
        if pname != constants::VERTEX_ATTRIB_ARRAY_POINTER {
            self.webgl_error(InvalidEnum);
            return 0;
        }
        match self.webgl_version() {
            WebGLVersion::WebGL1 => {
                let current_vao = self.current_vao(cx);
                let data = handle_potential_webgl_error!(
                    self,
                    current_vao.get_vertex_attrib(index).ok_or(InvalidValue),
                    return 0
                );
                data.offset as i64
            },
            WebGLVersion::WebGL2 => {
                let current_vao = self.current_vao_webgl2(cx);
                let data = handle_potential_webgl_error!(
                    self,
                    current_vao.get_vertex_attrib(index).ok_or(InvalidValue),
                    return 0
                );
                data.offset as i64
            },
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn Hint(&self, target: u32, mode: u32) {
        if target != constants::GENERATE_MIPMAP_HINT &&
            !self.extension_manager.is_hint_target_enabled(target)
        {
            return self.webgl_error(InvalidEnum);
        }

        match mode {
            constants::FASTEST | constants::NICEST | constants::DONT_CARE => (),

            _ => return self.webgl_error(InvalidEnum),
        }

        self.send_command(WebGLCommand::Hint(target, mode));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.5>
    fn IsBuffer(&self, buffer: Option<&WebGLBuffer>) -> bool {
        // deleteBuffer frees the name at once, even while the buffer stays attached (to a
        // vertex array or transform feedback object): isBuffer is false from then on.
        buffer.is_some_and(|buf| {
            self.validate_ownership(buf).is_ok() &&
                buf.target().is_some() &&
                !buf.is_marked_for_deletion()
        })
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn IsEnabled(&self, cap: u32) -> bool {
        handle_potential_webgl_error!(self, self.capabilities.is_enabled(cap), false)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.6>
    fn IsFramebuffer(&self, frame_buffer: Option<&WebGLFramebuffer>) -> bool {
        frame_buffer.is_some_and(|buf| {
            self.validate_ownership(buf).is_ok() && buf.target().is_some() && !buf.is_deleted()
        })
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn IsProgram(&self, program: Option<&WebGLProgram>) -> bool {
        program.is_some_and(|p| self.validate_ownership(p).is_ok() && !p.is_deleted())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.7>
    fn IsRenderbuffer(&self, render_buffer: Option<&WebGLRenderbuffer>) -> bool {
        render_buffer.is_some_and(|buf| {
            self.validate_ownership(buf).is_ok() && buf.ever_bound() && !buf.is_deleted()
        })
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn IsShader(&self, shader: Option<&WebGLShader>) -> bool {
        shader.is_some_and(|s| self.validate_ownership(s).is_ok() && !s.is_deleted())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn IsTexture(&self, texture: Option<&WebGLTexture>) -> bool {
        texture.is_some_and(|tex| {
            self.validate_ownership(tex).is_ok() && tex.target().is_some() && !tex.is_invalid()
        })
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn LineWidth(&self, width: f32) {
        if width.is_nan() || width <= 0f32 {
            return self.webgl_error(InvalidValue);
        }

        self.send_command(WebGLCommand::LineWidth(width))
    }

    /// NOTE: Usage of this function could affect rendering while we keep using
    ///   readback to render to the page.
    ///
    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn PixelStorei(&self, param_name: u32, param_value: i32) {
        let mut texture_settings = self.texture_unpacking_settings.get();
        match param_name {
            constants::UNPACK_FLIP_Y_WEBGL => {
                texture_settings.set(TextureUnpacking::FLIP_Y_AXIS, param_value != 0);
            },
            constants::UNPACK_PREMULTIPLY_ALPHA_WEBGL => {
                texture_settings.set(TextureUnpacking::PREMULTIPLY_ALPHA, param_value != 0);
            },
            constants::UNPACK_COLORSPACE_CONVERSION_WEBGL => {
                let convert = match param_value as u32 {
                    constants::BROWSER_DEFAULT_WEBGL => true,
                    constants::NONE => false,
                    _ => return self.webgl_error(InvalidEnum),
                };
                texture_settings.set(TextureUnpacking::CONVERT_COLORSPACE, convert);
            },
            constants::UNPACK_ALIGNMENT => {
                match param_value {
                    1 | 2 | 4 | 8 => (),
                    _ => return self.webgl_error(InvalidValue),
                }
                self.texture_unpacking_alignment.set(param_value as u32);
                return;
            },
            constants::PACK_ALIGNMENT => {
                match param_value {
                    1 | 2 | 4 | 8 => (),
                    _ => return self.webgl_error(InvalidValue),
                }
                // We never actually change the actual value on the GL side
                // because it's better to receive the pixels without the padding
                // and then write the result at the right place in ReadPixels.
                self.texture_packing_alignment.set(param_value as u8);
                return;
            },
            _ => return self.webgl_error(InvalidEnum),
        }
        self.texture_unpacking_settings.set(texture_settings);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn PolygonOffset(&self, factor: f32, units: f32) {
        self.send_command(WebGLCommand::PolygonOffset(factor, units))
    }

    // https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.12
    fn ReadPixels(
        &self,
        no_gc: &NoGC,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        format: u32,
        pixel_type: u32,
        mut pixels: CustomAutoRooterGuard<Option<ArrayBufferView>>,
    ) {
        handle_potential_webgl_error!(self, self.validate_framebuffer(), return);

        let pixels =
            handle_potential_webgl_error!(self, pixels.as_mut().ok_or(InvalidValue), return);

        if width < 0 || height < 0 {
            return self.webgl_error(InvalidValue);
        }

        // A format or type GL ES 2.0 doesn't define for readPixels is INVALID_ENUM; a defined
        // pair other than the ones below is INVALID_OPERATION.
        if !matches!(
            format,
            constants::ALPHA |
                constants::RGB |
                constants::RGBA |
                constants::LUMINANCE |
                constants::LUMINANCE_ALPHA
        ) || !matches!(
            pixel_type,
            constants::UNSIGNED_BYTE |
                constants::UNSIGNED_SHORT_5_6_5 |
                constants::UNSIGNED_SHORT_4_4_4_4 |
                constants::UNSIGNED_SHORT_5_5_5_1 |
                constants::FLOAT |
                HALF_FLOAT_OES
        ) {
            return self.webgl_error(InvalidEnum);
        }

        // RGBA/UNSIGNED_BYTE always; RGBA/FLOAT into a Float32Array from a float colour buffer
        // (WEBGL_color_buffer_float / EXT_color_buffer_half_float).
        let float_read = format == constants::RGBA &&
            pixel_type == constants::FLOAT &&
            (self.extension_manager.is_float_buffer_renderable() ||
                self.extension_manager.is_half_float_buffer_renderable()) &&
            self.bound_draw_framebuffer
                .get()
                .is_some_and(|fb| fb.color_attachment_is_float());
        if !float_read && (format != constants::RGBA || pixel_type != constants::UNSIGNED_BYTE) {
            return self.webgl_error(InvalidOperation);
        }

        let array_type_matches = match pixels.get_array_type() {
            Type::Float32 => float_read,
            Type::Uint8 | Type::Uint8Clamped => !float_read,
            _ => false,
        };
        if !array_type_matches {
            return self.webgl_error(InvalidOperation);
        }

        let (fb_width, fb_height) = handle_potential_webgl_error!(
            self,
            self.get_current_framebuffer_size().ok_or(InvalidOperation),
            return
        );

        if width == 0 || height == 0 {
            return;
        }

        let bytes_per_pixel = if float_read { 16 } else { 4 };

        let row_len = handle_potential_webgl_error!(
            self,
            width.checked_mul(bytes_per_pixel).ok_or(InvalidOperation),
            return
        );

        let pack_alignment = self.texture_packing_alignment.get() as i32;
        let dest_padding = match row_len % pack_alignment {
            0 => 0,
            remainder => pack_alignment - remainder,
        };
        let dest_stride = row_len + dest_padding;

        let full_rows_len = handle_potential_webgl_error!(
            self,
            dest_stride.checked_mul(height - 1).ok_or(InvalidOperation),
            return
        );
        let required_dest_len = handle_potential_webgl_error!(
            self,
            full_rows_len.checked_add(row_len).ok_or(InvalidOperation),
            return
        );

        let dest = array_buffer_view_bytes_mut(pixels, no_gc);
        if dest.len() < required_dest_len as usize {
            return self.webgl_error(InvalidOperation);
        }

        let src_origin = Point2D::new(x, y);
        let src_size = Size2D::new(width as u32, height as u32);
        let fb_size = Size2D::new(fb_width as u32, fb_height as u32);
        let src_rect = match pixels::clip(src_origin, src_size.to_u32(), fb_size.to_u32()) {
            Some(rect) => rect,
            None => return,
        };

        // Note: we're casting a Rect<u64> back into a Rect<u32> here, but it's okay because
        //  it used u32 data types to begin with. It just got converted to Rect<u64> in
        //  pixels::clip
        let src_rect = src_rect.to_u32();

        let mut dest_offset = 0;
        if x < 0 {
            dest_offset += -x * bytes_per_pixel;
        }
        if y < 0 {
            dest_offset += -y * dest_stride;
        }

        let (sender, receiver) = generic_channel::channel().unwrap();
        self.send_command(WebGLCommand::ReadPixels(
            src_rect, format, pixel_type, sender,
        ));
        let (src, _) = receiver.recv().unwrap();

        let src_row_len = src_rect.size.width as usize * bytes_per_pixel as usize;
        for i in 0..src_rect.size.height {
            let dest_start = dest_offset as usize + i as usize * dest_stride as usize;
            let dest_end = dest_start + src_row_len;
            let src_start = i as usize * src_row_len;
            let src_end = src_start + src_row_len;
            dest[dest_start..dest_end].copy_from_slice(&src[src_start..src_end]);
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn SampleCoverage(&self, value: f32, invert: bool) {
        self.send_command(WebGLCommand::SampleCoverage(value, invert));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.4>
    fn Scissor(&self, x: i32, y: i32, width: i32, height: i32) {
        if width < 0 || height < 0 {
            return self.webgl_error(InvalidValue);
        }

        let width = width as u32;
        let height = height as u32;

        self.current_scissor.set((x, y, width, height));
        self.send_command(WebGLCommand::Scissor(x, y, width, height));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn StencilFunc(&self, func: u32, ref_: i32, mask: u32) {
        match func {
            constants::NEVER |
            constants::LESS |
            constants::EQUAL |
            constants::LEQUAL |
            constants::GREATER |
            constants::NOTEQUAL |
            constants::GEQUAL |
            constants::ALWAYS => {
                self.set_stencil_state(constants::FRONT_AND_BACK, |face| {
                    face.reference = ref_;
                    face.value_mask = mask;
                });
                self.send_command(WebGLCommand::StencilFunc(func, ref_, mask))
            },
            _ => self.webgl_error(InvalidEnum),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn StencilFuncSeparate(&self, face: u32, func: u32, ref_: i32, mask: u32) {
        match face {
            constants::FRONT | constants::BACK | constants::FRONT_AND_BACK => (),
            _ => return self.webgl_error(InvalidEnum),
        }

        match func {
            constants::NEVER |
            constants::LESS |
            constants::EQUAL |
            constants::LEQUAL |
            constants::GREATER |
            constants::NOTEQUAL |
            constants::GEQUAL |
            constants::ALWAYS => {
                self.set_stencil_state(face, |state| {
                    state.reference = ref_;
                    state.value_mask = mask;
                });
                self.send_command(WebGLCommand::StencilFuncSeparate(face, func, ref_, mask))
            },
            _ => self.webgl_error(InvalidEnum),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn StencilMask(&self, mask: u32) {
        self.set_stencil_state(constants::FRONT_AND_BACK, |face| face.write_mask = mask);
        self.send_command(WebGLCommand::StencilMask(mask))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn StencilMaskSeparate(&self, face: u32, mask: u32) {
        match face {
            constants::FRONT | constants::BACK | constants::FRONT_AND_BACK => {
                self.set_stencil_state(face, |state| state.write_mask = mask);
                self.send_command(WebGLCommand::StencilMaskSeparate(face, mask))
            },
            _ => self.webgl_error(InvalidEnum),
        };
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn StencilOp(&self, fail: u32, zfail: u32, zpass: u32) {
        if self.validate_stencil_actions(fail) &&
            self.validate_stencil_actions(zfail) &&
            self.validate_stencil_actions(zpass)
        {
            self.send_command(WebGLCommand::StencilOp(fail, zfail, zpass));
        } else {
            self.webgl_error(InvalidEnum)
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn StencilOpSeparate(&self, face: u32, fail: u32, zfail: u32, zpass: u32) {
        match face {
            constants::FRONT | constants::BACK | constants::FRONT_AND_BACK => (),
            _ => return self.webgl_error(InvalidEnum),
        }

        if self.validate_stencil_actions(fail) &&
            self.validate_stencil_actions(zfail) &&
            self.validate_stencil_actions(zpass)
        {
            self.send_command(WebGLCommand::StencilOpSeparate(face, fail, zfail, zpass))
        } else {
            self.webgl_error(InvalidEnum)
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn LinkProgram(&self, program: &WebGLProgram) {
        handle_potential_webgl_error!(self, self.validate_ownership(program), return);
        if program.is_deleted() {
            return self.webgl_error(InvalidValue);
        }
        handle_potential_webgl_error!(self, program.link());
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn ShaderSource(&self, shader: &WebGLShader, source: DOMString) {
        handle_potential_webgl_error!(self, self.validate_ownership(shader), return);
        shader.set_source(source)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetShaderSource(&self, shader: &WebGLShader) -> Option<DOMString> {
        handle_potential_webgl_error!(self, self.validate_ownership(shader), return None);
        Some(shader.source())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform1f(&self, location: Option<&WebGLUniformLocation>, val: f32) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL | constants::FLOAT => {},
                _ => return Err(InvalidOperation),
            }
            self.send_command(WebGLCommand::Uniform1f(location.id(), val));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform1i(&self, location: Option<&WebGLUniformLocation>, val: i32) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL | constants::INT => {},
                sampler if is_sampler_type(sampler) => {
                    if val < 0 || val as u32 >= self.limits.max_combined_texture_image_units {
                        return Err(InvalidValue);
                    }
                },
                _ => return Err(InvalidOperation),
            }
            if is_sampler_type(location.type_()) &&
                let Some(program) = self.current_program.get()
            {
                program.set_sampler_units(location.id(), &[val]);
            }
            self.send_command(WebGLCommand::Uniform1i(location.id(), val));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform1iv(&self, location: Option<&WebGLUniformLocation>, val: Int32ArrayOrLongSequence) {
        self.uniform1iv(location, val, 0, 0)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform1fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Float32ArrayOrUnrestrictedFloatSequence,
    ) {
        self.uniform1fv(location, val, 0, 0)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform2f(&self, location: Option<&WebGLUniformLocation>, x: f32, y: f32) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC2 | constants::FLOAT_VEC2 => {},
                _ => return Err(InvalidOperation),
            }
            self.send_command(WebGLCommand::Uniform2f(location.id(), x, y));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform2fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Float32ArrayOrUnrestrictedFloatSequence,
    ) {
        self.uniform2fv(location, val, 0, 0)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform2i(&self, location: Option<&WebGLUniformLocation>, x: i32, y: i32) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC2 | constants::INT_VEC2 => {},
                _ => return Err(InvalidOperation),
            }
            self.send_command(WebGLCommand::Uniform2i(location.id(), x, y));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform2iv(&self, location: Option<&WebGLUniformLocation>, val: Int32ArrayOrLongSequence) {
        self.uniform2iv(location, val, 0, 0)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform3f(&self, location: Option<&WebGLUniformLocation>, x: f32, y: f32, z: f32) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC3 | constants::FLOAT_VEC3 => {},
                _ => return Err(InvalidOperation),
            }
            self.send_command(WebGLCommand::Uniform3f(location.id(), x, y, z));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform3fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Float32ArrayOrUnrestrictedFloatSequence,
    ) {
        self.uniform3fv(location, val, 0, 0)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform3i(&self, location: Option<&WebGLUniformLocation>, x: i32, y: i32, z: i32) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC3 | constants::INT_VEC3 => {},
                _ => return Err(InvalidOperation),
            }
            self.send_command(WebGLCommand::Uniform3i(location.id(), x, y, z));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform3iv(&self, location: Option<&WebGLUniformLocation>, val: Int32ArrayOrLongSequence) {
        self.uniform3iv(location, val, 0, 0)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform4i(&self, location: Option<&WebGLUniformLocation>, x: i32, y: i32, z: i32, w: i32) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC4 | constants::INT_VEC4 => {},
                _ => return Err(InvalidOperation),
            }
            self.send_command(WebGLCommand::Uniform4i(location.id(), x, y, z, w));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform4iv(&self, location: Option<&WebGLUniformLocation>, val: Int32ArrayOrLongSequence) {
        self.uniform4iv(location, val, 0, 0)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform4f(&self, location: Option<&WebGLUniformLocation>, x: f32, y: f32, z: f32, w: f32) {
        self.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC4 | constants::FLOAT_VEC4 => {},
                _ => return Err(InvalidOperation),
            }
            self.send_command(WebGLCommand::Uniform4f(location.id(), x, y, z, w));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform4fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Float32ArrayOrUnrestrictedFloatSequence,
    ) {
        self.uniform4fv(location, val, 0, 0)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn UniformMatrix2fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        transpose: bool,
        val: Float32ArrayOrUnrestrictedFloatSequence,
    ) {
        self.uniform_matrix_2fv(location, transpose, val, 0, 0)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn UniformMatrix3fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        transpose: bool,
        val: Float32ArrayOrUnrestrictedFloatSequence,
    ) {
        self.uniform_matrix_3fv(location, transpose, val, 0, 0)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn UniformMatrix4fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        transpose: bool,
        val: Float32ArrayOrUnrestrictedFloatSequence,
    ) {
        self.uniform_matrix_4fv(location, transpose, val, 0, 0)
    }

    // https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10
    fn GetUniform(
        &self,
        cx: &mut JSContext,
        program: &WebGLProgram,
        location: &WebGLUniformLocation,
        mut rval: MutableHandleValue,
    ) {
        handle_potential_webgl_error!(
            self,
            self.uniform_check_program(program, location),
            return rval.set(NullValue())
        );

        let triple = (self, program.id(), location.id());

        match location.type_() {
            constants::BOOL => rval.set(BooleanValue(uniform_get(
                triple,
                WebGLCommand::GetUniformBool,
            ))),
            constants::BOOL_VEC2 => {
                uniform_get(triple, WebGLCommand::GetUniformBool2).to_jsval(cx, rval)
            },
            constants::BOOL_VEC3 => {
                uniform_get(triple, WebGLCommand::GetUniformBool3).to_jsval(cx, rval)
            },
            constants::BOOL_VEC4 => {
                uniform_get(triple, WebGLCommand::GetUniformBool4).to_jsval(cx, rval)
            },
            constants::INT => {
                rval.set(Int32Value(uniform_get(triple, WebGLCommand::GetUniformInt)))
            },
            sampler if is_sampler_type(sampler) => {
                rval.set(Int32Value(uniform_get(triple, WebGLCommand::GetUniformInt)))
            },
            constants::INT_VEC2 => {
                uniform_typed::<Int32>(cx, &uniform_get(triple, WebGLCommand::GetUniformInt2), rval)
            },
            constants::INT_VEC3 => {
                uniform_typed::<Int32>(cx, &uniform_get(triple, WebGLCommand::GetUniformInt3), rval)
            },
            constants::INT_VEC4 => {
                uniform_typed::<Int32>(cx, &uniform_get(triple, WebGLCommand::GetUniformInt4), rval)
            },
            constants::FLOAT => rval
                .set(DoubleValue(
                    uniform_get(triple, WebGLCommand::GetUniformFloat) as f64,
                )),
            constants::FLOAT_VEC2 => uniform_typed::<Float32>(
                cx,
                &uniform_get(triple, WebGLCommand::GetUniformFloat2),
                rval,
            ),
            constants::FLOAT_VEC3 => uniform_typed::<Float32>(
                cx,
                &uniform_get(triple, WebGLCommand::GetUniformFloat3),
                rval,
            ),
            constants::FLOAT_VEC4 | constants::FLOAT_MAT2 => uniform_typed::<Float32>(
                cx,
                &uniform_get(triple, WebGLCommand::GetUniformFloat4),
                rval,
            ),
            constants::FLOAT_MAT3 => uniform_typed::<Float32>(
                cx,
                &uniform_get(triple, WebGLCommand::GetUniformFloat9),
                rval,
            ),
            constants::FLOAT_MAT4 => uniform_typed::<Float32>(
                cx,
                &uniform_get(triple, WebGLCommand::GetUniformFloat16),
                rval,
            ),
            _ => panic!("wrong uniform type"),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn UseProgram(&self, program: Option<&WebGLProgram>) {
        if let Some(program) = program {
            handle_potential_webgl_error!(self, self.validate_ownership(program), return);
            if program.is_deleted() || !program.is_linked() {
                return self.webgl_error(InvalidOperation);
            }
            if program.is_in_use() {
                return;
            }
            program.in_use(true);
        }
        match self.current_program.get() {
            Some(ref current) if program != Some(&**current) => current.in_use(false),
            _ => {},
        }
        self.send_command(WebGLCommand::UseProgram(program.map(|p| p.id())));
        self.current_program.set(program);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn ValidateProgram(&self, program: &WebGLProgram) {
        handle_potential_webgl_error!(self, self.validate_ownership(program), return);
        if let Err(e) = program.validate() {
            self.webgl_error(e);
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib1f(&self, cx: &mut JSContext, indx: u32, x: f32) {
        self.vertex_attrib(cx, indx, x, 0f32, 0f32, 1f32)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib1fv(
        &self,
        cx: &mut JSContext,
        indx: u32,
        v: Float32ArrayOrUnrestrictedFloatSequence,
    ) {
        let values = match v {
            Float32ArrayOrUnrestrictedFloatSequence::Float32Array(v) => typed_array_to_vec(&*v),
            Float32ArrayOrUnrestrictedFloatSequence::UnrestrictedFloatSequence(v) => v,
        };
        if values.is_empty() {
            // https://github.com/KhronosGroup/WebGL/issues/2700
            return self.webgl_error(InvalidValue);
        }
        self.vertex_attrib(cx, indx, values[0], 0f32, 0f32, 1f32);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib2f(&self, cx: &mut JSContext, indx: u32, x: f32, y: f32) {
        self.vertex_attrib(cx, indx, x, y, 0f32, 1f32)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib2fv(
        &self,
        cx: &mut JSContext,
        indx: u32,
        v: Float32ArrayOrUnrestrictedFloatSequence,
    ) {
        let values = match v {
            Float32ArrayOrUnrestrictedFloatSequence::Float32Array(v) => typed_array_to_vec(&*v),
            Float32ArrayOrUnrestrictedFloatSequence::UnrestrictedFloatSequence(v) => v,
        };
        if values.len() < 2 {
            // https://github.com/KhronosGroup/WebGL/issues/2700
            return self.webgl_error(InvalidValue);
        }
        self.vertex_attrib(cx, indx, values[0], values[1], 0f32, 1f32);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib3f(&self, cx: &mut JSContext, indx: u32, x: f32, y: f32, z: f32) {
        self.vertex_attrib(cx, indx, x, y, z, 1f32)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib3fv(
        &self,
        cx: &mut JSContext,
        indx: u32,
        v: Float32ArrayOrUnrestrictedFloatSequence,
    ) {
        let values = match v {
            Float32ArrayOrUnrestrictedFloatSequence::Float32Array(v) => typed_array_to_vec(&*v),
            Float32ArrayOrUnrestrictedFloatSequence::UnrestrictedFloatSequence(v) => v,
        };
        if values.len() < 3 {
            // https://github.com/KhronosGroup/WebGL/issues/2700
            return self.webgl_error(InvalidValue);
        }
        self.vertex_attrib(cx, indx, values[0], values[1], values[2], 1f32);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib4f(&self, cx: &mut JSContext, indx: u32, x: f32, y: f32, z: f32, w: f32) {
        self.vertex_attrib(cx, indx, x, y, z, w)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib4fv(
        &self,
        cx: &mut JSContext,
        indx: u32,
        v: Float32ArrayOrUnrestrictedFloatSequence,
    ) {
        let values = match v {
            Float32ArrayOrUnrestrictedFloatSequence::Float32Array(v) => typed_array_to_vec(&*v),
            Float32ArrayOrUnrestrictedFloatSequence::UnrestrictedFloatSequence(v) => v,
        };
        if values.len() < 4 {
            // https://github.com/KhronosGroup/WebGL/issues/2700
            return self.webgl_error(InvalidValue);
        }
        self.vertex_attrib(cx, indx, values[0], values[1], values[2], values[3]);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttribPointer(
        &self,
        cx: &mut JSContext,
        index: u32,
        size: i32,
        type_: u32,
        normalized: bool,
        stride: i32,
        offset: i64,
    ) {
        let res = match self.webgl_version() {
            WebGLVersion::WebGL1 => self
                .current_vao(cx)
                .vertex_attrib_pointer(index, size, type_, normalized, stride, offset),
            WebGLVersion::WebGL2 => self
                .current_vao_webgl2(cx)
                .vertex_attrib_pointer(index, size, type_, normalized, stride, offset),
        };
        handle_potential_webgl_error!(self, res);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.4>
    fn Viewport(&self, x: i32, y: i32, width: i32, height: i32) {
        if width < 0 || height < 0 {
            return self.webgl_error(InvalidValue);
        }

        self.send_command(WebGLCommand::SetViewport(x, y, width, height))
    }

    // https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8
    fn TexImage2D(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        internal_format: i32,
        width: i32,
        height: i32,
        border: i32,
        format: u32,
        data_type: u32,
        pixels: CustomAutoRooterGuard<Option<ArrayBufferView>>,
    ) -> ErrorResult {
        if !self.extension_manager.is_tex_type_enabled(data_type) {
            self.webgl_error(InvalidEnum);
            return Ok(());
        }

        let validator = TexImage2DValidator::new(
            self,
            target,
            level,
            internal_format as u32,
            width,
            height,
            border,
            format,
            data_type,
        );

        let TexImage2DValidatorResult {
            texture,
            target,
            width,
            height,
            level,
            border,
            internal_format,
            format,
            data_type,
        } = match validator.validate() {
            Ok(result) => result,
            Err(_) => return Ok(()), // NB: The validator sets the correct error for us.
        };

        if !internal_format.compatible_data_types().contains(&data_type) {
            return {
                self.webgl_error(InvalidOperation);
                Ok(())
            };
        }
        if texture.is_immutable() {
            return {
                self.webgl_error(InvalidOperation);
                Ok(())
            };
        }

        let unpacking_alignment = self.texture_unpacking_alignment.get();

        let expected_byte_length = match self.validate_tex_image_2d_data(
            width,
            height,
            format,
            data_type,
            unpacking_alignment,
            pixels.as_ref(),
        ) {
            Ok(byte_length) => byte_length,
            Err(()) => return Ok(()),
        };

        // If data is null, the texture is initialized to 0. Partii (perf): no zero-filled buffer is
        // sent; an empty one tells the WebGL thread to upload zeros itself (webgl_thread.rs TexImage2D).
        let buff = match *pixels {
            None => GenericSharedMemory::from_bytes(&[]),
            Some(ref data) => {
                GenericSharedMemory::from_bytes(array_buffer_view_bytes(&data, no_gc))
            },
        };

        // From the WebGL spec:
        //
        //     "If pixels is non-null but its size is less than what
        //      is required by the specified width, height, format,
        //      type, and pixel storage parameters, generates an
        //      INVALID_OPERATION error."
        if pixels.is_some() && buff.len() < expected_byte_length as usize {
            return {
                self.webgl_error(InvalidOperation);
                Ok(())
            };
        }

        let size = Size2D::new(width, height);

        let (alpha_treatment, y_axis_treatment) =
            self.get_current_unpack_state(Alpha::NotPremultiplied);

        self.tex_image_2d(
            &texture,
            target,
            data_type,
            internal_format,
            format,
            level,
            border,
            unpacking_alignment,
            size,
            TexSource::Pixels(TexPixels::from_array(
                buff,
                size,
                alpha_treatment,
                y_axis_treatment,
            )),
        );

        Ok(())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn TexImage2D_(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        internal_format: i32,
        format: u32,
        data_type: u32,
        source: TexImageSource,
    ) -> ErrorResult {
        if !self.extension_manager.is_tex_type_enabled(data_type) {
            self.webgl_error(InvalidEnum);
            return Ok(());
        }

        let pixels = match self.get_image_pixels(no_gc, source)? {
            Some(pixels) => pixels,
            None => return Ok(()),
        };

        let validator = TexImage2DValidator::new(
            self,
            target,
            level,
            internal_format as u32,
            pixels.size().width as i32,
            pixels.size().height as i32,
            0,
            format,
            data_type,
        );

        let TexImage2DValidatorResult {
            texture,
            target,
            level,
            border,
            internal_format,
            format,
            data_type,
            ..
        } = match validator.validate() {
            Ok(result) => result,
            Err(_) => return Ok(()), // NB: The validator sets the correct error for us.
        };

        if !internal_format.compatible_data_types().contains(&data_type) {
            return {
                self.webgl_error(InvalidOperation);
                Ok(())
            };
        }
        if texture.is_immutable() {
            return {
                self.webgl_error(InvalidOperation);
                Ok(())
            };
        }

        self.tex_image_2d(
            &texture,
            target,
            data_type,
            internal_format,
            format,
            level,
            border,
            1,
            pixels.size(),
            TexSource::Pixels(pixels),
        );
        Ok(())
    }

    // https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8
    fn TexSubImage2D(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        data_type: u32,
        pixels: CustomAutoRooterGuard<Option<ArrayBufferView>>,
    ) -> ErrorResult {
        let validator = TexImage2DValidator::new(
            self, target, level, format, width, height, 0, format, data_type,
        )
        .sub_image();
        let TexImage2DValidatorResult {
            texture,
            target,
            width,
            height,
            level,
            format,
            data_type,
            ..
        } = match validator.validate() {
            Ok(result) => result,
            Err(_) => return Ok(()), // NB: The validator sets the correct error for us.
        };

        let unpacking_alignment = self.texture_unpacking_alignment.get();

        let expected_byte_length = match self.validate_tex_image_2d_data(
            width,
            height,
            format,
            data_type,
            unpacking_alignment,
            pixels.as_ref(),
        ) {
            Ok(byte_length) => byte_length,
            Err(()) => return Ok(()),
        };

        let buff = handle_potential_webgl_error!(
            self,
            pixels
                .as_ref()
                .map(|p| GenericSharedMemory::from_bytes(array_buffer_view_bytes(&p, no_gc)))
                .ok_or(InvalidValue),
            return Ok(())
        );

        // From the WebGL spec:
        //
        //     "If pixels is non-null but its size is less than what
        //      is required by the specified width, height, format,
        //      type, and pixel storage parameters, generates an
        //      INVALID_OPERATION error."
        if buff.len() < expected_byte_length as usize {
            return {
                self.webgl_error(InvalidOperation);
                Ok(())
            };
        }

        let (alpha_treatment, y_axis_treatment) =
            self.get_current_unpack_state(Alpha::NotPremultiplied);

        self.tex_sub_image_2d(
            texture,
            target,
            level,
            xoffset,
            yoffset,
            format,
            data_type,
            unpacking_alignment,
            TexPixels::from_array(
                buff,
                Size2D::new(width, height),
                alpha_treatment,
                y_axis_treatment,
            ),
        );
        Ok(())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn TexSubImage2D_(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        format: u32,
        data_type: u32,
        source: TexImageSource,
    ) -> ErrorResult {
        let pixels = match self.get_image_pixels(no_gc, source)? {
            Some(pixels) => pixels,
            None => return Ok(()),
        };

        let validator = TexImage2DValidator::new(
            self,
            target,
            level,
            format,
            pixels.size().width as i32,
            pixels.size().height as i32,
            0,
            format,
            data_type,
        )
        .sub_image();
        let TexImage2DValidatorResult {
            texture,
            target,
            level,
            format,
            data_type,
            ..
        } = match validator.validate() {
            Ok(result) => result,
            Err(_) => return Ok(()), // NB: The validator sets the correct error for us.
        };

        self.tex_sub_image_2d(
            texture, target, level, xoffset, yoffset, format, data_type, 1, pixels,
        );
        Ok(())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn TexParameterf(&self, target: u32, name: u32, value: f32) {
        self.tex_parameter(target, name, TexParameterValue::Float(value))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn TexParameteri(&self, target: u32, name: u32, value: i32) {
        self.tex_parameter(target, name, TexParameterValue::Int(value))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.6>
    fn CheckFramebufferStatus(&self, target: u32) -> u32 {
        // From the GLES 2.0.25 spec, 4.4 ("Framebuffer Objects"):
        //
        //    "If target is not FRAMEBUFFER, INVALID_ENUM is
        //     generated. If CheckFramebufferStatus generates an
        //     error, 0 is returned."
        if target != constants::FRAMEBUFFER {
            self.webgl_error(InvalidEnum);
            return 0;
        }

        match self.bound_draw_framebuffer.get() {
            Some(fb) => fb.check_status(),
            None => constants::FRAMEBUFFER_COMPLETE,
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.7>
    fn RenderbufferStorage(&self, target: u32, internal_format: u32, width: i32, height: i32) {
        self.renderbuffer_storage(target, 0, internal_format, width, height)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.6>
    fn FramebufferRenderbuffer(
        &self,
        target: u32,
        attachment: u32,
        renderbuffertarget: u32,
        rb: Option<&WebGLRenderbuffer>,
    ) {
        if let Some(rb) = rb {
            handle_potential_webgl_error!(self, self.validate_ownership(rb), return);
        }

        if target != constants::FRAMEBUFFER || renderbuffertarget != constants::RENDERBUFFER {
            return self.webgl_error(InvalidEnum);
        }

        match self.bound_draw_framebuffer.get() {
            Some(fb) => handle_potential_webgl_error!(self, fb.renderbuffer(attachment, rb)),
            None => self.webgl_error(InvalidOperation),
        };
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.6>
    fn FramebufferTexture2D(
        &self,
        target: u32,
        attachment: u32,
        textarget: u32,
        texture: Option<&WebGLTexture>,
        level: i32,
    ) {
        if let Some(texture) = texture {
            handle_potential_webgl_error!(self, self.validate_ownership(texture), return);
        }

        if target != constants::FRAMEBUFFER {
            return self.webgl_error(InvalidEnum);
        }

        // From the GLES 2.0.25 spec, page 113:
        //
        //     "level specifies the mipmap level of the texture image
        //      to be attached to the framebuffer and must be
        //      0. Otherwise, INVALID_VALUE is generated."
        if level != 0 {
            return self.webgl_error(InvalidValue);
        }

        match self.bound_draw_framebuffer.get() {
            Some(fb) => handle_potential_webgl_error!(
                self,
                fb.texture2d(attachment, textarget, texture, level)
            ),
            None => self.webgl_error(InvalidOperation),
        };
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetAttachedShaders(&self, program: &WebGLProgram) -> Option<Vec<DomRoot<WebGLShader>>> {
        handle_potential_webgl_error!(self, self.validate_ownership(program), return None);
        handle_potential_webgl_error!(self, program.attached_shaders().map(Some), None)
    }

    /// <https://immersive-web.github.io/webxr/#dom-webglrenderingcontextbase-makexrcompatible>
    #[cfg(feature = "webxr")]
    fn MakeXRCompatible(&self, cx: &mut js::context::JSContext) -> RootedPromise {
        // XXXManishearth Fill in with compatibility checks when rust-webxr supports this
        Promise::new_resolved(cx, &self.global(), ())
    }
}

#[derive(Default, JSTraceable, MallocSizeOf)]
struct Capabilities {
    value: Cell<CapFlags>,
}

impl Capabilities {
    fn set(&self, cap: u32, set: bool) -> WebGLResult<bool> {
        let cap = CapFlags::from_enum(cap)?;
        let mut value = self.value.get();
        if value.contains(cap) == set {
            return Ok(false);
        }
        value.set(cap, set);
        self.value.set(value);
        Ok(true)
    }

    fn is_enabled(&self, cap: u32) -> WebGLResult<bool> {
        Ok(self.value.get().contains(CapFlags::from_enum(cap)?))
    }
}

impl Default for CapFlags {
    fn default() -> Self {
        CapFlags::DITHER
    }
}

macro_rules! capabilities {
    ($name:ident, $next:ident, $($rest:ident,)*) => {
        capabilities!($name, $next, $($rest,)* [$name = 1;]);
    };
    ($prev:ident, $name:ident, $($rest:ident,)* [$($tt:tt)*]) => {
        capabilities!($name, $($rest,)* [$($tt)* $name = Self::$prev.bits() << 1;]);
    };
    ($prev:ident, [$($name:ident = $value:expr;)*]) => {
        #[derive(Clone, Copy, JSTraceable, MallocSizeOf)]
        pub(crate) struct CapFlags(u16);

        bitflags! {
            impl CapFlags: u16 {
                $(const $name = $value;)*
            }
        }

        impl CapFlags {
            fn from_enum(cap: u32) -> WebGLResult<Self> {
                match cap {
                    $(constants::$name => Ok(Self::$name),)*
                    _ => Err(InvalidEnum),
                }
            }
        }
    };
}

capabilities! {
    BLEND,
    CULL_FACE,
    DEPTH_TEST,
    DITHER,
    POLYGON_OFFSET_FILL,
    SAMPLE_ALPHA_TO_COVERAGE,
    SAMPLE_COVERAGE,
    SCISSOR_TEST,
    STENCIL_TEST,
}

#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
#[derive(JSTraceable, MallocSizeOf)]
pub(crate) struct Textures {
    active_unit: Cell<u32>,
    units: Box<[TextureUnit]>,
    /// Partii (perf): counts texture and sampler binding changes; each unit keeps the count at its
    /// last change (TextureUnit::changed_at), for the draw-state cache.
    binding_sequence: Cell<u64>,
}

impl Textures {
    fn new(max_combined_textures: u32) -> Self {
        Self {
            active_unit: Default::default(),
            units: (0..max_combined_textures)
                .map(|_| Default::default())
                .collect::<Vec<_>>()
                .into(),
            binding_sequence: Default::default(),
        }
    }

    pub(crate) fn binding_sequence(&self) -> u64 {
        self.binding_sequence.get()
    }

    /// A texture or sampler binding of `unit` changed.
    pub(crate) fn note_unit_changed(&self, unit: u32) {
        let Some(unit) = self.units.get(unit as usize) else {
            return;
        };
        let sequence = self.binding_sequence.get() + 1;
        self.binding_sequence.set(sequence);
        unit.changed_at.set(sequence);
    }

    fn note_active_unit_changed(&self) {
        self.note_unit_changed(self.active_unit.get());
    }

    /// Whether a binding of `unit` changed after binding sequence number `sequence`.
    pub(crate) fn unit_changed_since(&self, unit: u32, sequence: u64) -> bool {
        self.units
            .get(unit as usize)
            .is_some_and(|unit| unit.changed_at.get() > sequence)
    }

    pub(crate) fn active_unit_enum(&self) -> u32 {
        self.active_unit.get() + constants::TEXTURE0
    }

    fn set_active_unit_enum(&self, index: u32) -> WebGLResult<()> {
        if (constants::TEXTURE0..constants::TEXTURE0 + self.units.len() as u32).contains(&index) {
            self.active_unit.set(index - constants::TEXTURE0);
            Ok(())
        } else {
            Err(InvalidEnum)
        }
    }

    pub(crate) fn active_texture_slot(
        &self,
        target: u32,
        webgl_version: WebGLVersion,
    ) -> WebGLResult<&MutNullableDom<WebGLTexture>> {
        let active_unit = self.active_unit();
        let is_webgl2 = webgl_version == WebGLVersion::WebGL2;
        match target {
            constants::TEXTURE_2D => Ok(&active_unit.tex_2d),
            constants::TEXTURE_CUBE_MAP => Ok(&active_unit.tex_cube_map),
            WebGL2RenderingContextConstants::TEXTURE_2D_ARRAY if is_webgl2 => {
                Ok(&active_unit.tex_2d_array)
            },
            WebGL2RenderingContextConstants::TEXTURE_3D if is_webgl2 => Ok(&active_unit.tex_3d),
            _ => Err(InvalidEnum),
        }
    }

    pub(crate) fn active_texture_for_image_target(
        &self,
        target: TexImageTarget,
    ) -> Option<DomRoot<WebGLTexture>> {
        let active_unit = self.active_unit();
        match target {
            TexImageTarget::Texture2D => active_unit.tex_2d.get(),
            TexImageTarget::Texture2DArray => active_unit.tex_2d_array.get(),
            TexImageTarget::Texture3D => active_unit.tex_3d.get(),
            TexImageTarget::CubeMap |
            TexImageTarget::CubeMapPositiveX |
            TexImageTarget::CubeMapNegativeX |
            TexImageTarget::CubeMapPositiveY |
            TexImageTarget::CubeMapNegativeY |
            TexImageTarget::CubeMapPositiveZ |
            TexImageTarget::CubeMapNegativeZ => active_unit.tex_cube_map.get(),
        }
    }

    /// The texture bound to `target` (TEXTURE_2D, _CUBE_MAP, _3D, _2D_ARRAY) on unit `unit`.
    pub(crate) fn texture_at(&self, unit: u32, target: u32) -> Option<DomRoot<WebGLTexture>> {
        let unit = self.units.get(unit as usize)?;
        match target {
            constants::TEXTURE_2D => unit.tex_2d.get(),
            constants::TEXTURE_CUBE_MAP => unit.tex_cube_map.get(),
            WebGL2RenderingContextConstants::TEXTURE_2D_ARRAY => unit.tex_2d_array.get(),
            WebGL2RenderingContextConstants::TEXTURE_3D => unit.tex_3d.get(),
            _ => None,
        }
    }

    fn active_unit(&self) -> &TextureUnit {
        &self.units[self.active_unit.get() as usize]
    }

    fn iter(&self) -> impl Iterator<Item = (u32, &TextureUnit)> {
        self.units
            .iter()
            .enumerate()
            .map(|(index, unit)| (index as u32 + constants::TEXTURE0, unit))
    }
}

#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
#[derive(Default, JSTraceable, MallocSizeOf)]
struct TextureUnit {
    tex_2d: MutNullableDom<WebGLTexture>,
    tex_cube_map: MutNullableDom<WebGLTexture>,
    tex_2d_array: MutNullableDom<WebGLTexture>,
    tex_3d: MutNullableDom<WebGLTexture>,
    /// Partii (perf): Textures::binding_sequence at this unit's last texture or sampler change.
    changed_at: Cell<u64>,
}

impl TextureUnit {
    fn unbind(&self, texture: &WebGLTexture) -> Option<u32> {
        let fields = [
            (&self.tex_2d, constants::TEXTURE_2D),
            (&self.tex_cube_map, constants::TEXTURE_CUBE_MAP),
            (
                &self.tex_2d_array,
                WebGL2RenderingContextConstants::TEXTURE_2D_ARRAY,
            ),
            (&self.tex_3d, WebGL2RenderingContextConstants::TEXTURE_3D),
        ];
        for &(slot, target) in &fields {
            if slot.get().is_some_and(|t| texture == &*t) {
                slot.set(None);
                return Some(target);
            }
        }
        None
    }
}

pub(crate) struct TexPixels {
    data: GenericSharedMemory,
    size: Size2D<u32>,
    pixel_format: Option<PixelFormat>,
    alpha_treatment: Option<AlphaTreatment>,
    y_axis_treatment: YAxisTreatment,
}

impl TexPixels {
    fn new(
        data: GenericSharedMemory,
        size: Size2D<u32>,
        pixel_format: PixelFormat,
        alpha_treatment: Option<AlphaTreatment>,
        y_axis_treatment: YAxisTreatment,
    ) -> Self {
        Self {
            data,
            size,
            pixel_format: Some(pixel_format),
            alpha_treatment,
            y_axis_treatment,
        }
    }

    pub(crate) fn from_array(
        data: GenericSharedMemory,
        size: Size2D<u32>,
        alpha_treatment: Option<AlphaTreatment>,
        y_axis_treatment: YAxisTreatment,
    ) -> Self {
        Self {
            data,
            size,
            pixel_format: None,
            alpha_treatment,
            y_axis_treatment,
        }
    }

    pub(crate) fn size(&self) -> Size2D<u32> {
        self.size
    }

    pub(crate) fn pixel_format(&self) -> Option<PixelFormat> {
        self.pixel_format
    }

    pub(crate) fn alpha_treatment(&self) -> Option<AlphaTreatment> {
        self.alpha_treatment
    }

    pub(crate) fn y_axis_treatment(&self) -> YAxisTreatment {
        self.y_axis_treatment
    }

    pub(crate) fn into_shared_memory(self) -> GenericSharedMemory {
        self.data
    }

    /// Applies a pending UNPACK_FLIP_Y_WEBGL to the whole image now (an image source: 4 bytes
    /// per pixel, rows of its width). WebGL 2 flips the source before UNPACK_SKIP_* and the
    /// upload size pick a window out of it, so the flip can't wait for the WebGL thread, which
    /// only sees the window.
    pub(crate) fn flip_source_y(mut self) -> Self {
        if self.y_axis_treatment != YAxisTreatment::Flipped {
            return self;
        }
        let row = self.size.width as usize * 4;
        let rows = self.size.height as usize;
        let mut flipped = Vec::with_capacity(row * rows);
        for y in (0..rows).rev() {
            flipped.extend_from_slice(&self.data[y * row..(y + 1) * row]);
        }
        self.data = GenericSharedMemory::from_vec(flipped);
        self.y_axis_treatment = YAxisTreatment::AsIs;
        self
    }

    /// Replaces the pixel data (same pixel format and treatments) with `f(data)`, of `size`.
    pub(crate) fn map_data(self, size: Size2D<u32>, f: impl FnOnce(&[u8]) -> Vec<u8>) -> Self {
        let data = GenericSharedMemory::from_vec(f(&self.data));
        Self { data, size, ..self }
    }

    /// Crops (or zero-pads) an image source (RGBA8 or BGRA8, as every TexImageSource is) to
    /// `width` x `height`, starting at its top-left corner. WebGL 2 uploads from a TexImageSource
    /// with an explicit size read that window of the source; 3D uploads read `depth` slices stacked
    /// vertically, so callers pass `height * depth`.
    pub(crate) fn crop_image_source(self, width: u32, height: u32) -> Self {
        if self.size.width == width && self.size.height == height {
            return self;
        }
        debug_assert!(self.pixel_format.is_some());
        let src = &*self.data;
        let src_stride = self.size.width as usize * 4;
        let dst_stride = width as usize * 4;
        let copy = dst_stride.min(src_stride);
        let mut out = vec![0u8; dst_stride * height as usize];
        for row in 0..(height.min(self.size.height) as usize) {
            out[row * dst_stride..row * dst_stride + copy]
                .copy_from_slice(&src[row * src_stride..row * src_stride + copy]);
        }
        Self {
            data: GenericSharedMemory::from_vec(out),
            size: Size2D::new(width, height),
            ..self
        }
    }
}

/// Component type and bits (red, green, blue, alpha) of a colour format, for copyTexImage2D.
/// Unsized formats count as 8-bit normalized.
fn copy_format_info(format: TexFormat) -> Option<(u8, [u8; 4])> {
    use TexFormat::*;
    const N: u8 = 0; // normalized fixed point
    const S: u8 = 1; // sRGB
    const F: u8 = 2; // float
    const I: u8 = 3; // signed integer
    const U: u8 = 4; // unsigned integer
    Some(match format {
        R8 => (N, [8, 0, 0, 0]),
        RG8 => (N, [8, 8, 0, 0]),
        RGB8 | RGB => (N, [8, 8, 8, 0]),
        RGBA8 | RGBA => (N, [8, 8, 8, 8]),
        Alpha => (N, [0, 0, 0, 8]),
        Luminance => (N, [8, 0, 0, 0]),
        LuminanceAlpha => (N, [8, 0, 0, 8]),
        RGB565 => (N, [5, 6, 5, 0]),
        RGBA4 => (N, [4, 4, 4, 4]),
        RGB5A1 => (N, [5, 5, 5, 1]),
        RGB10A2 => (N, [10, 10, 10, 2]),
        SRGB8 => (S, [8, 8, 8, 0]),
        SRGB8Alpha8 => (S, [8, 8, 8, 8]),
        R16f => (F, [16, 0, 0, 0]),
        RG16f => (F, [16, 16, 0, 0]),
        RGB16f => (F, [16, 16, 16, 0]),
        RGBA16f => (F, [16, 16, 16, 16]),
        R32f => (F, [32, 0, 0, 0]),
        RG32f => (F, [32, 32, 0, 0]),
        RGB32f => (F, [32, 32, 32, 0]),
        RGBA32f => (F, [32, 32, 32, 32]),
        R11fG11fB10f => (F, [11, 11, 10, 0]),
        R8i => (I, [8, 0, 0, 0]),
        RG8i => (I, [8, 8, 0, 0]),
        RGBA8i => (I, [8, 8, 8, 8]),
        R16i => (I, [16, 0, 0, 0]),
        RG16i => (I, [16, 16, 0, 0]),
        RGBA16i => (I, [16, 16, 16, 16]),
        R32i => (I, [32, 0, 0, 0]),
        RG32i => (I, [32, 32, 0, 0]),
        RGBA32i => (I, [32, 32, 32, 32]),
        R8ui => (U, [8, 0, 0, 0]),
        RG8ui => (U, [8, 8, 0, 0]),
        RGBA8ui => (U, [8, 8, 8, 8]),
        R16ui => (U, [16, 0, 0, 0]),
        RG16ui => (U, [16, 16, 0, 0]),
        RGBA16ui => (U, [16, 16, 16, 16]),
        R32ui => (U, [32, 0, 0, 0]),
        RG32ui => (U, [32, 32, 0, 0]),
        RGBA32ui => (U, [32, 32, 32, 32]),
        RGB10A2ui => (U, [10, 10, 10, 2]),
        // RGB integer formats aren't colour-renderable, so only ever a copy destination.
        RGB8i => (I, [8, 8, 8, 0]),
        RGB16i => (I, [16, 16, 16, 0]),
        RGB32i => (I, [32, 32, 32, 0]),
        RGB8ui => (U, [8, 8, 8, 0]),
        RGB16ui => (U, [16, 16, 16, 0]),
        RGB32ui => (U, [32, 32, 32, 0]),
        _ => return None,
    })
}

/// GL ES 3.0 copyTexImage2D: the destination's components must exist in the source, with the
/// same component type, and a sized destination must match the source's bits exactly.
/// Unsized destinations take any normalized source.
fn copy_tex_image_compatible(source: TexFormat, destination: TexFormat) -> bool {
    let (Some((source_type, source_bits)), Some((destination_type, destination_bits))) =
        (copy_format_info(source), copy_format_info(destination))
    else {
        return false;
    };
    if source_type != destination_type {
        return false;
    }
    let unsized_destination = !destination.is_sized();
    (0..4).all(|c| {
        destination_bits[c] == 0 ||
            (source_bits[c] != 0 &&
                (unsized_destination || source_bits[c] == destination_bits[c]))
    })
}

/// copyTexSubImage*: the destination image's components must exist in the source, of the same
/// component type (bit counts may differ). Formats this doesn't know are left to GL.
fn copy_tex_sub_image_compatible(source: TexFormat, destination: TexFormat) -> bool {
    let (Some((source_type, source_bits)), Some((destination_type, destination_bits))) =
        (copy_format_info(source), copy_format_info(destination))
    else {
        return true;
    };
    source_type == destination_type &&
        (0..4).all(|c| destination_bits[c] == 0 || source_bits[c] != 0)
}

/// Whether a uniform type is a sampler (set with uniform1i / uniform1iv, read back as an int):
/// WebGL 1's 2D and cube samplers and WebGL 2's 3D, array, shadow and integer ones.
pub(crate) fn is_sampler_type(type_: u32) -> bool {
    use WebGL2RenderingContextConstants as c2;
    matches!(
        type_,
        constants::SAMPLER_2D |
            constants::SAMPLER_CUBE |
            c2::SAMPLER_3D |
            c2::SAMPLER_2D_ARRAY |
            c2::SAMPLER_2D_SHADOW |
            c2::SAMPLER_2D_ARRAY_SHADOW |
            c2::SAMPLER_CUBE_SHADOW |
            c2::INT_SAMPLER_2D |
            c2::INT_SAMPLER_3D |
            c2::INT_SAMPLER_CUBE |
            c2::INT_SAMPLER_2D_ARRAY |
            c2::UNSIGNED_INT_SAMPLER_2D |
            c2::UNSIGNED_INT_SAMPLER_3D |
            c2::UNSIGNED_INT_SAMPLER_CUBE |
            c2::UNSIGNED_INT_SAMPLER_2D_ARRAY
    )
}

/// Where a compressed texture upload reads its data from.
pub(crate) enum CompressedSource<'a> {
    Bytes(&'a [u8]),
    /// `size` bytes at `offset` in the bound PIXEL_UNPACK_BUFFER (WebGL 2; validated by the caller).
    PixelUnpackBuffer {
        offset: u32,
        size: u32,
    },
}

impl CompressedSource<'_> {
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Bytes(bytes) => bytes.len(),
            Self::PixelUnpackBuffer { size, .. } => *size as usize,
        }
    }

    pub(crate) fn into_command_data(self) -> CompressedTexData {
        match self {
            Self::Bytes(bytes) => {
                CompressedTexData::Bytes(GenericSharedMemory::from_bytes(bytes).into())
            },
            Self::PixelUnpackBuffer { offset, size } => {
                CompressedTexData::PixelUnpackBuffer { offset, size }
            },
        }
    }
}

pub(crate) enum TexSource {
    Pixels(TexPixels),
    BufferOffset(i64),
}

pub(crate) fn array_buffer_type_to_sized_type(type_: Type) -> Option<SizedDataType> {
    match type_ {
        Type::Uint8 | Type::Uint8Clamped => Some(SizedDataType::Uint8),
        Type::Uint16 => Some(SizedDataType::Uint16),
        Type::Uint32 => Some(SizedDataType::Uint32),
        Type::Int8 => Some(SizedDataType::Int8),
        Type::Int16 => Some(SizedDataType::Int16),
        Type::Int32 => Some(SizedDataType::Int32),
        Type::Float32 => Some(SizedDataType::Float32),
        Type::Float16 |
        Type::Float64 |
        Type::BigInt64 |
        Type::BigUint64 |
        Type::MaxTypedArrayViewType |
        Type::Int64 |
        Type::Simd128 => None,
    }
}

/// Clips a copyTexSubImage source rectangle to the read framebuffer, moving the destination
/// offset with it. None when nothing is inside.
pub(crate) fn clip_copy_to_read_framebuffer(
    (read_width, read_height): (i32, i32),
    xoffset: i32,
    yoffset: i32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Option<(i32, i32, i32, i32, i32, i32)> {
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x as i64 + width as i64).min(read_width as i64);
    let y1 = (y as i64 + height as i64).min(read_height as i64);
    if x1 <= x0 as i64 || y1 <= y0 as i64 {
        return None;
    }
    Some((
        xoffset + (x0 - x),
        yoffset + (y0 - y),
        x0,
        y0,
        (x1 - x0 as i64) as i32,
        (y1 - y0 as i64) as i32,
    ))
}

/// Partii (perf): what a draw may skip of its draw-time state checks (draw_state_cached).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum DrawStateCache {
    /// Run them all.
    Miss,
    /// Skip them all.
    Hit,
    /// Only a texture or sampler binding the program samples changed: run the sampler check
    /// (sampler types, feedback loops) and skip the rest.
    Samplers,
}

/// Partii (perf): PARTII_WEBGL_DRAW_CACHE=0 validates every draw's state in full, as before the draw
/// validation cache (for A/B measurements).
fn draw_cache_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| !matches!(std::env::var("PARTII_WEBGL_DRAW_CACHE").as_deref(), Ok("0")))
}

/// Partii (perf): a context's draw-state generation. Every command sent to the WebGL thread, by the
/// context or by one of its objects, moves it on, except those that cannot change the outcome of a
/// draw's state checks (framebuffer completeness, uniform buffers, attribute types and buffers,
/// sampler types and feedback loops, fragment outputs, transform feedback buffers). Each of those
/// checks mirrors GL state, and GL state only changes by a command, so a program whose checks passed
/// with the bound vertex array at the current generation passes them again: draws skip them until
/// the next generation. The
/// checks that read a draw's own arguments (mode, counts, offsets, index type, buffer ranges) run
/// on every draw. Starts at 1; a program never validated holds 0.
#[derive(Clone)]
pub(crate) struct DrawStateGen(Rc<Cell<u64>>);

impl Default for DrawStateGen {
    fn default() -> Self {
        DrawStateGen(Rc::new(Cell::new(1)))
    }
}

impl DrawStateGen {
    #[inline]
    pub(crate) fn get(&self) -> u64 {
        self.0.get()
    }

    /// Moves the generation on: for state changed without a command off the allowlist (the
    /// front/back stencil agreement, WebGLRenderingContext::set_stencil_state).
    #[inline]
    pub(crate) fn bump(&self) {
        self.0.set(self.0.get() + 1);
    }

    /// Moves the generation on unless `command` leaves every draw-time state check as it was:
    /// uniforms other than uniform1i(v) (which set sampler units), useProgram and bindVertexArray
    /// (the cache is per vertex array and program), bindTexture and bindSampler (each unit's
    /// binding sequence number covers them: Textures), draws, clears, fixed-function state no check reads, and queries.
    #[inline]
    pub(crate) fn note(&self, command: &WebGLCommand) {
        use WebGLCommand as C;
        let keeps = matches!(
            command,
            C::Uniform1f(..) |
                C::Uniform1fv(..) |
                C::Uniform1ui(..) |
                C::Uniform1uiv(..) |
                C::Uniform2f(..) |
                C::Uniform2fv(..) |
                C::Uniform2i(..) |
                C::Uniform2ui(..) |
                C::Uniform2iv(..) |
                C::Uniform2uiv(..) |
                C::Uniform3f(..) |
                C::Uniform3fv(..) |
                C::Uniform3i(..) |
                C::Uniform3ui(..) |
                C::Uniform3iv(..) |
                C::Uniform3uiv(..) |
                C::Uniform4f(..) |
                C::Uniform4fv(..) |
                C::Uniform4i(..) |
                C::Uniform4ui(..) |
                C::Uniform4iv(..) |
                C::Uniform4uiv(..) |
                C::UniformMatrix2fv(..) |
                C::UniformMatrix3fv(..) |
                C::UniformMatrix4fv(..) |
                C::UniformMatrix3x2fv(..) |
                C::UniformMatrix4x2fv(..) |
                C::UniformMatrix2x3fv(..) |
                C::UniformMatrix4x3fv(..) |
                C::UniformMatrix2x4fv(..) |
                C::UniformMatrix3x4fv(..) |
                C::UseProgram(..) |
                C::BindVertexArray(..) |
                C::BindTexture(..) |
                C::BindSampler(..) |
                C::DrawArrays { .. } |
                C::DrawArraysInstanced { .. } |
                C::DrawElements { .. } |
                C::DrawElementsInstanced { .. } |
                C::DrawArraysInstancedBaseInstance { .. } |
                C::DrawElementsInstancedBaseVertexBaseInstance { .. } |
                C::Clear(..) |
                C::ClearColor(..) |
                C::ClearDepth(..) |
                C::ClearStencil(..) |
                C::SetViewport(..) |
                C::Scissor(..) |
                C::BlendColor(..) |
                C::BlendEquation(..) |
                C::BlendEquationSeparate(..) |
                C::BlendFunc(..) |
                C::BlendFuncSeparate(..) |
                C::DepthFunc(..) |
                C::DepthMask(..) |
                C::DepthRange(..) |
                C::CullFace(..) |
                C::FrontFace(..) |
                C::PolygonOffset(..) |
                C::LineWidth(..) |
                C::StencilFunc(..) |
                C::StencilFuncSeparate(..) |
                C::StencilMask(..) |
                C::StencilMaskSeparate(..) |
                C::StencilOp(..) |
                C::StencilOpSeparate(..) |
                C::Hint(..) |
                C::SampleCoverage(..) |
                C::ActiveTexture(..) |
                C::PixelStorei(..) |
                C::Flush |
                C::Finish(..) |
                C::GetParameterBool(..) |
                C::GetParameterBool4(..) |
                C::GetParameterInt(..) |
                C::GetParameterUInt(..) |
                C::GetParameterInt2(..) |
                C::GetParameterInt4(..) |
                C::GetParameterFloat(..) |
                C::GetParameterFloat2(..) |
                C::GetParameterFloat4(..)
        );
        // PARTII_WEBGL_TEX_UNIT_CACHE=0: texture and sampler binds move the generation (round 8).
        let keeps = keeps &&
            (tex_unit_cache_enabled() || !matches!(command, C::BindTexture(..) | C::BindSampler(..)));
        if !keeps {
            self.0.set(self.0.get() + 1);
        }
    }
}

/// Partii (perf): PARTII_WEBGL_TEX_UNIT_CACHE=0 makes texture and sampler binds end every draw-state
/// record again, as before per-unit binding sequence numbers (for A/B measurements).
fn tex_unit_cache_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| !matches!(std::env::var("PARTII_WEBGL_TEX_UNIT_CACHE").as_deref(), Ok("0")))
}

/// Partii (perf): PARTII_WEBGL_VAO_POOL=0 creates each vertex array by a round trip (CreateVertexArray)
/// rather than from the pooled names (for A/B measurements).
fn vao_pool_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| !matches!(std::env::var("PARTII_WEBGL_VAO_POOL").as_deref(), Ok("0")))
}

/// OES_texture_half_float's HALF_FLOAT_OES (WebGL 1 readPixels accepts the enum).
const HALF_FLOAT_OES: u32 = 0x8D61;

/// One face's stencil reference and masks (stencilFunc / stencilMask).
#[derive(Clone, Copy)]
struct StencilFace {
    reference: i32,
    value_mask: u32,
    write_mask: u32,
}

#[derive(Clone, Copy)]
struct StencilState {
    front: StencilFace,
    back: StencilFace,
}

impl Default for StencilState {
    fn default() -> Self {
        let face = StencilFace {
            reference: 0,
            value_mask: u32::MAX,
            write_mask: u32::MAX,
        };
        StencilState {
            front: face,
            back: face,
        }
    }
}

impl StencilState {
    /// Whether front and back disagree in the stencil buffer's bits: every stencil format a
    /// WebGL framebuffer can have here holds 8, the reference clamped to [0, 255] as GL does.
    fn mismatched(&self) -> bool {
        const BITS: u32 = 0xff;
        let (front, back) = (self.front, self.back);
        (front.write_mask ^ back.write_mask) & BITS != 0 ||
            (front.value_mask ^ back.value_mask) & BITS != 0 ||
            front.reference.clamp(0, BITS as i32) != back.reference.clamp(0, BITS as i32)
    }
}

/// Partii (perf): names reserved per object kind (WebGLRenderingContext::take_object_name): 128, or
/// PARTII_WEBGL_NAME_BATCH (1 = a round trip per create, as before; for A/B measurements).
fn object_name_batch() -> u32 {
    static BATCH: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *BATCH.get_or_init(|| {
        std::env::var("PARTII_WEBGL_NAME_BATCH")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n: &u32| n >= 1)
            .unwrap_or(128)
    })
}

#[derive(Default)]
pub(crate) struct ObjectNamePool {
    names: std::collections::VecDeque<u32>,
    pending: Option<GenericReceiver<Vec<u32>>>,
}

/// One pool per WebGLObjectNameKind.
#[derive(Default)]
pub(crate) struct ObjectNamePools([ObjectNamePool; 5]);

// Names and a channel: no GC things.
#[expect(unsafe_code)]
unsafe impl JSTraceable for ObjectNamePools {
    unsafe fn trace(&self, _trc: *mut JSTracer) {}
}
