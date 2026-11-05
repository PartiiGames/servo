/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
#![expect(unsafe_code)]
use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::{slice, thread};

use bitflags::bitflags;
use byteorder::{ByteOrder, NativeEndian, WriteBytesExt};
use euclid::default::Size2D;
use glow::{
    self as gl, ActiveTransformFeedback, Context as Gl, HasContext, NativeTransformFeedback,
    NativeUniformLocation, NativeVertexArray, PixelUnpackData, ShaderPrecisionFormat,
    bytes_per_type, components_per_format,
};
use half::f16;
use itertools::Itertools;
use log::{debug, error, trace, warn};
use mozangle::shaders::{BuiltInResources, CompileOptions, Output, ShaderValidator};
use paint_api::{
    CrossProcessPaintApi, PainterSurfmanDetailsMap, SerializableImageData,
    WebRenderExternalImageIdManager, WebRenderImageHandlerType,
};
use parking_lot::RwLock;
use pixels::{self, PixelFormat, SnapshotAlphaMode, unmultiply_inplace};
use rustc_hash::FxHashMap;
use servo_base::Epoch;
use servo_base::generic_channel::{
    GenericReceiver, GenericSender, GenericSharedMemory, RoutedReceiver,
};
use servo_base::id::PainterId;
use servo_canvas_traits::webgl;
#[cfg(feature = "webxr")]
use servo_canvas_traits::webgl::WebXRCommand;
use servo_canvas_traits::webgl::{
    ActiveAttribInfo, ActiveUniformBlockInfo, ActiveUniformInfo, AlphaTreatment, CompressedTexData,
    EmulatedBuiltinUniforms,
    GLContextAttributes, GLLimits, GlType, InternalFormatIntVec, ProgramLinkInfo, TexDataType,
    TexFormat, WebGLBufferId, WebGLChan, WebGLCommand, WebGLCommandBacktrace, WebGLContextId,
    WebGLCreateContextResult, WebGLFramebufferBindingRequest, WebGLFramebufferId, WebGLMsg,
    WebGLMsgSender, WebGLObjectNameKind, WebGLProgramId, WebGLQueryId, WebGLRenderbufferId,
    ShaderCompileInfo, ShaderTranslation, WebGLSLVersion,
    WebGLSamplerId, WebGLShaderId, WebGLSyncId, WebGLTextureId, WebGLVersion, WebGLVertexArrayId,
    YAxisTreatment,
};
use surfman::chains::{PreserveBuffer, SwapChains, SwapChainsAPI};
use surfman::{
    self, Context, ContextAttributeFlags, ContextAttributes, Device, GLVersion, SurfaceAccess,
    SurfaceInfo, SurfaceType,
};
use webrender_api::units::DeviceIntSize;
use webrender_api::{
    ExternalImageData, ExternalImageId, ExternalImageType, ImageBufferKind, ImageDescriptor,
    ImageDescriptorFlags, ImageFormat, ImageKey,
};

use crate::webgl_limits::GLLimitsDetect;
#[cfg(feature = "webxr")]
use crate::webxr::{WebXRBridge, WebXRBridgeInit};

type GLint = i32;

fn native_uniform_location(location: i32) -> Option<NativeUniformLocation> {
    location.try_into().ok().map(NativeUniformLocation)
}

/// A map which tracks whether a given WebGL context is "busy" ie whether WebRender has
/// currently taken a surface from its [`SwapChain`] for rendering purposes. Contexts will
/// only be deleted once no WebRender instance is using it for rendering. This ensures
/// that all Surfman `Surface`s can be released properly on the [`WebGLThread`].
pub type WebGLContextBusyMap = Arc<RwLock<HashMap<WebGLContextId, usize>>>;

pub(crate) struct GLContextData {
    pub(crate) ctx: Context,
    pub(crate) gl: Rc<glow::Context>,
    device: Rc<Device>,
    state: GLState,
    attributes: GLContextAttributes,
    /// The context should be removed, but the [`WebGLThread`] is currently waiting on
    /// WebRender to finish rendering to the context in order to delete it.
    marked_for_deletion: bool,
}

#[derive(Debug)]
pub struct GLState {
    _webgl_version: WebGLVersion,
    _gl_version: GLVersion,
    requested_flags: ContextAttributeFlags,
    // This is the WebGL view of the color mask
    // The GL view may be different: if the GL context supports alpha
    // but the WebGL context doesn't, then color_write_mask.3 might be true
    // but the GL color write mask is false.
    color_write_mask: [bool; 4],
    clear_color: (f32, f32, f32, f32),
    scissor_test_enabled: bool,
    // The WebGL view of the stencil write mask (see comment re `color_write_mask`)
    stencil_write_mask: (u32, u32),
    // Kept here rather than read back: glGetIntegerv cannot return a full GLuint.
    stencil_value_mask: (u32, u32),
    stencil_test_enabled: bool,
    stencil_clear_value: i32,
    // The WebGL view of the depth write mask (see comment re `color_write_mask`)
    depth_write_mask: bool,
    depth_test_enabled: bool,
    depth_clear_value: f64,
    // True when the default framebuffer is bound to DRAW_FRAMEBUFFER
    drawing_to_default_framebuffer: bool,
    default_vao: Option<NativeVertexArray>,
    /// WebGL 2 contexts on desktop GL are core profile, which has no ALPHA, LUMINANCE or
    /// LUMINANCE_ALPHA textures: they are stored as R / RG textures with a swizzle.
    emulate_luminance: bool,
    /// The scissor box has no area (glScissor with a zero width or height).
    scissor_empty: bool,
    /// ANY_SAMPLES_PASSED(_CONSERVATIVE) queries between beginQuery and endQuery.
    active_occlusion_queries: u8,
    /// Between beginTransformFeedback and endTransformFeedback (paused included).
    transform_feedback_active: bool,
    /// Opaque black 1x1 textures (2D, cube map) that stand in for incomplete ones.
    incomplete_textures: (Option<gl::NativeTexture>, Option<gl::NativeTexture>),
}

impl GLState {
    /// Whether a draw can be dropped: with an empty scissor box it writes nothing, and Mesa's
    /// llvmpipe resets an active occlusion query to 0 for such a draw issued after ones that
    /// passed samples (occlusion-query-scissor, anglebug 7157). Transform feedback still
    /// captures scissored-out draws, so those always run.
    fn draw_writes_nothing_under_query(&self) -> bool {
        self.scissor_test_enabled &&
            self.scissor_empty &&
            self.active_occlusion_queries > 0 &&
            !self.transform_feedback_active
    }

    // Are we faking having no alpha / depth / stencil?
    fn fake_no_alpha(&self) -> bool {
        self.drawing_to_default_framebuffer &
            !self.requested_flags.contains(ContextAttributeFlags::ALPHA)
    }

    fn fake_no_depth(&self) -> bool {
        self.drawing_to_default_framebuffer &
            !self.requested_flags.contains(ContextAttributeFlags::DEPTH)
    }

    fn fake_no_stencil(&self) -> bool {
        self.drawing_to_default_framebuffer &
            !self
                .requested_flags
                .contains(ContextAttributeFlags::STENCIL)
    }

    // We maintain invariants between the GLState object and the GL state.
    fn restore_invariant(&self, gl: &Gl) {
        self.restore_clear_invariant(gl);
        self.restore_scissor_invariant(gl);
        self.restore_alpha_invariant(gl);
        self.restore_depth_invariant(gl);
        self.restore_stencil_invariant(gl);
    }

    fn restore_clear_invariant(&self, gl: &Gl) {
        let (r, g, b, a) = self.clear_color;
        unsafe {
            gl.clear_color(r, g, b, a);
            gl.clear_depth(self.depth_clear_value);
            gl.clear_stencil(self.stencil_clear_value);
        }
    }

    fn restore_scissor_invariant(&self, gl: &Gl) {
        if self.scissor_test_enabled {
            unsafe { gl.enable(gl::SCISSOR_TEST) };
        } else {
            unsafe { gl.disable(gl::SCISSOR_TEST) };
        }
    }

    fn restore_alpha_invariant(&self, gl: &Gl) {
        let [r, g, b, a] = self.color_write_mask;
        if self.fake_no_alpha() {
            unsafe { gl.color_mask(r, g, b, false) };
        } else {
            unsafe { gl.color_mask(r, g, b, a) };
        }
    }

    fn restore_depth_invariant(&self, gl: &Gl) {
        unsafe {
            if self.fake_no_depth() {
                gl.depth_mask(false);
                gl.disable(gl::DEPTH_TEST);
            } else {
                gl.depth_mask(self.depth_write_mask);
                if self.depth_test_enabled {
                    gl.enable(gl::DEPTH_TEST);
                } else {
                    gl.disable(gl::DEPTH_TEST);
                }
            }
        }
    }

    fn restore_stencil_invariant(&self, gl: &Gl) {
        unsafe {
            if self.fake_no_stencil() {
                gl.stencil_mask(0);
                gl.disable(gl::STENCIL_TEST);
            } else {
                let (f, b) = self.stencil_write_mask;
                gl.stencil_mask_separate(gl::FRONT, f);
                gl.stencil_mask_separate(gl::BACK, b);
                if self.stencil_test_enabled {
                    gl.enable(gl::STENCIL_TEST);
                } else {
                    gl.disable(gl::STENCIL_TEST);
                }
            }
        }
    }
}

impl Default for GLState {
    fn default() -> GLState {
        GLState {
            _gl_version: GLVersion { major: 1, minor: 0 },
            _webgl_version: WebGLVersion::WebGL1,
            requested_flags: ContextAttributeFlags::empty(),
            emulate_luminance: false,
            scissor_empty: false,
            active_occlusion_queries: 0,
            transform_feedback_active: false,
            incomplete_textures: (None, None),
            color_write_mask: [true, true, true, true],
            clear_color: (0., 0., 0., 0.),
            scissor_test_enabled: false,
            // GL's initial stencil writemask is all ones.
            stencil_write_mask: (u32::MAX, u32::MAX),
            stencil_value_mask: (u32::MAX, u32::MAX),
            stencil_test_enabled: false,
            stencil_clear_value: 0,
            depth_write_mask: true,
            depth_test_enabled: false,
            depth_clear_value: 1.,
            default_vao: None,
            drawing_to_default_framebuffer: true,
        }
    }
}

/// A WebGLThread manages the life cycle and message multiplexing of
/// a set of WebGLContexts living in the same thread.
pub(crate) struct WebGLThread {
    /// The GPU device.
    device_map: HashMap<PainterId, Rc<Device>>,
    /// Channel used to generate/update or delete `ImageKey`s.
    paint_api: CrossProcessPaintApi,
    /// Map of live WebGLContexts.
    contexts: FxHashMap<WebGLContextId, GLContextData>,
    /// Cached information for WebGLContexts.
    cached_context_info: FxHashMap<WebGLContextId, WebGLContextInfo>,
    /// Current bound context.
    bound_context_id: Option<WebGLContextId>,
    /// A [`WebRenderExternalImageIdManager`] used to generate new [`ExternalImageId`]s for our
    /// WebGL contexts.
    external_image_id_manager: WebRenderExternalImageIdManager,
    /// The receiver that will be used for processing WebGL messages.
    receiver: RoutedReceiver<WebGLMsg>,
    /// The receiver that should be used to send WebGL messages for processing.
    sender: GenericSender<WebGLMsg>,
    /// The swap chains used by webrender
    webrender_swap_chains: SwapChains<WebGLContextId, Device>,
    /// The per-painter details of the underlying surfman connection.
    painter_surfman_details_map: PainterSurfmanDetailsMap,
    /// A usage map used to delay the deletion of WebGL contexts until all WebRender
    /// rendering is finished, so that any existing `Surface`s can be properly released.
    busy_webgl_context_map: WebGLContextBusyMap,

    #[cfg(feature = "webxr")]
    /// The bridge to WebXR
    pub webxr_bridge: Option<WebXRBridge>,
}

/// The data required to initialize an instance of the WebGLThread type.
pub(crate) struct WebGLThreadInit {
    pub paint_api: CrossProcessPaintApi,
    pub external_image_id_manager: WebRenderExternalImageIdManager,
    pub sender: GenericSender<WebGLMsg>,
    pub receiver: GenericReceiver<WebGLMsg>,
    pub webrender_swap_chains: SwapChains<WebGLContextId, Device>,
    pub painter_surfman_details_map: PainterSurfmanDetailsMap,
    pub busy_webgl_context_map: WebGLContextBusyMap,
    #[cfg(feature = "webxr")]
    pub webxr_init: WebXRBridgeInit,
}

// A size at which it should be safe to create GL contexts
const SAFE_VIEWPORT_DIMS: [u32; 2] = [1024, 1024];

impl WebGLThread {
    /// Create a new instance of WebGLThread.
    pub(crate) fn new(
        WebGLThreadInit {
            paint_api,
            external_image_id_manager: external_images,
            sender,
            receiver,
            webrender_swap_chains,
            painter_surfman_details_map,
            busy_webgl_context_map,
            #[cfg(feature = "webxr")]
            webxr_init,
        }: WebGLThreadInit,
    ) -> Self {
        WebGLThread {
            device_map: Default::default(),
            paint_api,
            contexts: Default::default(),
            cached_context_info: Default::default(),
            bound_context_id: None,
            external_image_id_manager: external_images,
            sender,
            receiver: receiver.route_preserving_errors(),
            webrender_swap_chains,
            painter_surfman_details_map,
            busy_webgl_context_map,
            #[cfg(feature = "webxr")]
            webxr_bridge: Some(WebXRBridge::new(webxr_init)),
        }
    }

    /// Perform all initialization required to run an instance of WebGLThread
    /// in parallel on its own dedicated thread.
    pub(crate) fn run_on_own_thread(init: WebGLThreadInit) -> JoinHandle<()> {
        thread::Builder::new()
            .name("WebGL".to_owned())
            .spawn(move || {
                let mut data = WebGLThread::new(init);
                data.process();
            })
            .expect("Thread spawning failed")
    }

    fn process(&mut self) {
        let webgl_chan = WebGLChan(self.sender.clone());
        let mut prof = frame_prof::Acc::default();
        while let Ok(Ok(msg)) = self.receiver.recv() {
            if !frame_prof::enabled() {
                if self.handle_msg(msg, &webgl_chan) {
                    break;
                }
                continue;
            }
            let class = frame_prof::class_of(&msg);
            let kind = match &msg {
                WebGLMsg::WebGLCommand(_, command, _) => command_kind(command),
                _ => "msg",
            };
            let start = std::time::Instant::now();
            let exit = self.handle_msg(msg, &webgl_chan);
            prof.record(class, start);
            frame_prof::trace_msg(class, kind, start);
            if exit {
                break;
            }
        }
    }

    /// Enable GL_POINT_SPRITE and GL_PROGRAM_POINT_SIZE on desktop OpenGL.
    ///
    /// FIXME(nox): Should probably be done by surfman.
    /// FIXME(sagudev): Do we even need to do this?
    fn ensure_point_sprite_and_program_point_size_enabled(gl_context_data: &GLContextData) {
        // Points sprites are enabled by default in OpenGL 3.2 core
        // and in GLES.
        if gl_context_data.gl.version().is_embedded {
            return;
        }

        // Rather than doing version detection, it does not hurt to enable GL_POINT_SPRITE and
        // PROGRAM_POINT_SIZE always.
        const GL_POINT_SPRITE: u32 = 0x8861;
        unsafe { gl_context_data.gl.enable(GL_POINT_SPRITE) };
        let error = unsafe { gl_context_data.gl.get_error() };
        if error != 0 {
            warn!("Error enabling GL point sprites: {error}");
        }

        unsafe { gl_context_data.gl.enable(gl::PROGRAM_POINT_SIZE) };
        let error = unsafe { gl_context_data.gl.get_error() };
        if error != 0 {
            warn!("Error enabling GL program point size: {error}");
        }
    }

    /// Handles a generic WebGLMsg message
    fn handle_msg(&mut self, msg: WebGLMsg, webgl_chan: &WebGLChan) -> bool {
        trace!("processing {:?}", msg);
        match msg {
            WebGLMsg::CreateContext(painter_id, version, size, attributes, result_sender) => {
                let result = self.create_webgl_context(painter_id, version, size, attributes);

                result_sender
                    .send(result.map(|(id, limits)| {
                        let data = self
                            .make_current_if_needed(id)
                            .expect("WebGLContext not found");

                        Self::ensure_point_sprite_and_program_point_size_enabled(data);

                        let glsl_version = Self::get_glsl_version(&data.gl);
                        let api_type = if data.gl.version().is_embedded {
                            GlType::Gles
                        } else {
                            GlType::Gl
                        };

                        WebGLCreateContextResult {
                            sender: WebGLMsgSender::new(id, webgl_chan.clone()),
                            limits,
                            glsl_version,
                            api_type,
                        }
                    }))
                    .unwrap();
            },
            WebGLMsg::SetImageKey(ctx_id, image_key) => {
                self.handle_set_image_key(ctx_id, image_key);
            },
            WebGLMsg::ResizeContext(ctx_id, size, sender) => {
                let _ = sender.send(self.resize_webgl_context(ctx_id, size));
            },
            WebGLMsg::RemoveContext(ctx_id) => {
                self.remove_webgl_context(ctx_id);
            },
            WebGLMsg::WebGLCommand(ctx_id, command, backtrace) => {
                self.handle_webgl_command(ctx_id, command, backtrace);
            },
            WebGLMsg::WebXRCommand(_command) => {
                #[cfg(feature = "webxr")]
                self.handle_webxr_command(_command);
            },
            WebGLMsg::SwapBuffers(swap_ids, canvas_epoch, sent_time) => {
                self.handle_swap_buffers(canvas_epoch, swap_ids, sent_time);
            },
            WebGLMsg::FinishedRenderingToContext(context_id) => {
                self.handle_finished_rendering_to_context(context_id);
            },
            WebGLMsg::ClearPainterResources(painter_id, sender) => {
                self.device_map.remove(&painter_id);
                if let Err(error) = sender.send(()) {
                    warn!("Failed to send response to WebGLMsg::ClearPainterResources ({error})");
                }
            },
            WebGLMsg::Exit => {
                // Call remove_context functions in order to correctly delete WebRender image keys.
                let context_ids: Vec<WebGLContextId> = self.contexts.keys().copied().collect();
                for id in context_ids {
                    self.remove_webgl_context(id);
                }
                return true;
            },
        }

        false
    }

    fn get_or_create_device_for_painter(
        &mut self,
        painter_id: PainterId,
    ) -> Result<Rc<Device>, String> {
        let entry = self.device_map.entry(painter_id);
        if let Entry::Occupied(entry) = entry {
            return Ok(entry.get().clone());
        }

        // This can happen if the Webview was dropped while one of its ScriptThreads
        // is still issuing asynchronous commands to the WebGL thread.
        let Some(surfman_details) = self.painter_surfman_details_map.get(painter_id) else {
            return Err(format!("No PainterSurfmanDetails found for {painter_id:?}"));
        };

        // Gracefully handle failure to create a device.
        let Ok(device) = surfman_details
            .connection
            .create_device(&surfman_details.adapter)
        else {
            return Err("Could not open WebGL device".into());
        };

        Ok(entry.or_insert(Rc::new(device)).clone())
    }

    #[cfg(feature = "webxr")]
    /// Handles a WebXR message
    fn handle_webxr_command(&mut self, command: WebXRCommand) {
        trace!("processing {:?}", command);
        // Take `webxr_bridge` from the `WebGLThread` in order to avoid a double mutable borrow.
        let Some(mut webxr_bridge) = self.webxr_bridge.take() else {
            return;
        };
        match command {
            WebXRCommand::CreateLayerManager(sender) => {
                let result = webxr_bridge.create_layer_manager(self);
                let _ = sender.send(result);
            },
            WebXRCommand::DestroyLayerManager(manager_id) => {
                webxr_bridge.destroy_layer_manager(manager_id);
            },
            WebXRCommand::CreateLayer(manager_id, context_id, layer_init, sender) => {
                let result = webxr_bridge.create_layer(manager_id, self, context_id, layer_init);
                let _ = sender.send(result);
            },
            WebXRCommand::DestroyLayer(manager_id, context_id, layer_id) => {
                webxr_bridge.destroy_layer(manager_id, self, context_id, layer_id);
            },
            WebXRCommand::BeginFrame(manager_id, layers, sender) => {
                let result = webxr_bridge.begin_frame(manager_id, self, &layers[..]);
                let _ = sender.send(result);
            },
            WebXRCommand::EndFrame(manager_id, layers, sender) => {
                let result = webxr_bridge.end_frame(manager_id, self, &layers[..]);
                let _ = sender.send(result);
            },
        }

        self.webxr_bridge.replace(webxr_bridge);
    }

    fn device_for_context(&self, context_id: WebGLContextId) -> Rc<Device> {
        self.maybe_device_for_context(context_id)
            .expect("Should be called with a valid WebGLContextId")
    }

    /// A function like `Self::device_for_context`, except that it does not panic if the context
    /// cannot be found. This is useful for WebXR, which might try to access WebGL contexts after
    /// they have been cleaned up.
    pub(crate) fn maybe_device_for_context(
        &self,
        context_id: WebGLContextId,
    ) -> Option<Rc<Device>> {
        self.contexts
            .get(&context_id)
            .map(|context| context.device.clone())
    }

    /// Handles a WebGLCommand for a specific WebGLContext
    fn handle_webgl_command(
        &mut self,
        context_id: WebGLContextId,
        command: WebGLCommand,
        backtrace: WebGLCommandBacktrace,
    ) {
        if self.cached_context_info.get_mut(&context_id).is_none() {
            return;
        }
        let data = self.make_current_if_needed_mut(context_id);
        if let Some(data) = data {
            // Partii (perf stream, diagnostic): PARTII_WEBGL_STATS=cmd logs each command of 0.3 ms or more.
            let timed = webgl_cmd_stats_enabled()
                .then(|| (command_kind(&command), std::time::Instant::now()));
            WebGLImpl::apply(
                &data.device,
                &data.ctx,
                &data.gl,
                &mut data.state,
                &data.attributes,
                command,
                backtrace,
            );
            if let Some((kind, start)) = timed {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                if ms >= 0.3 {
                    eprintln!(
                        "PARTII_WEBGL_CMD t={:.1} {} {:.2}",
                        webgl_stats_epoch().elapsed().as_secs_f64() * 1000.0,
                        kind,
                        ms
                    );
                }
            }
        }
    }

    /// Creates a new WebGLContext
    fn create_webgl_context(
        &mut self,
        painter_id: PainterId,
        webgl_version: WebGLVersion,
        requested_size: Size2D<u32>,
        attributes: GLContextAttributes,
    ) -> Result<(WebGLContextId, webgl::GLLimits), String> {
        debug!(
            "WebGLThread::create_webgl_context({:?}, {:?}, {:?})",
            webgl_version, requested_size, attributes
        );

        // Creating a new GLContext may make the current bound context_id dirty.
        // Clear it to ensure that  make_current() is called in subsequent commands.
        self.bound_context_id = None;
        // Partii (perf stream, diagnostic): PARTII_WEBGL_STATS=1 logs how long each step of context creation takes.
        let mut stats = webgl_stats_enabled().then(|| (std::time::Instant::now(), Vec::<(&'static str, f64)>::new()));
        let mark = |stats: &mut Option<(std::time::Instant, Vec<(&'static str, f64)>)>, step: &'static str| {
            if let Some((t, steps)) = stats.as_mut() {
                steps.push((step, t.elapsed().as_secs_f64() * 1000.0));
                *t = std::time::Instant::now();
            }
        };

        // This can happen if the Webview was dropped while one of its ScriptThreads
        // is still issuing asynchronous commands to the WebGL thread.
        let Some(painter_surfman_details) = self.painter_surfman_details_map.get(painter_id) else {
            return Err(format!(
                "PainterSurfmanDetails not found for {painter_id:?}"
            ));
        };

        let api_type = match painter_surfman_details.connection.gl_api() {
            surfman::GLApi::GL => GlType::Gl,
            surfman::GLApi::GLES => GlType::Gles,
        };

        let requested_flags =
            attributes.to_surfman_context_attribute_flags(webgl_version, api_type);
        // Some GL implementations seem to only allow famebuffers
        // to have alpha, depth and stencil if their creating context does.
        // WebGL requires all contexts to be able to create framebuffers with
        // alpha, depth and stencil. So we always create a context with them,
        // and fake not having them if requested.
        let flags = requested_flags |
            ContextAttributeFlags::ALPHA |
            ContextAttributeFlags::DEPTH |
            ContextAttributeFlags::STENCIL;
        let context_attributes = &ContextAttributes {
            version: webgl_version.to_surfman_version(api_type),
            flags,
        };

        let device = self.get_or_create_device_for_painter(painter_id)?;
        mark(&mut stats, "device");
        let context_descriptor = device
            .create_context_descriptor(context_attributes)
            .map_err(|err| format!("Failed to create context descriptor: {:?}", err))?;

        let safe_size = Size2D::new(
            requested_size.width.min(SAFE_VIEWPORT_DIMS[0]).max(1),
            requested_size.height.min(SAFE_VIEWPORT_DIMS[1]).max(1),
        );
        let surface_type = SurfaceType::Generic {
            size: safe_size.to_i32(),
        };
        let surface_access = self.surface_access();

        let mut ctx = device
            .create_context(&context_descriptor, None)
            .map_err(|err| format!("Failed to create the GL context: {:?}", err))?;
        mark(&mut stats, "create_context");
        let surface = device
            .create_surface(&ctx, surface_access, surface_type)
            .map_err(|err| format!("Failed to create the initial surface: {:?}", err))?;
        device
            .bind_surface_to_context(&mut ctx, surface)
            .map_err(|err| format!("Failed to bind initial surface: {:?}", err))?;
        // https://github.com/pcwalton/surfman/issues/7
        device
            .make_context_current(&ctx)
            .map_err(|err| format!("Failed to make new context current: {:?}", err))?;
        mark(&mut stats, "surface+bind+current");

        let context_id = WebGLContextId(
            self.external_image_id_manager
                .next_id(WebRenderImageHandlerType::WebGl)
                .0,
        );

        self.webrender_swap_chains
            .create_attached_swap_chain(context_id, &*device, &mut ctx, surface_access)
            .map_err(|err| format!("Failed to create swap chain: {:?}", err))?;
        mark(&mut stats, "swap_chain");

        let Some(swap_chain) = self.webrender_swap_chains.get(context_id) else {
            return Err("Failed to get the swap chain".into());
        };

        debug!(
            "Created webgl context {:?}/{:?}",
            context_id,
            device.context_id(&ctx),
        );

        let gl = unsafe {
            Rc::new(match api_type {
                GlType::Gl => glow::Context::from_loader_function(|symbol_name| {
                    device.get_proc_address(&ctx, symbol_name)
                }),
                GlType::Gles => glow::Context::from_loader_function(|symbol_name| {
                    device.get_proc_address(&ctx, symbol_name)
                }),
            })
        };

        mark(&mut stats, "gl_loader");
        let limits = GLLimits::detect(&gl, webgl_version);
        mark(&mut stats, "limits");

        let size = clamp_viewport(&gl, requested_size);
        debug_assert_eq!(unsafe { gl.get_error() }, gl::NO_ERROR);

        if safe_size != size {
            debug!("Resizing swap chain from {:?} to {:?}", safe_size, size);
            swap_chain
                .resize(&device, &mut ctx, size.to_i32())
                .map_err(|err| format!("Failed to resize swap chain: {:?}", err))?;
        }

        let descriptor = device.context_descriptor(&ctx);
        let descriptor_attributes = device.context_descriptor_attributes(&descriptor);
        let gl_version = descriptor_attributes.version;
        let has_alpha = requested_flags.contains(ContextAttributeFlags::ALPHA);

        device.make_context_current(&ctx).unwrap();
        let framebuffer = device
            .context_surface_info(&ctx)
            .map_err(|err| format!("Failed to get context surface info: {:?}", err))?
            .ok_or_else(|| "Failed to get context surface info".to_string())?
            .framebuffer_object;

        unsafe {
            gl.bind_framebuffer(gl::FRAMEBUFFER, framebuffer);
            gl.viewport(0, 0, size.width as i32, size.height as i32);
            gl.scissor(0, 0, size.width as i32, size.height as i32);
            gl.clear_color(0., 0., 0., !has_alpha as u32 as f32);
            gl.clear_depth(1.);
            gl.clear_stencil(0);
            gl.clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT | gl::STENCIL_BUFFER_BIT);
            gl.clear_color(0., 0., 0., 0.);
            debug_assert_eq!(gl.get_error(), gl::NO_ERROR);
        }

        let default_vao = if let Some(vao) = WebGLImpl::create_vertex_array(&gl) {
            unsafe { gl.bind_vertex_array(Some(vao.glow())) }
            Some(vao.glow())
        } else {
            None
        };
        debug_assert_eq!(unsafe { gl.get_error() }, gl::NO_ERROR);

        // WebGL 2.0 §5.18: primitive restart with the fixed index (the type's maximum value) is
        // always on, and the enum is not a valid capability for content. Desktop GL has it from
        // 4.3 or ARB_ES3_compatibility (Mesa exposes it on any core version); GLES 3 has it off.
        if webgl_version == WebGLVersion::WebGL2 {
            unsafe {
                gl.enable(gl::PRIMITIVE_RESTART_FIXED_INDEX);
                let error = gl.get_error();
                if error != gl::NO_ERROR {
                    warn!("PRIMITIVE_RESTART_FIXED_INDEX is unsupported ({error:#x})");
                }
            }
        }
        // GLES (and so WebGL) always encodes writes, blends, clears and blits into sRGB attachments;
        // desktop GL only does with FRAMEBUFFER_SRGB on. It has no effect on linear attachments,
        // so the default framebuffer is unchanged.
        if api_type == GlType::Gl {
            unsafe {
                gl.enable(gl::FRAMEBUFFER_SRGB);
                let error = gl.get_error();
                if error != gl::NO_ERROR {
                    warn!("FRAMEBUFFER_SRGB is unsupported ({error:#x})");
                }
            }
        }

        let state = GLState {
            _gl_version: gl_version,
            _webgl_version: webgl_version,
            requested_flags,
            default_vao,
            emulate_luminance: webgl_version == WebGLVersion::WebGL2 && api_type == GlType::Gl,
            ..Default::default()
        };
        debug!("Created state {:?}", state);

        state.restore_invariant(&gl);
        debug_assert_eq!(unsafe { gl.get_error() }, gl::NO_ERROR);
        mark(&mut stats, "resize+clear+state");
        if let Some((_, steps)) = &stats {
            let line: Vec<String> = steps.iter().map(|(k, v)| format!("{k}={v:.2}")).collect();
            eprintln!("partii-webgl-stats: create {webgl_version:?} {}x{} {}", size.width, size.height, line.join(" "));
        }

        self.contexts.insert(
            context_id,
            GLContextData {
                ctx,
                device,
                gl,
                state,
                attributes,
                marked_for_deletion: false,
            },
        );

        self.cached_context_info.insert(
            context_id,
            WebGLContextInfo {
                image_key: None,
                size: size.to_i32(),
                alpha: has_alpha,
            },
        );

        Ok((context_id, limits))
    }

    /// Resizes a WebGLContext
    fn resize_webgl_context(
        &mut self,
        context_id: WebGLContextId,
        requested_size: Size2D<u32>,
    ) -> Result<(), String> {
        self.make_current_if_needed(context_id);

        let Some(data) = self.contexts.get_mut(&context_id) else {
            return Err("Missing WebGL context!".into());
        };

        let size = clamp_viewport(&data.gl, requested_size);
        debug_assert_eq!(unsafe { data.gl.get_error() }, gl::NO_ERROR);

        // Check to see if any of the current framebuffer bindings are the surface we're about to
        // throw out. If so, we'll have to reset them after destroying the surface.
        let framebuffer_rebinding_info =
            FramebufferRebindingInfo::detect(&data.device, &data.ctx, &data.gl);

        // Resize the swap chains
        if let Some(swap_chain) = self.webrender_swap_chains.get(context_id) {
            let alpha = data
                .state
                .requested_flags
                .contains(ContextAttributeFlags::ALPHA);
            let clear_color = [0.0, 0.0, 0.0, !alpha as i32 as f32];
            swap_chain
                .resize(&data.device, &mut data.ctx, size.to_i32())
                .map_err(|err| format!("Failed to resize swap chain: {:?}", err))?;
            swap_chain
                .clear_surface(&data.device, &mut data.ctx, &data.gl, clear_color)
                .map_err(|err| format!("Failed to clear resized swap chain: {:?}", err))?;
        } else {
            error!("Failed to find swap chain");
        }

        // Reset framebuffer bindings as appropriate.
        framebuffer_rebinding_info.apply(&data.device, &data.ctx, &data.gl);
        debug_assert_eq!(unsafe { data.gl.get_error() }, gl::NO_ERROR);

        let has_alpha = data
            .state
            .requested_flags
            .contains(ContextAttributeFlags::ALPHA);
        self.update_webrender_image_for_context(context_id, size.to_i32(), has_alpha, None);

        Ok(())
    }

    /// Note that rendering has finished in WebRender for this context. If the context
    /// is marked for deletion, it will now be deleted.
    fn handle_finished_rendering_to_context(&mut self, context_id: WebGLContextId) {
        let marked_for_deletion = self
            .contexts
            .get(&context_id)
            .is_some_and(|context_data| context_data.marked_for_deletion);
        if marked_for_deletion {
            self.remove_webgl_context(context_id);
        }
    }

    /// Removes a WebGLContext and releases attached resources.
    fn remove_webgl_context(&mut self, context_id: WebGLContextId) {
        {
            let mut busy_webgl_context_map = self.busy_webgl_context_map.write();
            let entry = busy_webgl_context_map.entry(context_id);
            match entry {
                Entry::Vacant(..) => {},
                Entry::Occupied(occupied_entry) if *occupied_entry.get() > 0 => {
                    // WebRender is in the process of rendering this WebGL context, so wait until it
                    // finishes in order to release it.
                    if let Some(context_data) = self.contexts.get_mut(&context_id) {
                        context_data.marked_for_deletion = true;
                    }
                    return;
                },
                Entry::Occupied(occupied_entry) => {
                    occupied_entry.remove();
                },
            }
        }

        // Release webrender image keys.
        if let Some(image_key) = self
            .cached_context_info
            .remove(&context_id)
            .and_then(|info| info.image_key)
        {
            self.paint_api.delete_image(image_key);
        }

        if !self.contexts.contains_key(&context_id) {
            return;
        };

        // We need to make the context current so its resources can be disposed of.
        self.make_current_if_needed(context_id);

        // Destroy WebXR layers associated with this context
        #[cfg(feature = "webxr")]
        {
            // We must temporarily take the WebXRBridge, as we are passing self to a
            // method on the bridge.
            let mut webxr_bridge = self.webxr_bridge.take();
            if let Some(webxr_bridge) = &mut webxr_bridge {
                webxr_bridge.destroy_all_layers(self, context_id.into());
            }
            self.webxr_bridge = webxr_bridge;
        }

        // Release GL context.
        let Some(mut data) = self.contexts.remove(&context_id) else {
            return;
        };

        // Destroy the swap chains
        self.webrender_swap_chains
            .destroy(context_id, &data.device, &mut data.ctx)
            .unwrap();

        // Destroy the context
        data.device.destroy_context(&mut data.ctx).unwrap();

        // Removing a GLContext may make the current bound context_id dirty.
        self.bound_context_id = None;
    }

    fn handle_swap_buffers(
        &mut self,
        canvas_epoch: Option<Epoch>,
        context_ids: Vec<WebGLContextId>,
        _sent_time: u64,
    ) {
        debug!("handle_swap_buffers()");
        for context_id in context_ids {
            self.make_current_if_needed(context_id)
                .expect("Where's the GL data?");

            let data = self
                .contexts
                .get_mut(&context_id)
                .expect("Missing WebGL context");

            // Ensure there are no pending GL errors from other parts of the pipeline.
            debug_assert_eq!(unsafe { data.gl.get_error() }, gl::NO_ERROR);

            // Partii (perf, measurement builds): PARTII_FRAME_PROF=finish waits for the frame's
            // rasterization here, so it shows apart from the swap.
            if frame_prof::finish() {
                let start = std::time::Instant::now();
                unsafe { data.gl.finish() };
                frame_prof::add_raster(start.elapsed());
            }

            // Check to see if any of the current framebuffer bindings are the surface we're about
            // to swap out. If so, we'll have to reset them after destroying the surface.
            let framebuffer_rebinding_info =
                FramebufferRebindingInfo::detect(&data.device, &data.ctx, &data.gl);
            debug_assert_eq!(unsafe { data.gl.get_error() }, gl::NO_ERROR);

            debug!("Getting swap chain for {:?}", context_id);
            let swap_chain = self
                .webrender_swap_chains
                .get(context_id)
                .expect("Where's the swap chain?");

            debug!("Swapping {:?}", context_id);
            swap_chain
                .swap_buffers(
                    &data.device,
                    &mut data.ctx,
                    if data.attributes.preserve_drawing_buffer {
                        PreserveBuffer::Yes(&data.gl)
                    } else {
                        PreserveBuffer::No
                    },
                )
                .unwrap();
            debug_assert_eq!(unsafe { data.gl.get_error() }, gl::NO_ERROR);

            if !data.attributes.preserve_drawing_buffer {
                debug!("Clearing {:?}", context_id);
                let alpha = data
                    .state
                    .requested_flags
                    .contains(ContextAttributeFlags::ALPHA);
                let clear_color = [0.0, 0.0, 0.0, !alpha as i32 as f32];
                swap_chain
                    .clear_surface(&data.device, &mut data.ctx, &data.gl, clear_color)
                    .unwrap();
                debug_assert_eq!(unsafe { data.gl.get_error() }, gl::NO_ERROR);
            }

            // Rebind framebuffers as appropriate.
            debug!("Rebinding {:?}", context_id);
            framebuffer_rebinding_info.apply(&data.device, &data.ctx, &data.gl);
            debug_assert_eq!(unsafe { data.gl.get_error() }, gl::NO_ERROR);

            let SurfaceInfo {
                size,
                framebuffer_object,
                id,
                ..
            } = data
                .device
                .context_surface_info(&data.ctx)
                .unwrap()
                .unwrap();
            debug!(
                "... rebound framebuffer {:?}, new back buffer surface is {:?}",
                framebuffer_object, id
            );

            let has_alpha = data
                .state
                .requested_flags
                .contains(ContextAttributeFlags::ALPHA);
            self.update_webrender_image_for_context(context_id, size, has_alpha, canvas_epoch);
        }
    }

    /// Which access mode to use
    fn surface_access(&self) -> SurfaceAccess {
        SurfaceAccess::GPUOnly
    }

    /// Gets a reference to a Context for a given WebGLContextId and makes it current if required.
    pub(crate) fn make_current_if_needed(
        &mut self,
        context_id: WebGLContextId,
    ) -> Option<&GLContextData> {
        let data = self.contexts.get(&context_id);

        if let Some(data) = data &&
            Some(context_id) != self.bound_context_id
        {
            data.device.make_context_current(&data.ctx).unwrap();
            self.bound_context_id = Some(context_id);
        }

        data
    }

    /// Gets a mutable reference to a GLContextWrapper for a WebGLContextId and makes it current if required.
    pub(crate) fn make_current_if_needed_mut(
        &mut self,
        context_id: WebGLContextId,
    ) -> Option<&mut GLContextData> {
        let data = self.contexts.get_mut(&context_id);
        if let Some(ref data) = data &&
            Some(context_id) != self.bound_context_id
        {
            data.device.make_context_current(&data.ctx).unwrap();
            self.bound_context_id = Some(context_id);
        }

        data
    }

    /// Tell WebRender to invalidate any cached tiles for a given `WebGLContextId`
    /// when the underlying surface has changed e.g due to resize or buffer swap
    fn update_webrender_image_for_context(
        &mut self,
        context_id: WebGLContextId,
        size: Size2D<i32>,
        has_alpha: bool,
        canvas_epoch: Option<Epoch>,
    ) {
        let image_data = self.external_image_data(context_id);
        let info = self.cached_context_info.get_mut(&context_id).unwrap();
        info.size = size;
        info.alpha = has_alpha;

        if let Some(image_key) = info.image_key {
            self.paint_api.update_image(
                image_key,
                info.image_descriptor(),
                image_data,
                canvas_epoch,
            );
        }
    }

    /// Helper function to create a `ImageData::External` instance.
    fn external_image_data(&self, context_id: WebGLContextId) -> SerializableImageData {
        // TODO(pcwalton): Add `GL_TEXTURE_EXTERNAL_OES`?
        let device = self.device_for_context(context_id);
        let image_buffer_kind = match device.surface_gl_texture_target() {
            gl::TEXTURE_RECTANGLE => ImageBufferKind::TextureRect,
            _ => ImageBufferKind::Texture2D,
        };

        let data = ExternalImageData {
            id: ExternalImageId(context_id.0),
            channel_index: 0,
            image_type: ExternalImageType::TextureHandle(image_buffer_kind),
            normalized_uvs: false,
        };
        SerializableImageData::External(data)
    }

    /// Gets the GLSL Version supported by a GLContext.
    fn get_glsl_version(gl: &Gl) -> WebGLSLVersion {
        let version = unsafe { gl.get_parameter_string(gl::SHADING_LANGUAGE_VERSION) };
        // Fomat used by SHADING_LANGUAGE_VERSION query : major.minor[.release] [vendor info]
        let mut values = version.split(&['.', ' '][..]);
        let major = values
            .next()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(1);
        let minor = values
            .next()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(20);

        WebGLSLVersion { major, minor }
    }

    fn handle_set_image_key(&mut self, context_id: WebGLContextId, image_key: ImageKey) {
        let external_image_data = self.external_image_data(context_id);
        let Some(info) = self.cached_context_info.get_mut(&context_id) else {
            self.paint_api.delete_image(image_key);
            return;
        };

        if let Some(old_image_key) = info.image_key.replace(image_key) {
            self.paint_api.delete_image(old_image_key);
            return;
        }

        self.paint_api.add_image(
            image_key,
            info.image_descriptor(),
            external_image_data,
            false,
        );
    }
}

/// Helper struct to store cached WebGLContext information.
struct WebGLContextInfo {
    image_key: Option<ImageKey>,
    size: Size2D<i32>,
    alpha: bool,
}

impl WebGLContextInfo {
    /// Helper function to create a `ImageDescriptor`.
    fn image_descriptor(&self) -> ImageDescriptor {
        let mut flags = ImageDescriptorFlags::empty();
        flags.set(ImageDescriptorFlags::IS_OPAQUE, !self.alpha);
        ImageDescriptor {
            size: DeviceIntSize::new(self.size.width, self.size.height),
            stride: None,
            format: ImageFormat::BGRA8,
            offset: 0,
            flags,
        }
    }
}

/// WebGL Commands Implementation
pub struct WebGLImpl;

impl WebGLImpl {
    pub fn apply(
        device: &Device,
        ctx: &Context,
        gl: &Gl,
        state: &mut GLState,
        attributes: &GLContextAttributes,
        command: WebGLCommand,
        _backtrace: WebGLCommandBacktrace,
    ) {
        // Ensure there are no pending GL errors from other parts of the pipeline.
        debug_assert_eq!(unsafe { gl.get_error() }, gl::NO_ERROR);

        if state.draw_writes_nothing_under_query() &&
            matches!(
                command,
                WebGLCommand::DrawArrays { .. } |
                    WebGLCommand::DrawArraysInstanced { .. } |
                    WebGLCommand::DrawElements { .. } |
                    WebGLCommand::DrawElementsInstanced { .. } |
                    WebGLCommand::DrawArraysInstancedBaseInstance { .. } |
                    WebGLCommand::DrawElementsInstancedBaseVertexBaseInstance { .. }
            )
        {
            return;
        }

        match command {
            WebGLCommand::GetContextAttributes(ref sender) => sender.send(*attributes).unwrap(),
            WebGLCommand::ActiveTexture(target) => unsafe { gl.active_texture(target) },
            WebGLCommand::AttachShader(program_id, shader_id) => unsafe {
                gl.attach_shader(program_id.glow(), shader_id.glow())
            },
            WebGLCommand::DetachShader(program_id, shader_id) => unsafe {
                gl.detach_shader(program_id.glow(), shader_id.glow())
            },
            WebGLCommand::BindAttribLocation(program_id, index, ref name) => unsafe {
                gl.bind_attrib_location(program_id.glow(), index, &to_name_in_compiled_shader(name))
            },
            WebGLCommand::BlendColor(r, g, b, a) => unsafe { gl.blend_color(r, g, b, a) },
            WebGLCommand::BlendEquation(mode) => unsafe { gl.blend_equation(mode) },
            WebGLCommand::BlendEquationSeparate(mode_rgb, mode_alpha) => unsafe {
                gl.blend_equation_separate(mode_rgb, mode_alpha)
            },
            WebGLCommand::BlendFunc(src, dest) => unsafe { gl.blend_func(src, dest) },
            WebGLCommand::BlendFuncSeparate(src_rgb, dest_rgb, src_alpha, dest_alpha) => unsafe {
                gl.blend_func_separate(src_rgb, dest_rgb, src_alpha, dest_alpha)
            },
            WebGLCommand::BufferData(buffer_type, ref receiver, usage) => unsafe {
                gl.buffer_data_u8_slice(buffer_type, &receiver.recv().unwrap(), usage)
            },
            WebGLCommand::BufferSubData(buffer_type, offset, ref receiver) => unsafe {
                gl.buffer_sub_data_u8_slice(buffer_type, offset as i32, &receiver.recv().unwrap())
            },
            WebGLCommand::CopyBufferSubData(src, dst, src_offset, dst_offset, size) => {
                unsafe {
                    gl.copy_buffer_sub_data(
                        src,
                        dst,
                        src_offset as i32,
                        dst_offset as i32,
                        size as i32,
                    )
                };
            },
            WebGLCommand::GetBufferSubData(buffer_type, offset, length, ref sender) => unsafe {
                let ptr = gl.map_buffer_range(
                    buffer_type,
                    offset as i32,
                    length as i32,
                    gl::MAP_READ_BIT,
                );
                let data: &[u8] = slice::from_raw_parts(ptr as _, length);
                let buffer = GenericSharedMemory::from_bytes(data);
                sender.send(buffer).unwrap();
                gl.unmap_buffer(buffer_type);
            },
            WebGLCommand::Clear(mask) => {
                unsafe { gl.clear(mask) };
            },
            WebGLCommand::ClearColor(r, g, b, a) => {
                state.clear_color = (r, g, b, a);
                unsafe { gl.clear_color(r, g, b, a) };
            },
            WebGLCommand::ClearDepth(depth) => {
                let value = depth.clamp(0., 1.) as f64;
                state.depth_clear_value = value;
                unsafe { gl.clear_depth(value) }
            },
            WebGLCommand::ClearStencil(stencil) => {
                state.stencil_clear_value = stencil;
                unsafe { gl.clear_stencil(stencil) };
            },
            WebGLCommand::ColorMask(r, g, b, a) => {
                state.color_write_mask = [r, g, b, a];
                state.restore_alpha_invariant(gl);
            },
            WebGLCommand::CopyTexImage2D {
                target,
                level,
                internal_format,
                x,
                y,
                width,
                height,
                read_size,
            } => unsafe {
                copy_tex_image_2d(
                    state,
                    gl,
                    target,
                    level,
                    internal_format,
                    (x, y, width, height),
                    read_size,
                )
            },
            WebGLCommand::CopyTexSubImage2D(
                target,
                level,
                xoffset,
                yoffset,
                x,
                y,
                width,
                height,
            ) => unsafe {
                match luminance_copy_kind(state, gl, target) {
                    Some(kind) => copy_luminance_via_cpu(
                        gl,
                        target,
                        level,
                        (xoffset, yoffset, None),
                        (x, y, width, height),
                        kind,
                    ),
                    None => gl.copy_tex_sub_image_2d(
                        target, level, xoffset, yoffset, x, y, width, height,
                    ),
                }
            },
            WebGLCommand::CullFace(mode) => unsafe { gl.cull_face(mode) },
            WebGLCommand::DepthFunc(func) => unsafe { gl.depth_func(func) },
            WebGLCommand::DepthMask(flag) => {
                state.depth_write_mask = flag;
                state.restore_depth_invariant(gl);
            },
            WebGLCommand::DepthRange(near, far) => unsafe {
                gl.depth_range(near.clamp(0., 1.) as f64, far.clamp(0., 1.) as f64)
            },
            WebGLCommand::Disable(cap) => match cap {
                gl::SCISSOR_TEST => {
                    state.scissor_test_enabled = false;
                    state.restore_scissor_invariant(gl);
                },
                gl::DEPTH_TEST => {
                    state.depth_test_enabled = false;
                    state.restore_depth_invariant(gl);
                },
                gl::STENCIL_TEST => {
                    state.stencil_test_enabled = false;
                    state.restore_stencil_invariant(gl);
                },
                _ => unsafe { gl.disable(cap) },
            },
            WebGLCommand::Enable(cap) => match cap {
                gl::SCISSOR_TEST => {
                    state.scissor_test_enabled = true;
                    state.restore_scissor_invariant(gl);
                },
                gl::DEPTH_TEST => {
                    state.depth_test_enabled = true;
                    state.restore_depth_invariant(gl);
                },
                gl::STENCIL_TEST => {
                    state.stencil_test_enabled = true;
                    state.restore_stencil_invariant(gl);
                },
                _ => unsafe { gl.enable(cap) },
            },
            WebGLCommand::FramebufferRenderbuffer(target, attachment, renderbuffertarget, rb) => {
                let attach = |attachment| unsafe {
                    gl.framebuffer_renderbuffer(
                        target,
                        attachment,
                        renderbuffertarget,
                        rb.map(WebGLRenderbufferId::glow),
                    )
                };
                if attachment == gl::DEPTH_STENCIL_ATTACHMENT {
                    attach(gl::DEPTH_ATTACHMENT);
                    attach(gl::STENCIL_ATTACHMENT);
                } else {
                    attach(attachment);
                }
            },
            WebGLCommand::FramebufferTexture2D(target, attachment, textarget, texture, level) => {
                let attach = |attachment| unsafe {
                    gl.framebuffer_texture_2d(
                        target,
                        attachment,
                        textarget,
                        texture.map(WebGLTextureId::glow),
                        level,
                    )
                };
                if attachment == gl::DEPTH_STENCIL_ATTACHMENT {
                    attach(gl::DEPTH_ATTACHMENT);
                    attach(gl::STENCIL_ATTACHMENT);
                } else {
                    attach(attachment)
                }
            },
            WebGLCommand::FrontFace(mode) => unsafe { gl.front_face(mode) },
            WebGLCommand::DisableVertexAttribArray(attrib_id) => unsafe {
                gl.disable_vertex_attrib_array(attrib_id)
            },
            WebGLCommand::EnableVertexAttribArray(attrib_id) => unsafe {
                gl.enable_vertex_attrib_array(attrib_id)
            },
            WebGLCommand::Hint(name, val) => unsafe { gl.hint(name, val) },
            WebGLCommand::LineWidth(width) => {
                unsafe { gl.line_width(width) };
                // In OpenGL Core Profile >3.2, any non-1.0 value will generate INVALID_VALUE.
                if width != 1.0 {
                    let _ = unsafe { gl.get_error() };
                }
            },
            WebGLCommand::PixelStorei(name, val) => unsafe { gl.pixel_store_i32(name, val) },
            WebGLCommand::PolygonOffset(factor, units) => unsafe {
                gl.polygon_offset(factor, units)
            },
            WebGLCommand::ReadPixels(rect, format, pixel_type, ref sender) => {
                let len = bytes_per_type(pixel_type) *
                    read_components(format) *
                    rect.size.area() as usize;
                let mut pixels = vec![0; len];
                unsafe {
                    // We don't want any alignment padding on pixel rows.
                    gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
                    gl.read_pixels(
                        rect.origin.x as i32,
                        rect.origin.y as i32,
                        rect.size.width as i32,
                        rect.size.height as i32,
                        format,
                        pixel_type,
                        glow::PixelPackData::Slice(Some(&mut pixels)),
                    )
                };
                let alpha_mode = match (attributes.alpha, attributes.premultiplied_alpha) {
                    (true, premultiplied) => SnapshotAlphaMode::Transparent { premultiplied },
                    (false, _) => SnapshotAlphaMode::Opaque,
                };
                sender
                    .send((GenericSharedMemory::from_vec(pixels), alpha_mode))
                    .unwrap();
            },
            WebGLCommand::ReadPixelsPP(rect, format, pixel_type, offset) => unsafe {
                gl.read_pixels(
                    rect.origin.x,
                    rect.origin.y,
                    rect.size.width,
                    rect.size.height,
                    format,
                    pixel_type,
                    glow::PixelPackData::BufferOffset(offset as u32),
                );
            },
            WebGLCommand::RenderbufferStorage(target, format, width, height) => unsafe {
                gl.renderbuffer_storage(target, format, width, height)
            },
            WebGLCommand::RenderbufferStorageMultisample(
                target,
                samples,
                format,
                width,
                height,
            ) => unsafe {
                gl.renderbuffer_storage_multisample(target, samples, format, width, height)
            },
            WebGLCommand::SampleCoverage(value, invert) => unsafe {
                gl.sample_coverage(value, invert)
            },
            WebGLCommand::Scissor(x, y, width, height) => {
                // FIXME(nox): Kinda unfortunate that some u32 values could
                // end up as negative numbers here, but I don't even think
                // that can happen in the real world.
                unsafe { gl.scissor(x, y, width as i32, height as i32) };
                state.scissor_empty = width == 0 || height == 0;
            },
            WebGLCommand::StencilFunc(func, ref_, mask) => {
                state.stencil_value_mask = (mask, mask);
                unsafe { gl.stencil_func(func, ref_, mask) }
            },
            WebGLCommand::StencilFuncSeparate(face, func, ref_, mask) => {
                if face != gl::BACK {
                    state.stencil_value_mask.0 = mask;
                }
                if face != gl::FRONT {
                    state.stencil_value_mask.1 = mask;
                }
                unsafe { gl.stencil_func_separate(face, func, ref_, mask) }
            },
            WebGLCommand::StencilMask(mask) => {
                state.stencil_write_mask = (mask, mask);
                state.restore_stencil_invariant(gl);
            },
            WebGLCommand::StencilMaskSeparate(face, mask) => {
                match face {
                    gl::FRONT => state.stencil_write_mask.0 = mask,
                    gl::BACK => state.stencil_write_mask.1 = mask,
                    gl::FRONT_AND_BACK => state.stencil_write_mask = (mask, mask),
                    _ => debug_assert!(
                        false,
                        "stencilMaskSeparate face {face:#x} should have been rejected by the DOM"
                    ),
                }
                state.restore_stencil_invariant(gl);
            },
            WebGLCommand::StencilOp(fail, zfail, zpass) => unsafe {
                gl.stencil_op(fail, zfail, zpass)
            },
            WebGLCommand::StencilOpSeparate(face, fail, zfail, zpass) => unsafe {
                gl.stencil_op_separate(face, fail, zfail, zpass)
            },
            WebGLCommand::GetRenderbufferParameter(target, pname, ref chan) => {
                Self::get_renderbuffer_parameter(gl, target, pname, chan)
            },
            WebGLCommand::CreateTransformFeedback(ref sender) => {
                let value = unsafe { gl.create_transform_feedback() }.ok();
                sender
                    .send(value.map(|ntf| ntf.0.get()).unwrap_or_default())
                    .unwrap()
            },
            WebGLCommand::DeleteTransformFeedback(id) => {
                if let Some(tf) = NonZeroU32::new(id) {
                    unsafe { gl.delete_transform_feedback(NativeTransformFeedback(tf)) };
                }
            },
            WebGLCommand::IsTransformFeedback(id, ref sender) => {
                let value = NonZeroU32::new(id)
                    .map(|id| unsafe { gl.is_transform_feedback(NativeTransformFeedback(id)) })
                    .unwrap_or_default();
                sender.send(value).unwrap()
            },
            WebGLCommand::BindTransformFeedback(target, id) => {
                unsafe {
                    gl.bind_transform_feedback(
                        target,
                        NonZeroU32::new(id).map(NativeTransformFeedback),
                    )
                };
            },
            WebGLCommand::BeginTransformFeedback(mode) => {
                unsafe { gl.begin_transform_feedback(mode) };
                state.transform_feedback_active = true;
            },
            WebGLCommand::EndTransformFeedback() => {
                unsafe { gl.end_transform_feedback() };
                state.transform_feedback_active = false;
            },
            WebGLCommand::PauseTransformFeedback() => {
                unsafe { gl.pause_transform_feedback() };
            },
            WebGLCommand::ResumeTransformFeedback() => {
                unsafe { gl.resume_transform_feedback() };
            },
            WebGLCommand::GetTransformFeedbackVarying(program, index, ref sender) => {
                let ActiveTransformFeedback { size, tftype, name } =
                    unsafe { gl.get_transform_feedback_varying(program.glow(), index) }.unwrap();
                // We need to split, because the name starts with '_u' prefix.
                let name = from_name_in_compiled_shader(&name);
                sender.send((size, tftype, name)).unwrap();
            },
            WebGLCommand::TransformFeedbackVaryings(program, ref varyings, buffer_mode) => {
                let varyings: Vec<String> = varyings
                    .iter()
                    .map(|varying| to_name_in_compiled_shader(varying))
                    .collect();
                let varyings_refs: Vec<&str> = varyings.iter().map(String::as_ref).collect();
                unsafe {
                    gl.transform_feedback_varyings(
                        program.glow(),
                        varyings_refs.as_slice(),
                        buffer_mode,
                    )
                };
            },
            WebGLCommand::GetFramebufferAttachmentParameter(
                target,
                attachment,
                pname,
                ref chan,
            ) => Self::get_framebuffer_attachment_parameter(gl, target, attachment, pname, chan),
            WebGLCommand::GetShaderPrecisionFormat(shader_type, precision_type, ref chan) => {
                Self::shader_precision_format(gl, shader_type, precision_type, chan)
            },
            WebGLCommand::GetExtensions(ref chan) => Self::get_extensions(gl, chan),
            WebGLCommand::GetFragDataLocation(program_id, ref name, ref sender) => {
                let location = unsafe {
                    gl.get_frag_data_location(program_id.glow(), &to_name_in_compiled_shader(name))
                };
                sender.send(location).unwrap();
            },
            WebGLCommand::GetUniformLocation(program_id, ref name, ref chan) => {
                Self::uniform_location(gl, program_id, name, chan)
            },
            WebGLCommand::GetShaderInfoLog(shader_id, ref chan) => {
                Self::shader_info_log(gl, shader_id, chan)
            },
            WebGLCommand::GetProgramInfoLog(program_id, ref chan) => {
                Self::program_info_log(gl, program_id, chan)
            },
            WebGLCommand::CompileShader(shader_id, ref source, ref translation, ref sender) => {
                let info = Self::translate_shader(gl, source, translation);
                if let Some(ref object_code) = info.object_code {
                    Self::compile_shader(gl, shader_id, object_code);
                }
                let _ = sender.send(info);
            },
            WebGLCommand::CreateBuffer(ref chan) => Self::create_buffer(gl, chan),
            WebGLCommand::CreateFramebuffer(ref chan) => Self::create_framebuffer(gl, chan),
            WebGLCommand::CreateRenderbuffer(ref chan) => Self::create_renderbuffer(gl, chan),
            WebGLCommand::CreateTexture(ref chan) => Self::create_texture(gl, chan),
            WebGLCommand::GenObjectNames(kind, count, ref chan) => {
                // The same glGen* calls the Create* commands make, `count` at a time; a name that fails
                // ends the batch (the script thread falls back to Create* when it gets none).
                let mut names = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    let name = unsafe {
                        match kind {
                            WebGLObjectNameKind::Buffer => gl.create_buffer().map(|n| n.0.get()),
                            WebGLObjectNameKind::Framebuffer => gl.create_framebuffer().map(|n| n.0.get()),
                            WebGLObjectNameKind::Renderbuffer => {
                                gl.create_renderbuffer().map(|n| n.0.get())
                            },
                            WebGLObjectNameKind::Texture => gl.create_texture().map(|n| n.0.get()),
                            WebGLObjectNameKind::VertexArray => {
                                gl.create_vertex_array().map(|n| n.0.get())
                            },
                        }
                    };
                    match name {
                        Ok(name) => names.push(name),
                        Err(_) => break,
                    }
                }
                let _ = chan.send(names);
            },
            WebGLCommand::CreateProgram(ref chan) => Self::create_program(gl, chan),
            WebGLCommand::CreateShader(shader_type, ref chan) => {
                Self::create_shader(gl, shader_type, chan)
            },
            WebGLCommand::DeleteBuffer(id) => unsafe { gl.delete_buffer(id.glow()) },
            WebGLCommand::DeleteFramebuffer(id) => unsafe { gl.delete_framebuffer(id.glow()) },
            WebGLCommand::DeleteRenderbuffer(id) => unsafe { gl.delete_renderbuffer(id.glow()) },
            WebGLCommand::DeleteTexture(id) => unsafe { gl.delete_texture(id.glow()) },
            WebGLCommand::DeleteProgram(id) => unsafe { gl.delete_program(id.glow()) },
            WebGLCommand::DeleteShader(id) => unsafe { gl.delete_shader(id.glow()) },
            WebGLCommand::BindBuffer(target, id) => unsafe {
                gl.bind_buffer(target, id.map(WebGLBufferId::glow))
            },
            WebGLCommand::BindFramebuffer(target, request) => {
                Self::bind_framebuffer(gl, target, request, ctx, device, state)
            },
            WebGLCommand::BindRenderbuffer(target, id) => unsafe {
                gl.bind_renderbuffer(target, id.map(WebGLRenderbufferId::glow))
            },
            WebGLCommand::BindTexture(target, id) => unsafe {
                gl.bind_texture(target, id.map(WebGLTextureId::glow))
            },
            WebGLCommand::BindIncompleteTexture(target) => {
                Self::bind_incomplete_texture(gl, state, target)
            },
            WebGLCommand::BlitFrameBuffer(
                src_x0,
                src_y0,
                src_x1,
                src_y1,
                dst_x0,
                dst_y0,
                dst_x1,
                dst_y1,
                mask,
                filter,
            ) => unsafe {
                gl.blit_framebuffer(
                    src_x0, src_y0, src_x1, src_y1, dst_x0, dst_y0, dst_x1, dst_y1, mask, filter,
                );
                // With alpha: false the canvas is RGBA whose alpha is kept at 1 by the colour
                // mask, which a blit ignores: put the alpha back.
                if mask & gl::COLOR_BUFFER_BIT != 0 && state.fake_no_alpha() {
                    gl.disable(gl::SCISSOR_TEST);
                    gl.color_mask(false, false, false, true);
                    gl.clear_color(0., 0., 0., 1.);
                    gl.clear(gl::COLOR_BUFFER_BIT);
                    state.restore_clear_invariant(gl);
                    state.restore_scissor_invariant(gl);
                    state.restore_alpha_invariant(gl);
                }
            },
            WebGLCommand::Uniform1f(uniform_id, v) => unsafe {
                gl.uniform_1_f32(native_uniform_location(uniform_id).as_ref(), v)
            },
            WebGLCommand::Uniform1fv(uniform_id, ref v) => unsafe {
                gl.uniform_1_f32_slice(native_uniform_location(uniform_id).as_ref(), v)
            },
            WebGLCommand::Uniform1i(uniform_id, v) => unsafe {
                gl.uniform_1_i32(native_uniform_location(uniform_id).as_ref(), v)
            },
            WebGLCommand::Uniform1iv(uniform_id, ref v) => unsafe {
                gl.uniform_1_i32_slice(native_uniform_location(uniform_id).as_ref(), v)
            },
            WebGLCommand::Uniform1ui(uniform_id, v) => unsafe {
                gl.uniform_1_u32(native_uniform_location(uniform_id).as_ref(), v)
            },
            WebGLCommand::Uniform1uiv(uniform_id, ref v) => unsafe {
                gl.uniform_1_u32_slice(native_uniform_location(uniform_id).as_ref(), v)
            },
            WebGLCommand::Uniform2f(uniform_id, x, y) => unsafe {
                gl.uniform_2_f32(native_uniform_location(uniform_id).as_ref(), x, y)
            },
            WebGLCommand::Uniform2fv(uniform_id, ref v) => unsafe {
                gl.uniform_2_f32_slice(native_uniform_location(uniform_id).as_ref(), v)
            },
            WebGLCommand::Uniform2i(uniform_id, x, y) => unsafe {
                gl.uniform_2_i32(native_uniform_location(uniform_id).as_ref(), x, y)
            },
            WebGLCommand::Uniform2iv(uniform_id, ref v) => unsafe {
                gl.uniform_2_i32_slice(native_uniform_location(uniform_id).as_ref(), v)
            },
            WebGLCommand::Uniform2ui(uniform_id, x, y) => unsafe {
                gl.uniform_2_u32(native_uniform_location(uniform_id).as_ref(), x, y)
            },
            WebGLCommand::Uniform2uiv(uniform_id, ref v) => unsafe {
                gl.uniform_2_u32_slice(native_uniform_location(uniform_id).as_ref(), v)
            },
            WebGLCommand::Uniform3f(uniform_id, x, y, z) => unsafe {
                gl.uniform_3_f32(native_uniform_location(uniform_id).as_ref(), x, y, z)
            },
            WebGLCommand::Uniform3fv(uniform_id, ref v) => unsafe {
                gl.uniform_3_f32_slice(native_uniform_location(uniform_id).as_ref(), v)
            },
            WebGLCommand::Uniform3i(uniform_id, x, y, z) => unsafe {
                gl.uniform_3_i32(native_uniform_location(uniform_id).as_ref(), x, y, z)
            },
            WebGLCommand::Uniform3iv(uniform_id, ref v) => unsafe {
                gl.uniform_3_i32_slice(native_uniform_location(uniform_id).as_ref(), v)
            },
            WebGLCommand::Uniform3ui(uniform_id, x, y, z) => unsafe {
                gl.uniform_3_u32(native_uniform_location(uniform_id).as_ref(), x, y, z)
            },
            WebGLCommand::Uniform3uiv(uniform_id, ref v) => unsafe {
                gl.uniform_3_u32_slice(native_uniform_location(uniform_id).as_ref(), v)
            },
            WebGLCommand::Uniform4f(uniform_id, x, y, z, w) => unsafe {
                gl.uniform_4_f32(native_uniform_location(uniform_id).as_ref(), x, y, z, w)
            },
            WebGLCommand::Uniform4fv(uniform_id, ref v) => unsafe {
                gl.uniform_4_f32_slice(native_uniform_location(uniform_id).as_ref(), v)
            },
            WebGLCommand::Uniform4i(uniform_id, x, y, z, w) => unsafe {
                gl.uniform_4_i32(native_uniform_location(uniform_id).as_ref(), x, y, z, w)
            },
            WebGLCommand::Uniform4iv(uniform_id, ref v) => unsafe {
                gl.uniform_4_i32_slice(native_uniform_location(uniform_id).as_ref(), v)
            },
            WebGLCommand::Uniform4ui(uniform_id, x, y, z, w) => unsafe {
                gl.uniform_4_u32(native_uniform_location(uniform_id).as_ref(), x, y, z, w)
            },
            WebGLCommand::Uniform4uiv(uniform_id, ref v) => unsafe {
                gl.uniform_4_u32_slice(native_uniform_location(uniform_id).as_ref(), v)
            },
            WebGLCommand::UniformMatrix2fv(uniform_id, ref v) => unsafe {
                gl.uniform_matrix_2_f32_slice(
                    native_uniform_location(uniform_id).as_ref(),
                    false,
                    v,
                )
            },
            WebGLCommand::UniformMatrix3fv(uniform_id, ref v) => unsafe {
                gl.uniform_matrix_3_f32_slice(
                    native_uniform_location(uniform_id).as_ref(),
                    false,
                    v,
                )
            },
            WebGLCommand::UniformMatrix4fv(uniform_id, ref v) => unsafe {
                gl.uniform_matrix_4_f32_slice(
                    native_uniform_location(uniform_id).as_ref(),
                    false,
                    v,
                )
            },
            WebGLCommand::UniformMatrix3x2fv(uniform_id, ref v) => unsafe {
                gl.uniform_matrix_3x2_f32_slice(
                    native_uniform_location(uniform_id).as_ref(),
                    false,
                    v,
                )
            },
            WebGLCommand::UniformMatrix4x2fv(uniform_id, ref v) => unsafe {
                gl.uniform_matrix_4x2_f32_slice(
                    native_uniform_location(uniform_id).as_ref(),
                    false,
                    v,
                )
            },
            WebGLCommand::UniformMatrix2x3fv(uniform_id, ref v) => unsafe {
                gl.uniform_matrix_2x3_f32_slice(
                    native_uniform_location(uniform_id).as_ref(),
                    false,
                    v,
                )
            },
            WebGLCommand::UniformMatrix4x3fv(uniform_id, ref v) => unsafe {
                gl.uniform_matrix_4x3_f32_slice(
                    native_uniform_location(uniform_id).as_ref(),
                    false,
                    v,
                )
            },
            WebGLCommand::UniformMatrix2x4fv(uniform_id, ref v) => unsafe {
                gl.uniform_matrix_2x4_f32_slice(
                    native_uniform_location(uniform_id).as_ref(),
                    false,
                    v,
                )
            },
            WebGLCommand::UniformMatrix3x4fv(uniform_id, ref v) => unsafe {
                gl.uniform_matrix_3x4_f32_slice(
                    native_uniform_location(uniform_id).as_ref(),
                    false,
                    v,
                )
            },
            WebGLCommand::ValidateProgram(program_id) => unsafe {
                gl.validate_program(program_id.glow())
            },
            WebGLCommand::VertexAttrib(attrib_id, x, y, z, w) => unsafe {
                gl.vertex_attrib_4_f32(attrib_id, x, y, z, w)
            },
            WebGLCommand::VertexAttribI(attrib_id, x, y, z, w) => unsafe {
                gl.vertex_attrib_4_i32(attrib_id, x, y, z, w)
            },
            WebGLCommand::VertexAttribU(attrib_id, x, y, z, w) => unsafe {
                gl.vertex_attrib_4_u32(attrib_id, x, y, z, w)
            },
            WebGLCommand::VertexAttribPointer2f(attrib_id, size, normalized, stride, offset) => unsafe {
                gl.vertex_attrib_pointer_f32(
                    attrib_id,
                    size,
                    gl::FLOAT,
                    normalized,
                    stride,
                    offset as _,
                )
            },
            WebGLCommand::VertexAttribPointer(
                attrib_id,
                size,
                data_type,
                normalized,
                stride,
                offset,
            ) => unsafe {
                gl.vertex_attrib_pointer_f32(
                    attrib_id,
                    size,
                    data_type,
                    normalized,
                    stride,
                    offset as _,
                )
            },
            WebGLCommand::VertexAttribIPointer(attrib_id, size, data_type, stride, offset) => unsafe {
                gl.vertex_attrib_pointer_i32(attrib_id, size, data_type, stride, offset as _)
            },
            WebGLCommand::SetViewport(x, y, width, height) => unsafe {
                gl.viewport(x, y, width, height)
            },
            WebGLCommand::TexImage3D {
                target,
                level,
                internal_format,
                size,
                depth,
                format,
                data_type,
                effective_data_type,
                unpacking_alignment,
                alpha_treatment,
                y_axis_treatment,
                pixel_format,
                ref data,
            } => {
                let pixels = prepare_pixels_3d(
                    internal_format,
                    data_type,
                    size,
                    depth,
                    unpacking_alignment,
                    alpha_treatment,
                    y_axis_treatment,
                    pixel_format,
                    Cow::Borrowed(data),
                );

                let (internal_format, format) = luminance_formats(
                    state,
                    gl,
                    target,
                    internal_format,
                    format,
                    effective_data_type,
                );
                unsafe {
                    gl.pixel_store_i32(gl::UNPACK_ALIGNMENT, unpacking_alignment as i32);
                    gl.tex_image_3d(
                        target,
                        level as i32,
                        internal_format as i32,
                        size.width as i32,
                        size.height as i32,
                        depth as i32,
                        0,
                        format,
                        effective_data_type,
                        PixelUnpackData::Slice(Some(&pixels)),
                    );
                }
            },
            WebGLCommand::TexImage2D {
                target,
                level,
                internal_format,
                size,
                format,
                data_type,
                effective_data_type,
                unpacking_alignment,
                alpha_treatment,
                y_axis_treatment,
                pixel_format,
                ref data,
            } => {
                // Partii (perf): texImage2D with null pixels arrives with no data (the DOM sends none
                // instead of a zero-filled copy). Zeros need no premultiply or flip, so upload a fresh
                // zeroed buffer (calloc: untouched pages) instead of copying and converting one.
                // Phaser's boot makes ~80 full-canvas render targets this way (hexii's start frame).
                let pixels = if data.is_empty() && size.width > 0 && size.height > 0 {
                    let cpp = data_type.element_size() * format.components() /
                        data_type.components_per_element();
                    let a = unpacking_alignment.max(1);
                    let stride = (size.width * cpp + a - 1) & !(a - 1);
                    Cow::Owned(vec![0u8; (stride * (size.height - 1) + size.width * cpp) as usize])
                } else {
                    prepare_pixels(
                        internal_format,
                        data_type,
                        size,
                        unpacking_alignment,
                        alpha_treatment,
                        y_axis_treatment,
                        pixel_format,
                        Cow::Borrowed(data),
                    )
                };

                let (internal_format, format) = luminance_formats(
                    state,
                    gl,
                    target,
                    internal_format,
                    format,
                    effective_data_type,
                );
                unsafe {
                    gl.pixel_store_i32(gl::UNPACK_ALIGNMENT, unpacking_alignment as i32);
                    gl.tex_image_2d(
                        target,
                        level as i32,
                        internal_format as i32,
                        size.width as i32,
                        size.height as i32,
                        0,
                        format,
                        effective_data_type,
                        PixelUnpackData::Slice(Some(&pixels)),
                    );
                }
            },
            WebGLCommand::TexImage3DPBO {
                target,
                level,
                internal_format,
                size,
                depth,
                format,
                effective_data_type,
                unpacking_alignment,
                offset,
            } => unsafe {
                let (internal_format, format) = luminance_formats(
                    state,
                    gl,
                    target,
                    internal_format,
                    format,
                    effective_data_type,
                );
                gl.pixel_store_i32(gl::UNPACK_ALIGNMENT, unpacking_alignment as i32);
                gl.tex_image_3d(
                    target,
                    level as i32,
                    internal_format as i32,
                    size.width as i32,
                    size.height as i32,
                    depth as i32,
                    0,
                    format,
                    effective_data_type,
                    PixelUnpackData::BufferOffset(offset as u32),
                );
            },
            WebGLCommand::TexSubImage3D {
                target,
                level,
                xoffset,
                yoffset,
                zoffset,
                size,
                depth,
                format,
                data_type,
                effective_data_type,
                unpacking_alignment,
                alpha_treatment,
                y_axis_treatment,
                pixel_format,
                ref data,
            } => {
                let pixels = prepare_pixels_3d(
                    format,
                    data_type,
                    size,
                    depth,
                    unpacking_alignment,
                    alpha_treatment,
                    y_axis_treatment,
                    pixel_format,
                    Cow::Borrowed(data),
                );
                unsafe {
                    gl.pixel_store_i32(gl::UNPACK_ALIGNMENT, unpacking_alignment as i32);
                    gl.tex_sub_image_3d(
                        target,
                        level as i32,
                        xoffset,
                        yoffset,
                        zoffset,
                        size.width as i32,
                        size.height as i32,
                        depth as i32,
                        luminance_transfer_format(state, format),
                        effective_data_type,
                        PixelUnpackData::Slice(Some(&pixels)),
                    );
                }
            },
            WebGLCommand::TexSubImage3DPBO {
                target,
                level,
                xoffset,
                yoffset,
                zoffset,
                size,
                depth,
                format,
                effective_data_type,
                unpacking_alignment,
                offset,
            } => unsafe {
                gl.pixel_store_i32(gl::UNPACK_ALIGNMENT, unpacking_alignment as i32);
                gl.tex_sub_image_3d(
                    target,
                    level as i32,
                    xoffset,
                    yoffset,
                    zoffset,
                    size.width as i32,
                    size.height as i32,
                    depth as i32,
                    luminance_transfer_format(state, format),
                    effective_data_type,
                    PixelUnpackData::BufferOffset(offset as u32),
                );
            },
            WebGLCommand::TexSubImage2DPBO {
                target,
                level,
                xoffset,
                yoffset,
                size,
                format,
                effective_data_type,
                unpacking_alignment,
                offset,
            } => unsafe {
                gl.pixel_store_i32(gl::UNPACK_ALIGNMENT, unpacking_alignment as i32);
                gl.tex_sub_image_2d(
                    target,
                    level as i32,
                    xoffset,
                    yoffset,
                    size.width as i32,
                    size.height as i32,
                    luminance_transfer_format(state, format),
                    effective_data_type,
                    PixelUnpackData::BufferOffset(offset as u32),
                );
            },
            WebGLCommand::CopyTexSubImage3D(
                target,
                level,
                xoffset,
                yoffset,
                zoffset,
                x,
                y,
                width,
                height,
            ) => unsafe {
                match luminance_copy_kind(state, gl, target) {
                    Some(kind) => copy_luminance_via_cpu(
                        gl,
                        target,
                        level,
                        (xoffset, yoffset, Some(zoffset)),
                        (x, y, width, height),
                        kind,
                    ),
                    None => gl.copy_tex_sub_image_3d(
                        target, level, xoffset, yoffset, zoffset, x, y, width, height,
                    ),
                }
            },
            WebGLCommand::TexImage2DPBO {
                target,
                level,
                internal_format,
                size,
                format,
                effective_data_type,
                unpacking_alignment,
                offset,
            } => unsafe {
                let (internal_format, format) = luminance_formats(
                    state,
                    gl,
                    target,
                    internal_format,
                    format,
                    effective_data_type,
                );
                gl.pixel_store_i32(gl::UNPACK_ALIGNMENT, unpacking_alignment as i32);

                gl.tex_image_2d(
                    target,
                    level as i32,
                    internal_format as i32,
                    size.width as i32,
                    size.height as i32,
                    0,
                    format,
                    effective_data_type,
                    PixelUnpackData::BufferOffset(offset as u32),
                );
            },
            WebGLCommand::TexSubImage2D {
                target,
                level,
                xoffset,
                yoffset,
                size,
                format,
                data_type,
                effective_data_type,
                unpacking_alignment,
                alpha_treatment,
                y_axis_treatment,
                pixel_format,
                ref data,
            } => {
                let pixels = prepare_pixels(
                    format,
                    data_type,
                    size,
                    unpacking_alignment,
                    alpha_treatment,
                    y_axis_treatment,
                    pixel_format,
                    Cow::Borrowed(data),
                );

                unsafe {
                    gl.pixel_store_i32(gl::UNPACK_ALIGNMENT, unpacking_alignment as i32);
                    gl.tex_sub_image_2d(
                        target,
                        level as i32,
                        xoffset,
                        yoffset,
                        size.width as i32,
                        size.height as i32,
                        luminance_transfer_format(state, format),
                        effective_data_type,
                        glow::PixelUnpackData::Slice(Some(&pixels)),
                    );
                }
            },
            WebGLCommand::CompressedTexImage2D {
                target,
                level,
                internal_format,
                size,
                ref data,
            } => with_compressed_bytes(gl, data, |bytes| unsafe {
                gl.compressed_tex_image_2d(
                    target,
                    level as i32,
                    internal_format as i32,
                    size.width as i32,
                    size.height as i32,
                    0,
                    bytes.len() as i32,
                    bytes,
                )
            }),
            WebGLCommand::CompressedTexSubImage2D {
                target,
                level,
                xoffset,
                yoffset,
                size,
                format,
                ref data,
            } => unsafe {
                gl.compressed_tex_sub_image_2d(
                    target,
                    level,
                    xoffset,
                    yoffset,
                    size.width as i32,
                    size.height as i32,
                    format,
                    compressed_unpack_data(data),
                )
            },
            WebGLCommand::CompressedTexImage3D {
                target,
                level,
                internal_format,
                size,
                depth,
                ref data,
            } => with_compressed_bytes(gl, data, |bytes| unsafe {
                gl.compressed_tex_image_3d(
                    target,
                    level as i32,
                    internal_format as i32,
                    size.width as i32,
                    size.height as i32,
                    depth as i32,
                    0,
                    bytes.len() as i32,
                    bytes,
                )
            }),
            WebGLCommand::CompressedTexSubImage3D {
                target,
                level,
                xoffset,
                yoffset,
                zoffset,
                size,
                depth,
                format,
                ref data,
            } => unsafe {
                gl.compressed_tex_sub_image_3d(
                    target,
                    level,
                    xoffset,
                    yoffset,
                    zoffset,
                    size.width as i32,
                    size.height as i32,
                    depth as i32,
                    format,
                    compressed_unpack_data(data),
                )
            },
            WebGLCommand::TexStorage2D(target, levels, internal_format, width, height) => unsafe {
                gl.tex_storage_2d(
                    target,
                    levels as i32,
                    internal_format.as_gl_constant(),
                    width as i32,
                    height as i32,
                )
            },
            WebGLCommand::TexStorage3D(target, levels, internal_format, width, height, depth) => unsafe {
                gl.tex_storage_3d(
                    target,
                    levels as i32,
                    internal_format.as_gl_constant(),
                    width as i32,
                    height as i32,
                    depth as i32,
                )
            },
            WebGLCommand::DrawingBufferWidth(ref sender) => {
                let size = device
                    .context_surface_info(ctx)
                    .unwrap()
                    .expect("Where's the front buffer?")
                    .size;
                sender.send(size.width).unwrap()
            },
            WebGLCommand::DrawingBufferHeight(ref sender) => {
                let size = device
                    .context_surface_info(ctx)
                    .unwrap()
                    .expect("Where's the front buffer?")
                    .size;
                sender.send(size.height).unwrap()
            },
            WebGLCommand::Finish(ref sender) => Self::finish(gl, sender),
            WebGLCommand::Flush => unsafe { gl.flush() },
            WebGLCommand::GenerateMipmap(target) => unsafe { gl.generate_mipmap(target) },
            WebGLCommand::CreateVertexArray(ref chan) => {
                let id = Self::create_vertex_array(gl);
                let _ = chan.send(id);
            },
            WebGLCommand::DeleteVertexArray(id) => {
                unsafe { gl.delete_vertex_array(id.glow()) };
            },
            WebGLCommand::BindVertexArray(id) => {
                let id = id.map(WebGLVertexArrayId::glow).or(state.default_vao);
                unsafe { gl.bind_vertex_array(id) }
            },
            WebGLCommand::GetParameterBool(param, ref sender) => {
                let value = match param {
                    webgl::ParameterBool::DepthWritemask => state.depth_write_mask,
                    _ => unsafe { gl.get_parameter_bool(param as u32) },
                };
                sender.send(value).unwrap()
            },
            WebGLCommand::FenceSync(ref sender) => {
                let value = unsafe { gl.fence_sync(gl::SYNC_GPU_COMMANDS_COMPLETE, 0).unwrap() };
                sender.send(WebGLSyncId::from_glow(value)).unwrap();
            },
            WebGLCommand::IsSync(sync_id, ref sender) => {
                let value = unsafe { gl.is_sync(sync_id.glow()) };
                sender.send(value).unwrap();
            },
            WebGLCommand::ClientWaitSync(sync_id, flags, timeout, ref sender) => {
                let value = unsafe { gl.client_wait_sync(sync_id.glow(), flags, timeout as _) };
                sender.send(value).unwrap();
            },
            WebGLCommand::WaitSync(sync_id, flags, timeout) => {
                unsafe { gl.wait_sync(sync_id.glow(), flags, timeout as u64) };
            },
            WebGLCommand::GetSyncParameter(sync_id, param, ref sender) => {
                let value = unsafe { gl.get_sync_parameter_i32(sync_id.glow(), param) };
                sender.send(value as u32).unwrap();
            },
            WebGLCommand::DeleteSync(sync_id) => {
                unsafe { gl.delete_sync(sync_id.glow()) };
            },
            WebGLCommand::GetParameterBool4(param, ref sender) => {
                let value = match param {
                    webgl::ParameterBool4::ColorWritemask => state.color_write_mask,
                };
                sender.send(value).unwrap()
            },
            WebGLCommand::GetParameterInt(param, ref sender) => {
                let value = match param {
                    webgl::ParameterInt::AlphaBits if state.fake_no_alpha() => 0,
                    webgl::ParameterInt::DepthBits if state.fake_no_depth() => 0,
                    webgl::ParameterInt::StencilBits if state.fake_no_stencil() => 0,
                    // Desktop core profiles have no GL_RED_BITS etc.: ask the bound draw
                    // framebuffer's attachments (our default framebuffer is an FBO too).
                    webgl::ParameterInt::RedBits |
                    webgl::ParameterInt::GreenBits |
                    webgl::ParameterInt::BlueBits |
                    webgl::ParameterInt::AlphaBits |
                    webgl::ParameterInt::DepthBits |
                    webgl::ParameterInt::StencilBits
                        if !gl.version().is_embedded =>
                    {
                        framebuffer_bits(gl, param)
                    },
                    _ => unsafe { gl.get_parameter_i32(param as u32) },
                };
                sender.send(value).unwrap()
            },
            WebGLCommand::GetParameterUInt(param, ref sender) => {
                let value = match param {
                    webgl::ParameterUInt::StencilWritemask => state.stencil_write_mask.0,
                    webgl::ParameterUInt::StencilBackWritemask => state.stencil_write_mask.1,
                    webgl::ParameterUInt::StencilValueMask => state.stencil_value_mask.0,
                    webgl::ParameterUInt::StencilBackValueMask => state.stencil_value_mask.1,
                };
                sender.send(value).unwrap()
            },
            WebGLCommand::GetParameterInt2(param, ref sender) => {
                let mut value = [0; 2];
                unsafe {
                    gl.get_parameter_i32_slice(param as u32, &mut value);
                }
                sender.send(value).unwrap()
            },
            WebGLCommand::GetParameterInt4(param, ref sender) => {
                let mut value = [0; 4];
                unsafe {
                    gl.get_parameter_i32_slice(param as u32, &mut value);
                }
                sender.send(value).unwrap()
            },
            WebGLCommand::GetParameterFloat(param, ref sender) => {
                let mut value = [0.];
                unsafe {
                    gl.get_parameter_f32_slice(param as u32, &mut value);
                }
                sender.send(value[0]).unwrap()
            },
            WebGLCommand::GetParameterFloat2(param, ref sender) => {
                let mut value = [0.; 2];
                unsafe {
                    gl.get_parameter_f32_slice(param as u32, &mut value);
                }
                sender.send(value).unwrap()
            },
            WebGLCommand::GetParameterFloat4(param, ref sender) => {
                let mut value = [0.; 4];
                unsafe {
                    gl.get_parameter_f32_slice(param as u32, &mut value);
                }
                sender.send(value).unwrap()
            },
            WebGLCommand::GetProgramValidateStatus(program, ref sender) => sender
                .send(unsafe { gl.get_program_validate_status(program.glow()) })
                .unwrap(),
            WebGLCommand::GetProgramActiveUniforms(program, ref sender) => sender
                .send(unsafe { gl.get_program_parameter_i32(program.glow(), gl::ACTIVE_UNIFORMS) })
                .unwrap(),
            WebGLCommand::GetCurrentVertexAttrib(index, ref sender) => {
                let mut value = [0.; 4];
                unsafe {
                    gl.get_vertex_attrib_parameter_f32_slice(
                        index,
                        gl::CURRENT_VERTEX_ATTRIB,
                        &mut value,
                    );
                }
                sender.send(value).unwrap();
            },
            WebGLCommand::GetTexParameterFloat(target, param, ref sender) => {
                sender
                    .send(unsafe { gl.get_tex_parameter_f32(target, param as u32) })
                    .unwrap();
            },
            WebGLCommand::GetTexParameterInt(target, param, ref sender) => {
                sender
                    .send(unsafe { gl.get_tex_parameter_i32(target, param as u32) })
                    .unwrap();
            },
            WebGLCommand::GetTexParameterBool(target, param, ref sender) => {
                sender
                    .send(unsafe { gl.get_tex_parameter_i32(target, param as u32) } != 0)
                    .unwrap();
            },
            WebGLCommand::GetInternalFormatIntVec(target, internal_format, param, ref sender) => {
                match param {
                    InternalFormatIntVec::Samples => {
                        let mut count = [0; 1];
                        unsafe {
                            gl.get_internal_format_i32_slice(
                                target,
                                internal_format,
                                gl::NUM_SAMPLE_COUNTS,
                                &mut count,
                            )
                        };
                        assert!(count[0] >= 0);

                        let mut values = vec![0; count[0] as usize];
                        unsafe {
                            gl.get_internal_format_i32_slice(
                                target,
                                internal_format,
                                param as u32,
                                &mut values,
                            )
                        };
                        sender.send(values).unwrap()
                    },
                }
            },
            WebGLCommand::TexParameteri(target, param, value) => unsafe {
                gl.tex_parameter_i32(target, param, value)
            },
            WebGLCommand::TexParameterf(target, param, value) => unsafe {
                gl.tex_parameter_f32(target, param, value)
            },
            WebGLCommand::LinkProgram(program_id, ref sender) => {
                // Script resolves links lazily (KHR_parallel_shader_compile), so the program may
                // be gone before the answer arrives; that is not an error.
                let _ = sender.send(Self::link_program(gl, program_id));
                return;
            },
            WebGLCommand::UseProgram(program_id) => unsafe {
                gl.use_program(program_id.map(|p| p.glow()))
            },
            WebGLCommand::DrawArrays { mode, first, count } => unsafe {
                gl.draw_arrays(mode, first, count)
            },
            WebGLCommand::DrawArraysInstanced {
                mode,
                first,
                count,
                primcount,
            } => unsafe { gl.draw_arrays_instanced(mode, first, count, primcount) },
            WebGLCommand::DrawElements {
                mode,
                count,
                type_,
                offset,
            } => unsafe { gl.draw_elements(mode, count, type_, offset as _) },
            WebGLCommand::DrawElementsInstanced {
                mode,
                count,
                type_,
                offset,
                primcount,
            } => unsafe {
                gl.draw_elements_instanced(mode, count, type_, offset as i32, primcount)
            },
            WebGLCommand::DrawArraysInstancedBaseInstance {
                mode,
                first,
                count,
                primcount,
                base_instance,
            } => unsafe {
                gl.draw_arrays_instanced_base_instance(mode, first, count, primcount, base_instance)
            },
            WebGLCommand::DrawElementsInstancedBaseVertexBaseInstance {
                mode,
                count,
                type_,
                offset,
                primcount,
                base_vertex,
                base_instance,
            } => unsafe {
                gl.draw_elements_instanced_base_vertex_base_instance(
                    mode,
                    count,
                    type_,
                    offset as i32,
                    primcount,
                    base_vertex,
                    base_instance,
                )
            },
            WebGLCommand::VertexAttribDivisor { index, divisor } => unsafe {
                gl.vertex_attrib_divisor(index, divisor)
            },
            WebGLCommand::GetUniformBool(program_id, loc, ref sender) => {
                let mut value = [0];
                unsafe {
                    gl.get_uniform_i32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value[0] != 0).unwrap();
            },
            WebGLCommand::GetUniformBool2(program_id, loc, ref sender) => {
                let mut value = [0; 2];
                unsafe {
                    gl.get_uniform_i32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                let value = [value[0] != 0, value[1] != 0];
                sender.send(value).unwrap();
            },
            WebGLCommand::GetUniformBool3(program_id, loc, ref sender) => {
                let mut value = [0; 3];
                unsafe {
                    gl.get_uniform_i32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                let value = [value[0] != 0, value[1] != 0, value[2] != 0];
                sender.send(value).unwrap();
            },
            WebGLCommand::GetUniformBool4(program_id, loc, ref sender) => {
                let mut value = [0; 4];
                unsafe {
                    gl.get_uniform_i32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                let value = [value[0] != 0, value[1] != 0, value[2] != 0, value[3] != 0];
                sender.send(value).unwrap();
            },
            WebGLCommand::GetUniformInt(program_id, loc, ref sender) => {
                let mut value = [0];
                unsafe {
                    gl.get_uniform_i32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value[0]).unwrap();
            },
            WebGLCommand::GetUniformInt2(program_id, loc, ref sender) => {
                let mut value = [0; 2];
                unsafe {
                    gl.get_uniform_i32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap();
            },
            WebGLCommand::GetUniformInt3(program_id, loc, ref sender) => {
                let mut value = [0; 3];
                unsafe {
                    gl.get_uniform_i32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap();
            },
            WebGLCommand::GetUniformInt4(program_id, loc, ref sender) => {
                let mut value = [0; 4];
                unsafe {
                    gl.get_uniform_i32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap();
            },
            WebGLCommand::GetUniformUint(program_id, loc, ref sender) => {
                let mut value = [0];
                unsafe {
                    gl.get_uniform_u32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value[0]).unwrap();
            },
            WebGLCommand::GetUniformUint2(program_id, loc, ref sender) => {
                let mut value = [0; 2];
                unsafe {
                    gl.get_uniform_u32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap();
            },
            WebGLCommand::GetUniformUint3(program_id, loc, ref sender) => {
                let mut value = [0; 3];
                unsafe {
                    gl.get_uniform_u32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap();
            },
            WebGLCommand::GetUniformUint4(program_id, loc, ref sender) => {
                let mut value = [0; 4];
                unsafe {
                    gl.get_uniform_u32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap();
            },
            WebGLCommand::GetUniformFloat(program_id, loc, ref sender) => {
                let mut value = [0.];
                unsafe {
                    gl.get_uniform_f32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value[0]).unwrap();
            },
            WebGLCommand::GetUniformFloat2(program_id, loc, ref sender) => {
                let mut value = [0.; 2];
                unsafe {
                    gl.get_uniform_f32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap();
            },
            WebGLCommand::GetUniformFloat3(program_id, loc, ref sender) => {
                let mut value = [0.; 3];
                unsafe {
                    gl.get_uniform_f32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap();
            },
            WebGLCommand::GetUniformFloat4(program_id, loc, ref sender) => {
                let mut value = [0.; 4];
                unsafe {
                    gl.get_uniform_f32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap();
            },
            WebGLCommand::GetUniformFloat9(program_id, loc, ref sender) => {
                let mut value = [0.; 9];
                unsafe {
                    gl.get_uniform_f32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap();
            },
            WebGLCommand::GetUniformFloat16(program_id, loc, ref sender) => {
                let mut value = [0.; 16];
                unsafe {
                    gl.get_uniform_f32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap();
            },
            WebGLCommand::GetUniformFloat2x3(program_id, loc, ref sender) => {
                let mut value = [0.; 2 * 3];
                unsafe {
                    gl.get_uniform_f32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap()
            },
            WebGLCommand::GetUniformFloat2x4(program_id, loc, ref sender) => {
                let mut value = [0.; 2 * 4];
                unsafe {
                    gl.get_uniform_f32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap()
            },
            WebGLCommand::GetUniformFloat3x2(program_id, loc, ref sender) => {
                let mut value = [0.; 3 * 2];
                unsafe {
                    gl.get_uniform_f32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap()
            },
            WebGLCommand::GetUniformFloat3x4(program_id, loc, ref sender) => {
                let mut value = [0.; 3 * 4];
                unsafe {
                    gl.get_uniform_f32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap()
            },
            WebGLCommand::GetUniformFloat4x2(program_id, loc, ref sender) => {
                let mut value = [0.; 4 * 2];
                unsafe {
                    gl.get_uniform_f32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap()
            },
            WebGLCommand::GetUniformFloat4x3(program_id, loc, ref sender) => {
                let mut value = [0.; 4 * 3];
                unsafe {
                    gl.get_uniform_f32(
                        program_id.glow(),
                        &NativeUniformLocation(loc as u32),
                        &mut value,
                    );
                }
                sender.send(value).unwrap()
            },
            WebGLCommand::GetUniformBlockIndex(program_id, ref name, ref sender) => {
                let name = to_name_in_compiled_shader(name);
                let index = unsafe { gl.get_uniform_block_index(program_id.glow(), &name) };
                // TODO(#34300): use Option<u32>
                sender.send(index.unwrap_or(gl::INVALID_INDEX)).unwrap();
            },
            WebGLCommand::GetUniformIndices(program_id, ref names, ref sender) => {
                let names = names
                    .iter()
                    .map(|name| to_name_in_compiled_shader(name))
                    .collect::<Vec<_>>();
                let name_strs = names.iter().map(|name| name.as_str()).collect::<Vec<_>>();
                let indices = unsafe {
                    gl.get_uniform_indices(program_id.glow(), &name_strs)
                        .iter()
                        .map(|index| index.unwrap_or(gl::INVALID_INDEX))
                        .collect()
                };
                sender.send(indices).unwrap();
            },
            WebGLCommand::GetActiveUniforms(program_id, ref indices, pname, ref sender) => {
                let results =
                    unsafe { gl.get_active_uniforms_parameter(program_id.glow(), indices, pname) };
                sender.send(results).unwrap();
            },
            WebGLCommand::GetActiveUniformBlockName(program_id, block_idx, ref sender) => {
                // The translator renames user blocks (`_u` prefix); content gets the source name,
                // which getUniformBlockIndex maps back.
                let name =
                    unsafe { gl.get_active_uniform_block_name(program_id.glow(), block_idx) };
                sender.send(from_name_in_compiled_shader(&name)).unwrap();
            },
            WebGLCommand::GetActiveUniformBlockParameter(
                program_id,
                block_idx,
                pname,
                ref sender,
            ) => {
                let size = match pname {
                    gl::UNIFORM_BLOCK_ACTIVE_UNIFORM_INDICES => unsafe {
                        gl.get_active_uniform_block_parameter_i32(
                            program_id.glow(),
                            block_idx,
                            gl::UNIFORM_BLOCK_ACTIVE_UNIFORMS,
                        ) as usize
                    },
                    _ => 1,
                };
                let mut result = vec![0; size];
                unsafe {
                    gl.get_active_uniform_block_parameter_i32_slice(
                        program_id.glow(),
                        block_idx,
                        pname,
                        &mut result,
                    )
                };
                sender.send(result).unwrap();
            },
            WebGLCommand::UniformBlockBinding(program_id, block_idx, block_binding) => unsafe {
                gl.uniform_block_binding(program_id.glow(), block_idx, block_binding)
            },
            WebGLCommand::InitializeFramebuffer {
                color,
                depth,
                stencil,
            } => Self::initialize_framebuffer(gl, state, color, depth, stencil),
            WebGLCommand::BeginQuery(target, query_id) => {
                unsafe { gl.begin_query(target, query_id.glow()) };
                if is_occlusion_query(target) {
                    state.active_occlusion_queries += 1;
                }
            },
            WebGLCommand::EndQuery(target) => {
                unsafe { gl.end_query(target) };
                if is_occlusion_query(target) {
                    state.active_occlusion_queries =
                        state.active_occlusion_queries.saturating_sub(1);
                }
            },
            WebGLCommand::DeleteQuery(query_id) => {
                unsafe { gl.delete_query(query_id.glow()) };
            },
            WebGLCommand::GenerateQuery(ref sender) => {
                // TODO(#34300): use Option<WebGLQueryId>
                let id = unsafe { gl.create_query().unwrap() };
                sender.send(WebGLQueryId::from_glow(id)).unwrap()
            },
            WebGLCommand::GetQueryState(ref sender, query_id, pname) => {
                let value = unsafe { gl.get_query_parameter_u32(query_id.glow(), pname) };
                sender.send(value).unwrap()
            },
            WebGLCommand::GenerateSampler(ref sender) => {
                let id = unsafe { gl.create_sampler().unwrap() };
                sender.send(WebGLSamplerId::from_glow(id)).unwrap()
            },
            WebGLCommand::DeleteSampler(sampler_id) => {
                unsafe { gl.delete_sampler(sampler_id.glow()) };
            },
            WebGLCommand::BindSampler(unit, sampler_id) => {
                unsafe { gl.bind_sampler(unit, sampler_id.map(|id| id.glow())) };
            },
            WebGLCommand::SetSamplerParameterInt(sampler_id, pname, value) => {
                unsafe { gl.sampler_parameter_i32(sampler_id.glow(), pname, value) };
            },
            WebGLCommand::SetSamplerParameterFloat(sampler_id, pname, value) => {
                unsafe { gl.sampler_parameter_f32(sampler_id.glow(), pname, value) };
            },
            WebGLCommand::GetSamplerParameterInt(sampler_id, pname, ref sender) => {
                let value = unsafe { gl.get_sampler_parameter_i32(sampler_id.glow(), pname) };
                sender.send(value).unwrap();
            },
            WebGLCommand::GetSamplerParameterFloat(sampler_id, pname, ref sender) => {
                let value = unsafe { gl.get_sampler_parameter_f32(sampler_id.glow(), pname) };
                sender.send(value).unwrap();
            },
            WebGLCommand::BindBufferBase(target, index, id) => {
                // https://searchfox.org/mozilla-central/rev/13b081a62d3f3e3e3120f95564529257b0bf451c/dom/canvas/WebGLContextBuffers.cpp#208-210
                // BindBufferBase/Range will fail (on some drivers) if the buffer name has
                // never been bound. (GenBuffers makes a name, but BindBuffer initializes
                // that name as a real buffer object)
                let id = id.map(WebGLBufferId::glow);
                unsafe {
                    gl.bind_buffer(target, id);
                    gl.bind_buffer(target, None);
                    gl.bind_buffer_base(target, index, id);
                }
            },
            WebGLCommand::BindBufferRange(target, index, id, offset, size) => {
                // https://searchfox.org/mozilla-central/rev/13b081a62d3f3e3e3120f95564529257b0bf451c/dom/canvas/WebGLContextBuffers.cpp#208-210
                // BindBufferBase/Range will fail (on some drivers) if the buffer name has
                // never been bound. (GenBuffers makes a name, but BindBuffer initializes
                // that name as a real buffer object)
                let id = id.map(WebGLBufferId::glow);
                unsafe {
                    gl.bind_buffer(target, id);
                    gl.bind_buffer(target, None);
                    gl.bind_buffer_range(target, index, id, offset as i32, size as i32);
                }
            },
            WebGLCommand::ClearBufferfv(buffer, draw_buffer, ref value) => unsafe {
                gl.clear_buffer_f32_slice(buffer, draw_buffer as u32, value)
            },
            WebGLCommand::ClearBufferiv(buffer, draw_buffer, ref value) => unsafe {
                gl.clear_buffer_i32_slice(buffer, draw_buffer as u32, value)
            },
            WebGLCommand::ClearBufferuiv(buffer, draw_buffer, ref value) => unsafe {
                gl.clear_buffer_u32_slice(buffer, draw_buffer as u32, value)
            },
            WebGLCommand::ClearBufferfi(buffer, draw_buffer, depth, stencil) => unsafe {
                gl.clear_buffer_depth_stencil(buffer, draw_buffer as u32, depth, stencil)
            },
            WebGLCommand::InvalidateFramebuffer(target, ref attachments) => unsafe {
                gl.invalidate_framebuffer(target, attachments)
            },
            WebGLCommand::InvalidateSubFramebuffer(target, ref attachments, x, y, w, h) => unsafe {
                gl.invalidate_sub_framebuffer(target, attachments, x, y, w, h)
            },
            WebGLCommand::FramebufferTextureLayer(target, attachment, tex_id, level, layer) => {
                let tex_id = tex_id.map(WebGLTextureId::glow);
                let attach = |attachment| unsafe {
                    gl.framebuffer_texture_layer(target, attachment, tex_id, level, layer)
                };

                if attachment == gl::DEPTH_STENCIL_ATTACHMENT {
                    attach(gl::DEPTH_ATTACHMENT);
                    attach(gl::STENCIL_ATTACHMENT);
                } else {
                    attach(attachment)
                }
            },
            WebGLCommand::ReadBuffer(buffer) => {
                // WebGL's default framebuffer is surfman's framebuffer object, whose colour
                // image is COLOR_ATTACHMENT0: GL rejects BACK there, which left the read buffer
                // at NONE after readBuffer(NONE) then readBuffer(BACK).
                let buffer = if buffer == gl::BACK {
                    let surface_fbo = device
                        .context_surface_info(ctx)
                        .ok()
                        .flatten()
                        .and_then(|info| info.framebuffer_object);
                    let bound =
                        unsafe { gl.get_parameter_framebuffer(gl::READ_FRAMEBUFFER_BINDING) };
                    if surface_fbo.is_some() && bound == surface_fbo {
                        gl::COLOR_ATTACHMENT0
                    } else {
                        buffer
                    }
                } else {
                    buffer
                };
                unsafe { gl.read_buffer(buffer) }
            },
            WebGLCommand::DrawBuffers(ref buffers) => unsafe { gl.draw_buffers(buffers) },
        }

        // If debug asertions are enabled, then check the error state.
        #[cfg(debug_assertions)]
        {
            let error = unsafe { gl.get_error() };
            if error != gl::NO_ERROR {
                error!("Last GL operation failed: {:?}", command);
                if error == gl::INVALID_FRAMEBUFFER_OPERATION {
                    let framebuffer_bindings =
                        unsafe { gl.get_parameter_framebuffer(gl::DRAW_FRAMEBUFFER_BINDING) };
                    debug!(
                        "(thread {:?}) Current draw framebuffer binding: {:?}",
                        ::std::thread::current().id(),
                        framebuffer_bindings
                    );
                }
                #[cfg(feature = "webgl_backtrace")]
                {
                    error!("Backtrace from failed WebGL API:\n{}", _backtrace.backtrace);
                    if let Some(backtrace) = _backtrace.js_backtrace {
                        error!("JS backtrace from failed WebGL API:\n{}", backtrace);
                    }
                }
                // TODO(servo#30568) revert to panic!() once underlying bug is fixed
                log::warn!(
                    "debug assertion failed! Unexpected WebGL error: 0x{:x} ({}) [{:?}]",
                    error,
                    error,
                    command
                );
            }
        }
    }

    /// Binds (creating it the first time) the opaque black 1x1 texture for `target` on the
    /// active unit. Creating it binds only `target`, which ends up bound to it anyway.
    fn bind_incomplete_texture(gl: &Gl, state: &mut GLState, target: u32) {
        let slot = match target {
            gl::TEXTURE_2D => &mut state.incomplete_textures.0,
            gl::TEXTURE_CUBE_MAP => &mut state.incomplete_textures.1,
            _ => return,
        };
        if let Some(texture) = *slot {
            unsafe { gl.bind_texture(target, Some(texture)) };
            return;
        }
        let Ok(texture) = (unsafe { gl.create_texture() }) else {
            return;
        };
        let black = [0u8, 0, 0, 255];
        let faces: &[u32] = if target == gl::TEXTURE_CUBE_MAP {
            &[
                gl::TEXTURE_CUBE_MAP_POSITIVE_X,
                gl::TEXTURE_CUBE_MAP_NEGATIVE_X,
                gl::TEXTURE_CUBE_MAP_POSITIVE_Y,
                gl::TEXTURE_CUBE_MAP_NEGATIVE_Y,
                gl::TEXTURE_CUBE_MAP_POSITIVE_Z,
                gl::TEXTURE_CUBE_MAP_NEGATIVE_Z,
            ]
        } else {
            &[gl::TEXTURE_2D]
        };
        unsafe {
            gl.bind_texture(target, Some(texture));
            gl.pixel_store_i32(gl::UNPACK_ALIGNMENT, 1);
            for &face in faces {
                gl.tex_image_2d(
                    face,
                    0,
                    gl::RGBA as i32,
                    1,
                    1,
                    0,
                    gl::RGBA,
                    gl::UNSIGNED_BYTE,
                    PixelUnpackData::Slice(Some(&black)),
                );
            }
            gl.pixel_store_i32(gl::UNPACK_ALIGNMENT, 4);
            gl.tex_parameter_i32(target, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
            gl.tex_parameter_i32(target, gl::TEXTURE_MAG_FILTER, gl::NEAREST as i32);
        }
        *slot = Some(texture);
    }

    fn initialize_framebuffer(gl: &Gl, state: &GLState, color: bool, depth: bool, stencil: bool) {
        let bits = [
            (color, gl::COLOR_BUFFER_BIT),
            (depth, gl::DEPTH_BUFFER_BIT),
            (stencil, gl::STENCIL_BUFFER_BIT),
        ]
        .iter()
        .fold(0, |bits, &(enabled, bit)| {
            bits | if enabled { bit } else { 0 }
        });

        unsafe {
            gl.disable(gl::SCISSOR_TEST);
            gl.color_mask(true, true, true, true);
            gl.clear_color(0., 0., 0., 0.);
            gl.depth_mask(true);
            gl.clear_depth(1.);
            gl.stencil_mask_separate(gl::FRONT, 0xFFFFFFFF);
            gl.stencil_mask_separate(gl::BACK, 0xFFFFFFFF);
            gl.clear_stencil(0);
            gl.clear(bits);
        }

        state.restore_invariant(gl);
    }

    fn link_program(gl: &Gl, program: WebGLProgramId) -> ProgramLinkInfo {
        unsafe { gl.link_program(program.glow()) };
        let linked = unsafe { gl.get_program_link_status(program.glow()) };
        if !linked {
            return ProgramLinkInfo {
                linked: false,
                active_attribs: vec![].into(),
                active_uniforms: vec![].into(),
                active_uniform_blocks: vec![].into(),
                transform_feedback_length: Default::default(),
                transform_feedback_mode: Default::default(),
                transform_feedback_varying_bytes: vec![].into(),
                emulated_builtins: Default::default(),
                hidden_uniform_indices: vec![].into(),
            };
        }
        let num_active_attribs =
            unsafe { gl.get_program_parameter_i32(program.glow(), gl::ACTIVE_ATTRIBUTES) };
        let active_attribs = (0..num_active_attribs as u32)
            .map(|i| {
                let active_attribute =
                    unsafe { gl.get_active_attribute(program.glow(), i) }.unwrap();
                let name = &active_attribute.name;
                let location = if name.starts_with("gl_") {
                    None
                } else {
                    unsafe { gl.get_attrib_location(program.glow(), name) }
                };
                ActiveAttribInfo {
                    name: from_name_in_compiled_shader(name),
                    size: active_attribute.size,
                    type_: active_attribute.atype,
                    location,
                }
            })
            .collect::<Vec<_>>()
            .into();

        let num_active_uniforms =
            unsafe { gl.get_program_parameter_i32(program.glow(), gl::ACTIVE_UNIFORMS) };
        let mut emulated_builtins = EmulatedBuiltinUniforms::default();
        let mut hidden_uniform_indices = vec![];
        let active_uniforms = (0..num_active_uniforms as u32)
            .filter_map(|i| {
                let active_uniform = unsafe { gl.get_active_uniform(program.glow(), i) }.unwrap();
                // Uniforms the translator added for gl_DrawID and friends: user names all carry
                // the `_u` prefix, so these never collide. Content must not see them.
                if active_uniform.name.starts_with("angle_") {
                    let location = unsafe {
                        gl.get_uniform_location(program.glow(), &active_uniform.name)
                    }
                    .map(|location| location.0 as i32);
                    emulated_builtins.record(&active_uniform.name, location);
                    hidden_uniform_indices.push(i);
                    return None;
                }
                let is_array = active_uniform.name.ends_with("[0]");
                let active_uniform_name = active_uniform
                    .name
                    .strip_suffix("[0]")
                    .unwrap_or_else(|| &active_uniform.name);
                Some(ActiveUniformInfo {
                    base_name: from_name_in_compiled_shader(active_uniform_name).into(),
                    size: if is_array {
                        Some(active_uniform.size)
                    } else {
                        None
                    },
                    type_: active_uniform.utype,
                    bind_index: None,
                    location: unsafe {
                        gl.get_uniform_location(program.glow(), &active_uniform.name)
                    }
                    .map(|location| location.0 as i32),
                })
            })
            .collect::<Vec<_>>()
            .into();

        let num_active_uniform_blocks =
            unsafe { gl.get_program_parameter_i32(program.glow(), gl::ACTIVE_UNIFORM_BLOCKS) };
        let active_uniform_blocks = (0..num_active_uniform_blocks as u32)
            .map(|i| {
                let name = from_name_in_compiled_shader(&unsafe {
                    gl.get_active_uniform_block_name(program.glow(), i)
                });
                let size = unsafe {
                    gl.get_active_uniform_block_parameter_i32(
                        program.glow(),
                        i,
                        gl::UNIFORM_BLOCK_DATA_SIZE,
                    )
                };
                ActiveUniformBlockInfo { name, size }
            })
            .collect::<Vec<_>>()
            .into();

        let transform_feedback_length = unsafe {
            gl.get_program_parameter_i32(program.glow(), gl::TRANSFORM_FEEDBACK_VARYINGS)
        };
        let transform_feedback_mode = unsafe {
            gl.get_program_parameter_i32(program.glow(), gl::TRANSFORM_FEEDBACK_BUFFER_MODE)
        };

        let transform_feedback_varying_bytes = (0..transform_feedback_length.max(0) as u32)
            .map(|i| {
                unsafe { gl.get_transform_feedback_varying(program.glow(), i) }
                    .map(|varying| {
                        varying.size.max(0) as u32 * transform_feedback_type_bytes(varying.tftype)
                    })
                    .unwrap_or(0)
            })
            .collect::<Vec<_>>()
            .into();

        ProgramLinkInfo {
            linked: true,
            active_attribs,
            active_uniforms,
            active_uniform_blocks,
            transform_feedback_length,
            transform_feedback_mode,
            emulated_builtins,
            hidden_uniform_indices: hidden_uniform_indices.into(),
            transform_feedback_varying_bytes,
        }
    }

    fn finish(gl: &Gl, chan: &GenericSender<()>) {
        unsafe { gl.finish() };
        chan.send(()).unwrap();
    }

    fn shader_precision_format(
        gl: &Gl,
        shader_type: u32,
        precision_type: u32,
        chan: &GenericSender<(i32, i32, i32)>,
    ) {
        let ShaderPrecisionFormat {
            range_min,
            range_max,
            precision,
        } = unsafe {
            gl.get_shader_precision_format(shader_type, precision_type)
                .unwrap_or_else(|| {
                    ShaderPrecisionFormat::common_desktop_hardware(
                        precision_type,
                        gl.version().is_embedded,
                    )
                })
        };
        chan.send((range_min, range_max, precision)).unwrap();
    }

    /// This is an implementation of `getSupportedExtensions()` from
    /// <https://registry.khronos.org/webgl/specs/latest/1.0/#5.14>
    fn get_extensions(gl: &Gl, result_sender: &GenericSender<String>) {
        let _ = result_sender.send(gl.supported_extensions().iter().join(" "));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.6>
    fn get_framebuffer_attachment_parameter(
        gl: &Gl,
        target: u32,
        attachment: u32,
        pname: u32,
        chan: &GenericSender<i32>,
    ) {
        let parameter =
            unsafe { gl.get_framebuffer_attachment_parameter_i32(target, attachment, pname) };
        chan.send(parameter).unwrap();
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.7>
    fn get_renderbuffer_parameter(gl: &Gl, target: u32, pname: u32, chan: &GenericSender<i32>) {
        let parameter = unsafe { gl.get_renderbuffer_parameter_i32(target, pname) };
        chan.send(parameter).unwrap();
    }

    fn uniform_location(
        gl: &Gl,
        program_id: WebGLProgramId,
        name: &str,
        chan: &GenericSender<i32>,
    ) {
        let location = unsafe {
            gl.get_uniform_location(program_id.glow(), &to_name_in_compiled_shader(name))
        };
        // (#34300): replace this with WebGLUniformId
        chan.send(location.map(|l| l.0).unwrap_or_default() as i32)
            .unwrap();
    }

    fn shader_info_log(gl: &Gl, shader_id: WebGLShaderId, chan: &GenericSender<String>) {
        let log = unsafe { gl.get_shader_info_log(shader_id.glow()) };
        chan.send(log).unwrap();
    }

    fn program_info_log(gl: &Gl, program_id: WebGLProgramId, chan: &GenericSender<String>) {
        let log = unsafe { gl.get_program_info_log(program_id.glow()) };
        chan.send(log).unwrap();
    }

    fn create_buffer(gl: &Gl, chan: &GenericSender<Option<WebGLBufferId>>) {
        let buffer = unsafe { gl.create_buffer() }
            .ok()
            .map(WebGLBufferId::from_glow);
        chan.send(buffer).unwrap();
    }

    fn create_framebuffer(gl: &Gl, chan: &GenericSender<Option<WebGLFramebufferId>>) {
        let framebuffer = unsafe { gl.create_framebuffer() }
            .ok()
            .map(WebGLFramebufferId::from_glow);
        chan.send(framebuffer).unwrap();
    }

    fn create_renderbuffer(gl: &Gl, chan: &GenericSender<Option<WebGLRenderbufferId>>) {
        let renderbuffer = unsafe { gl.create_renderbuffer() }
            .ok()
            .map(WebGLRenderbufferId::from_glow);
        chan.send(renderbuffer).unwrap();
    }

    fn create_texture(gl: &Gl, chan: &GenericSender<Option<WebGLTextureId>>) {
        let texture = unsafe { gl.create_texture() }
            .ok()
            .map(WebGLTextureId::from_glow);
        chan.send(texture).unwrap();
    }

    fn create_program(gl: &Gl, chan: &GenericSender<Option<WebGLProgramId>>) {
        let program = unsafe { gl.create_program() }
            .ok()
            .map(WebGLProgramId::from_glow);
        chan.send(program).unwrap();
    }

    fn create_shader(gl: &Gl, shader_type: u32, chan: &GenericSender<Option<WebGLShaderId>>) {
        let shader = unsafe { gl.create_shader(shader_type) }
            .ok()
            .map(WebGLShaderId::from_glow);
        chan.send(shader).unwrap();
    }

    fn create_vertex_array(gl: &Gl) -> Option<WebGLVertexArrayId> {
        let vao = unsafe { gl.create_vertex_array() }
            .ok()
            .map(WebGLVertexArrayId::from_glow);
        if vao.is_none() {
            let code = unsafe { gl.get_error() };
            warn!("Failed to create vertex array with error code {:x}", code);
        }
        vao
    }

    #[inline]
    fn bind_framebuffer(
        gl: &Gl,
        target: u32,
        request: WebGLFramebufferBindingRequest,
        ctx: &Context,
        device: &Device,
        state: &mut GLState,
    ) {
        let id = match request {
            WebGLFramebufferBindingRequest::Explicit(id) => Some(id.glow()),
            WebGLFramebufferBindingRequest::Default => {
                device
                    .context_surface_info(ctx)
                    .unwrap()
                    .expect("No surface attached!")
                    .framebuffer_object
            },
        };

        debug!("WebGLImpl::bind_framebuffer: {:?}", id);
        unsafe { gl.bind_framebuffer(target, id) };

        if (target == gl::FRAMEBUFFER) || (target == gl::DRAW_FRAMEBUFFER) {
            state.drawing_to_default_framebuffer =
                request == WebGLFramebufferBindingRequest::Default;
            state.restore_invariant(gl);
        }
    }

    #[inline]
    /// Translates a WebGL shader with ANGLE into the GLSL this context takes.
    fn translate_shader(gl: &Gl, source: &str, t: &ShaderTranslation) -> ShaderCompileInfo {
        static INITIALIZATION: std::sync::Once = std::sync::Once::new();
        INITIALIZATION.call_once(|| mozangle::shaders::initialize().unwrap());

        let mut params = BuiltInResources {
            MaxVertexAttribs: t.max_vertex_attribs,
            MaxVertexUniformVectors: t.max_vertex_uniform_vectors,
            MaxVertexTextureImageUnits: t.max_vertex_texture_image_units,
            MaxCombinedTextureImageUnits: t.max_combined_texture_image_units,
            MaxTextureImageUnits: t.max_texture_image_units,
            MaxFragmentUniformVectors: t.max_fragment_uniform_vectors,

            MaxVertexOutputVectors: t.max_vertex_output_vectors,
            MaxFragmentInputVectors: t.max_fragment_input_vectors,
            MaxVaryingVectors: t.max_varying_vectors,

            OES_standard_derivatives: t.oes_standard_derivatives as _,
            EXT_shader_texture_lod: t.ext_shader_texture_lod as _,
            EXT_frag_depth: t.ext_frag_depth as _,
            ANGLE_multi_draw: t.angle_multi_draw as _,

            FragmentPrecisionHigh: 1,
            ..Default::default()
        };
        if t.webgl2 {
            params.MinProgramTexelOffset = t.min_program_texel_offset;
            params.MaxProgramTexelOffset = t.max_program_texel_offset;
            params.MaxDrawBuffers = t.max_draw_buffers;
        }

        let is_gles = gl.version().is_embedded;
        let validator = if !t.webgl2 {
            let output_format = if is_gles { Output::Essl } else { Output::Glsl };
            ShaderValidator::for_webgl(t.shader_type, output_format, &params)
        } else {
            let output_format = if is_gles {
                Output::Essl
            } else {
                let glsl_version = WebGLThread::get_glsl_version(gl);
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
            ShaderValidator::for_webgl2(t.shader_type, output_format, &params)
        };
        let Some(validator) = validator else {
            return ShaderCompileInfo {
                object_code: None,
                info_log: String::new(),
            };
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

        let object_code = match validator.compile(&[source], options) {
            Ok(()) => {
                let object_code = validator.object_code();
                debug!("Shader translated: {}", object_code);
                Some(object_code)
            },
            Err(error) => {
                debug!("Shader translation failed: {}", error);
                None
            },
        };
        ShaderCompileInfo {
            object_code,
            info_log: validator.info_log(),
        }
    }

    fn compile_shader(gl: &Gl, shader_id: WebGLShaderId, source: &str) {
        unsafe {
            gl.shader_source(shader_id.glow(), source);
            gl.compile_shader(shader_id.glow());
        }
    }
}

/// ANGLE adds a `_u` prefix to variable names:
///
/// <https://chromium.googlesource.com/angle/angle/+/855d964bd0d05f6b2cb303f625506cf53d37e94f>
///
/// To avoid hard-coding this we would need to use the `sh::GetAttributes` and `sh::GetUniforms`
/// API to look up the `x.name` and `x.mappedName` members.
const ANGLE_NAME_PREFIX: &str = "_u";

/// Adds `_u` prefix to variable names. Built-ins (`gl_Position`, `gl_PointSize`, which transform
/// feedback may record) keep their names.
fn to_name_in_compiled_shader(s: &str) -> String {
    map_dot_separated(s, |s, mapped| {
        if !s.starts_with("gl_") {
            mapped.push_str(ANGLE_NAME_PREFIX);
        }
        mapped.push_str(s);
    })
}

/// Removes `_u` prefix from variable names
fn from_name_in_compiled_shader(s: &str) -> String {
    map_dot_separated(s, |s, mapped| {
        mapped.push_str(if let Some(stripped) = s.strip_prefix(ANGLE_NAME_PREFIX) {
            stripped
        } else {
            s
        })
    })
}

fn map_dot_separated<F: Fn(&str, &mut String)>(s: &str, f: F) -> String {
    let mut iter = s.split('.');
    let mut mapped = String::new();
    f(iter.next().unwrap(), &mut mapped);
    for s in iter {
        mapped.push('.');
        f(s, &mut mapped);
    }
    mapped
}

#[expect(clippy::too_many_arguments)]
fn prepare_pixels(
    internal_format: TexFormat,
    data_type: TexDataType,
    size: Size2D<u32>,
    unpacking_alignment: u32,
    alpha_treatment: Option<AlphaTreatment>,
    y_axis_treatment: YAxisTreatment,
    pixel_format: Option<PixelFormat>,
    mut pixels: Cow<[u8]>,
) -> Cow<[u8]> {
    match alpha_treatment {
        Some(AlphaTreatment::Premultiply) => {
            if let Some(pixel_format) = pixel_format {
                match pixel_format {
                    PixelFormat::BGRA8 | PixelFormat::RGBA8 => {},
                    _ => unimplemented!("unsupported pixel format ({:?})", pixel_format),
                }
                premultiply_inplace(TexFormat::RGBA, TexDataType::UnsignedByte, pixels.to_mut());
            } else {
                premultiply_inplace(internal_format, data_type, pixels.to_mut());
            }
        },
        Some(AlphaTreatment::Unmultiply) => {
            assert!(pixel_format.is_some());
            unmultiply_inplace::<false>(pixels.to_mut());
        },
        None => {},
    }

    if let Some(pixel_format) = pixel_format {
        pixels = image_to_tex_image_data(
            pixel_format,
            internal_format,
            data_type,
            pixels.into_owned(),
        )
        .into();
    }

    if y_axis_treatment == YAxisTreatment::Flipped {
        // FINISHME: Consider doing premultiply and flip in a single mutable Vec.
        pixels = flip_pixels_y(
            internal_format,
            data_type,
            size.width as usize,
            size.height as usize,
            unpacking_alignment as usize,
            pixels.into_owned(),
        )
        .into();
    }

    pixels
}

/// Components per pixel of a readPixels format. glow's components_per_format panics on the
/// *_INTEGER formats, which WebGL 2 reads integer framebuffers with.
fn read_components(format: u32) -> usize {
    match format {
        gl::RED_INTEGER | gl::GREEN_INTEGER | gl::BLUE_INTEGER => 1,
        gl::RG_INTEGER => 2,
        gl::RGB_INTEGER => 3,
        gl::RGBA_INTEGER => 4,
        _ => components_per_format(format),
    }
}

/// RED_BITS ... STENCIL_BITS of the bound draw framebuffer, from its attachments.
fn framebuffer_bits(gl: &Gl, param: webgl::ParameterInt) -> i32 {
    use webgl::ParameterInt as P;
    let default_framebuffer = unsafe { gl.get_parameter_i32(gl::DRAW_FRAMEBUFFER_BINDING) } == 0;
    let color = if default_framebuffer {
        gl::BACK_LEFT
    } else {
        gl::COLOR_ATTACHMENT0
    };
    let (attachment, pname) = match param {
        P::RedBits => (color, gl::FRAMEBUFFER_ATTACHMENT_RED_SIZE),
        P::GreenBits => (color, gl::FRAMEBUFFER_ATTACHMENT_GREEN_SIZE),
        P::BlueBits => (color, gl::FRAMEBUFFER_ATTACHMENT_BLUE_SIZE),
        P::AlphaBits => (color, gl::FRAMEBUFFER_ATTACHMENT_ALPHA_SIZE),
        P::DepthBits if default_framebuffer => (gl::DEPTH, gl::FRAMEBUFFER_ATTACHMENT_DEPTH_SIZE),
        P::DepthBits => (gl::DEPTH_ATTACHMENT, gl::FRAMEBUFFER_ATTACHMENT_DEPTH_SIZE),
        P::StencilBits if default_framebuffer => {
            (gl::STENCIL, gl::FRAMEBUFFER_ATTACHMENT_STENCIL_SIZE)
        },
        _ => (
            gl::STENCIL_ATTACHMENT,
            gl::FRAMEBUFFER_ATTACHMENT_STENCIL_SIZE,
        ),
    };
    unsafe {
        let object_type = gl.get_framebuffer_attachment_parameter_i32(
            gl::DRAW_FRAMEBUFFER,
            attachment,
            gl::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE,
        );
        if object_type == gl::NONE as i32 {
            return 0;
        }
        gl.get_framebuffer_attachment_parameter_i32(gl::DRAW_FRAMEBUFFER, attachment, pname)
    }
}

/// The glow argument for a compressed sub-image upload.
fn compressed_unpack_data(data: &CompressedTexData) -> glow::CompressedPixelUnpackData<'_> {
    match data {
        CompressedTexData::Bytes(bytes) => glow::CompressedPixelUnpackData::Slice(bytes),
        CompressedTexData::PixelUnpackBuffer { offset, size } => {
            glow::CompressedPixelUnpackData::BufferRange(*offset..*offset + *size)
        },
    }
}

/// Runs a compressed full-image upload with client bytes. glow's compressedTexImage2D/3D take
/// only a slice, so a PIXEL_UNPACK_BUFFER source is read back (glMapBufferRange works on GL 3.0+
/// and GLES 3.0) and uploaded with the buffer unbound, then rebound.
fn with_compressed_bytes(gl: &Gl, data: &CompressedTexData, upload: impl FnOnce(&[u8])) {
    let (offset, size) = match data {
        CompressedTexData::Bytes(bytes) => return upload(bytes),
        CompressedTexData::PixelUnpackBuffer { offset, size } => (*offset as i32, *size as i32),
    };
    unsafe {
        let bound = gl.get_parameter_i32(gl::PIXEL_UNPACK_BUFFER_BINDING);
        let Some(buffer) = NonZeroU32::new(bound as u32).map(glow::NativeBuffer) else {
            return;
        };
        let mut bytes = vec![0u8; size as usize];
        if size > 0 {
            let ptr = gl.map_buffer_range(gl::PIXEL_UNPACK_BUFFER, offset, size, gl::MAP_READ_BIT);
            if ptr.is_null() {
                return;
            }
            std::ptr::copy_nonoverlapping(ptr, bytes.as_mut_ptr(), size as usize);
            gl.unmap_buffer(gl::PIXEL_UNPACK_BUFFER);
        }
        gl.bind_buffer(gl::PIXEL_UNPACK_BUFFER, None);
        upload(&bytes);
        gl.bind_buffer(gl::PIXEL_UNPACK_BUFFER, Some(buffer));
    }
}

/// Like [`prepare_pixels`], for a 3D upload of `depth` slices of `size` each, stored one after
/// another. Alpha and format conversion work per pixel; a Y flip is applied to each slice on its
/// own (the WebGL 2 rule for TexImageSource uploads into 3D textures and 2D arrays).
#[allow(clippy::too_many_arguments)]
fn prepare_pixels_3d(
    internal_format: TexFormat,
    data_type: TexDataType,
    size: Size2D<u32>,
    depth: u32,
    unpacking_alignment: u32,
    alpha_treatment: Option<AlphaTreatment>,
    y_axis_treatment: YAxisTreatment,
    pixel_format: Option<PixelFormat>,
    pixels: Cow<[u8]>,
) -> Cow<[u8]> {
    if depth <= 1 {
        return prepare_pixels(
            internal_format,
            data_type,
            size,
            unpacking_alignment,
            alpha_treatment,
            y_axis_treatment,
            pixel_format,
            pixels,
        );
    }
    let all_slices = Size2D::new(size.width, size.height * depth);
    let pixels = prepare_pixels(
        internal_format,
        data_type,
        all_slices,
        unpacking_alignment,
        alpha_treatment,
        YAxisTreatment::AsIs,
        pixel_format,
        pixels,
    );
    if y_axis_treatment != YAxisTreatment::Flipped || size.height == 0 {
        return pixels;
    }
    let cpp = (data_type.element_size() * internal_format.components() /
        data_type.components_per_element()) as usize;
    let alignment = unpacking_alignment as usize;
    let stride = (size.width as usize * cpp + alignment - 1) & !(alignment - 1);
    let slice_len = stride * size.height as usize;
    let mut out = Vec::with_capacity(pixels.len());
    for z in 0..depth as usize {
        let start = z * slice_len;
        let end = (start + slice_len).min(pixels.len());
        let mut slice = pixels[start..end].to_vec();
        slice.resize(slice_len, 0);
        out.extend(flip_pixels_y(
            internal_format,
            data_type,
            size.width as usize,
            size.height as usize,
            alignment,
            slice,
        ));
    }
    out.into()
}

/// Translates an image in rgba8 (red in the first byte) format to
/// the format that was requested of TexImage.
///
/// Only the unsized format (which channels) and the type matter: unsigned bytes are copied,
/// float and half-float types get values normalized to [0, 1] (as WebGL specifies for image
/// sources), and packed types are packed. Integer formats take the bytes as they are.
fn image_to_tex_image_data(
    pixel_format: PixelFormat,
    format: TexFormat,
    data_type: TexDataType,
    mut pixels: Vec<u8>,
) -> Vec<u8> {
    match pixel_format {
        PixelFormat::BGRA8 => pixels::rgba8_byte_swap_colors_inplace(&mut pixels),
        PixelFormat::RGBA8 => {},
        _ => unimplemented!("unsupported pixel format ({:?})", pixel_format),
    }

    let channels: &[usize] = match format.to_unsized() {
        TexFormat::Red | TexFormat::RedInteger | TexFormat::Luminance => &[0],
        TexFormat::Alpha => &[3],
        TexFormat::RG | TexFormat::RGInteger => &[0, 1],
        TexFormat::LuminanceAlpha => &[0, 3],
        TexFormat::RGB | TexFormat::RGBInteger => &[0, 1, 2],
        _ => &[0, 1, 2, 3],
    };
    let pixel_count = pixels.len() / 4;
    let unit = |v: u8| v as f32 / 255.0;
    let mut out = Vec::<u8>::with_capacity(pixel_count * 16);
    for p in pixels.chunks_exact(4) {
        match data_type {
            TexDataType::UnsignedByte => out.extend(channels.iter().map(|&c| p[c])),
            TexDataType::Float => {
                for &c in channels {
                    out.write_f32::<NativeEndian>(unit(p[c])).unwrap();
                }
            },
            TexDataType::HalfFloat => {
                for &c in channels {
                    out.write_u16::<NativeEndian>(f16::from_f32(unit(p[c])).to_bits())
                        .unwrap();
                }
            },
            TexDataType::UnsignedShort565 => {
                let v = ((p[0] as u16 >> 3) << 11) | ((p[1] as u16 >> 2) << 5) | (p[2] as u16 >> 3);
                out.write_u16::<NativeEndian>(v).unwrap();
            },
            TexDataType::UnsignedShort4444 => {
                let v = ((p[0] as u16 >> 4) << 12) |
                    ((p[1] as u16 >> 4) << 8) |
                    ((p[2] as u16 >> 4) << 4) |
                    (p[3] as u16 >> 4);
                out.write_u16::<NativeEndian>(v).unwrap();
            },
            TexDataType::UnsignedShort5551 => {
                let v = ((p[0] as u16 >> 3) << 11) |
                    ((p[1] as u16 >> 3) << 6) |
                    ((p[2] as u16 >> 3) << 1) |
                    (p[3] as u16 >> 7);
                out.write_u16::<NativeEndian>(v).unwrap();
            },
            TexDataType::UnsignedInt2101010Rev => {
                let ten = |v: u8| ((v as u32 * 1023 + 127) / 255) & 0x3ff;
                let v =
                    ten(p[0]) | (ten(p[1]) << 10) | (ten(p[2]) << 20) | ((p[3] as u32 >> 6) << 30);
                out.write_u32::<NativeEndian>(v).unwrap();
            },
            TexDataType::UnsignedInt10f11f11fRev => {
                // Unsigned 11- and 10-bit floats share the half float's 5-bit exponent; drop
                // mantissa bits (inputs are in [0, 1], so never negative).
                let half = |v: u8| f16::from_f32(unit(v)).to_bits() as u32;
                let v = (half(p[0]) >> 4) | ((half(p[1]) >> 4) << 11) | ((half(p[2]) >> 5) << 22);
                out.write_u32::<NativeEndian>(v).unwrap();
            },
            TexDataType::UnsignedInt5999Rev => {
                out.write_u32::<NativeEndian>(rgb9_e5(unit(p[0]), unit(p[1]), unit(p[2])))
                    .unwrap();
            },
            // Validation keeps other types away from image sources.
            _ => unreachable!("Unsupported formats {:?} {:?}", format, data_type),
        }
    }
    out
}

/// Encodes an RGB colour as GL_RGB9_E5 (shared exponent), per the EXT_texture_shared_exponent
/// reference algorithm.
fn rgb9_e5(r: f32, g: f32, b: f32) -> u32 {
    const MANTISSA_BITS: i32 = 9;
    const EXP_BIAS: i32 = 15;
    const MAX_EXP: i32 = 31;
    let max_value = ((1 << MANTISSA_BITS) - 1) as f32 / (1 << MANTISSA_BITS) as f32 *
        2f32.powi(MAX_EXP - EXP_BIAS);
    let clamp = |v: f32| v.clamp(0.0, max_value);
    let (r, g, b) = (clamp(r), clamp(g), clamp(b));
    let max = r.max(g).max(b);
    let mut exp =
        (max.max(f32::MIN_POSITIVE).log2().floor() as i32).max(-EXP_BIAS - 1) + 1 + EXP_BIAS;
    let mut denom = 2f32.powi(exp - EXP_BIAS - MANTISSA_BITS);
    if (max / denom + 0.5).floor() as i32 == 1 << MANTISSA_BITS {
        denom *= 2.0;
        exp += 1;
    }
    let m = |v: f32| ((v / denom + 0.5).floor() as u32) & 0x1ff;
    m(r) | (m(g) << 9) | (m(b) << 18) | ((exp as u32 & 0x1f) << 27)
}

fn premultiply_inplace(format: TexFormat, data_type: TexDataType, pixels: &mut [u8]) {
    match (format, data_type) {
        (TexFormat::RGBA, TexDataType::UnsignedByte) => {
            pixels::rgba8_premultiply_inplace(pixels);
        },
        (TexFormat::LuminanceAlpha, TexDataType::UnsignedByte) => {
            for la in pixels.chunks_mut(2) {
                la[0] = pixels::multiply_u8_color(la[0], la[1]);
            }
        },
        (TexFormat::RGBA, TexDataType::UnsignedShort5551) => {
            for rgba in pixels.chunks_mut(2) {
                if NativeEndian::read_u16(rgba) & 1 == 0 {
                    NativeEndian::write_u16(rgba, 0);
                }
            }
        },
        (TexFormat::RGBA, TexDataType::UnsignedShort4444) => {
            for rgba in pixels.chunks_mut(2) {
                let pix = NativeEndian::read_u16(rgba);
                let extend_to_8_bits = |val| (val | (val << 4)) as u8;
                let r = extend_to_8_bits((pix >> 12) & 0x0f);
                let g = extend_to_8_bits((pix >> 8) & 0x0f);
                let b = extend_to_8_bits((pix >> 4) & 0x0f);
                let a = extend_to_8_bits(pix & 0x0f);
                NativeEndian::write_u16(
                    rgba,
                    (((pixels::multiply_u8_color(r, a) & 0xf0) as u16) << 8) |
                        (((pixels::multiply_u8_color(g, a) & 0xf0) as u16) << 4) |
                        ((pixels::multiply_u8_color(b, a) & 0xf0) as u16) |
                        ((a & 0x0f) as u16),
                );
            }
        },
        // Other formats don't have alpha, so return their data untouched.
        _ => {},
    }
}

/// Flips the pixels in the Vec on the Y axis.
fn flip_pixels_y(
    internal_format: TexFormat,
    data_type: TexDataType,
    width: usize,
    height: usize,
    unpacking_alignment: usize,
    pixels: Vec<u8>,
) -> Vec<u8> {
    let cpp = (data_type.element_size() * internal_format.components() /
        data_type.components_per_element()) as usize;

    let stride = (width * cpp + unpacking_alignment - 1) & !(unpacking_alignment - 1);

    let mut flipped = Vec::<u8>::with_capacity(pixels.len());

    for y in 0..height {
        let flipped_y = height - 1 - y;
        let start = flipped_y * stride;

        flipped.extend_from_slice(&pixels[start..(start + width * cpp)]);
        flipped.extend(vec![0u8; stride - width * cpp]);
    }

    flipped
}

// Clamp a size to the current GL context's max viewport
fn clamp_viewport(gl: &Gl, size: Size2D<u32>) -> Size2D<u32> {
    let mut max_viewport = [i32::MAX, i32::MAX];
    let mut max_renderbuffer = [i32::MAX];

    unsafe {
        gl.get_parameter_i32_slice(gl::MAX_VIEWPORT_DIMS, &mut max_viewport);
        gl.get_parameter_i32_slice(gl::MAX_RENDERBUFFER_SIZE, &mut max_renderbuffer);
    }
    Size2D::new(
        size.width
            .min(max_viewport[0] as u32)
            .min(max_renderbuffer[0] as u32)
            .max(1),
        size.height
            .min(max_viewport[1] as u32)
            .min(max_renderbuffer[0] as u32)
            .max(1),
    )
}

trait ToSurfmanVersion {
    fn to_surfman_version(self, api_type: GlType) -> GLVersion;
}

impl ToSurfmanVersion for WebGLVersion {
    fn to_surfman_version(self, api_type: GlType) -> GLVersion {
        if api_type == GlType::Gles {
            return GLVersion::new(3, 0);
        }
        match self {
            // We make use of GL_PACK_PIXEL_BUFFER, which needs at least GL2.1
            // We make use of compatibility mode, which needs at most GL3.0
            WebGLVersion::WebGL1 => GLVersion::new(2, 1),
            // The WebGL2 conformance tests use std140 layout, which needs at GL3.1
            WebGLVersion::WebGL2 => GLVersion::new(3, 2),
        }
    }
}

trait SurfmanContextAttributeFlagsConvert {
    fn to_surfman_context_attribute_flags(
        &self,
        webgl_version: WebGLVersion,
        api_type: GlType,
    ) -> ContextAttributeFlags;
}

impl SurfmanContextAttributeFlagsConvert for GLContextAttributes {
    fn to_surfman_context_attribute_flags(
        &self,
        webgl_version: WebGLVersion,
        api_type: GlType,
    ) -> ContextAttributeFlags {
        let mut flags = ContextAttributeFlags::empty();
        flags.set(ContextAttributeFlags::ALPHA, self.alpha);
        flags.set(ContextAttributeFlags::DEPTH, self.depth);
        flags.set(ContextAttributeFlags::STENCIL, self.stencil);
        if (webgl_version == WebGLVersion::WebGL1) && (api_type == GlType::Gl) {
            flags.set(ContextAttributeFlags::COMPATIBILITY_PROFILE, true);
        }
        flags
    }
}

bitflags! {
    struct FramebufferRebindingFlags: u8 {
        const REBIND_READ_FRAMEBUFFER = 0x1;
        const REBIND_DRAW_FRAMEBUFFER = 0x2;
    }
}

struct FramebufferRebindingInfo {
    flags: FramebufferRebindingFlags,
    viewport: [GLint; 4],
}

impl FramebufferRebindingInfo {
    fn detect(device: &Device, context: &Context, gl: &Gl) -> FramebufferRebindingInfo {
        unsafe {
            let read_framebuffer = gl.get_parameter_framebuffer(gl::READ_FRAMEBUFFER_BINDING);
            let draw_framebuffer = gl.get_parameter_framebuffer(gl::DRAW_FRAMEBUFFER_BINDING);

            let context_surface_framebuffer = device
                .context_surface_info(context)
                .unwrap()
                .unwrap()
                .framebuffer_object;

            let mut flags = FramebufferRebindingFlags::empty();
            if context_surface_framebuffer == read_framebuffer {
                flags.insert(FramebufferRebindingFlags::REBIND_READ_FRAMEBUFFER);
            }
            if context_surface_framebuffer == draw_framebuffer {
                flags.insert(FramebufferRebindingFlags::REBIND_DRAW_FRAMEBUFFER);
            }

            let mut viewport = [0; 4];
            gl.get_parameter_i32_slice(gl::VIEWPORT, &mut viewport);

            FramebufferRebindingInfo { flags, viewport }
        }
    }

    fn apply(self, device: &Device, context: &Context, gl: &Gl) {
        if self.flags.is_empty() {
            return;
        }

        let context_surface_framebuffer = device
            .context_surface_info(context)
            .unwrap()
            .unwrap()
            .framebuffer_object;
        if self
            .flags
            .contains(FramebufferRebindingFlags::REBIND_READ_FRAMEBUFFER)
        {
            unsafe { gl.bind_framebuffer(gl::READ_FRAMEBUFFER, context_surface_framebuffer) };
        }
        if self
            .flags
            .contains(FramebufferRebindingFlags::REBIND_DRAW_FRAMEBUFFER)
        {
            unsafe { gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, context_surface_framebuffer) };
        }

        unsafe {
            gl.viewport(
                self.viewport[0],
                self.viewport[1],
                self.viewport[2],
                self.viewport[3],
            )
        };
    }
}

/// Size in bytes of one element of a transform feedback varying type.
fn transform_feedback_type_bytes(type_: u32) -> u32 {
    match type_ {
        gl::FLOAT | gl::INT | gl::UNSIGNED_INT => 4,
        gl::FLOAT_VEC2 | gl::INT_VEC2 | gl::UNSIGNED_INT_VEC2 => 8,
        gl::FLOAT_VEC3 | gl::INT_VEC3 | gl::UNSIGNED_INT_VEC3 => 12,
        gl::FLOAT_VEC4 | gl::INT_VEC4 | gl::UNSIGNED_INT_VEC4 | gl::FLOAT_MAT2 => 16,
        gl::FLOAT_MAT2x3 | gl::FLOAT_MAT3x2 => 24,
        gl::FLOAT_MAT2x4 | gl::FLOAT_MAT4x2 => 32,
        gl::FLOAT_MAT3 => 36,
        gl::FLOAT_MAT3x4 | gl::FLOAT_MAT4x3 => 48,
        gl::FLOAT_MAT4 => 64,
        _ => 16,
    }
}

/// The texture target texture parameters are set on for an image target: the cube map for
/// its faces.
fn texture_parameter_target(target: u32) -> u32 {
    match target {
        gl::TEXTURE_CUBE_MAP_POSITIVE_X |
        gl::TEXTURE_CUBE_MAP_NEGATIVE_X |
        gl::TEXTURE_CUBE_MAP_POSITIVE_Y |
        gl::TEXTURE_CUBE_MAP_NEGATIVE_Y |
        gl::TEXTURE_CUBE_MAP_POSITIVE_Z |
        gl::TEXTURE_CUBE_MAP_NEGATIVE_Z => gl::TEXTURE_CUBE_MAP,
        other => other,
    }
}

/// The R / RG transfer format standing in for ALPHA, LUMINANCE and LUMINANCE_ALPHA on core
/// profile contexts (same bytes per pixel, so client data needs no conversion).
fn is_occlusion_query(target: u32) -> bool {
    matches!(
        target,
        gl::ANY_SAMPLES_PASSED | gl::ANY_SAMPLES_PASSED_CONSERVATIVE
    )
}

fn luminance_transfer_format(state: &GLState, format: TexFormat) -> u32 {
    if !state.emulate_luminance {
        return format.as_gl_constant();
    }
    match format.as_gl_constant() {
        gl::ALPHA | gl::LUMINANCE => gl::RED,
        gl::LUMINANCE_ALPHA => gl::RG,
        other => other,
    }
}

/// Internal and transfer formats for a texImage: on core profile contexts the legacy
/// formats become R / RG textures, and the texture gets the swizzle that makes them sample
/// as the format they stand for. Other formats reset the swizzle, as the texture object may
/// have held an emulated format before.
fn luminance_formats(
    state: &GLState,
    gl: &Gl,
    target: u32,
    internal_format: TexFormat,
    format: TexFormat,
    data_type: u32,
) -> (u32, u32) {
    if !state.emulate_luminance {
        return (internal_format.as_gl_constant(), format.as_gl_constant());
    }
    let (two_channels, swizzle) = match internal_format.as_gl_constant() {
        gl::ALPHA => (false, [gl::ZERO, gl::ZERO, gl::ZERO, gl::RED]),
        gl::LUMINANCE => (false, [gl::RED, gl::RED, gl::RED, gl::ONE]),
        gl::LUMINANCE_ALPHA => (true, [gl::RED, gl::RED, gl::RED, gl::GREEN]),
        _ => {
            set_texture_swizzle(gl, target, [gl::RED, gl::GREEN, gl::BLUE, gl::ALPHA]);
            return (internal_format.as_gl_constant(), format.as_gl_constant());
        },
    };
    let internal_format = match (data_type, two_channels) {
        (gl::FLOAT, false) => gl::R32F,
        (gl::FLOAT, true) => gl::RG32F,
        (gl::HALF_FLOAT | 0x8D61, false) => gl::R16F,
        (gl::HALF_FLOAT | 0x8D61, true) => gl::RG16F,
        (_, false) => gl::R8,
        (_, true) => gl::RG8,
    };
    set_texture_swizzle(gl, target, swizzle);
    (internal_format, luminance_transfer_format(state, format))
}

fn set_texture_swizzle(gl: &Gl, target: u32, swizzle: [u32; 4]) {
    let target = texture_parameter_target(target);
    let names = [
        gl::TEXTURE_SWIZZLE_R,
        gl::TEXTURE_SWIZZLE_G,
        gl::TEXTURE_SWIZZLE_B,
        gl::TEXTURE_SWIZZLE_A,
    ];
    for (name, value) in names.into_iter().zip(swizzle) {
        unsafe { gl.tex_parameter_i32(target, name, value as i32) };
    }
}

/// Which emulated legacy format copies into the texture bound to `target` must produce,
/// when a plain copy would take the wrong channels: ALPHA (alpha into R) or
/// LUMINANCE_ALPHA (red and alpha into RG). LUMINANCE copies red into R and needs nothing.
#[derive(Clone, Copy)]
enum LuminanceCopy {
    Alpha,
    LuminanceAlpha,
}

fn luminance_copy_kind(state: &GLState, gl: &Gl, target: u32) -> Option<LuminanceCopy> {
    if !state.emulate_luminance {
        return None;
    }
    let target = texture_parameter_target(target);
    let swizzle_r = unsafe { gl.get_tex_parameter_i32(target, gl::TEXTURE_SWIZZLE_R) } as u32;
    let swizzle_a = unsafe { gl.get_tex_parameter_i32(target, gl::TEXTURE_SWIZZLE_A) } as u32;
    match (swizzle_r, swizzle_a) {
        (gl::ZERO, gl::RED) => Some(LuminanceCopy::Alpha),
        (gl::RED, gl::GREEN) => Some(LuminanceCopy::LuminanceAlpha),
        _ => None,
    }
}

/// Copies a framebuffer region into an emulated ALPHA / LUMINANCE_ALPHA texture through
/// client memory, picking the channels a GL copy can't. Pixels outside the read framebuffer
/// come out as zero.
unsafe fn copy_luminance_via_cpu(
    gl: &Gl,
    target: u32,
    level: i32,
    (xoffset, yoffset, zoffset): (i32, i32, Option<i32>),
    (x, y, width, height): (i32, i32, i32, i32),
    kind: LuminanceCopy,
) {
    if width <= 0 || height <= 0 {
        return;
    }
    let (width_u, height_u) = (width as usize, height as usize);
    let mut rgba = vec![0u8; width_u * height_u * 4];
    unsafe {
        let pack_buffer = gl.get_parameter_buffer(gl::PIXEL_PACK_BUFFER_BINDING);
        let unpack_buffer = gl.get_parameter_buffer(gl::PIXEL_UNPACK_BUFFER_BINDING);
        let pack_alignment = gl.get_parameter_i32(gl::PACK_ALIGNMENT);
        let unpack_alignment = gl.get_parameter_i32(gl::UNPACK_ALIGNMENT);
        gl.bind_buffer(gl::PIXEL_PACK_BUFFER, None);
        gl.bind_buffer(gl::PIXEL_UNPACK_BUFFER, None);
        gl.pixel_store_i32(gl::PACK_ALIGNMENT, 4);
        gl.pixel_store_i32(gl::UNPACK_ALIGNMENT, 1);
        gl.read_pixels(
            x,
            y,
            width,
            height,
            gl::RGBA,
            gl::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(&mut rgba)),
        );
        let (format, data): (u32, Vec<u8>) = match kind {
            LuminanceCopy::Alpha => (gl::RED, rgba.chunks_exact(4).map(|p| p[3]).collect()),
            LuminanceCopy::LuminanceAlpha => (
                gl::RG,
                rgba.chunks_exact(4).flat_map(|p| [p[0], p[3]]).collect(),
            ),
        };
        match zoffset {
            Some(zoffset) => gl.tex_sub_image_3d(
                target,
                level,
                xoffset,
                yoffset,
                zoffset,
                width,
                height,
                1,
                format,
                gl::UNSIGNED_BYTE,
                PixelUnpackData::Slice(Some(&data)),
            ),
            None => gl.tex_sub_image_2d(
                target,
                level,
                xoffset,
                yoffset,
                width,
                height,
                format,
                gl::UNSIGNED_BYTE,
                PixelUnpackData::Slice(Some(&data)),
            ),
        }
        gl.bind_buffer(gl::PIXEL_PACK_BUFFER, pack_buffer);
        gl.bind_buffer(gl::PIXEL_UNPACK_BUFFER, unpack_buffer);
        gl.pixel_store_i32(gl::PACK_ALIGNMENT, pack_alignment);
        gl.pixel_store_i32(gl::UNPACK_ALIGNMENT, unpack_alignment);
    }
}

/// copyTexImage2D. WebGL defines pixels outside the read framebuffer as zero where GL leaves
/// them undefined, so a partly outside copy allocates a zeroed image and copies the inside
/// part. Copies into emulated legacy formats (see `luminance_formats`) allocate the R / RG
/// image and copy through `copy_luminance_via_cpu` when the channels need picking.
unsafe fn copy_tex_image_2d(
    state: &GLState,
    gl: &Gl,
    target: u32,
    level: i32,
    internal_format: u32,
    (x, y, width, height): (i32, i32, i32, i32),
    (read_width, read_height): (i32, i32),
) {
    let unsized_format = matches!(
        internal_format,
        gl::ALPHA | gl::LUMINANCE | gl::LUMINANCE_ALPHA | gl::RGB | gl::RGBA
    );
    let emulated = state.emulate_luminance &&
        matches!(
            internal_format,
            gl::ALPHA | gl::LUMINANCE | gl::LUMINANCE_ALPHA
        );
    let inside = x >= 0 &&
        y >= 0 &&
        x as i64 + width as i64 <= read_width as i64 &&
        y as i64 + height as i64 <= read_height as i64;
    if inside && !emulated {
        unsafe { gl.copy_tex_image_2d(target, level, internal_format, x, y, width, height, 0) };
        return;
    }
    if !unsized_format {
        // Sized formats would need their own transfer format and type for the zeroed image;
        // leave the outside pixels to the GL.
        unsafe { gl.copy_tex_image_2d(target, level, internal_format, x, y, width, height, 0) };
        return;
    }
    let format = TexFormat::from_gl_constant(internal_format).unwrap_or(TexFormat::RGBA);
    let (gl_internal_format, gl_format) =
        luminance_formats(state, gl, target, format, format, gl::UNSIGNED_BYTE);
    let bytes_per_pixel = match gl_format {
        gl::RED | gl::ALPHA | gl::LUMINANCE => 1,
        gl::RG | gl::LUMINANCE_ALPHA => 2,
        gl::RGB => 3,
        _ => 4,
    };
    let zeros = vec![0u8; width.max(0) as usize * height.max(0) as usize * bytes_per_pixel];
    unsafe {
        let unpack_buffer = gl.get_parameter_buffer(gl::PIXEL_UNPACK_BUFFER_BINDING);
        let unpack_alignment = gl.get_parameter_i32(gl::UNPACK_ALIGNMENT);
        gl.bind_buffer(gl::PIXEL_UNPACK_BUFFER, None);
        gl.pixel_store_i32(gl::UNPACK_ALIGNMENT, 1);
        gl.tex_image_2d(
            target,
            level,
            gl_internal_format as i32,
            width,
            height,
            0,
            gl_format,
            gl::UNSIGNED_BYTE,
            PixelUnpackData::Slice(Some(&zeros)),
        );
        gl.bind_buffer(gl::PIXEL_UNPACK_BUFFER, unpack_buffer);
        gl.pixel_store_i32(gl::UNPACK_ALIGNMENT, unpack_alignment);
    }
    // The part of the source inside the read framebuffer.
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x as i64 + width as i64).min(read_width as i64) as i32;
    let y1 = (y as i64 + height as i64).min(read_height as i64) as i32;
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let destination = (x0 - x, y0 - y, None);
    let source = (x0, y0, x1 - x0, y1 - y0);
    let kind = match (emulated, internal_format) {
        (true, gl::ALPHA) => Some(LuminanceCopy::Alpha),
        (true, gl::LUMINANCE_ALPHA) => Some(LuminanceCopy::LuminanceAlpha),
        _ => None,
    };
    match kind {
        Some(kind) => unsafe {
            copy_luminance_via_cpu(gl, target, level, destination, source, kind)
        },
        None => unsafe {
            gl.copy_tex_sub_image_2d(
                target,
                level,
                destination.0,
                destination.1,
                source.0,
                source.1,
                source.2,
                source.3,
            )
        },
    }
}

/// Partii (perf stream): `PARTII_WEBGL_STATS` set logs WebGL context creation timing to stderr.
fn webgl_stats_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PARTII_WEBGL_STATS").is_some())
}

/// Partii (perf stream, measurement builds only): `PARTII_FRAME_PROF=1` logs, every 2 s, the WebGL thread's
/// wall time per second by message class (busy vs waiting for messages), for the frame's critical path.
mod frame_prof {
    use std::time::{Duration, Instant};

    use servo_canvas_traits::webgl::{WebGLCommand as C, WebGLMsg};

    pub(super) fn enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("PARTII_FRAME_PROF").is_some())
    }

    pub(super) fn trace() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PARTII_FRAME_PROF").is_ok_and(|v| v == "trace"))
    }

    /// `PARTII_FRAME_PROF=trace`: each WebGL message of 4 ms or more, on the wall clock the script trace uses.
    pub(super) fn trace_msg(class: usize, kind: &str, start: Instant) {
        if !trace() {
            return;
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        if ms >= 4.0 {
            let d = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let wall = (d.as_micros() % 10_000_000_000) as f64 / 1000.0 - ms;
            eprintln!("partii-trace webgl {}:{kind} at={wall:.1} ms={ms:.1}", NAMES[class]);
        }
    }

    pub(super) fn finish() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PARTII_FRAME_PROF").is_ok_and(|v| v == "finish"))
    }

    thread_local!(static RASTER: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) });

    pub(super) fn add_raster(d: Duration) {
        RASTER.set(RASTER.get() + d);
    }

    pub(super) const NAMES: [&str; 9] =
        ["swap", "draw", "clear", "upload", "buffer", "sync", "uniform", "state", "other"];

    pub(super) fn class_of(msg: &WebGLMsg) -> usize {
        let WebGLMsg::WebGLCommand(_, command, _) = msg else {
            return if matches!(msg, WebGLMsg::SwapBuffers(..)) { 0 } else { 8 };
        };
        match command {
            C::DrawArrays { .. } |
            C::DrawElements { .. } |
            C::DrawArraysInstanced { .. } |
            C::DrawElementsInstanced { .. } |
            C::DrawArraysInstancedBaseInstance { .. } |
            C::DrawElementsInstancedBaseVertexBaseInstance { .. } => 1,
            C::Clear(..) | C::ClearBufferfv(..) | C::ClearBufferiv(..) | C::ClearBufferuiv(..) |
            C::ClearBufferfi(..) | C::BlitFrameBuffer(..) => 2,
            C::TexImage2D { .. } |
            C::TexSubImage2D { .. } |
            C::TexImage3D { .. } |
            C::TexSubImage3D { .. } |
            C::CompressedTexImage2D { .. } |
            C::GenerateMipmap(..) |
            C::TexStorage2D(..) |
            C::RenderbufferStorage(..) |
            C::RenderbufferStorageMultisample(..) => 3,
            C::BufferData(..) | C::BufferSubData(..) => 4,
            C::ReadPixels(..) | C::ReadPixelsPP(..) | C::Finish(..) | C::GetProgramValidateStatus(..) => 5,
            C::Uniform1f(..) | C::Uniform1fv(..) | C::Uniform1i(..) | C::Uniform1iv(..) |
            C::Uniform2fv(..) | C::Uniform3fv(..) | C::Uniform4fv(..) | C::Uniform4f(..) |
            C::UniformMatrix3fv(..) | C::UniformMatrix4fv(..) => 6,
            _ => 7,
        }
    }

    #[derive(Default)]
    pub(super) struct Acc {
        since: Option<Instant>,
        last_end: Option<Instant>,
        idle: Duration,
        busy: [Duration; 9],
        count: [u32; 9],
        max_swap: Duration,
    }

    impl Acc {
        pub(super) fn record(&mut self, class: usize, start: Instant) {
            let now = Instant::now();
            let since = *self.since.get_or_insert(start);
            if let Some(last) = self.last_end {
                self.idle += start.saturating_duration_since(last);
            }
            self.last_end = Some(now);
            let d = now - start;
            self.busy[class] += d;
            self.count[class] += 1;
            if class == 0 {
                self.max_swap = self.max_swap.max(d);
            }
            let w = (now - since).as_secs_f64();
            if w >= 2.0 {
                let ms = |d: Duration| d.as_secs_f64() * 1000.0 / w;
                let total: Duration = self.busy.iter().sum();
                let mut line = format!(
                    "partii-webgl-prof: busy_ms_s={:.1} idle_ms_s={:.1} swaps_s={:.1} swap_max_ms={:.2}",
                    ms(total),
                    ms(self.idle),
                    self.count[0] as f64 / w,
                    self.max_swap.as_secs_f64() * 1000.0
                );
                line += &format!(" raster_in_swap_ms_s={:.1}", ms(RASTER.replace(Duration::ZERO)));
                for (i, name) in NAMES.iter().enumerate() {
                    line += &format!(" {name}_ms_s={:.1} {name}_n_s={:.0}", ms(self.busy[i]), self.count[i] as f64 / w);
                }
                eprintln!("{line}");
                *self = Acc::default();
                self.since = Some(now);
                self.last_end = Some(now);
            }
        }
    }
}

/// Partii (perf stream): `PARTII_WEBGL_STATS=cmd` also times each WebGL command (see handle_webgl_command).
fn webgl_cmd_stats_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PARTII_WEBGL_STATS").is_ok_and(|v| v == "cmd"))
}

/// Partii (perf stream): the time origin of PARTII_WEBGL_CMD lines (the first command timed).
fn webgl_stats_epoch() -> std::time::Instant {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(std::time::Instant::now)
}

/// Partii (perf stream): a short name for a WebGL command, without formatting its data.
fn command_kind(command: &WebGLCommand) -> &'static str {
    match command {
        WebGLCommand::TexImage2D { .. } => "TexImage2D",
        WebGLCommand::TexSubImage2D { .. } => "TexSubImage2D",
        WebGLCommand::TexImage3D { .. } => "TexImage3D",
        WebGLCommand::TexSubImage3D { .. } => "TexSubImage3D",
        WebGLCommand::CompressedTexImage2D { .. } => "CompressedTexImage2D",
        WebGLCommand::DrawArrays { .. } => "DrawArrays",
        WebGLCommand::DrawElements { .. } => "DrawElements",
        WebGLCommand::DrawArraysInstanced { .. } => "DrawArraysInstanced",
        WebGLCommand::DrawElementsInstanced { .. } => "DrawElementsInstanced",
        WebGLCommand::CopyTexImage2D { .. } => "CopyTexImage2D",
        WebGLCommand::CompileShader(..) => "CompileShader",
        WebGLCommand::LinkProgram(..) => "LinkProgram",
        WebGLCommand::GenObjectNames(..) => "GenObjectNames",
        WebGLCommand::CreateProgram(..) => "CreateProgram",
        WebGLCommand::CreateShader(..) => "CreateShader",
        WebGLCommand::BufferData(..) => "BufferData",
        WebGLCommand::BufferSubData(..) => "BufferSubData",
        WebGLCommand::Clear(..) => "Clear",
        WebGLCommand::GenerateMipmap(..) => "GenerateMipmap",
        WebGLCommand::ReadPixels(..) => "ReadPixels",
        WebGLCommand::ReadPixelsPP(..) => "ReadPixelsPP",
        WebGLCommand::CreateFramebuffer(..) => "CreateFramebuffer",
        WebGLCommand::CreateTexture(..) => "CreateTexture",
        WebGLCommand::CreateRenderbuffer(..) => "CreateRenderbuffer",
        WebGLCommand::RenderbufferStorage(..) => "RenderbufferStorage",
        WebGLCommand::RenderbufferStorageMultisample(..) => "RenderbufferStorageMultisample",
        WebGLCommand::TexStorage2D(..) => "TexStorage2D",
        WebGLCommand::GetProgramValidateStatus(..) => "GetProgramValidateStatus",
        WebGLCommand::UseProgram(..) => "UseProgram",
        WebGLCommand::BlitFrameBuffer(..) => "BlitFrameBuffer",
        WebGLCommand::FramebufferTexture2D(..) => "FramebufferTexture2D",
        WebGLCommand::Finish(..) => "Finish",
        WebGLCommand::GetShaderInfoLog(..) => "GetShaderInfoLog",
        WebGLCommand::GetProgramInfoLog(..) => "GetProgramInfoLog",
        _ => "other",
    }
}
