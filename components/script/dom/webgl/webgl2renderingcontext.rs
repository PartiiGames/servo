/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::{cmp, ptr};

use bitflags::bitflags;
use dom_struct::dom_struct;
use euclid::default::{Point2D, Rect, Size2D};
use js::context::{JSContext, NoGC};
use js::conversions::ToJSValConvertible;
use js::jsapi::{JSObject, Type};
use js::jsval::{BooleanValue, DoubleValue, Int32Value, NullValue, ObjectValue, UInt32Value};
use js::rust::{CustomAutoRooterGuard, HandleObject, MutableHandleObject, MutableHandleValue};
use js::typedarray::{ArrayBufferView, Float32, Int32, Uint32};
use pixels::{Alpha, Snapshot};
use script_bindings::interfaces::WebGL2RenderingContextHelpers;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_base::generic_channel::{self, GenericSharedMemory};
use servo_canvas_traits::webgl::WebGLError::*;
use servo_canvas_traits::webgl::{
    GLContextAttributes, InternalFormatParameter, TexDataType, TexFormat, WebGLCommand,
    WebGLContextId, WebGLFramebufferBindingRequest, WebGLResult, WebGLVersion, YAxisTreatment,
    webgl_channel,
};
use servo_config::pref;
use url::Host;
use webrender_api::ImageKey;

use super::validations::types::TexImageTarget;
use crate::canvas_context::CanvasContext;
#[cfg(feature = "webxr")]
use crate::dom::RootedPromise;
use crate::dom::bindings::buffer_source::{
    array_buffer_view_byte_len, array_buffer_view_bytes, array_buffer_view_bytes_mut,
    create_buffer_source, typed_array_to_vec,
};
use crate::dom::bindings::codegen::Bindings::WebGL2RenderingContextBinding::{
    WebGL2RenderingContextConstants as constants, WebGL2RenderingContextMethods,
};
use crate::dom::bindings::codegen::Bindings::WebGLRenderingContextBinding::{
    TexImageSource, WebGLContextAttributes, WebGLRenderingContextMethods,
};
use crate::dom::bindings::codegen::UnionTypes::{
    ArrayBufferViewOrArrayBuffer, Float32ArrayOrUnrestrictedFloatSequence,
    HTMLCanvasElementOrOffscreenCanvas as RootedHTMLCanvasElementOrOffscreenCanvas,
    Int32ArrayOrLongSequence, Uint32ArrayOrUnsignedLongSequence,
};
use crate::dom::bindings::error::{ErrorResult, Fallible};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::globalscope::GlobalScope;
#[cfg(feature = "webxr")]
use crate::dom::promise::Promise;
use crate::dom::webgl::validations::WebGLValidator;
use crate::dom::webgl::validations::tex_image_2d::{
    TexImage2DValidator, TexImage2DValidatorResult, TexStorageValidator, TexStorageValidatorResult,
    base_level_block_multiple, is_subimage_blockaligned, valid_s3tc_dimension,
};
use crate::dom::webgl::validations::tex_image_3d::{
    TexImage3DValidator, TexImage3DValidatorResult,
};
use crate::dom::webgl::webglactiveinfo::WebGLActiveInfo;
use crate::dom::webgl::webglbuffer::WebGLBuffer;
use crate::dom::webgl::webglframebuffer::{
    CompleteForRendering, WebGLFramebuffer, WebGLFramebufferAttachmentRoot,
};
use crate::dom::webgl::webglprogram::WebGLProgram;
use crate::dom::webgl::webglquery::WebGLQuery;
use crate::dom::webgl::webglrenderbuffer::WebGLRenderbuffer;
use crate::dom::webgl::webglrenderingcontext::{
    CompressedSource, DrawStateCache, Operation, TexPixels, TexSource, VertexAttrib,
    WebGLRenderingContext,
    array_buffer_type_to_sized_type, clip_copy_to_read_framebuffer, is_sampler_type, uniform_get,
    uniform_typed,
};
use crate::dom::webgl::webglsampler::{WebGLSampler, WebGLSamplerValue};
use crate::dom::webgl::webglshader::{FragOutputKind, WebGLShader};
use crate::dom::webgl::webglshaderprecisionformat::WebGLShaderPrecisionFormat;
use crate::dom::webgl::webglsync::WebGLSync;
use crate::dom::webgl::webgltexture::{TexCompression, TexCompressionValidation, WebGLTexture};
use crate::dom::webgl::webgltransformfeedback::{TransformFeedbackState, WebGLTransformFeedback};
use crate::dom::webgl::webgluniformlocation::WebGLUniformLocation;
use crate::dom::webgl::webglvertexarrayobject::WebGLVertexArrayObject;

#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
#[derive(JSTraceable, MallocSizeOf)]
pub(crate) struct IndexedBinding {
    buffer: MutNullableDom<WebGLBuffer>,
    start: Cell<i64>,
    size: Cell<i64>,
}

impl IndexedBinding {
    pub(crate) fn new() -> IndexedBinding {
        IndexedBinding {
            buffer: MutNullableDom::new(None),
            start: Cell::new(0),
            size: Cell::new(0),
        }
    }
}

#[dom_struct] // no need to report size here as it is reported as part of WebGLRenderingContext
pub(crate) struct WebGL2RenderingContext {
    reflector_: Reflector,
    base: Dom<WebGLRenderingContext>,
    occlusion_query: MutNullableDom<WebGLQuery>,
    primitives_query: MutNullableDom<WebGLQuery>,
    samplers: Box<[MutNullableDom<WebGLSampler>]>,
    bound_copy_read_buffer: MutNullableDom<WebGLBuffer>,
    bound_copy_write_buffer: MutNullableDom<WebGLBuffer>,
    bound_pixel_pack_buffer: MutNullableDom<WebGLBuffer>,
    bound_pixel_unpack_buffer: MutNullableDom<WebGLBuffer>,
    bound_transform_feedback_buffer: MutNullableDom<WebGLBuffer>,
    bound_uniform_buffer: MutNullableDom<WebGLBuffer>,
    indexed_uniform_buffer_bindings: Box<[IndexedBinding]>,
    indexed_transform_feedback_buffer_bindings: Box<[IndexedBinding]>,
    current_transform_feedback: MutNullableDom<WebGLTransformFeedback>,
    /// State of the default transform feedback object (bound when
    /// `current_transform_feedback` is null).
    default_transform_feedback: TransformFeedbackState,
    texture_pack_row_length: Cell<usize>,
    texture_pack_skip_pixels: Cell<usize>,
    texture_pack_skip_rows: Cell<usize>,
    /// UNPACK_ROW_LENGTH, UNPACK_IMAGE_HEIGHT, UNPACK_SKIP_PIXELS, UNPACK_SKIP_ROWS and
    /// UNPACK_SKIP_IMAGES. Kept here, not in GL: client data and image sources are repacked
    /// before they reach the WebGL thread, and PBO uploads set them in GL around the upload.
    #[no_trace]
    unpack_params: Cell<UnpackParams>,
    enable_rasterizer_discard: Cell<bool>,
    /// COLOR_WRITEMASK, for the draw check on draw buffers without a fragment output.
    #[ignore_malloc_size_of = "plain bools"]
    color_write_mask: Cell<[bool; 4]>,
    default_fb_readbuffer: Cell<u32>,
    default_fb_drawbuffer: Cell<u32>,
}

struct ReadPixelsAllowedFormats<'a> {
    array_types: &'a [Type],
    channels: usize,
}

struct ReadPixelsSizes {
    row_stride: usize,
    skipped_bytes: usize,
    size: usize,
}

impl WebGL2RenderingContext {
    fn new_inherited(
        cx: &mut JSContext,
        global: &GlobalScope,
        canvas: &RootedHTMLCanvasElementOrOffscreenCanvas,
        size: Size2D<u32>,
        attrs: GLContextAttributes,
    ) -> Option<WebGL2RenderingContext> {
        let base =
            WebGLRenderingContext::new(cx, global, canvas, WebGLVersion::WebGL2, size, attrs)?;

        let samplers = (0..base.limits().max_combined_texture_image_units)
            .map(|_| Default::default())
            .collect::<Vec<_>>()
            .into();
        let indexed_uniform_buffer_bindings = (0..base.limits().max_uniform_buffer_bindings)
            .map(|_| IndexedBinding::new())
            .collect::<Vec<_>>()
            .into();
        let indexed_transform_feedback_buffer_bindings =
            (0..base.limits().max_transform_feedback_separate_attribs)
                .map(|_| IndexedBinding::new())
                .collect::<Vec<_>>()
                .into();

        Some(WebGL2RenderingContext {
            reflector_: Reflector::new(),
            base: Dom::from_ref(&*base),
            occlusion_query: MutNullableDom::new(None),
            primitives_query: MutNullableDom::new(None),
            samplers,
            bound_copy_read_buffer: MutNullableDom::new(None),
            bound_copy_write_buffer: MutNullableDom::new(None),
            bound_pixel_pack_buffer: MutNullableDom::new(None),
            bound_pixel_unpack_buffer: MutNullableDom::new(None),
            bound_transform_feedback_buffer: MutNullableDom::new(None),
            bound_uniform_buffer: MutNullableDom::new(None),
            indexed_uniform_buffer_bindings,
            indexed_transform_feedback_buffer_bindings,
            current_transform_feedback: MutNullableDom::new(None),
            default_transform_feedback: TransformFeedbackState::new(&base),
            texture_pack_row_length: Cell::new(0),
            texture_pack_skip_pixels: Cell::new(0),
            texture_pack_skip_rows: Cell::new(0),
            unpack_params: Cell::new(UnpackParams::default()),
            enable_rasterizer_discard: Cell::new(false),
            color_write_mask: Cell::new([true; 4]),
            default_fb_readbuffer: Cell::new(constants::BACK),
            default_fb_drawbuffer: Cell::new(constants::BACK),
        })
    }

    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        global: &GlobalScope,
        canvas: &RootedHTMLCanvasElementOrOffscreenCanvas,
        size: Size2D<u32>,
        attrs: GLContextAttributes,
    ) -> Option<DomRoot<WebGL2RenderingContext>> {
        WebGL2RenderingContext::new_inherited(cx, global, canvas, size, attrs).map(|ctx| {
            let ctx = reflect_dom_object_with_cx(Box::new(ctx), global, cx);
            ctx.base.set_webgl2(&ctx);
            ctx
        })
    }

    pub(crate) fn set_image_key(&self, image_key: ImageKey) {
        self.base.set_image_key(image_key);
    }

    #[expect(unsafe_code)]
    pub(crate) fn is_webgl2_enabled(
        _cx: &mut js::context::JSContext,
        global: HandleObject,
    ) -> bool {
        if pref!(dom_webgl2_enabled) {
            return true;
        }

        let global = unsafe { GlobalScope::from_object(global.get()) };
        let origin = global.origin();
        let host = origin.host();
        WEBGL2_ORIGINS
            .iter()
            .any(|origin| host == Host::parse(origin).ok().as_ref())
    }
}

/// List of domains for which WebGL 2 is enabled automatically, regardless
/// of the status of the dom.webgl2.enabled preference.
static WEBGL2_ORIGINS: &[&str] = &["www.servoexperiments.com"];

impl WebGL2RenderingContext {
    pub(crate) fn current_vao(&self, cx: &mut JSContext) -> DomRoot<WebGLVertexArrayObject> {
        self.base.current_vao_webgl2(cx)
    }

    /// GL ES 3.0 2.12.6 / WebGL 2.0 5.x: every active uniform block needs a buffer at its
    /// binding point whose bound range holds at least UNIFORM_BLOCK_DATA_SIZE bytes.
    fn validate_uniform_block_for_draw(&self) -> WebGLResult<()> {
        let Some(program) = self.base.current_program() else {
            return Ok(());
        };
        let blocks = program.active_uniform_blocks();
        let bindings = program.uniform_block_bindings();
        for (block, &binding) in blocks.iter().zip(bindings.iter()) {
            let Some(indexed) = self.indexed_uniform_buffer_bindings.get(binding as usize) else {
                return Err(InvalidOperation);
            };
            let Some(buffer) = indexed.buffer.get() else {
                return Err(InvalidOperation);
            };
            // bindBufferBase binds the whole buffer (start 0, size 0); a range is clamped to
            // the buffer's current size.
            let mut available = buffer
                .capacity()
                .saturating_sub(indexed.start.get().max(0) as usize);
            if indexed.size.get() > 0 {
                available = available.min(indexed.size.get() as usize);
            }
            if block.size.max(0) as usize > available {
                return Err(InvalidOperation);
            }
        }
        Ok(())
    }

    /// GL ES 3.0 2.8 / WebGL 2.0 5.x: each active attribute must be fed the base type the
    /// shader reads (float, int or uint), else the draw is INVALID_OPERATION.
    fn validate_vertex_attribs_for_draw(&self, cx: &mut JSContext) -> WebGLResult<()> {
        let Some(program) = self.base.current_program() else {
            return Ok(());
        };
        let groups = [
            [
                constants::INT,
                constants::INT_VEC2,
                constants::INT_VEC3,
                constants::INT_VEC4,
            ],
            [
                constants::UNSIGNED_INT,
                constants::UNSIGNED_INT_VEC2,
                constants::UNSIGNED_INT_VEC3,
                constants::UNSIGNED_INT_VEC4,
            ],
            [
                constants::FLOAT,
                constants::FLOAT_VEC2,
                constants::FLOAT_VEC3,
                constants::FLOAT_VEC4,
            ],
        ];
        let vao = self.current_vao(cx);
        for prog_attrib in program.active_attribs().iter() {
            // Built-ins such as gl_VertexID (which three.js morph targets read) are active
            // attributes without a location; there is nothing to check for them.
            let Some(location) = prog_attrib.location else {
                continue;
            };
            let attrib = vao.get_vertex_attrib(location).ok_or(InvalidOperation)?;

            // The type the shader receives: vertexAttribPointer data (whatever its component
            // type) arrives as float, vertexAttribIPointer data as int or uint by its signedness.
            let current_vertex_attrib = self.base.current_vertex_attribs()[location as usize];
            let attrib_data_base_type = if !attrib.enabled_as_array {
                match current_vertex_attrib {
                    VertexAttrib::Int(_, _, _, _) => constants::INT,
                    VertexAttrib::Uint(_, _, _, _) => constants::UNSIGNED_INT,
                    VertexAttrib::Float(_, _, _, _) => constants::FLOAT,
                }
            } else if !attrib.integer {
                constants::FLOAT
            } else {
                match attrib.type_ {
                    constants::BYTE | constants::SHORT | constants::INT => constants::INT,
                    _ => constants::UNSIGNED_INT,
                }
            };
            // Matrices and other float types read float data.
            let shader_base_type = groups
                .iter()
                .find(|g| g.contains(&prog_attrib.type_))
                .map_or(constants::FLOAT, |g| g[0]);

            let contains = groups
                .iter()
                .find(|g| g.contains(&attrib_data_base_type) && g.contains(&shader_base_type));
            if contains.is_none() {
                return Err(InvalidOperation);
            }
        }
        Ok(())
    }

    /// WebGL 2.0 5.13 (fragment shader output types) and 5.14 (draw buffers): each draw
    /// buffer with an image needs a fragment shader output of its component type (float for
    /// float and fixed-point buffers, int, uint), else INVALID_OPERATION. A missing output is
    /// fine while the colour write mask is all off, and nothing applies with
    /// RASTERIZER_DISCARD. The canvas only gets the type check. Programs whose outputs
    /// couldn't be read from the translated shader are not checked.
    fn validate_fragment_outputs_for_draw(&self) -> WebGLResult<()> {
        if self.enable_rasterizer_discard.get() {
            return Ok(());
        }
        let Some(program) = self.base.current_program() else {
            return Ok(());
        };
        let outputs = program.fragment_outputs();
        let Some(outputs) = outputs.as_ref() else {
            return Ok(());
        };
        let user_framebuffer = self.base.get_draw_framebuffer_slot().get().is_some();
        let writes_color = self.color_write_mask.get().contains(&true);
        for index in 0..self.base.limits().max_draw_buffers as usize {
            let Some(class) = self.draw_buffer_class(index) else {
                continue;
            };
            let buffer_kind = match class {
                2 => FragOutputKind::Int,
                3 => FragOutputKind::Uint,
                _ => FragOutputKind::Float,
            };
            match outputs.get(index).copied().flatten() {
                Some(kind) if kind != buffer_kind => return Err(InvalidOperation),
                None if user_framebuffer && writes_color => return Err(InvalidOperation),
                _ => {},
            }
        }
        Ok(())
    }

    /// The draw-time state checks of WebGL 2 (framebuffer completeness, uniform buffers,
    /// attribute types, sampler types and feedback loops, fragment outputs); a failing one
    /// rejects the draw. Draws whose
    /// mode or counts are invalid skip them, so that the base draw reports its own error.
    /// Partii (perf): the draw-time state checks this draw still needs (draw_state_cached): none,
    /// the sampler check alone, or all of them (validate_draw_state). True when the rest of the
    /// draw may skip its own state checks too.
    fn draw_state_checks(
        &self,
        cx: &mut JSContext,
        mode: u32,
        first_or_offset: i64,
        count: i32,
        primcount: i32,
    ) -> WebGLResult<bool> {
        match self.base.draw_state_cached(cx) {
            DrawStateCache::Hit => Ok(true),
            DrawStateCache::Samplers if Self::draw_args_checkable(mode, first_or_offset, count, primcount) => {
                self.check_sampler_types_for_draw()?;
                if let Some(program) = self.base.current_program() {
                    self.base.mark_draw_state_valid(cx, &program);
                }
                Ok(true)
            },
            _ => {
                self.validate_draw_state(cx, mode, first_or_offset, count, primcount)?;
                Ok(false)
            },
        }
    }

    /// Whether a draw's mode and counts are valid, so that its state checks apply (otherwise the
    /// base draw reports the argument error).
    fn draw_args_checkable(mode: u32, first_or_offset: i64, count: i32, primcount: i32) -> bool {
        matches!(
            mode,
            constants::POINTS |
                constants::LINE_STRIP |
                constants::LINE_LOOP |
                constants::LINES |
                constants::TRIANGLE_STRIP |
                constants::TRIANGLE_FAN |
                constants::TRIANGLES
        ) && first_or_offset >= 0 &&
            count >= 0 &&
            primcount >= 0
    }

    fn validate_draw_state(
        &self,
        cx: &mut JSContext,
        mode: u32,
        first_or_offset: i64,
        count: i32,
        primcount: i32,
    ) -> WebGLResult<()> {
        let valid_mode = matches!(
            mode,
            constants::POINTS |
                constants::LINE_STRIP |
                constants::LINE_LOOP |
                constants::LINES |
                constants::TRIANGLE_STRIP |
                constants::TRIANGLE_FAN |
                constants::TRIANGLES
        );
        if !valid_mode || first_or_offset < 0 || count < 0 || primcount < 0 {
            return Ok(());
        }
        // An incomplete framebuffer is reported before anything about the draw's state (so
        // INVALID_FRAMEBUFFER_OPERATION wins over a feedback loop's INVALID_OPERATION).
        self.base.validate_framebuffer()?;
        self.base.validate_stencil_for_draw()?;
        self.validate_uniform_block_for_draw()?;
        self.validate_vertex_attribs_for_draw(cx)?;
        self.validate_sampler_types_for_draw()?;
        self.validate_fragment_outputs_for_draw()
    }

    pub(crate) fn base_context(&self) -> DomRoot<WebGLRenderingContext> {
        DomRoot::from_ref(&*self.base)
    }

    fn bound_buffer(
        &self,
        cx: &mut JSContext,
        target: u32,
    ) -> WebGLResult<Option<DomRoot<WebGLBuffer>>> {
        match target {
            constants::COPY_READ_BUFFER => Ok(self.bound_copy_read_buffer.get()),
            constants::COPY_WRITE_BUFFER => Ok(self.bound_copy_write_buffer.get()),
            constants::PIXEL_PACK_BUFFER => Ok(self.bound_pixel_pack_buffer.get()),
            constants::PIXEL_UNPACK_BUFFER => Ok(self.bound_pixel_unpack_buffer.get()),
            constants::TRANSFORM_FEEDBACK_BUFFER => Ok(self.bound_transform_feedback_buffer.get()),
            constants::UNIFORM_BUFFER => Ok(self.bound_uniform_buffer.get()),
            constants::ELEMENT_ARRAY_BUFFER => {
                Ok(self.current_vao(cx).element_array_buffer().get())
            },
            _ => self.base.bound_buffer(cx, target),
        }
    }

    pub(crate) fn buffer_usage(&self, usage: u32) -> WebGLResult<u32> {
        match usage {
            constants::STATIC_READ |
            constants::DYNAMIC_READ |
            constants::STREAM_READ |
            constants::STATIC_COPY |
            constants::DYNAMIC_COPY |
            constants::STREAM_COPY => Ok(usage),
            _ => self.base.buffer_usage(usage),
        }
    }

    fn unbind_from(&self, slot: &MutNullableDom<WebGLBuffer>, buffer: &WebGLBuffer) {
        if slot.get().is_some_and(|b| buffer == &*b) {
            buffer.decrement_attached_counter(Operation::Infallible);
            slot.set(None);
        }
    }

    fn calc_read_pixel_formats(
        &self,
        pixel_type: u32,
        format: u32,
    ) -> WebGLResult<ReadPixelsAllowedFormats<'_>> {
        let array_types = match pixel_type {
            constants::BYTE => &[Type::Int8][..],
            constants::SHORT => &[Type::Int16][..],
            constants::INT => &[Type::Int32][..],
            constants::UNSIGNED_BYTE => &[Type::Uint8, Type::Uint8Clamped][..],
            constants::UNSIGNED_SHORT |
            constants::UNSIGNED_SHORT_4_4_4_4 |
            constants::UNSIGNED_SHORT_5_5_5_1 |
            constants::UNSIGNED_SHORT_5_6_5 => &[Type::Uint16][..],
            constants::UNSIGNED_INT |
            constants::UNSIGNED_INT_2_10_10_10_REV |
            constants::UNSIGNED_INT_10F_11F_11F_REV |
            constants::UNSIGNED_INT_5_9_9_9_REV => &[Type::Uint32][..],
            constants::FLOAT => &[Type::Float32][..],
            constants::HALF_FLOAT => &[Type::Uint16][..],
            _ => return Err(InvalidEnum),
        };
        let channels = match format {
            constants::ALPHA | constants::RED | constants::RED_INTEGER => 1,
            constants::RG | constants::RG_INTEGER => 2,
            constants::RGB | constants::RGB_INTEGER => 3,
            constants::RGBA | constants::RGBA_INTEGER => 4,
            _ => return Err(InvalidEnum),
        };
        // A packed type holds a whole pixel in one element.
        let packed = matches!(
            pixel_type,
            constants::UNSIGNED_SHORT_4_4_4_4 |
                constants::UNSIGNED_SHORT_5_5_5_1 |
                constants::UNSIGNED_SHORT_5_6_5 |
                constants::UNSIGNED_INT_2_10_10_10_REV |
                constants::UNSIGNED_INT_10F_11F_11F_REV |
                constants::UNSIGNED_INT_5_9_9_9_REV
        );
        let channels = if packed { 1 } else { channels };
        Ok(ReadPixelsAllowedFormats {
            array_types,
            channels,
        })
    }

    fn calc_read_pixel_sizes(
        &self,
        width: i32,
        height: i32,
        bytes_per_pixel: usize,
    ) -> WebGLResult<ReadPixelsSizes> {
        if width < 0 || height < 0 {
            return Err(InvalidValue);
        }

        // See also https://www.khronos.org/registry/webgl/specs/latest/2.0/#6.36
        let pixels_per_row = if self.texture_pack_row_length.get() > 0 {
            self.texture_pack_row_length.get()
        } else {
            width as usize
        };
        if self.texture_pack_skip_pixels.get() + width as usize > pixels_per_row {
            return Err(InvalidOperation);
        }

        let bytes_per_row = pixels_per_row
            .checked_mul(bytes_per_pixel)
            .ok_or(InvalidOperation)?;
        let row_padding_bytes = {
            let pack_alignment = self.base.get_texture_packing_alignment() as usize;
            match bytes_per_row % pack_alignment {
                0 => 0,
                remainder => pack_alignment - remainder,
            }
        };
        let row_stride = bytes_per_row + row_padding_bytes;
        let size = if width == 0 || height == 0 {
            0
        } else {
            let full_row_bytes = row_stride
                .checked_mul(height as usize - 1)
                .ok_or(InvalidOperation)?;
            let last_row_bytes = bytes_per_pixel
                .checked_mul(width as usize)
                .ok_or(InvalidOperation)?;
            full_row_bytes
                .checked_add(last_row_bytes)
                .ok_or(InvalidOperation)?
        };
        let skipped_bytes = {
            let skipped_row_bytes = self
                .texture_pack_skip_rows
                .get()
                .checked_mul(row_stride)
                .ok_or(InvalidOperation)?;
            let skipped_pixel_bytes = self
                .texture_pack_skip_pixels
                .get()
                .checked_mul(bytes_per_pixel)
                .ok_or(InvalidOperation)?;
            skipped_row_bytes
                .checked_add(skipped_pixel_bytes)
                .ok_or(InvalidOperation)?
        };
        Ok(ReadPixelsSizes {
            row_stride,
            skipped_bytes,
            size,
        })
    }

    #[expect(clippy::too_many_arguments)]
    fn read_pixels_into(
        &self,
        no_gc: &NoGC,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        format: u32,
        pixel_type: u32,
        dst: &mut ArrayBufferView,
        dst_elem_offset: u32,
    ) {
        handle_potential_webgl_error!(self.base, self.base.validate_framebuffer(), return);

        if self.bound_pixel_pack_buffer.get().is_some() {
            return self.base.webgl_error(InvalidOperation);
        }

        let fb_slot = self.base.get_draw_framebuffer_slot();
        let fb_readbuffer_valid = match fb_slot.get() {
            Some(fb) => fb.attachment(fb.read_buffer()).is_some(),
            None => self.default_fb_readbuffer.get() != constants::NONE,
        };
        if !fb_readbuffer_valid {
            return self.base.webgl_error(InvalidOperation);
        }

        let dst_byte_offset = {
            let dst_elem_size = dst.get_array_type().byte_size().unwrap();
            dst_elem_offset as usize * dst_elem_size
        };
        if dst_byte_offset > array_buffer_view_byte_len(&dst) {
            return self.base.webgl_error(InvalidValue);
        }

        let dst_array_type = dst.get_array_type();
        let ReadPixelsAllowedFormats {
            array_types: allowed_array_types,
            channels,
        } = match self.calc_read_pixel_formats(pixel_type, format) {
            Ok(result) => result,
            Err(error) => return self.base.webgl_error(error),
        };
        if !allowed_array_types.contains(&dst_array_type) {
            return self.base.webgl_error(InvalidOperation);
        }
        // RGBA plus the type of the read buffer's class, or the implementation's pair
        // (WebGL 2.0, 3.7.10 readPixels; GL ES 3.0, 4.3.1).
        let (primary, implementation) = self.read_pixels_formats();
        if (format, pixel_type) != primary && (format, pixel_type) != implementation {
            return self.base.webgl_error(InvalidOperation);
        }

        let bytes_per_pixel = dst_array_type.byte_size().unwrap() * channels;
        let ReadPixelsSizes {
            row_stride,
            skipped_bytes,
            size,
        } = match self.calc_read_pixel_sizes(width, height, bytes_per_pixel) {
            Ok(result) => result,
            Err(error) => return self.base.webgl_error(error),
        };
        let dst_end = dst_byte_offset + skipped_bytes + size;
        let dst_pixels = array_buffer_view_bytes_mut(dst, no_gc);
        if dst_pixels.len() < dst_end {
            return self.base.webgl_error(InvalidOperation);
        }

        let dst_byte_offset = {
            let margin_left = cmp::max(0, -x) as usize;
            let margin_top = cmp::max(0, -y) as usize;
            dst_byte_offset +
                skipped_bytes +
                margin_left * bytes_per_pixel +
                margin_top * row_stride
        };
        let src_rect = {
            let (fb_width, fb_height) = handle_potential_webgl_error!(
                self.base,
                self.base
                    .get_current_framebuffer_size()
                    .ok_or(InvalidOperation),
                return
            );
            let src_origin = Point2D::new(x, y);
            let src_size = Size2D::new(width as u32, height as u32);
            let fb_size = Size2D::new(fb_width as u32, fb_height as u32);
            match pixels::clip(src_origin, src_size.to_u32(), fb_size.to_u32()) {
                Some(rect) => rect.to_u32(),
                None => return,
            }
        };
        let src_row_bytes = handle_potential_webgl_error!(
            self.base,
            src_rect
                .size
                .width
                .checked_mul(bytes_per_pixel as u32)
                .ok_or(InvalidOperation),
            return
        );

        let (sender, receiver) = generic_channel::channel().unwrap();
        self.base.send_command(WebGLCommand::ReadPixels(
            src_rect, format, pixel_type, sender,
        ));
        let (src, _) = receiver.recv().unwrap();

        for i in 0..src_rect.size.height as usize {
            let src_start = i * src_row_bytes as usize;
            let dst_start = dst_byte_offset + i * row_stride;
            dst_pixels[dst_start..dst_start + src_row_bytes as usize]
                .copy_from_slice(&src[src_start..src_start + src_row_bytes as usize]);
        }
    }

    fn uniform_vec_section_uint(
        &self,
        vec: Uint32ArrayOrUnsignedLongSequence,
        offset: u32,
        length: u32,
        uniform_size: usize,
        uniform_location: &WebGLUniformLocation,
    ) -> WebGLResult<Vec<u32>> {
        let vec = match vec {
            Uint32ArrayOrUnsignedLongSequence::Uint32Array(v) => typed_array_to_vec(&*v),
            Uint32ArrayOrUnsignedLongSequence::UnsignedLongSequence(v) => v,
        };
        self.base
            .uniform_vec_section::<u32>(vec, offset, length, uniform_size, uniform_location)
    }

    fn get_default_fb_attachment_param(
        &self,
        attachment: u32,
        pname: u32,
        mut retval: MutableHandleValue,
    ) -> WebGLResult<()> {
        match attachment {
            constants::BACK | constants::DEPTH | constants::STENCIL => {},
            _ => return Err(InvalidEnum),
        }

        if pname == constants::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME {
            retval.set(NullValue());
            return Ok(());
        }

        let attrs = self
            .GetContextAttributes()
            .unwrap_or_else(WebGLContextAttributes::empty);

        let intval = match pname {
            constants::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE => match attachment {
                constants::DEPTH if !attrs.depth => constants::NONE as _,
                constants::STENCIL if !attrs.stencil => constants::NONE as _,
                _ => constants::FRAMEBUFFER_DEFAULT as _,
            },
            constants::FRAMEBUFFER_ATTACHMENT_RED_SIZE |
            constants::FRAMEBUFFER_ATTACHMENT_GREEN_SIZE |
            constants::FRAMEBUFFER_ATTACHMENT_BLUE_SIZE => match attachment {
                constants::BACK => 8,
                _ => 0,
            },
            constants::FRAMEBUFFER_ATTACHMENT_ALPHA_SIZE => match attachment {
                constants::BACK if attrs.alpha => 8,
                constants::BACK => return Err(InvalidOperation),
                _ => 0,
            },
            constants::FRAMEBUFFER_ATTACHMENT_DEPTH_SIZE => match attachment {
                constants::DEPTH if attrs.depth => 24,
                constants::DEPTH => return Err(InvalidOperation),
                _ => 0,
            },
            constants::FRAMEBUFFER_ATTACHMENT_STENCIL_SIZE => match attachment {
                constants::STENCIL if attrs.stencil => 8,
                constants::STENCIL => return Err(InvalidOperation),
                _ => 0,
            },
            constants::FRAMEBUFFER_ATTACHMENT_COMPONENT_TYPE => match attachment {
                constants::DEPTH if attrs.depth => constants::UNSIGNED_NORMALIZED as _,
                constants::STENCIL if attrs.stencil => constants::UNSIGNED_INT as _,
                constants::DEPTH => return Err(InvalidOperation),
                constants::STENCIL => return Err(InvalidOperation),
                _ => constants::UNSIGNED_NORMALIZED as _,
            },
            constants::FRAMEBUFFER_ATTACHMENT_COLOR_ENCODING => match attachment {
                constants::DEPTH if !attrs.depth => return Err(InvalidOperation),
                constants::STENCIL if !attrs.stencil => return Err(InvalidOperation),
                _ => constants::LINEAR as _,
            },
            _ => return Err(InvalidEnum),
        };
        retval.set(Int32Value(intval));
        Ok(())
    }

    fn get_specific_fb_attachment_param(
        &self,
        cx: &mut JSContext,
        fb: &WebGLFramebuffer,
        target: u32,
        attachment: u32,
        pname: u32,
        mut rval: MutableHandleValue,
    ) -> WebGLResult<()> {
        use crate::dom::webgl::webglframebuffer::WebGLFramebufferAttachmentRoot::{
            Renderbuffer, Texture,
        };

        match attachment {
            constants::DEPTH_ATTACHMENT | constants::STENCIL_ATTACHMENT => {},
            constants::DEPTH_STENCIL_ATTACHMENT => {
                if pname == constants::FRAMEBUFFER_ATTACHMENT_COMPONENT_TYPE {
                    return Err(InvalidOperation);
                }

                let a = fb.attachment(constants::DEPTH_ATTACHMENT);
                let b = fb.attachment(constants::STENCIL_ATTACHMENT);
                match (a, b) {
                    (Some(Renderbuffer(ref a)), Some(Renderbuffer(ref b))) if a.id() == b.id() => {
                    },
                    (Some(Texture(ref a)), Some(Texture(ref b))) if a.id() == b.id() => {},
                    // Nothing at either point: OBJECT_TYPE is NONE and OBJECT_NAME null.
                    (None, None) => {},
                    _ => return Err(InvalidOperation),
                }
            },
            constants::COLOR_ATTACHMENT0..=constants::COLOR_ATTACHMENT15 => {
                let last_slot =
                    constants::COLOR_ATTACHMENT0 + self.base.limits().max_color_attachments - 1;
                if last_slot < attachment {
                    return Err(InvalidEnum);
                }
            },
            _ => return Err(InvalidEnum),
        }

        let attachment = match attachment {
            constants::DEPTH_STENCIL_ATTACHMENT => constants::DEPTH_ATTACHMENT,
            _ => attachment,
        };

        if pname == constants::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME {
            match fb.attachment(attachment) {
                Some(Renderbuffer(rb)) => {
                    rb.to_jsval(cx, rval);
                },
                Some(Texture(texture)) => {
                    texture.to_jsval(cx, rval);
                },
                _ => rval.set(NullValue()),
            }
            return Ok(());
        }

        match pname {
            constants::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE => {},
            _ => match fb.attachment(attachment) {
                Some(webgl_attachment) => match pname {
                    constants::FRAMEBUFFER_ATTACHMENT_RED_SIZE |
                    constants::FRAMEBUFFER_ATTACHMENT_GREEN_SIZE |
                    constants::FRAMEBUFFER_ATTACHMENT_BLUE_SIZE |
                    constants::FRAMEBUFFER_ATTACHMENT_ALPHA_SIZE |
                    constants::FRAMEBUFFER_ATTACHMENT_DEPTH_SIZE |
                    constants::FRAMEBUFFER_ATTACHMENT_STENCIL_SIZE |
                    constants::FRAMEBUFFER_ATTACHMENT_COMPONENT_TYPE |
                    constants::FRAMEBUFFER_ATTACHMENT_COLOR_ENCODING => {},
                    _ => match webgl_attachment {
                        WebGLFramebufferAttachmentRoot::Renderbuffer(_) => return Err(InvalidEnum),
                        WebGLFramebufferAttachmentRoot::Texture(_) => match pname {
                            constants::FRAMEBUFFER_ATTACHMENT_TEXTURE_LEVEL |
                            constants::FRAMEBUFFER_ATTACHMENT_TEXTURE_CUBE_MAP_FACE |
                            constants::FRAMEBUFFER_ATTACHMENT_TEXTURE_LAYER => {},
                            _ => return Err(InvalidEnum),
                        },
                    },
                },
                None => return Err(InvalidOperation),
            },
        }

        let (sender, receiver) = webgl_channel().unwrap();
        self.base
            .send_command(WebGLCommand::GetFramebufferAttachmentParameter(
                target, attachment, pname, sender,
            ));

        let retval = receiver.recv().unwrap();
        rval.set(Int32Value(retval));
        Ok(())
    }

    fn clearbuffer_array_size(&self, buffer: u32, draw_buffer: i32) -> WebGLResult<usize> {
        match buffer {
            constants::COLOR => {
                if draw_buffer < 0 || draw_buffer as u32 >= self.base.limits().max_draw_buffers {
                    return Err(InvalidValue);
                }
                Ok(4)
            },
            constants::DEPTH | constants::STENCIL | constants::DEPTH_STENCIL => {
                if draw_buffer != 0 {
                    return Err(InvalidValue);
                }
                Ok(1)
            },
            _ => unreachable!(),
        }
    }

    /// The component class (`color_component_class`) of the image that draw buffer `index`
    /// writes to, or None when it writes nothing. The canvas is normalized.
    fn draw_buffer_class(&self, index: usize) -> Option<u8> {
        match self.base.get_draw_framebuffer_slot().get() {
            None => {
                (index == 0 && self.default_fb_drawbuffer.get() != constants::NONE).then_some(0)
            },
            Some(fb) => match fb.draw_buffer_i(index) {
                constants::NONE => None,
                attachment => fb
                    .attachment_internal_format(attachment)
                    .map(color_component_class),
            },
        }
    }

    #[expect(clippy::too_many_arguments)]
    fn clear_buffer<T: Clone>(
        &self,
        buffer: u32,
        draw_buffer: i32,
        valid_buffers: &[u32],
        color_classes: &[u8],
        src_offset: u32,
        array: Vec<T>,
        msg: fn(u32, i32, Vec<T>) -> WebGLCommand,
    ) {
        if !valid_buffers.contains(&buffer) {
            return self.base.webgl_error(InvalidEnum);
        }

        let array_size = handle_potential_webgl_error!(
            self.base,
            self.clearbuffer_array_size(buffer, draw_buffer),
            return
        );
        // WebGL 2.0 5.13: clearBufferfv clears float and normalized buffers, clearBufferiv
        // signed and clearBufferuiv unsigned integer ones; anything else is INVALID_OPERATION.
        if buffer == constants::COLOR &&
            self.draw_buffer_class(draw_buffer as usize)
                .is_some_and(|class| !color_classes.contains(&class))
        {
            return self.base.webgl_error(InvalidOperation);
        }
        let src_offset = src_offset as usize;

        if array.len() < src_offset + array_size {
            return self.base.webgl_error(InvalidValue);
        }
        let array = array[src_offset..src_offset + array_size].to_vec();

        // Zero-initialize the framebuffer's attachments first if they haven't been yet (WebGL
        // does it lazily, on first use): otherwise the next draw or read does it and wipes what
        // this clear wrote. An incomplete framebuffer is INVALID_FRAMEBUFFER_OPERATION.
        if let Some(fb) = self.base.get_draw_framebuffer_slot().get() &&
            let CompleteForRendering::Incomplete = fb.check_status_for_rendering()
        {
            return self.base.webgl_error(InvalidFramebufferOperation);
        }

        self.base.send_command(msg(buffer, draw_buffer, array));
        self.mark_as_dirty();
    }

    fn valid_fb_attachment_values(&self, target: u32, attachments: &[u32]) -> bool {
        let fb_slot = match target {
            constants::FRAMEBUFFER | constants::DRAW_FRAMEBUFFER => {
                self.base.get_draw_framebuffer_slot()
            },
            constants::READ_FRAMEBUFFER => self.base.get_read_framebuffer_slot(),
            _ => {
                self.base.webgl_error(InvalidEnum);
                return false;
            },
        };

        if let Some(fb) = fb_slot.get() {
            if fb.check_status() != constants::FRAMEBUFFER_COMPLETE {
                return false;
            }

            for &attachment in attachments {
                match attachment {
                    constants::DEPTH_ATTACHMENT |
                    constants::STENCIL_ATTACHMENT |
                    constants::DEPTH_STENCIL_ATTACHMENT => {},
                    constants::COLOR_ATTACHMENT0..=constants::COLOR_ATTACHMENT15 => {
                        let last_slot = constants::COLOR_ATTACHMENT0 +
                            self.base.limits().max_color_attachments -
                            1;
                        // A COLOR_ATTACHMENTi past MAX_COLOR_ATTACHMENTS (ES 3.0 4.5).
                        if last_slot < attachment {
                            self.base.webgl_error(InvalidOperation);
                            return false;
                        }
                    },
                    _ => {
                        self.base.webgl_error(InvalidEnum);
                        return false;
                    },
                }
            }
        } else {
            for &attachment in attachments {
                match attachment {
                    constants::COLOR | constants::DEPTH | constants::STENCIL => {},
                    _ => {
                        self.base.webgl_error(InvalidEnum);
                        return false;
                    },
                }
            }
        }

        true
    }

    fn vertex_attrib_i(&self, cx: &mut JSContext, index: u32, x: i32, y: i32, z: i32, w: i32) {
        if index >= self.base.limits().max_vertex_attribs {
            return self.base.webgl_error(InvalidValue);
        }
        self.base.current_vertex_attribs()[index as usize] = VertexAttrib::Int(x, y, z, w);
        self.current_vao(cx)
            .set_vertex_attrib_type(index, constants::INT);
        self.base
            .send_command(WebGLCommand::VertexAttribI(index, x, y, z, w));
    }

    fn vertex_attrib_u(&self, cx: &mut JSContext, index: u32, x: u32, y: u32, z: u32, w: u32) {
        if index >= self.base.limits().max_vertex_attribs {
            return self.base.webgl_error(InvalidValue);
        }
        self.base.current_vertex_attribs()[index as usize] = VertexAttrib::Uint(x, y, z, w);
        self.current_vao(cx)
            .set_vertex_attrib_type(index, constants::UNSIGNED_INT);
        self.base
            .send_command(WebGLCommand::VertexAttribU(index, x, y, z, w));
    }

    #[expect(clippy::too_many_arguments)]
    fn tex_storage(
        &self,
        dimensions: u8,
        target: u32,
        levels: i32,
        internal_format: u32,
        width: i32,
        height: i32,
        depth: i32,
    ) {
        let expected_dimensions = match target {
            constants::TEXTURE_2D | constants::TEXTURE_CUBE_MAP => 2,
            constants::TEXTURE_3D | constants::TEXTURE_2D_ARRAY => 3,
            _ => return self.base.webgl_error(InvalidEnum),
        };
        if dimensions != expected_dimensions {
            return self.base.webgl_error(InvalidEnum);
        }

        let validator = TexStorageValidator::new(
            &self.base,
            dimensions,
            target,
            levels,
            internal_format,
            width,
            height,
            depth,
        );
        let TexStorageValidatorResult {
            texture,
            target,
            levels,
            internal_format,
            width,
            height,
            depth,
        } = match validator.validate() {
            Ok(result) => result,
            Err(_) => return, // NB: The validator sets the correct error for us.
        };
        // A compressed format on TEXTURE_3D only where compressedTexImage3D takes it (ASTC with
        // the HDR or sliced 3D extension); S3TC, RGTC, BPTC and ETC2 are 2D-array only.
        if target == TexImageTarget::Texture3D && internal_format.is_compressed() {
            let allowed = self
                .base
                .extension_manager()
                .get_tex_compression_format(internal_format.as_gl_constant())
                .is_some_and(|compression| self.compression_allows_texture_3d(&compression));
            if !allowed {
                return self.base.webgl_error(InvalidOperation);
            }
        }

        handle_potential_webgl_error!(
            self.base,
            texture.storage(target, levels, internal_format, width, height, depth)
        );
    }

    /// The bytes of a compressed upload from an ArrayBufferView: `src_offset` and
    /// `src_length_override` count elements of the view; a zero override means "to the end".
    fn compressed_src_bytes<'a>(
        &self,
        no_gc: &'a NoGC,
        view: &'a ArrayBufferView,
        src_offset: u32,
        src_length_override: u32,
    ) -> Option<&'a [u8]> {
        let elem_size = view.get_array_type().byte_size().unwrap_or(1);
        let data = array_buffer_view_bytes(&view, no_gc);
        let start = src_offset as usize * elem_size;
        if start > data.len() {
            self.base.webgl_error(InvalidValue);
            return None;
        }
        let len = match src_length_override {
            0 => data.len() - start,
            n => n as usize * elem_size,
        };
        if len > data.len() - start {
            self.base.webgl_error(InvalidValue);
            return None;
        }
        Some(&data[start..start + len])
    }

    /// A compressed upload of `image_size` bytes at `offset` in the bound PIXEL_UNPACK_BUFFER.
    fn compressed_pbo_source(
        &self,
        image_size: i32,
        offset: i64,
    ) -> Option<CompressedSource<'static>> {
        if self.bound_pixel_unpack_buffer.get().is_none() {
            self.base.webgl_error(InvalidOperation);
            return None;
        }
        if image_size < 0 || offset < 0 {
            self.base.webgl_error(InvalidValue);
            return None;
        }
        if !self.validate_pixel_unpack_buffer(offset, image_size as u64) || offset > u32::MAX as i64
        {
            return None;
        }
        Some(CompressedSource::PixelUnpackBuffer {
            offset: offset as u32,
            size: image_size as u32,
        })
    }

    /// Bytes a compressed image of `depth` slices of `width` x `height` takes.
    fn compressed_byte_len(
        compression: &TexCompression,
        width: u32,
        height: u32,
        depth: u32,
    ) -> u64 {
        let blocks_x = width.div_ceil(compression.block_width as u32) as u64;
        let blocks_y = height.div_ceil(compression.block_height as u32) as u64;
        blocks_x * blocks_y * depth as u64 * compression.bytes_per_block as u64
    }

    /// Whether `compression` may be used with a TEXTURE_3D target (not just TEXTURE_2D_ARRAY).
    /// Only ASTC can be, with KHR_texture_compression_astc_hdr or _sliced_3d.
    fn compression_allows_texture_3d(&self, compression: &TexCompression) -> bool {
        let format = compression.format.as_gl_constant();
        (0x93B0..=0x93DD).contains(&format) &&
            self.base.extension_manager().supports_any_gl_extension(&[
                "GL_KHR_texture_compression_astc_hdr",
                "GL_KHR_texture_compression_astc_sliced_3d",
            ])
    }

    /// <https://registry.khronos.org/OpenGL-Refpages/es3.0/html/glCompressedTexImage3D.xhtml>
    #[allow(clippy::too_many_arguments)]
    fn compressed_tex_image_3d(
        &self,
        target: u32,
        level: i32,
        internal_format: u32,
        width: i32,
        height: i32,
        depth: i32,
        border: i32,
        data: CompressedSource,
    ) {
        let target = match TexImageTarget::from_gl_constant(target) {
            Some(target) if target.dimensions() == 3 => target,
            _ => return self.base.webgl_error(InvalidEnum),
        };
        let Some(texture) = self.base.textures().active_texture_for_image_target(target) else {
            return self.base.webgl_error(InvalidOperation);
        };
        let Some(compression) = self
            .base
            .extension_manager()
            .get_tex_compression_format(internal_format)
        else {
            return self.base.webgl_error(InvalidEnum);
        };
        if level < 0 || width < 0 || height < 0 || depth < 0 || border != 0 {
            return self.base.webgl_error(InvalidValue);
        }
        let (level, width, height, depth) =
            (level as u32, width as u32, height as u32, depth as u32);
        let max_size = self.base.limits().max_3d_tex_size;
        if level > max_size.ilog2() {
            return self.base.webgl_error(InvalidValue);
        }
        let max_for_level = max_size >> level;
        if width > max_for_level || height > max_for_level || depth > max_for_level {
            return self.base.webgl_error(InvalidValue);
        }
        if texture.is_immutable() {
            return self.base.webgl_error(InvalidOperation);
        }
        if target == TexImageTarget::Texture3D && !self.compression_allows_texture_3d(&compression)
        {
            return self.base.webgl_error(InvalidOperation);
        }
        if data.len() as u64 != Self::compressed_byte_len(&compression, width, height, depth) {
            return self.base.webgl_error(InvalidValue);
        }
        let size_valid = match compression.validation {
            TexCompressionValidation::S3TC => {
                valid_s3tc_dimension(level, width, compression.block_width as u32) &&
                    valid_s3tc_dimension(level, height, compression.block_height as u32)
            },
            TexCompressionValidation::BaseLevelBlockMultiple => {
                base_level_block_multiple(level, width, height, &compression)
            },
            _ => true,
        };
        if !size_valid {
            return self.base.webgl_error(InvalidOperation);
        }
        handle_potential_webgl_error!(
            self.base,
            texture.initialize(
                target,
                width,
                height,
                depth,
                compression.format,
                level,
                Some(TexDataType::UnsignedByte)
            ),
            return
        );
        self.base.send_command(WebGLCommand::CompressedTexImage3D {
            target: target.as_gl_constant(),
            level,
            internal_format,
            size: Size2D::new(width, height),
            depth,
            data: data.into_command_data(),
        });
        if let Some(fb) = self.base.bound_draw_framebuffer() {
            fb.invalidate_texture(&texture);
        }
    }

    /// <https://registry.khronos.org/OpenGL-Refpages/es3.0/html/glCompressedTexSubImage3D.xhtml>
    #[allow(clippy::too_many_arguments)]
    fn compressed_tex_sub_image_3d(
        &self,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        width: i32,
        height: i32,
        depth: i32,
        format: u32,
        data: CompressedSource,
    ) {
        let target = match TexImageTarget::from_gl_constant(target) {
            Some(target) if target.dimensions() == 3 => target,
            _ => return self.base.webgl_error(InvalidEnum),
        };
        let Some(texture) = self.base.textures().active_texture_for_image_target(target) else {
            return self.base.webgl_error(InvalidOperation);
        };
        let Some(compression) = self
            .base
            .extension_manager()
            .get_tex_compression_format(format)
        else {
            return self.base.webgl_error(InvalidEnum);
        };
        if level < 0 || width < 0 || height < 0 || depth < 0 {
            return self.base.webgl_error(InvalidValue);
        }
        let (width, height, depth) = (width as u32, height as u32, depth as u32);
        let Some(info) = texture.image_info_for_target(&target, level as u32) else {
            return self.base.webgl_error(InvalidOperation);
        };
        if info.internal_format() != compression.format {
            return self.base.webgl_error(InvalidOperation);
        }
        let fits = |offset: i32, size: u32, max: u32| {
            offset >= 0 && offset as u64 + size as u64 <= max as u64
        };
        if !fits(xoffset, width, info.width()) ||
            !fits(yoffset, height, info.height()) ||
            !fits(zoffset, depth, info.depth())
        {
            return self.base.webgl_error(InvalidValue);
        }
        let aligned = match compression.validation {
            TexCompressionValidation::S3TC |
            TexCompressionValidation::SubImageBlockAligned |
            TexCompressionValidation::BaseLevelBlockMultiple => is_subimage_blockaligned(
                xoffset as u32,
                yoffset as u32,
                width,
                height,
                &compression,
                &info,
            ),
            TexCompressionValidation::None => true,
        };
        if !aligned {
            return self.base.webgl_error(InvalidOperation);
        }
        if data.len() as u64 != Self::compressed_byte_len(&compression, width, height, depth) {
            return self.base.webgl_error(InvalidValue);
        }
        self.base
            .send_command(WebGLCommand::CompressedTexSubImage3D {
                target: target.as_gl_constant(),
                level,
                xoffset,
                yoffset,
                zoffset,
                size: Size2D::new(width, height),
                depth,
                format,
                data: data.into_command_data(),
            });
    }

    /// The source layout of an upload of `depth` images of `width` x `height` pixels of `cpp`
    /// bytes, from client memory or a PIXEL_UNPACK_BUFFER, under the current unpack parameters.
    /// 2D uploads ignore UNPACK_IMAGE_HEIGHT and UNPACK_SKIP_IMAGES. Fails with
    /// INVALID_OPERATION when the skips do not fit the row length or image height
    /// (WebGL 2.0, 5.35).
    #[allow(clippy::too_many_arguments)]
    fn unpack_layout(
        &self,
        width: u32,
        height: u32,
        depth: u32,
        cpp: u32,
        alignment: u32,
        is_3d: bool,
    ) -> Option<UnpackLayout> {
        self.unpack_layout_with(width, height, depth, cpp, alignment, is_3d, false)
    }

    /// `unpack_layout`; with `dom_source` (an image source window) a zero UNPACK_IMAGE_HEIGHT
    /// puts no limit on UNPACK_SKIP_ROWS: the rows come from the source's own height (Chrome
    /// and the image_bitmap 3D sub-source tests skip half a bitmap's rows into one slice).
    #[allow(clippy::too_many_arguments)]
    fn unpack_layout_with(
        &self,
        width: u32,
        height: u32,
        depth: u32,
        cpp: u32,
        alignment: u32,
        is_3d: bool,
        dom_source: bool,
    ) -> Option<UnpackLayout> {
        let params = self.unpack_params.get();
        let (image_height, skip_images) = if is_3d {
            (params.image_height, params.skip_images)
        } else {
            (0, 0)
        };
        // A zero UNPACK_ROW_LENGTH / UNPACK_IMAGE_HEIGHT means the upload's own width / height,
        // so any skip at all then overruns (5.35). 2D uploads have no row-skip constraint.
        let row_pixels = if params.row_length > 0 {
            params.row_length
        } else {
            width
        };
        let rows_per_image = if image_height > 0 {
            image_height
        } else {
            height
        };
        if params.skip_pixels as u64 + width as u64 > row_pixels as u64 ||
            (is_3d &&
                !(dom_source && image_height == 0) &&
                params.skip_rows as u64 + height as u64 > rows_per_image as u64)
        {
            self.base.webgl_error(InvalidOperation);
            return None;
        }
        let (row_pixels, rows_per_image) = (row_pixels as u64, rows_per_image as u64);
        let (cpp, alignment) = (cpp as u64, alignment as u64);
        let row_stride = (row_pixels * cpp).div_ceil(alignment) * alignment;
        let image_stride = row_stride * rows_per_image;
        let first = skip_images as u64 * image_stride +
            params.skip_rows as u64 * row_stride +
            params.skip_pixels as u64 * cpp;
        let extent = if width == 0 || height == 0 || depth == 0 {
            0
        } else {
            (depth as u64 - 1) * image_stride +
                (height as u64 - 1) * row_stride +
                width as u64 * cpp
        };
        Some(UnpackLayout {
            row_stride,
            image_stride,
            first,
            end: first + extent,
        })
    }

    /// Copies an upload out of `src` (laid out per `layout`) into the layout the WebGL thread
    /// expects: rows of `width * cpp` bytes padded to `alignment`, images one after another.
    /// Bytes past the end of `src` read as zero.
    #[allow(clippy::too_many_arguments)]
    fn repack(
        src: &[u8],
        layout: &UnpackLayout,
        width: u32,
        height: u32,
        depth: u32,
        cpp: u32,
        alignment: u32,
    ) -> Vec<u8> {
        let row_bytes = (width * cpp) as usize;
        let dst_stride = row_bytes.div_ceil(alignment as usize) * alignment as usize;
        let mut out = vec![0u8; dst_stride * height as usize * depth as usize];
        for z in 0..depth as u64 {
            for y in 0..height as u64 {
                let from =
                    (layout.first + z * layout.image_stride + y * layout.row_stride) as usize;
                let to = ((z * height as u64 + y) as usize) * dst_stride;
                if from >= src.len() {
                    continue;
                }
                let n = row_bytes.min(src.len() - from);
                out[to..to + n].copy_from_slice(&src[from..from + n]);
            }
        }
        out
    }

    /// The client-memory bytes of an upload (`bytes` already starts at srcOffset), repacked if
    /// any unpack parameter is set. Fails with INVALID_OPERATION if `bytes` is too short.
    #[allow(clippy::too_many_arguments)]
    fn unpack_client_bytes(
        &self,
        bytes: &[u8],
        width: u32,
        height: u32,
        depth: u32,
        format: TexFormat,
        data_type: TexDataType,
        alignment: u32,
        is_3d: bool,
    ) -> Option<GenericSharedMemory> {
        let cpp =
            data_type.element_size() * format.components() / data_type.components_per_element();
        let layout = self.unpack_layout(width, height, depth, cpp, alignment, is_3d)?;
        if (bytes.len() as u64) < layout.end {
            self.base.webgl_error(InvalidOperation);
            return None;
        }
        if self.unpack_params.get() == UnpackParams::default() {
            return Some(GenericSharedMemory::from_bytes(
                &bytes[..layout.end as usize],
            ));
        }
        Some(GenericSharedMemory::from_vec(Self::repack(
            bytes, &layout, width, height, depth, cpp, alignment,
        )))
    }

    /// The `width` x `height` (x `depth`) window of an image source that a WebGL 2 upload with an
    /// explicit size reads: the source is RGBA8 or BGRA8 with rows of its own width (or
    /// UNPACK_ROW_LENGTH), offset by the skip parameters; UNPACK_ALIGNMENT does not apply.
    fn image_source_window(
        &self,
        pixels: TexPixels,
        width: u32,
        height: u32,
        depth: u32,
        is_3d: bool,
    ) -> Option<TexPixels> {
        let pixels = pixels.flip_source_y();
        let params = self.unpack_params.get();
        if params == UnpackParams::default() ||
            (!is_3d &&
                params.row_length == 0 &&
                params.skip_pixels == 0 &&
                params.skip_rows == 0)
        {
            return Some(pixels.crop_image_source(width, height * depth));
        }
        let source_width = pixels.size().width;
        let saved = self.unpack_params.get();
        if params.row_length == 0 {
            self.unpack_params.set(UnpackParams {
                row_length: source_width,
                ..params
            });
        }
        let layout = self.unpack_layout_with(width, height, depth, 4, 1, is_3d, true);
        self.unpack_params.set(saved);
        let layout = layout?;
        Some(pixels.map_data(Size2D::new(width, height * depth), |data| {
            Self::repack(data, &layout, width, height, depth, 4, 1)
        }))
    }

    /// Runs `upload`, which sends a PIXEL_UNPACK_BUFFER upload command, with the unpack
    /// parameters set in GL around it (the WebGL thread keeps GL at the defaults otherwise).
    fn with_unpack_params(&self, is_3d: bool, upload: impl FnOnce()) {
        let params = self.unpack_params.get();
        let values = [
            (constants::UNPACK_ROW_LENGTH, params.row_length),
            (constants::UNPACK_SKIP_PIXELS, params.skip_pixels),
            (constants::UNPACK_SKIP_ROWS, params.skip_rows),
            (
                constants::UNPACK_IMAGE_HEIGHT,
                if is_3d { params.image_height } else { 0 },
            ),
            (
                constants::UNPACK_SKIP_IMAGES,
                if is_3d { params.skip_images } else { 0 },
            ),
        ];
        let set: Vec<_> = values.into_iter().filter(|(_, v)| *v != 0).collect();
        for (name, value) in &set {
            self.base
                .send_command(WebGLCommand::PixelStorei(*name, *value as i32));
        }
        upload();
        for (name, _) in &set {
            self.base.send_command(WebGLCommand::PixelStorei(*name, 0));
        }
    }

    /// texImage2D from client memory (`src_offset` in elements of `view`), honouring the
    /// WebGL 2 unpack parameters.
    #[allow(clippy::too_many_arguments)]
    fn tex_image_2d_from_view(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        internalformat: i32,
        width: i32,
        height: i32,
        border: i32,
        format: u32,
        type_: u32,
        view: &ArrayBufferView,
        src_offset: u32,
    ) -> Fallible<()> {
        if type_ == constants::FLOAT_32_UNSIGNED_INT_24_8_REV {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        let Ok(TexImage2DValidatorResult {
            texture,
            target,
            width,
            height,
            level,
            border,
            internal_format,
            format,
            data_type,
        }) = TexImage2DValidator::new(
            &self.base,
            target,
            level,
            internalformat as u32,
            width,
            height,
            border,
            format,
            type_,
        )
        .validate()
        else {
            return Ok(());
        };
        let unpacking_alignment = self.base.texture_unpacking_alignment();
        // Checks that the view's element type matches `type`.
        if self
            .base
            .validate_tex_image_2d_data(
                width,
                height,
                format,
                data_type,
                unpacking_alignment,
                Some(view),
            )
            .is_err()
        {
            return Ok(());
        }
        let elem_size = view.get_array_type().byte_size().unwrap_or(1);
        let src_byte_offset = src_offset as usize * elem_size;
        let bytes = array_buffer_view_bytes(&view, no_gc);
        if src_byte_offset > bytes.len() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        let Some(buff) = self.unpack_client_bytes(
            &bytes[src_byte_offset..],
            width,
            height,
            1,
            format,
            data_type,
            unpacking_alignment,
            false,
        ) else {
            return Ok(());
        };
        let size = Size2D::new(width, height);
        let (alpha_treatment, y_axis_treatment) =
            self.base.get_current_unpack_state(Alpha::NotPremultiplied);
        self.base.tex_image_2d(
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

    /// texSubImage2D from client memory (`src_offset` in elements of `view`), honouring the
    /// WebGL 2 unpack parameters.
    #[allow(clippy::too_many_arguments)]
    fn tex_sub_image_2d_from_view(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
        view: &ArrayBufferView,
        src_offset: u32,
    ) -> Fallible<()> {
        let Some(TexSubImage2DArgs {
            texture,
            target,
            width,
            height,
            level,
            format,
            data_type,
        }) = self.validate_tex_sub_image_2d(
            target, level, xoffset, yoffset, width, height, format, type_,
        )
        else {
            return Ok(());
        };
        let unpacking_alignment = self.base.texture_unpacking_alignment();
        if self
            .base
            .validate_tex_image_2d_data(
                width,
                height,
                format,
                data_type,
                unpacking_alignment,
                Some(view),
            )
            .is_err()
        {
            return Ok(());
        }
        let elem_size = view.get_array_type().byte_size().unwrap_or(1);
        let src_byte_offset = src_offset as usize * elem_size;
        let bytes = array_buffer_view_bytes(&view, no_gc);
        if src_byte_offset > bytes.len() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        let Some(buff) = self.unpack_client_bytes(
            &bytes[src_byte_offset..],
            width,
            height,
            1,
            format,
            data_type,
            unpacking_alignment,
            false,
        ) else {
            return Ok(());
        };
        let (alpha_treatment, y_axis_treatment) =
            self.base.get_current_unpack_state(Alpha::NotPremultiplied);
        let size = Size2D::new(width, height);
        self.base.tex_sub_image_2d_from_source(
            texture,
            target,
            level,
            xoffset,
            yoffset,
            size,
            format,
            data_type,
            unpacking_alignment,
            TexSource::Pixels(TexPixels::from_array(
                buff,
                size,
                alpha_treatment,
                y_axis_treatment,
            )),
        );
        Ok(())
    }

    /// The (format, type) pairs readPixels accepts for the current read buffer: RGBA with the
    /// type of its component class, and the implementation's IMPLEMENTATION_COLOR_READ_* pair,
    /// which is RGBA/HALF_FLOAT for 16-bit float buffers (as in Chromium) and otherwise the same.
    fn read_pixels_formats(&self) -> ((u32, u32), (u32, u32)) {
        let internal_format = self
            .base
            .get_read_framebuffer_slot()
            .get()
            .and_then(|fb| fb.attachment_internal_format(fb.read_buffer()));
        let primary = match internal_format {
            Some(
                constants::R16F |
                constants::RG16F |
                constants::RGBA16F |
                constants::RGB16F |
                constants::R32F |
                constants::RG32F |
                constants::RGBA32F |
                constants::RGB32F |
                constants::R11F_G11F_B10F,
            ) => (constants::RGBA, constants::FLOAT),
            Some(
                constants::R8I |
                constants::R16I |
                constants::R32I |
                constants::RG8I |
                constants::RG16I |
                constants::RG32I |
                constants::RGBA8I |
                constants::RGBA16I |
                constants::RGBA32I,
            ) => (constants::RGBA_INTEGER, constants::INT),
            Some(
                constants::R8UI |
                constants::R16UI |
                constants::R32UI |
                constants::RG8UI |
                constants::RG16UI |
                constants::RG32UI |
                constants::RGBA8UI |
                constants::RGBA16UI |
                constants::RGBA32UI |
                constants::RGB10_A2UI,
            ) => (constants::RGBA_INTEGER, constants::UNSIGNED_INT),
            _ => (constants::RGBA, constants::UNSIGNED_BYTE),
        };
        let implementation = match internal_format {
            Some(constants::R16F | constants::RG16F | constants::RGBA16F | constants::RGB16F) => {
                (constants::RGBA, constants::HALF_FLOAT)
            },
            // GL ES 3.0 4.3.2: an RGB10_A2 buffer also reads as RGBA/UNSIGNED_INT_2_10_10_10_REV.
            Some(constants::RGB10_A2) => (constants::RGBA, constants::UNSIGNED_INT_2_10_10_10_REV),
            _ => primary,
        };
        (primary, implementation)
    }

    /// Makes `to` the bound transform feedback object (None: the default one) in the
    /// script-side mirror: the indexed TRANSFORM_FEEDBACK_BUFFER bindings are per-object
    /// state, so the outgoing object's are saved (unless it is being deleted) and the
    /// incoming object's restored.
    fn switch_transform_feedback(&self, to: Option<&WebGLTransformFeedback>, save_current: bool) {
        let copy = |from: &[IndexedBinding], into: &[IndexedBinding]| {
            for (from, into) in from.iter().zip(into.iter()) {
                into.buffer.set(from.buffer.get().as_deref());
                into.start.set(from.start.get());
                into.size.set(from.size.get());
            }
        };
        if save_current {
            self.with_current_transform_feedback(|tf| {
                copy(
                    &self.indexed_transform_feedback_buffer_bindings,
                    tf.saved_buffer_bindings(),
                )
            });
        }
        self.current_transform_feedback.set(to);
        self.with_current_transform_feedback(|tf| {
            copy(
                tf.saved_buffer_bindings(),
                &self.indexed_transform_feedback_buffer_bindings,
            )
        });
    }

    /// Runs `f` on the state of the bound transform feedback object, or of the default
    /// object when none is bound.
    fn with_current_transform_feedback<R>(
        &self,
        f: impl FnOnce(&TransformFeedbackState) -> R,
    ) -> R {
        match self.current_transform_feedback.get() {
            Some(tf) => f(tf.state()),
            None => f(&self.default_transform_feedback),
        }
    }

    /// Transform feedback checks for a draw (OpenGL ES 3.0.6 2.15.2): while recording, only
    /// non-indexed draws in the begin primitive mode are allowed, and every bound buffer
    /// (range) must hold all vertices recorded since beginTransformFeedback. Writing past
    /// the end is left to the driver otherwise. Returns the vertices this draw records
    /// (to count once it is issued), or None when nothing is recorded. Argument errors are
    /// left to the base draw, which reports them first.
    fn validate_transform_feedback_for_draw(
        &self,
        mode: u32,
        count: i32,
        primcount: i32,
        indexed: bool,
    ) -> WebGLResult<Option<u64>> {
        let (recording, primitive_mode, recorded) = self.with_current_transform_feedback(|tf| {
            (
                tf.is_active_unpaused(),
                tf.primitive_mode(),
                tf.vertices_recorded(),
            )
        });
        if !recording || count < 0 || primcount < 0 {
            return Ok(None);
        }
        let vertices_per_primitive = match mode {
            constants::POINTS => 1,
            constants::LINES => 2,
            constants::TRIANGLES => 3,
            constants::LINE_STRIP |
            constants::LINE_LOOP |
            constants::TRIANGLE_STRIP |
            constants::TRIANGLE_FAN => return Err(InvalidOperation),
            _ => return Ok(None),
        };
        if indexed || mode != primitive_mode {
            return Err(InvalidOperation);
        }
        let Some(program) = self.base.current_program() else {
            return Ok(None);
        };
        let count = count as u64;
        let vertices = (count - count % vertices_per_primitive) * primcount as u64;
        let total = recorded.checked_add(vertices).ok_or(InvalidOperation)?;
        for (index, stride) in program
            .transform_feedback_buffer_strides()
            .into_iter()
            .enumerate()
        {
            let binding = self
                .indexed_transform_feedback_buffer_bindings
                .get(index)
                .ok_or(InvalidOperation)?;
            let buffer = binding.buffer.get().ok_or(InvalidOperation)?;
            let mut available =
                (buffer.capacity() as u64).saturating_sub(binding.start.get().max(0) as u64);
            if binding.size.get() > 0 {
                available = available.min(binding.size.get() as u64);
            }
            let needed = total.checked_mul(stride).ok_or(InvalidOperation)?;
            if needed > available {
                return Err(InvalidOperation);
            }
        }
        Ok(Some(vertices))
    }

    /// WebGL 2.0 5.1: a buffer bound to one of the bound transform feedback object's
    /// indexed binding points that the current program records into may not be read by a
    /// draw at the same time, as an enabled vertex attribute array or as a uniform buffer
    /// the program uses. While recording, it may not be bound anywhere else at all.
    fn validate_transform_feedback_buffers_for_draw(&self, cx: &mut JSContext) -> WebGLResult<()> {
        let Some(program) = self.base.current_program() else {
            return Ok(());
        };
        if program.transform_feedback_varyings_length() <= 0 {
            return Ok(());
        }
        let used = program.transform_feedback_buffer_strides().len();
        let tf_buffers: Vec<_> = self
            .indexed_transform_feedback_buffer_bindings
            .iter()
            .take(used)
            .filter_map(|binding| binding.buffer.get())
            .map(|buffer| buffer.id())
            .collect();
        if tf_buffers.is_empty() {
            return Ok(());
        }
        let is_tf_buffer = |buffer: Option<&WebGLBuffer>| {
            buffer.is_some_and(|buffer| tf_buffers.contains(&buffer.id()))
        };
        let vao = self.base.current_vao_webgl2(cx);
        for attrib in program.active_attribs().iter() {
            let Some(location) = attrib.location else {
                continue;
            };
            if let Some(data) = vao.get_vertex_attrib(location) &&
                data.enabled_as_array &&
                is_tf_buffer(data.buffer.as_deref())
            {
                return Err(InvalidOperation);
            }
        }
        for &index in program.uniform_block_bindings().iter() {
            if let Some(binding) = self.indexed_uniform_buffer_bindings.get(index as usize) &&
                is_tf_buffer(binding.buffer.get().as_deref())
            {
                return Err(InvalidOperation);
            }
        }
        if self.with_current_transform_feedback(|tf| tf.is_active_unpaused()) {
            let generic = [
                self.base.array_buffer_slot().get(),
                vao.element_array_buffer().get(),
                self.bound_copy_read_buffer.get(),
                self.bound_copy_write_buffer.get(),
                self.bound_pixel_pack_buffer.get(),
                self.bound_pixel_unpack_buffer.get(),
                self.bound_uniform_buffer.get(),
            ];
            if generic.iter().any(|buffer| is_tf_buffer(buffer.as_deref())) ||
                self.indexed_uniform_buffer_bindings
                    .iter()
                    .any(|binding| is_tf_buffer(binding.buffer.get().as_deref()))
            {
                return Err(InvalidOperation);
            }
        }
        Ok(())
    }

    /// WebGL 2.0 5.1: a buffer bound to an indexed binding point of the bound transform feedback
    /// object and also to a binding point outside transform feedback (a generic target other than
    /// TRANSFORM_FEEDBACK_BUFFER, an indexed uniform buffer binding, or the bound vertex array) may
    /// not be read or written through the API: INVALID_OPERATION.
    fn is_double_bound_with_transform_feedback(&self, buffer: &WebGLBuffer) -> bool {
        let is_buffer =
            |slot: &MutNullableDom<WebGLBuffer>| slot.get().is_some_and(|b| buffer == &*b);
        if !self
            .indexed_transform_feedback_buffer_bindings
            .iter()
            .any(|binding| is_buffer(&binding.buffer))
        {
            return false;
        }
        [
            self.base.array_buffer_slot(),
            &self.bound_copy_read_buffer,
            &self.bound_copy_write_buffer,
            &self.bound_pixel_pack_buffer,
            &self.bound_pixel_unpack_buffer,
            &self.bound_uniform_buffer,
        ]
        .into_iter()
        .any(is_buffer) ||
            self.indexed_uniform_buffer_bindings
                .iter()
                .any(|binding| is_buffer(&binding.buffer)) ||
            self.base.current_vao_webgl2_binds_buffer(buffer)
    }

    /// `bound_buffer` for bufferData, bufferSubData, getBufferSubData and copyBufferSubData: a
    /// buffer double bound with transform feedback is INVALID_OPERATION.
    fn bound_buffer_for_data(
        &self,
        cx: &mut JSContext,
        target: u32,
    ) -> WebGLResult<Option<DomRoot<WebGLBuffer>>> {
        let buffer = self.bound_buffer(cx, target)?;
        if let Some(buffer) = &buffer &&
            self.is_double_bound_with_transform_feedback(buffer)
        {
            return Err(InvalidOperation);
        }
        Ok(buffer)
    }

    /// Issues a draw after the transform feedback checks, counting the recorded vertices
    /// when the draw goes through. `cached`: the draw-state checks passed already (draw_state_cached).
    #[allow(clippy::too_many_arguments)]
    fn draw_with_transform_feedback(
        &self,
        cx: &mut JSContext,
        mode: u32,
        count: i32,
        primcount: i32,
        indexed: bool,
        cached: bool,
        draw: impl FnOnce(&mut JSContext) -> WebGLResult<()>,
    ) -> WebGLResult<()> {
        // Reads no draw argument: one of the cached draw-state checks.
        if !cached {
            self.validate_transform_feedback_buffers_for_draw(cx)?;
        }
        let recorded =
            self.validate_transform_feedback_for_draw(mode, count, primcount, indexed)?;
        draw(cx)?;
        if let Some(vertices) = recorded {
            self.with_current_transform_feedback(|tf| tf.add_vertices_recorded(vertices));
        }
        Ok(())
    }

    /// drawArrays(Instanced) with every WebGL 2 check; also each WEBGL_multi_draw entry.
    pub(crate) fn draw_arrays_checked(
        &self,
        cx: &mut JSContext,
        mode: u32,
        first: i32,
        count: i32,
        primcount: i32,
    ) -> WebGLResult<()> {
        self.draw_arrays_checked_base_instance(cx, mode, first, count, primcount, 0)
    }

    /// `draw_arrays_checked` with instanced attributes starting at `base_instance`
    /// (WEBGL_(multi_)draw_instanced_base_vertex_base_instance).
    pub(crate) fn draw_arrays_checked_base_instance(
        &self,
        cx: &mut JSContext,
        mode: u32,
        first: i32,
        count: i32,
        primcount: i32,
        base_instance: u32,
    ) -> WebGLResult<()> {
        // Partii (perf): the state checks passed already for this program and vertex array, with
        // nothing sent since that could change their outcome (draw_state_cached, DrawStateGen).
        let cached = self.draw_state_checks(cx, mode, first as i64, count, primcount)?;
        self.draw_with_transform_feedback(cx, mode, count, primcount, false, cached, |cx| {
            self.base.draw_arrays_instanced_base_instance_checked(
                cx,
                mode,
                first,
                count,
                primcount,
                base_instance,
                cached,
            )
        })
    }

    /// drawElements(Instanced) with every WebGL 2 check; also each WEBGL_multi_draw entry.
    pub(crate) fn draw_elements_checked(
        &self,
        cx: &mut JSContext,
        mode: u32,
        count: i32,
        type_: u32,
        offset: i64,
        primcount: i32,
    ) -> WebGLResult<()> {
        self.draw_elements_checked_base_vertex_base_instance(
            cx, mode, count, type_, offset, primcount, 0, 0,
        )
    }

    /// `draw_elements_checked` with a base vertex and base instance
    /// (WEBGL_(multi_)draw_instanced_base_vertex_base_instance).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_elements_checked_base_vertex_base_instance(
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
        // Partii (perf): as in draw_arrays_checked_base_instance.
        let cached = self.draw_state_checks(cx, mode, offset, count, primcount)?;
        self.draw_with_transform_feedback(cx, mode, count, primcount, true, cached, |cx| {
            self.base.draw_elements_instanced_base_vertex_base_instance_checked(
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
        })
    }

    /// WebGL 2.0, 5.22 / GL ES 3.0, 2.11.2: each active sampler must sample a texture of its own
    /// kind: float samplers a float/normalized texture, isampler / usampler a signed / unsigned
    /// integer one, and shadow samplers a depth texture whose effective TEXTURE_COMPARE_MODE
    /// (the bound sampler object's, else the texture's) is COMPARE_REF_TO_TEXTURE. A depth
    /// texture in that mode is no longer a float texture. Otherwise the draw fails with
    /// INVALID_OPERATION. Units without a complete texture are left alone. Also catches
    /// texture/framebuffer feedback loops.
    fn validate_sampler_types_for_draw(&self) -> WebGLResult<()> {
        let Some(program) = self.base.current_program() else {
            return Ok(());
        };
        // Partii (perf): the units the samplers read, all of them before any check can fail, for
        // the draw-state cache's per-unit test (draw_state_cached).
        {
            let uniforms = program.active_uniforms();
            let mut units = program.draw_sampler_units_mut();
            units.clear();
            for uniform in uniforms.iter() {
                if sampler_target_and_kind(uniform.type_).is_none() {
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
        self.check_sampler_types_for_draw()
    }

    /// The sampler type and feedback-loop checks of validate_sampler_types_for_draw, without
    /// listing the program's sampler units again (the draw-state cache's sampler-only path: the
    /// units can't have changed, as uniform1i(v) and links end every record).
    fn check_sampler_types_for_draw(&self) -> WebGLResult<()> {
        let Some(program) = self.base.current_program() else {
            return Ok(());
        };
        for uniform in program.active_uniforms().iter() {
            let Some((target, sampler_kind)) = sampler_target_and_kind(uniform.type_) else {
                continue;
            };
            let Some(location) = uniform.location else {
                continue;
            };
            for element in 0..uniform.size.unwrap_or(1).max(1) {
                let unit = program.sampler_unit(location + element);
                let Some(texture) = self.base.textures().texture_at(unit, target) else {
                    continue;
                };
                let Some(format) = texture.base_internal_format() else {
                    continue;
                };
                let compare_mode = match self.samplers.get(unit as usize).and_then(|s| s.get()) {
                    Some(sampler) => sampler.compare_mode(),
                    None => texture.compare_mode(),
                };
                if texture_kind(format, compare_mode) != sampler_kind {
                    return Err(InvalidOperation);
                }
                // WebGL 2.0, 5.19: sampling a texture level that the draw framebuffer renders
                // into is a feedback loop.
                if let Some(framebuffer) = self.base.get_draw_framebuffer_slot().get() &&
                    let Some((first, last)) = texture.sampled_levels()
                {
                    if framebuffer
                        .texture_attachment_levels(&texture)
                        .into_iter()
                        .any(|level| level >= 0 && (first..=last).contains(&(level as u32)))
                    {
                        return Err(InvalidOperation);
                    }
                }
            }
        }
        Ok(())
    }

    /// True if UNPACK_FLIP_Y_WEBGL or UNPACK_PREMULTIPLY_ALPHA_WEBGL is set. 3D uploads from a
    /// PIXEL_UNPACK_BUFFER or an ArrayBufferView must then fail with INVALID_OPERATION.
    fn unpack_flip_or_premultiply(&self) -> bool {
        let (alpha_treatment, y_axis_treatment) =
            self.base.get_current_unpack_state(Alpha::NotPremultiplied);
        alpha_treatment.is_some() || y_axis_treatment == YAxisTreatment::Flipped
    }

    /// Bytes an upload of `depth` images of `width` x `height` reads, with rows padded to
    /// `unpacking_alignment` but no padding after the last row.
    fn unpack_byte_len(
        width: u32,
        height: u32,
        depth: u32,
        format: TexFormat,
        data_type: TexDataType,
        unpacking_alignment: u32,
    ) -> u64 {
        if width == 0 || height == 0 || depth == 0 {
            return 0;
        }
        let (width, height, depth) = (width as u64, height as u64, depth as u64);
        let cpp = (data_type.element_size() * format.components() /
            data_type.components_per_element()) as u64;
        let alignment = unpacking_alignment as u64;
        let bytes_last_row = cpp * width;
        let bytes_per_row = bytes_last_row.div_ceil(alignment) * alignment;
        let bytes_per_image = bytes_per_row * height;
        bytes_per_image * (depth - 1) + bytes_per_row * (height - 1) + bytes_last_row
    }

    /// Checks an upload from the bound PIXEL_UNPACK_BUFFER that reads `byte_len` bytes at `offset`.
    fn validate_pixel_unpack_buffer(&self, offset: i64, byte_len: u64) -> bool {
        let Some(buffer) = self.bound_pixel_unpack_buffer.get() else {
            self.base.webgl_error(InvalidOperation);
            return false;
        };
        if self.is_double_bound_with_transform_feedback(&buffer) {
            self.base.webgl_error(InvalidOperation);
            return false;
        }
        if offset < 0 {
            self.base.webgl_error(InvalidValue);
            return false;
        }
        if offset as u64 + byte_len > buffer.capacity() as u64 {
            self.base.webgl_error(InvalidOperation);
            return false;
        }
        true
    }

    /// Fails with INVALID_OPERATION if a buffer is bound to PIXEL_UNPACK_BUFFER: uploads from
    /// client memory or a TexImageSource are not allowed then.
    fn forbid_pixel_unpack_buffer(&self) -> bool {
        if self.bound_pixel_unpack_buffer.get().is_some() {
            self.base.webgl_error(InvalidOperation);
            return true;
        }
        false
    }

    /// Whether the WebGL thread can convert an RGBA8 image source into `format` / `data_type`
    /// (see `image_to_tex_image_data` there). Anything else fails with INVALID_OPERATION here
    /// rather than reaching the WebGL thread.
    fn can_convert_image_source(format: TexFormat, data_type: TexDataType) -> bool {
        use TexDataType as T;
        use TexFormat as F;
        let known_format = matches!(
            format.to_unsized(),
            F::Red |
                F::RedInteger |
                F::RG |
                F::RGInteger |
                F::RGB |
                F::RGBInteger |
                F::RGBA |
                F::RGBAInteger |
                F::Alpha |
                F::Luminance |
                F::LuminanceAlpha
        );
        let known_type = match data_type {
            T::UnsignedByte | T::Float | T::HalfFloat => true,
            T::UnsignedShort565 | T::UnsignedInt10f11f11fRev | T::UnsignedInt5999Rev => {
                format.to_unsized() == F::RGB
            },
            T::UnsignedShort4444 | T::UnsignedShort5551 | T::UnsignedInt2101010Rev => {
                format.to_unsized() == F::RGBA
            },
            _ => false,
        };
        known_format && known_type
    }

    /// Validation shared by the WebGL 2 texSubImage2D overloads: the WebGL 1 path validates
    /// `format` as if it were an internal format, which rejects the WebGL 2 transfer formats
    /// (RED, RG, *_INTEGER) with INVALID_ENUM.
    #[allow(clippy::too_many_arguments)]
    fn validate_tex_sub_image_2d(
        &self,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
    ) -> Option<TexSubImage2DArgs> {
        let target = match TexImageTarget::from_gl_constant(target) {
            Some(target) if target.dimensions() == 2 && target != TexImageTarget::CubeMap => target,
            _ => {
                self.base.webgl_error(InvalidEnum);
                return None;
            },
        };
        let Some(texture) = self.base.textures().active_texture_for_image_target(target) else {
            self.base.webgl_error(InvalidOperation);
            return None;
        };
        let version = self.base.webgl_version();
        let Some(data_type) = TexDataType::from_webgl_constant(type_, version)
            .filter(|data_type| data_type.required_webgl_version() <= version)
        else {
            self.base.webgl_error(InvalidEnum);
            return None;
        };
        let Some(format) = TexFormat::from_gl_constant(format)
            .filter(|format| !format.is_sized() || format.to_unsized() == *format)
        else {
            self.base.webgl_error(InvalidEnum);
            return None;
        };
        if level < 0 || width < 0 || height < 0 {
            self.base.webgl_error(InvalidValue);
            return None;
        }
        let level = level as u32;
        let Some(image_info) = texture.image_info_for_target(&target, level) else {
            self.base.webgl_error(InvalidOperation);
            return None;
        };
        let (width, height) = (width as u32, height as u32);
        let fits = |offset: i32, size: u32, max: u32| {
            offset >= 0 && offset as u64 + size as u64 <= max as u64
        };
        if !fits(xoffset, width, image_info.width()) || !fits(yoffset, height, image_info.height())
        {
            self.base.webgl_error(InvalidValue);
            return None;
        }
        let internal_format = image_info.internal_format();
        if internal_format.is_compressed() ||
            format != internal_format.to_unsized() ||
            !Self::type_allowed_for(internal_format, data_type)
        {
            self.base.webgl_error(InvalidOperation);
            return None;
        }
        Some(TexSubImage2DArgs {
            texture,
            target,
            width,
            height,
            level,
            format,
            data_type,
        })
    }

    /// Whether `data_type` may upload into `internal_format` in WebGL 2. Unsized formats take
    /// the GL ES 3.0 unsized combinations only: FLOAT and HALF_FLOAT with them are WebGL 1
    /// extensions (OES_texture_float), which WebGL 2 doesn't have.
    fn type_allowed_for(internal_format: TexFormat, data_type: TexDataType) -> bool {
        if !internal_format.is_sized() &&
            matches!(data_type, TexDataType::Float | TexDataType::HalfFloat)
        {
            return false;
        }
        internal_format.compatible_data_types().contains(&data_type)
    }

    /// Validation shared by the texSubImage3D overloads, per
    /// <https://registry.khronos.org/OpenGL-Refpages/es3.0/html/glTexSubImage3D.xhtml>
    /// and <https://registry.khronos.org/webgl/specs/latest/2.0/#3.7.6>.
    #[allow(clippy::too_many_arguments)]
    fn validate_tex_sub_image_3d(
        &self,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        width: i32,
        height: i32,
        depth: i32,
        format: u32,
        type_: u32,
    ) -> Option<TexSubImage3DArgs> {
        let target = match TexImageTarget::from_gl_constant(target) {
            Some(target) if target.dimensions() == 3 => target,
            _ => {
                self.base.webgl_error(InvalidEnum);
                return None;
            },
        };
        let Some(texture) = self.base.textures().active_texture_for_image_target(target) else {
            self.base.webgl_error(InvalidOperation);
            return None;
        };
        let version = self.base.webgl_version();
        let Some(data_type) = TexDataType::from_webgl_constant(type_, version)
            .filter(|data_type| data_type.required_webgl_version() <= version)
        else {
            self.base.webgl_error(InvalidEnum);
            return None;
        };
        let Some(format) = TexFormat::from_gl_constant(format)
            .filter(|format| !format.is_sized() || format.to_unsized() == *format)
        else {
            self.base.webgl_error(InvalidEnum);
            return None;
        };
        if level < 0 || width < 0 || height < 0 || depth < 0 {
            self.base.webgl_error(InvalidValue);
            return None;
        }
        let level = level as u32;
        let Some(image_info) = texture.image_info_for_target(&target, level) else {
            self.base.webgl_error(InvalidOperation);
            return None;
        };
        let (width, height, depth) = (width as u32, height as u32, depth as u32);
        let fits = |offset: i32, size: u32, max: u32| {
            offset >= 0 && offset as u64 + size as u64 <= max as u64
        };
        if !fits(xoffset, width, image_info.width()) ||
            !fits(yoffset, height, image_info.height()) ||
            !fits(zoffset, depth, image_info.depth())
        {
            self.base.webgl_error(InvalidValue);
            return None;
        }
        let internal_format = image_info.internal_format();
        if internal_format.is_compressed() ||
            format != internal_format.to_unsized() ||
            !Self::type_allowed_for(internal_format, data_type)
        {
            self.base.webgl_error(InvalidOperation);
            return None;
        }
        Some(TexSubImage3DArgs {
            target,
            level,
            xoffset,
            yoffset,
            zoffset,
            width,
            height,
            depth,
            format,
            data_type,
        })
    }

    fn tex_sub_image_3d(
        &self,
        args: TexSubImage3DArgs,
        unpacking_alignment: u32,
        source: TexSource,
    ) {
        let effective_data_type = self
            .base
            .extension_manager()
            .effective_type(args.data_type.as_gl_constant());
        let size = Size2D::new(args.width, args.height);
        let command = match source {
            TexSource::Pixels(pixels) => WebGLCommand::TexSubImage3D {
                target: args.target.as_gl_constant(),
                level: args.level,
                xoffset: args.xoffset,
                yoffset: args.yoffset,
                zoffset: args.zoffset,
                size,
                depth: args.depth,
                format: args.format,
                data_type: args.data_type,
                effective_data_type,
                unpacking_alignment,
                alpha_treatment: pixels.alpha_treatment(),
                y_axis_treatment: pixels.y_axis_treatment(),
                pixel_format: pixels.pixel_format(),
                data: pixels.into_shared_memory().into(),
            },
            TexSource::BufferOffset(offset) => WebGLCommand::TexSubImage3DPBO {
                target: args.target.as_gl_constant(),
                level: args.level,
                xoffset: args.xoffset,
                yoffset: args.yoffset,
                zoffset: args.zoffset,
                size,
                depth: args.depth,
                format: args.format,
                effective_data_type,
                unpacking_alignment,
                offset,
            },
        };
        if let WebGLCommand::TexSubImage3DPBO { .. } = command {
            self.with_unpack_params(true, || self.base.send_command(command));
        } else {
            self.base.send_command(command);
        }
    }

    #[expect(clippy::too_many_arguments)]
    fn tex_image_3d(
        &self,
        texture: &WebGLTexture,
        target: TexImageTarget,
        data_type: TexDataType,
        internal_format: TexFormat,
        format: TexFormat,
        level: u32,
        width: u32,
        height: u32,
        depth: u32,
        _border: u32,
        unpacking_alignment: u32,
        data: TexPixels,
    ) {
        handle_potential_webgl_error!(
            self.base,
            texture.initialize(
                target,
                width,
                height,
                depth,
                internal_format,
                level,
                Some(data_type)
            )
        );

        let internal_format = self
            .base
            .extension_manager()
            .get_effective_tex_internal_format(internal_format, data_type.as_gl_constant());
        let effective_data_type = self
            .base
            .extension_manager()
            .effective_type(data_type.as_gl_constant());

        self.base.send_command(WebGLCommand::TexImage3D {
            target: target.as_gl_constant(),
            level,
            internal_format,
            // Not data.size(): image sources arrive as one image of `depth` slices stacked
            // vertically, height * depth rows tall.
            size: Size2D::new(width, height),
            depth,
            format,
            data_type,
            effective_data_type,
            unpacking_alignment,
            alpha_treatment: data.alpha_treatment(),
            y_axis_treatment: data.y_axis_treatment(),
            pixel_format: data.pixel_format(),
            data: data.into_shared_memory().into(),
        });
        // TODO: Hint/tex_parameter

        if let Some(fb) = self.base.bound_draw_framebuffer() {
            fb.invalidate_texture(texture);
        }
    }
}

/// The component type of a colour format, which blitFramebuffer requires to match.
fn color_component_class(format: u32) -> u8 {
    match format {
        constants::R16F |
        constants::RG16F |
        constants::RGB16F |
        constants::RGBA16F |
        constants::R32F |
        constants::RG32F |
        constants::RGB32F |
        constants::RGBA32F |
        constants::R11F_G11F_B10F => 1,
        constants::R8I |
        constants::R16I |
        constants::R32I |
        constants::RG8I |
        constants::RG16I |
        constants::RG32I |
        constants::RGB8I |
        constants::RGB16I |
        constants::RGB32I |
        constants::RGBA8I |
        constants::RGBA16I |
        constants::RGBA32I => 2,
        constants::R8UI |
        constants::R16UI |
        constants::R32UI |
        constants::RG8UI |
        constants::RG16UI |
        constants::RG32UI |
        constants::RGB8UI |
        constants::RGB16UI |
        constants::RGB32UI |
        constants::RGBA8UI |
        constants::RGBA16UI |
        constants::RGBA32UI |
        constants::RGB10_A2UI => 3,
        _ => 0,
    }
}

/// WebGL 2's unpack pixel-store parameters (UNPACK_ALIGNMENT stays in the base context).
#[derive(Clone, Copy, Default, MallocSizeOf, PartialEq)]
struct UnpackParams {
    row_length: u32,
    image_height: u32,
    skip_pixels: u32,
    skip_rows: u32,
    skip_images: u32,
}

/// Where the pixels of an upload sit in the source, per the GL ES 3.0 unpack rules (3.7.2).
struct UnpackLayout {
    /// Bytes from the start of one row to the next.
    row_stride: u64,
    /// Bytes from the start of one image (3D slice) to the next.
    image_stride: u64,
    /// Offset of the first pixel read.
    first: u64,
    /// Bytes past the start of the source that the upload reads: `first` plus the extent.
    end: u64,
}

/// The kind of texture a sampler reads, for the draw-time sampler type check.
#[derive(Clone, Copy, PartialEq)]
enum SamplerKind {
    Float,
    Int,
    Uint,
    Shadow,
}

/// The texture target and kind of a sampler uniform type; None for non-samplers.
fn sampler_target_and_kind(type_: u32) -> Option<(u32, SamplerKind)> {
    use SamplerKind::*;
    Some(match type_ {
        constants::SAMPLER_2D => (constants::TEXTURE_2D, Float),
        constants::SAMPLER_3D => (constants::TEXTURE_3D, Float),
        constants::SAMPLER_CUBE => (constants::TEXTURE_CUBE_MAP, Float),
        constants::SAMPLER_2D_ARRAY => (constants::TEXTURE_2D_ARRAY, Float),
        constants::INT_SAMPLER_2D => (constants::TEXTURE_2D, Int),
        constants::INT_SAMPLER_3D => (constants::TEXTURE_3D, Int),
        constants::INT_SAMPLER_CUBE => (constants::TEXTURE_CUBE_MAP, Int),
        constants::INT_SAMPLER_2D_ARRAY => (constants::TEXTURE_2D_ARRAY, Int),
        constants::UNSIGNED_INT_SAMPLER_2D => (constants::TEXTURE_2D, Uint),
        constants::UNSIGNED_INT_SAMPLER_3D => (constants::TEXTURE_3D, Uint),
        constants::UNSIGNED_INT_SAMPLER_CUBE => (constants::TEXTURE_CUBE_MAP, Uint),
        constants::UNSIGNED_INT_SAMPLER_2D_ARRAY => (constants::TEXTURE_2D_ARRAY, Uint),
        constants::SAMPLER_2D_SHADOW => (constants::TEXTURE_2D, Shadow),
        constants::SAMPLER_CUBE_SHADOW => (constants::TEXTURE_CUBE_MAP, Shadow),
        constants::SAMPLER_2D_ARRAY_SHADOW => (constants::TEXTURE_2D_ARRAY, Shadow),
        _ => return None,
    })
}

/// The kind of sampler a texture with base level `format` can be read with.
fn texture_kind(format: TexFormat, compare_mode: u32) -> SamplerKind {
    use TexFormat as F;
    match format {
        F::R8i |
        F::R16i |
        F::R32i |
        F::RG8i |
        F::RG16i |
        F::RG32i |
        F::RGB8i |
        F::RGB16i |
        F::RGB32i |
        F::RGBA8i |
        F::RGBA16i |
        F::RGBA32i => SamplerKind::Int,
        F::R8ui |
        F::R16ui |
        F::R32ui |
        F::RG8ui |
        F::RG16ui |
        F::RG32ui |
        F::RGB8ui |
        F::RGB16ui |
        F::RGB32ui |
        F::RGBA8ui |
        F::RGBA16ui |
        F::RGBA32ui |
        F::RGB10A2ui => SamplerKind::Uint,
        F::DepthComponent |
        F::DepthStencil |
        F::DepthComponent16 |
        F::DepthComponent24 |
        F::DepthComponent32f |
        F::Depth24Stencil8 |
        F::Depth32fStencil8
            if compare_mode == constants::COMPARE_REF_TO_TEXTURE =>
        {
            SamplerKind::Shadow
        },
        _ => SamplerKind::Float,
    }
}

/// The validated arguments of a WebGL 2 texSubImage2D call.
struct TexSubImage2DArgs {
    texture: DomRoot<WebGLTexture>,
    target: TexImageTarget,
    width: u32,
    height: u32,
    level: u32,
    format: TexFormat,
    data_type: TexDataType,
}

/// The validated arguments of a texSubImage3D call.
struct TexSubImage3DArgs {
    target: TexImageTarget,
    level: u32,
    xoffset: i32,
    yoffset: i32,
    zoffset: i32,
    width: u32,
    height: u32,
    depth: u32,
    format: TexFormat,
    data_type: TexDataType,
}

impl CanvasContext for WebGL2RenderingContext {
    type ID = WebGLContextId;

    fn context_id(&self) -> Self::ID {
        self.base.context_id()
    }

    fn canvas(&self) -> Option<RootedHTMLCanvasElementOrOffscreenCanvas> {
        self.base.canvas()
    }

    fn resize(&self) {
        self.base.resize();
    }

    fn reset_bitmap(&self) {
        self.base.reset_bitmap();
    }

    fn get_image_data(&self) -> Option<Snapshot> {
        self.base.get_image_data()
    }

    fn mark_as_dirty(&self) {
        self.base.mark_as_dirty()
    }
}

impl WebGL2RenderingContextMethods<crate::DomTypeHolder> for WebGL2RenderingContext {
    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.1>
    fn Canvas(&self) -> RootedHTMLCanvasElementOrOffscreenCanvas {
        self.base.Canvas()
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.11>
    fn Flush(&self) {
        self.base.Flush()
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.11>
    fn Finish(&self) {
        self.base.Finish()
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.1>
    fn DrawingBufferWidth(&self) -> i32 {
        self.base.DrawingBufferWidth()
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.1>
    fn DrawingBufferHeight(&self) -> i32 {
        self.base.DrawingBufferHeight()
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
            self.base,
            self.bound_buffer(cx, target),
            return retval.set(NullValue())
        );
        self.base.get_buffer_param(buffer, parameter, retval)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn GetParameter(&self, cx: &mut JSContext, parameter: u32, mut rval: MutableHandleValue) {
        let unpack = self.unpack_params.get();
        let pixel_store = match parameter {
            constants::UNPACK_ROW_LENGTH => Some(unpack.row_length as usize),
            constants::UNPACK_IMAGE_HEIGHT => Some(unpack.image_height as usize),
            constants::UNPACK_SKIP_PIXELS => Some(unpack.skip_pixels as usize),
            constants::UNPACK_SKIP_ROWS => Some(unpack.skip_rows as usize),
            constants::UNPACK_SKIP_IMAGES => Some(unpack.skip_images as usize),
            constants::PACK_ROW_LENGTH => Some(self.texture_pack_row_length.get()),
            constants::PACK_SKIP_PIXELS => Some(self.texture_pack_skip_pixels.get()),
            constants::PACK_SKIP_ROWS => Some(self.texture_pack_skip_rows.get()),
            _ => None,
        };
        if let Some(value) = pixel_store {
            return rval.set(Int32Value(value as i32));
        }
        if matches!(
            parameter,
            constants::IMPLEMENTATION_COLOR_READ_FORMAT | constants::IMPLEMENTATION_COLOR_READ_TYPE
        ) {
            if self.base.validate_framebuffer().is_err() {
                self.base.webgl_error(InvalidOperation);
                return rval.set(NullValue());
            }
            let (_, (format, type_)) = self.read_pixels_formats();
            let value = if parameter == constants::IMPLEMENTATION_COLOR_READ_FORMAT {
                format
            } else {
                type_
            };
            return rval.set(Int32Value(value as i32));
        }
        match parameter {
            constants::VERSION => {
                "WebGL 2.0".to_jsval(cx, rval);
                return;
            },
            constants::SHADING_LANGUAGE_VERSION => {
                "WebGL GLSL ES 3.00".to_jsval(cx, rval);
                return;
            },
            constants::MAX_CLIENT_WAIT_TIMEOUT_WEBGL => {
                rval.set(DoubleValue(
                    self.base.limits().max_client_wait_timeout_webgl.as_nanos() as f64,
                ));
                return;
            },
            constants::MAX_SERVER_WAIT_TIMEOUT => {
                rval.set(DoubleValue(
                    self.base.limits().max_server_wait_timeout.as_nanos() as f64,
                ));
                return;
            },
            constants::SAMPLER_BINDING => {
                let idx = (self.base.textures().active_unit_enum() - constants::TEXTURE0) as usize;
                assert!(idx < self.samplers.len());
                let sampler = self.samplers[idx].get();
                sampler.to_jsval(cx, rval);
                return;
            },
            constants::COPY_READ_BUFFER_BINDING => {
                self.bound_copy_read_buffer.get().to_jsval(cx, rval);
                return;
            },
            constants::COPY_WRITE_BUFFER_BINDING => {
                self.bound_copy_write_buffer.get().to_jsval(cx, rval);
                return;
            },
            constants::PIXEL_PACK_BUFFER_BINDING => {
                self.bound_pixel_pack_buffer.get().to_jsval(cx, rval);
                return;
            },
            constants::PIXEL_UNPACK_BUFFER_BINDING => {
                self.bound_pixel_unpack_buffer.get().to_jsval(cx, rval);
                return;
            },
            constants::TRANSFORM_FEEDBACK_BUFFER_BINDING => {
                self.bound_transform_feedback_buffer
                    .get()
                    .to_jsval(cx, rval);
                return;
            },
            constants::UNIFORM_BUFFER_BINDING => {
                self.bound_uniform_buffer.get().to_jsval(cx, rval);
                return;
            },
            constants::TRANSFORM_FEEDBACK_BINDING => {
                self.current_transform_feedback.get().to_jsval(cx, rval);
                return;
            },
            constants::ELEMENT_ARRAY_BUFFER_BINDING => {
                let buffer = self.current_vao(cx).element_array_buffer().get();
                buffer.to_jsval(cx, rval);
                return;
            },
            constants::VERTEX_ARRAY_BINDING => {
                let vao = self.current_vao(cx);
                let vao = vao.id().map(|_| &*vao);
                vao.to_jsval(cx, rval);
                return;
            },
            // NOTE: DRAW_FRAMEBUFFER_BINDING is the same as FRAMEBUFFER_BINDING, handled on the WebGL1 side
            constants::READ_FRAMEBUFFER_BINDING => {
                self.base
                    .get_read_framebuffer_slot()
                    .get()
                    .to_jsval(cx, rval);
                return;
            },
            constants::READ_BUFFER => {
                let buffer = match self.base.get_read_framebuffer_slot().get() {
                    Some(fb) => fb.read_buffer(),
                    None => self.default_fb_readbuffer.get(),
                };
                rval.set(UInt32Value(buffer));
                return;
            },
            constants::DRAW_BUFFER0..=constants::DRAW_BUFFER15 => {
                let buffer = match self.base.get_read_framebuffer_slot().get() {
                    Some(fb) => {
                        let idx = parameter - constants::DRAW_BUFFER0;
                        fb.draw_buffer_i(idx as usize)
                    },
                    None if parameter == constants::DRAW_BUFFER0 => {
                        self.default_fb_readbuffer.get()
                    },
                    None => constants::NONE,
                };
                rval.set(UInt32Value(buffer));
                return;
            },
            constants::MAX_TEXTURE_LOD_BIAS => {
                rval.set(DoubleValue(self.base.limits().max_texture_lod_bias as f64));
                return;
            },
            constants::MAX_COMBINED_FRAGMENT_UNIFORM_COMPONENTS => {
                rval.set(DoubleValue(
                    self.base.limits().max_combined_fragment_uniform_components as f64,
                ));
                return;
            },
            constants::MAX_COMBINED_VERTEX_UNIFORM_COMPONENTS => {
                rval.set(DoubleValue(
                    self.base.limits().max_combined_vertex_uniform_components as f64,
                ));
                return;
            },
            constants::MAX_ELEMENT_INDEX => {
                rval.set(DoubleValue(self.base.limits().max_element_index as f64));
                return;
            },
            constants::MAX_UNIFORM_BLOCK_SIZE => {
                rval.set(DoubleValue(
                    self.base.limits().max_uniform_block_size as f64,
                ));
                return;
            },
            constants::MIN_PROGRAM_TEXEL_OFFSET => {
                rval.set(Int32Value(self.base.limits().min_program_texel_offset));
                return;
            },
            _ => {},
        }

        let limit = match parameter {
            constants::MAX_3D_TEXTURE_SIZE => Some(self.base.limits().max_3d_texture_size),
            constants::MAX_ARRAY_TEXTURE_LAYERS => {
                Some(self.base.limits().max_array_texture_layers)
            },
            constants::MAX_COLOR_ATTACHMENTS => Some(self.base.limits().max_color_attachments),
            constants::MAX_COMBINED_UNIFORM_BLOCKS => {
                Some(self.base.limits().max_combined_uniform_blocks)
            },
            constants::MAX_DRAW_BUFFERS => Some(self.base.limits().max_draw_buffers),
            constants::MAX_ELEMENTS_INDICES => Some(self.base.limits().max_elements_indices),
            constants::MAX_ELEMENTS_VERTICES => Some(self.base.limits().max_elements_vertices),
            constants::MAX_FRAGMENT_INPUT_COMPONENTS => {
                Some(self.base.limits().max_fragment_input_components)
            },
            constants::MAX_FRAGMENT_UNIFORM_BLOCKS => {
                Some(self.base.limits().max_fragment_uniform_blocks)
            },
            constants::MAX_FRAGMENT_UNIFORM_COMPONENTS => {
                Some(self.base.limits().max_fragment_uniform_components)
            },
            constants::MAX_PROGRAM_TEXEL_OFFSET => {
                Some(self.base.limits().max_program_texel_offset)
            },
            constants::MAX_SAMPLES => Some(self.base.limits().max_samples),
            constants::MAX_UNIFORM_BUFFER_BINDINGS => {
                Some(self.base.limits().max_uniform_buffer_bindings)
            },
            constants::MAX_VARYING_COMPONENTS => Some(self.base.limits().max_varying_components),
            constants::MAX_VERTEX_OUTPUT_COMPONENTS => {
                Some(self.base.limits().max_vertex_output_components)
            },
            constants::MAX_VERTEX_UNIFORM_BLOCKS => {
                Some(self.base.limits().max_vertex_uniform_blocks)
            },
            constants::MAX_VERTEX_UNIFORM_COMPONENTS => {
                Some(self.base.limits().max_vertex_uniform_components)
            },
            constants::UNIFORM_BUFFER_OFFSET_ALIGNMENT => {
                Some(self.base.limits().uniform_buffer_offset_alignment)
            },
            _ => None,
        };
        if let Some(limit) = limit {
            rval.set(UInt32Value(limit));
            return;
        }

        self.base.GetParameter(cx, parameter, rval)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn GetTexParameter(
        &self,
        cx: &mut JSContext,
        target: u32,
        pname: u32,
        retval: MutableHandleValue,
    ) {
        self.base.GetTexParameter(cx, target, pname, retval)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn GetError(&self) -> u32 {
        self.base.GetError()
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.2>
    fn GetContextAttributes(&self) -> Option<WebGLContextAttributes> {
        self.base.GetContextAttributes()
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.13>
    fn IsContextLost(&self) -> bool {
        self.base.IsContextLost()
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.14>
    fn GetSupportedExtensions(&self) -> Option<Vec<DOMString>> {
        self.base.GetSupportedExtensions()
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.14>
    fn GetExtension(
        &self,
        cx: &mut js::context::JSContext,
        name: DOMString,
        return_value: MutableHandleObject,
    ) {
        self.base.GetExtension(cx, name, return_value)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.4>
    fn GetFramebufferAttachmentParameter(
        &self,
        cx: &mut JSContext,
        target: u32,
        attachment: u32,
        pname: u32,
        mut rval: MutableHandleValue,
    ) {
        let fb_slot = match target {
            constants::FRAMEBUFFER | constants::DRAW_FRAMEBUFFER => {
                self.base.get_draw_framebuffer_slot()
            },
            constants::READ_FRAMEBUFFER => self.base.get_read_framebuffer_slot(),
            _ => {
                self.base.webgl_error(InvalidEnum);
                rval.set(NullValue());
                return;
            },
        };

        if let Some(fb) = fb_slot.get() {
            // A selected framebuffer is bound to the target
            handle_potential_webgl_error!(
                self.base,
                fb.validate_transparent(),
                return rval.set(NullValue())
            );
            handle_potential_webgl_error!(
                self.base,
                self.get_specific_fb_attachment_param(
                    cx,
                    &fb,
                    target,
                    attachment,
                    pname,
                    rval.reborrow()
                ),
                rval.set(NullValue())
            )
        } else {
            // The default framebuffer is bound to the target
            handle_potential_webgl_error!(
                self.base,
                self.get_default_fb_attachment_param(attachment, pname, rval.reborrow()),
                rval.set(NullValue())
            )
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.7>
    fn GetRenderbufferParameter(
        &self,
        cx: &mut JSContext,
        target: u32,
        pname: u32,
        mut retval: MutableHandleValue,
    ) {
        // RENDERBUFFER_SAMPLES is WebGL 2 only: the sample count renderbufferStorageMultisample
        // asked for (0 for single-sampled storage).
        if target == constants::RENDERBUFFER && pname == constants::RENDERBUFFER_SAMPLES {
            let Some(renderbuffer) = self.base.bound_renderbuffer() else {
                self.base.webgl_error(InvalidOperation);
                return retval.set(NullValue());
            };
            return retval.set(Int32Value(renderbuffer.samples()));
        }
        self.base
            .GetRenderbufferParameter(cx, target, pname, retval)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn ActiveTexture(&self, texture: u32) {
        self.base.ActiveTexture(texture)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn BlendColor(&self, r: f32, g: f32, b: f32, a: f32) {
        self.base.BlendColor(r, g, b, a)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn BlendEquation(&self, mode: u32) {
        self.base.BlendEquation(mode)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn BlendEquationSeparate(&self, mode_rgb: u32, mode_alpha: u32) {
        self.base.BlendEquationSeparate(mode_rgb, mode_alpha)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn BlendFunc(&self, src_factor: u32, dest_factor: u32) {
        self.base.BlendFunc(src_factor, dest_factor)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn BlendFuncSeparate(&self, src_rgb: u32, dest_rgb: u32, src_alpha: u32, dest_alpha: u32) {
        self.base
            .BlendFuncSeparate(src_rgb, dest_rgb, src_alpha, dest_alpha)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn AttachShader(&self, program: &WebGLProgram, shader: &WebGLShader) {
        self.base.AttachShader(program, shader)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn DetachShader(&self, program: &WebGLProgram, shader: &WebGLShader) {
        self.base.DetachShader(program, shader)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn BindAttribLocation(&self, program: &WebGLProgram, index: u32, name: DOMString) {
        self.base.BindAttribLocation(program, index, name)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.2>
    fn BindBuffer(&self, cx: &mut JSContext, target: u32, buffer: Option<&WebGLBuffer>) {
        let current_vao;
        let slot = match target {
            constants::COPY_READ_BUFFER => &self.bound_copy_read_buffer,
            constants::COPY_WRITE_BUFFER => &self.bound_copy_write_buffer,
            constants::PIXEL_PACK_BUFFER => &self.bound_pixel_pack_buffer,
            constants::PIXEL_UNPACK_BUFFER => &self.bound_pixel_unpack_buffer,
            constants::TRANSFORM_FEEDBACK_BUFFER => &self.bound_transform_feedback_buffer,
            constants::UNIFORM_BUFFER => &self.bound_uniform_buffer,
            constants::ELEMENT_ARRAY_BUFFER => {
                current_vao = self.current_vao(cx);
                current_vao.element_array_buffer()
            },
            _ => return self.base.BindBuffer(cx, target, buffer),
        };
        self.base.bind_buffer_maybe(slot, target, buffer);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.6>
    fn BindFramebuffer(&self, target: u32, framebuffer: Option<&WebGLFramebuffer>) {
        handle_potential_webgl_error!(
            self.base,
            self.base.validate_new_framebuffer_binding(framebuffer),
            return
        );

        let (bind_read, bind_draw) = match target {
            constants::FRAMEBUFFER => (true, true),
            constants::READ_FRAMEBUFFER => (true, false),
            constants::DRAW_FRAMEBUFFER => (false, true),
            _ => return self.base.webgl_error(InvalidEnum),
        };
        if bind_read {
            self.base.bind_framebuffer_to(
                target,
                framebuffer,
                self.base.get_read_framebuffer_slot(),
            );
        }
        if bind_draw {
            self.base.bind_framebuffer_to(
                target,
                framebuffer,
                self.base.get_draw_framebuffer_slot(),
            );
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.7>
    fn BindRenderbuffer(&self, target: u32, renderbuffer: Option<&WebGLRenderbuffer>) {
        self.base.BindRenderbuffer(target, renderbuffer)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn BindTexture(&self, target: u32, texture: Option<&WebGLTexture>) {
        self.base.BindTexture(target, texture)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn GenerateMipmap(&self, target: u32) {
        self.base.GenerateMipmap(target)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.5>
    fn BufferData_(
        &self,
        cx: &mut JSContext,
        target: u32,
        data: Option<ArrayBufferViewOrArrayBuffer>,
        usage: u32,
    ) {
        let usage = handle_potential_webgl_error!(self.base, self.buffer_usage(usage), return);
        let bound_buffer = handle_potential_webgl_error!(
            self.base,
            self.bound_buffer_for_data(cx, target),
            return
        );
        self.base
            .buffer_data(cx.no_gc(), target, data, usage, bound_buffer)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.5>
    fn BufferData(&self, cx: &mut JSContext, target: u32, size: i64, usage: u32) {
        let usage = handle_potential_webgl_error!(self.base, self.buffer_usage(usage), return);
        let bound_buffer = handle_potential_webgl_error!(
            self.base,
            self.bound_buffer_for_data(cx, target),
            return
        );
        self.base.buffer_data_(target, size, usage, bound_buffer)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.3>
    fn BufferData__(
        &self,
        cx: &mut JSContext,
        target: u32,
        data: CustomAutoRooterGuard<ArrayBufferView>,
        usage: u32,
        elem_offset: u32,
        length: u32,
    ) {
        let usage = handle_potential_webgl_error!(self.base, self.buffer_usage(usage), return);
        let bound_buffer = handle_potential_webgl_error!(
            self.base,
            self.bound_buffer_for_data(cx, target),
            return
        );
        let bound_buffer =
            handle_potential_webgl_error!(self.base, bound_buffer.ok_or(InvalidOperation), return);

        let elem_size = data.get_array_type().byte_size().unwrap();
        let elem_count = array_buffer_view_byte_len(&data) / elem_size;
        let elem_offset = elem_offset as usize;
        let byte_offset = elem_offset * elem_size;

        if byte_offset > array_buffer_view_byte_len(&data) {
            return self.base.webgl_error(InvalidValue);
        }

        let copy_count = if length == 0 {
            elem_count - elem_offset
        } else {
            length as usize
        };
        if copy_count == 0 {
            return;
        }
        let copy_bytes = copy_count * elem_size;

        if byte_offset + copy_bytes > array_buffer_view_byte_len(&data) {
            return self.base.webgl_error(InvalidValue);
        }

        let data_end = byte_offset + copy_bytes;
        let data = array_buffer_view_bytes(&data, cx.no_gc());
        let data = &data[byte_offset..data_end];
        handle_potential_webgl_error!(self.base, bound_buffer.buffer_data(target, data, usage));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.5>
    fn BufferSubData(
        &self,
        cx: &mut JSContext,
        target: u32,
        offset: i64,
        data: ArrayBufferViewOrArrayBuffer,
    ) {
        let bound_buffer = handle_potential_webgl_error!(
            self.base,
            self.bound_buffer_for_data(cx, target),
            return
        );
        self.base
            .buffer_sub_data(cx.no_gc(), target, offset, data, bound_buffer)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.3>
    fn BufferSubData_(
        &self,
        cx: &mut JSContext,
        target: u32,
        dst_byte_offset: i64,
        src_data: CustomAutoRooterGuard<ArrayBufferView>,
        src_elem_offset: u32,
        length: u32,
    ) {
        let bound_buffer = handle_potential_webgl_error!(
            self.base,
            self.bound_buffer_for_data(cx, target),
            return
        );
        let bound_buffer =
            handle_potential_webgl_error!(self.base, bound_buffer.ok_or(InvalidOperation), return);

        let src_elem_size = src_data.get_array_type().byte_size().unwrap();
        let src_elem_count = array_buffer_view_byte_len(&src_data) / src_elem_size;
        let src_elem_offset = src_elem_offset as usize;
        let src_byte_offset = src_elem_offset * src_elem_size;

        if dst_byte_offset < 0 || src_byte_offset > array_buffer_view_byte_len(&src_data) {
            return self.base.webgl_error(InvalidValue);
        }

        let copy_count = if length == 0 {
            src_elem_count - src_elem_offset
        } else {
            length as usize
        };
        if copy_count == 0 {
            return;
        }
        let copy_bytes = copy_count * src_elem_size;

        let dst_byte_offset = dst_byte_offset as usize;
        if dst_byte_offset + copy_bytes > bound_buffer.capacity() ||
            src_byte_offset + copy_bytes > array_buffer_view_byte_len(&src_data)
        {
            return self.base.webgl_error(InvalidValue);
        }

        let (sender, receiver) = generic_channel::channel().unwrap();
        self.base.send_command(WebGLCommand::BufferSubData(
            target,
            dst_byte_offset as isize,
            receiver,
        ));
        let src_end = src_byte_offset + copy_bytes;
        let data = array_buffer_view_bytes(&src_data, cx.no_gc());
        let data = &data[src_byte_offset..src_end];
        let buffer = GenericSharedMemory::from_bytes(data);
        sender.send(buffer).unwrap();
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.3>
    fn CopyBufferSubData(
        &self,
        cx: &mut JSContext,
        read_target: u32,
        write_target: u32,
        read_offset: i64,
        write_offset: i64,
        size: i64,
    ) {
        if read_offset < 0 || write_offset < 0 || size < 0 {
            return self.base.webgl_error(InvalidValue);
        }

        let read_buffer = handle_potential_webgl_error!(
            self.base,
            self.bound_buffer_for_data(cx, read_target),
            return
        );
        let read_buffer =
            handle_potential_webgl_error!(self.base, read_buffer.ok_or(InvalidOperation), return);

        let write_buffer = handle_potential_webgl_error!(
            self.base,
            self.bound_buffer_for_data(cx, write_target),
            return
        );
        let write_buffer =
            handle_potential_webgl_error!(self.base, write_buffer.ok_or(InvalidOperation), return);

        let read_until = read_offset + size;
        let write_until = write_offset + size;
        if read_until as usize > read_buffer.capacity() ||
            write_until as usize > write_buffer.capacity()
        {
            return self.base.webgl_error(InvalidValue);
        }

        if read_target == write_target {
            let is_separate = read_until <= write_offset || write_until <= read_offset;
            if !is_separate {
                return self.base.webgl_error(InvalidValue);
            }
        }
        let src_is_elemarray = read_buffer
            .target()
            .is_some_and(|t| t == constants::ELEMENT_ARRAY_BUFFER);
        let dst_is_elemarray = write_buffer
            .target()
            .is_some_and(|t| t == constants::ELEMENT_ARRAY_BUFFER);
        if src_is_elemarray != dst_is_elemarray {
            return self.base.webgl_error(InvalidOperation);
        }

        self.base.send_command(WebGLCommand::CopyBufferSubData(
            read_target,
            write_target,
            read_offset,
            write_offset,
            size,
        ));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.3>
    fn GetBufferSubData(
        &self,
        cx: &mut JSContext,
        target: u32,
        src_byte_offset: i64,
        mut dst_buffer: CustomAutoRooterGuard<ArrayBufferView>,
        dst_elem_offset: u32,
        length: u32,
    ) {
        let bound_buffer = handle_potential_webgl_error!(
            self.base,
            self.bound_buffer_for_data(cx, target),
            return
        );
        let bound_buffer =
            handle_potential_webgl_error!(self.base, bound_buffer.ok_or(InvalidOperation), return);

        let dst_elem_size = dst_buffer.get_array_type().byte_size().unwrap();
        let dst_elem_count = array_buffer_view_byte_len(&dst_buffer) / dst_elem_size;
        let dst_elem_offset = dst_elem_offset as usize;
        let dst_byte_offset = dst_elem_offset * dst_elem_size;

        if src_byte_offset < 0 || dst_byte_offset > array_buffer_view_byte_len(&dst_buffer) {
            return self.base.webgl_error(InvalidValue);
        }

        let copy_count = if length == 0 {
            dst_elem_count - dst_elem_offset
        } else {
            length as usize
        };
        if copy_count == 0 {
            return;
        }
        let copy_bytes = copy_count * dst_elem_size;

        // WebGL 2.0 3.7.3: reading the TRANSFORM_FEEDBACK_BUFFER target while transform feedback
        // is active is an error, whatever buffer is bound there.
        if target == constants::TRANSFORM_FEEDBACK_BUFFER &&
            self.with_current_transform_feedback(|tf| tf.is_active())
        {
            return self.base.webgl_error(InvalidOperation);
        }

        let src_byte_offset = src_byte_offset as usize;
        if src_byte_offset + copy_bytes > bound_buffer.capacity() ||
            dst_byte_offset + copy_bytes > array_buffer_view_byte_len(&dst_buffer)
        {
            return self.base.webgl_error(InvalidValue);
        }

        let (sender, receiver) = generic_channel::channel().unwrap();
        self.base.send_command(WebGLCommand::GetBufferSubData(
            target,
            src_byte_offset,
            copy_bytes,
            sender,
        ));
        let data = receiver.recv().unwrap();
        let dst_end = dst_byte_offset + copy_bytes;
        let dst_buffer = array_buffer_view_bytes_mut(&mut dst_buffer, cx.no_gc());
        dst_buffer[dst_byte_offset..dst_end].copy_from_slice(&data);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.6>
    fn CompressedTexImage2D(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        internal_format: u32,
        width: i32,
        height: i32,
        border: i32,
        pixels: CustomAutoRooterGuard<ArrayBufferView>,
        src_offset: u32,
        src_length_override: u32,
    ) {
        if self.forbid_pixel_unpack_buffer() {
            return;
        }
        let Some(data) = self.compressed_src_bytes(no_gc, &pixels, src_offset, src_length_override)
        else {
            return;
        };
        self.base.compressed_tex_image_2d(
            target,
            level,
            internal_format,
            width,
            height,
            border,
            CompressedSource::Bytes(data),
        )
    }

    /// <https://registry.khronos.org/webgl/specs/latest/2.0/#3.7.6>
    ///
    /// compressedTexImage2D from the bound PIXEL_UNPACK_BUFFER.
    fn CompressedTexImage2D_(
        &self,
        _no_gc: &NoGC,
        target: u32,
        level: i32,
        internal_format: u32,
        width: i32,
        height: i32,
        border: i32,
        image_size: i32,
        offset: i64,
    ) {
        let Some(source) = self.compressed_pbo_source(image_size, offset) else {
            return;
        };
        self.base.compressed_tex_image_2d(
            target,
            level,
            internal_format,
            width,
            height,
            border,
            source,
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
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
        pixels: CustomAutoRooterGuard<ArrayBufferView>,
        src_offset: u32,
        src_length_override: u32,
    ) {
        if self.forbid_pixel_unpack_buffer() {
            return;
        }
        let Some(data) = self.compressed_src_bytes(no_gc, &pixels, src_offset, src_length_override)
        else {
            return;
        };
        self.base.compressed_tex_sub_image_2d(
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

    /// <https://registry.khronos.org/webgl/specs/latest/2.0/#3.7.6>
    ///
    /// compressedTexSubImage2D from the bound PIXEL_UNPACK_BUFFER.
    fn CompressedTexSubImage2D_(
        &self,
        _no_gc: &NoGC,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        image_size: i32,
        offset: i64,
    ) {
        let Some(source) = self.compressed_pbo_source(image_size, offset) else {
            return;
        };
        self.base.compressed_tex_sub_image_2d(
            target, level, xoffset, yoffset, width, height, format, source,
        )
    }

    /// <https://registry.khronos.org/webgl/specs/latest/2.0/#3.7.6>
    ///
    /// compressedTexImage3D from the bound PIXEL_UNPACK_BUFFER.
    fn CompressedTexImage3D(
        &self,
        _no_gc: &NoGC,
        target: u32,
        level: i32,
        internal_format: u32,
        width: i32,
        height: i32,
        depth: i32,
        border: i32,
        image_size: i32,
        offset: i64,
    ) {
        let Some(source) = self.compressed_pbo_source(image_size, offset) else {
            return;
        };
        self.compressed_tex_image_3d(
            target,
            level,
            internal_format,
            width,
            height,
            depth,
            border,
            source,
        )
    }

    /// <https://registry.khronos.org/webgl/specs/latest/2.0/#3.7.6>
    fn CompressedTexImage3D_(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        internal_format: u32,
        width: i32,
        height: i32,
        depth: i32,
        border: i32,
        src_data: CustomAutoRooterGuard<ArrayBufferView>,
        src_offset: u32,
        src_length_override: u32,
    ) {
        if self.forbid_pixel_unpack_buffer() {
            return;
        }
        let Some(data) =
            self.compressed_src_bytes(no_gc, &src_data, src_offset, src_length_override)
        else {
            return;
        };
        self.compressed_tex_image_3d(
            target,
            level,
            internal_format,
            width,
            height,
            depth,
            border,
            CompressedSource::Bytes(data),
        )
    }

    /// <https://registry.khronos.org/webgl/specs/latest/2.0/#3.7.6>
    ///
    /// compressedTexSubImage3D from the bound PIXEL_UNPACK_BUFFER.
    fn CompressedTexSubImage3D(
        &self,
        _no_gc: &NoGC,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        width: i32,
        height: i32,
        depth: i32,
        format: u32,
        image_size: i32,
        offset: i64,
    ) {
        let Some(source) = self.compressed_pbo_source(image_size, offset) else {
            return;
        };
        self.compressed_tex_sub_image_3d(
            target, level, xoffset, yoffset, zoffset, width, height, depth, format, source,
        )
    }

    /// <https://registry.khronos.org/webgl/specs/latest/2.0/#3.7.6>
    fn CompressedTexSubImage3D_(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        width: i32,
        height: i32,
        depth: i32,
        format: u32,
        src_data: CustomAutoRooterGuard<ArrayBufferView>,
        src_offset: u32,
        src_length_override: u32,
    ) {
        if self.forbid_pixel_unpack_buffer() {
            return;
        }
        let Some(data) =
            self.compressed_src_bytes(no_gc, &src_data, src_offset, src_length_override)
        else {
            return;
        };
        self.compressed_tex_sub_image_3d(
            target,
            level,
            xoffset,
            yoffset,
            zoffset,
            width,
            height,
            depth,
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
        self.base
            .CopyTexImage2D(target, level, internal_format, x, y, width, height, border)
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
        self.base
            .CopyTexSubImage2D(target, level, xoffset, yoffset, x, y, width, height)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.11>
    fn Clear(&self, mask: u32) {
        // WebGL 2.0 5.13: clear() on an integer colour buffer is INVALID_OPERATION.
        if mask & constants::COLOR_BUFFER_BIT != 0 &&
            (0..self.base.limits().max_draw_buffers as usize)
                .any(|i| matches!(self.draw_buffer_class(i), Some(2 | 3)))
        {
            return self.base.webgl_error(InvalidOperation);
        }
        self.base.Clear(mask)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn ClearColor(&self, red: f32, green: f32, blue: f32, alpha: f32) {
        self.base.ClearColor(red, green, blue, alpha)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn ClearDepth(&self, depth: f32) {
        self.base.ClearDepth(depth)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn ClearStencil(&self, stencil: i32) {
        self.base.ClearStencil(stencil)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn ColorMask(&self, r: bool, g: bool, b: bool, a: bool) {
        self.color_write_mask.set([r, g, b, a]);
        self.base.ColorMask(r, g, b, a)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn CullFace(&self, mode: u32) {
        self.base.CullFace(mode)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn FrontFace(&self, mode: u32) {
        self.base.FrontFace(mode)
    }
    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn DepthFunc(&self, func: u32) {
        self.base.DepthFunc(func)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn DepthMask(&self, flag: bool) {
        self.base.DepthMask(flag)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn DepthRange(&self, near: f32, far: f32) {
        self.base.DepthRange(near, far)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn Enable(&self, cap: u32) {
        match cap {
            constants::RASTERIZER_DISCARD => {
                self.enable_rasterizer_discard.set(true);
                self.base.send_command(WebGLCommand::Enable(cap));
            },
            _ => self.base.Enable(cap),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn Disable(&self, cap: u32) {
        match cap {
            constants::RASTERIZER_DISCARD => {
                self.enable_rasterizer_discard.set(false);
                self.base.send_command(WebGLCommand::Disable(cap));
            },
            _ => self.base.Disable(cap),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn CompileShader(&self, shader: &WebGLShader) {
        self.base.CompileShader(shader)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.5>
    fn CreateBuffer(&self, cx: &mut JSContext) -> Option<DomRoot<WebGLBuffer>> {
        self.base.CreateBuffer(cx)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.6>
    fn CreateFramebuffer(&self, cx: &mut JSContext) -> Option<DomRoot<WebGLFramebuffer>> {
        self.base.CreateFramebuffer(cx)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.7>
    fn CreateRenderbuffer(&self, cx: &mut JSContext) -> Option<DomRoot<WebGLRenderbuffer>> {
        self.base.CreateRenderbuffer(cx)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn CreateTexture(&self, cx: &mut JSContext) -> Option<DomRoot<WebGLTexture>> {
        self.base.CreateTexture(cx)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn CreateProgram(&self, cx: &mut JSContext) -> Option<DomRoot<WebGLProgram>> {
        self.base.CreateProgram(cx)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn CreateShader(&self, cx: &mut JSContext, shader_type: u32) -> Option<DomRoot<WebGLShader>> {
        self.base.CreateShader(cx, shader_type)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.17>
    fn CreateVertexArray(&self, cx: &mut JSContext) -> Option<DomRoot<WebGLVertexArrayObject>> {
        self.base.create_vertex_array_webgl2(cx)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.5>
    fn DeleteBuffer(&self, cx: &mut JSContext, buffer: Option<&WebGLBuffer>) {
        let buffer = match buffer {
            Some(buffer) => buffer,
            None => return,
        };
        handle_potential_webgl_error!(self.base, self.base.validate_ownership(buffer), return);
        if buffer.is_marked_for_deletion() {
            return;
        }
        self.current_vao(cx).unbind_buffer(buffer);
        self.unbind_from(self.base.array_buffer_slot(), buffer);
        self.unbind_from(&self.bound_copy_read_buffer, buffer);
        self.unbind_from(&self.bound_copy_write_buffer, buffer);
        self.unbind_from(&self.bound_pixel_pack_buffer, buffer);
        self.unbind_from(&self.bound_pixel_unpack_buffer, buffer);
        self.unbind_from(&self.bound_transform_feedback_buffer, buffer);
        self.unbind_from(&self.bound_uniform_buffer, buffer);

        for binding in self.indexed_uniform_buffer_bindings.iter() {
            self.unbind_from(&binding.buffer, buffer);
        }
        for binding in self.indexed_transform_feedback_buffer_bindings.iter() {
            self.unbind_from(&binding.buffer, buffer);
        }

        buffer.mark_for_deletion(Operation::Infallible);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.6>
    fn DeleteFramebuffer(&self, framebuffer: Option<&WebGLFramebuffer>) {
        self.base.DeleteFramebuffer(framebuffer)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.7>
    fn DeleteRenderbuffer(&self, renderbuffer: Option<&WebGLRenderbuffer>) {
        self.base.DeleteRenderbuffer(renderbuffer)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn DeleteTexture(&self, texture: Option<&WebGLTexture>) {
        self.base.DeleteTexture(texture)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn DeleteProgram(&self, program: Option<&WebGLProgram>) {
        self.base.DeleteProgram(program)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn DeleteShader(&self, shader: Option<&WebGLShader>) {
        self.base.DeleteShader(shader)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.17>
    fn DeleteVertexArray(&self, cx: &mut JSContext, vertex_array: Option<&WebGLVertexArrayObject>) {
        self.base.delete_vertex_array_webgl2(cx, vertex_array);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.11>
    fn DrawArrays(&self, cx: &mut JSContext, mode: u32, first: i32, count: i32) {
        handle_potential_webgl_error!(
            self.base,
            self.draw_arrays_checked(cx, mode, first, count, 1)
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.11>
    fn DrawElements(&self, cx: &mut JSContext, mode: u32, count: i32, type_: u32, offset: i64) {
        handle_potential_webgl_error!(
            self.base,
            self.draw_elements_checked(cx, mode, count, type_, offset, 1)
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn EnableVertexAttribArray(&self, cx: &mut JSContext, attrib_id: u32) {
        self.base.EnableVertexAttribArray(cx, attrib_id)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn DisableVertexAttribArray(&self, cx: &mut JSContext, attrib_id: u32) {
        self.base.DisableVertexAttribArray(cx, attrib_id)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn GetActiveUniform(
        &self,
        cx: &mut JSContext,
        program: &WebGLProgram,
        index: u32,
    ) -> Option<DomRoot<WebGLActiveInfo>> {
        self.base.GetActiveUniform(cx, program, index)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn GetActiveAttrib(
        &self,
        cx: &mut JSContext,
        program: &WebGLProgram,
        index: u32,
    ) -> Option<DomRoot<WebGLActiveInfo>> {
        self.base.GetActiveAttrib(cx, program, index)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn GetAttribLocation(&self, program: &WebGLProgram, name: DOMString) -> i32 {
        self.base.GetAttribLocation(program, name)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.7>
    fn GetFragDataLocation(&self, program: &WebGLProgram, name: DOMString) -> i32 {
        handle_potential_webgl_error!(self.base, self.base.validate_ownership(program), return -1);
        handle_potential_webgl_error!(self.base, program.get_frag_data_location(name), -1)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetProgramInfoLog(&self, program: &WebGLProgram) -> Option<DOMString> {
        self.base.GetProgramInfoLog(program)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetProgramParameter(
        &self,
        cx: &mut JSContext,
        program: &WebGLProgram,
        param_id: u32,
        mut retval: MutableHandleValue,
    ) {
        handle_potential_webgl_error!(
            self.base,
            self.base.validate_ownership(program),
            return retval.set(NullValue())
        );
        if program.is_deleted() {
            self.base.webgl_error(InvalidOperation);
            return retval.set(NullValue());
        }
        match param_id {
            constants::TRANSFORM_FEEDBACK_VARYINGS => {
                retval.set(Int32Value(program.transform_feedback_varyings_length()))
            },
            constants::TRANSFORM_FEEDBACK_BUFFER_MODE => {
                retval.set(Int32Value(program.transform_feedback_buffer_mode()))
            },
            constants::ACTIVE_UNIFORM_BLOCKS => {
                retval.set(Int32Value(program.active_uniform_blocks().len() as i32))
            },
            _ => self.base.GetProgramParameter(cx, program, param_id, retval),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetShaderInfoLog(&self, shader: &WebGLShader) -> Option<DOMString> {
        self.base.GetShaderInfoLog(shader)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetShaderParameter(
        &self,
        cx: &mut JSContext,
        shader: &WebGLShader,
        param_id: u32,
        retval: MutableHandleValue,
    ) {
        self.base.GetShaderParameter(cx, shader, param_id, retval)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetShaderPrecisionFormat(
        &self,
        cx: &mut JSContext,
        shader_type: u32,
        precision_type: u32,
    ) -> Option<DomRoot<WebGLShaderPrecisionFormat>> {
        self.base
            .GetShaderPrecisionFormat(cx, shader_type, precision_type)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.2>
    fn GetIndexedParameter(
        &self,
        cx: &mut JSContext,
        target: u32,
        index: u32,
        mut retval: MutableHandleValue,
    ) {
        let bindings = match target {
            constants::TRANSFORM_FEEDBACK_BUFFER_BINDING |
            constants::TRANSFORM_FEEDBACK_BUFFER_SIZE |
            constants::TRANSFORM_FEEDBACK_BUFFER_START => {
                &self.indexed_transform_feedback_buffer_bindings
            },
            constants::UNIFORM_BUFFER_BINDING |
            constants::UNIFORM_BUFFER_SIZE |
            constants::UNIFORM_BUFFER_START => &self.indexed_uniform_buffer_bindings,
            _ => {
                self.base.webgl_error(InvalidEnum);
                return retval.set(NullValue());
            },
        };

        let binding = match bindings.get(index as usize) {
            Some(binding) => binding,
            None => {
                self.base.webgl_error(InvalidValue);
                return retval.set(NullValue());
            },
        };

        match target {
            constants::TRANSFORM_FEEDBACK_BUFFER_BINDING | constants::UNIFORM_BUFFER_BINDING => {
                binding.buffer.get().to_jsval(cx, retval)
            },
            constants::TRANSFORM_FEEDBACK_BUFFER_START | constants::UNIFORM_BUFFER_START => {
                retval.set(Int32Value(binding.start.get() as _))
            },
            constants::TRANSFORM_FEEDBACK_BUFFER_SIZE | constants::UNIFORM_BUFFER_SIZE => {
                retval.set(Int32Value(binding.size.get() as _))
            },
            _ => unreachable!(),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn GetUniformLocation(
        &self,
        cx: &mut JSContext,
        program: &WebGLProgram,
        name: DOMString,
    ) -> Option<DomRoot<WebGLUniformLocation>> {
        self.base.GetUniformLocation(cx, program, name)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetVertexAttrib(
        &self,
        cx: &mut JSContext,
        index: u32,
        pname: u32,
        retval: MutableHandleValue,
    ) {
        self.base.GetVertexAttrib(cx, index, pname, retval)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn GetVertexAttribOffset(&self, cx: &mut JSContext, index: u32, pname: u32) -> i64 {
        self.base.GetVertexAttribOffset(cx, index, pname)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn Hint(&self, target: u32, mode: u32) {
        self.base.Hint(target, mode)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.5>
    fn IsBuffer(&self, buffer: Option<&WebGLBuffer>) -> bool {
        self.base.IsBuffer(buffer)
    }

    // TODO: We could write this without IPC, recording the calls to `enable` and `disable`.
    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.2>
    fn IsEnabled(&self, cap: u32) -> bool {
        match cap {
            constants::RASTERIZER_DISCARD => self.enable_rasterizer_discard.get(),
            _ => self.base.IsEnabled(cap),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.6>
    fn IsFramebuffer(&self, frame_buffer: Option<&WebGLFramebuffer>) -> bool {
        self.base.IsFramebuffer(frame_buffer)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn IsProgram(&self, program: Option<&WebGLProgram>) -> bool {
        self.base.IsProgram(program)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.7>
    fn IsRenderbuffer(&self, render_buffer: Option<&WebGLRenderbuffer>) -> bool {
        self.base.IsRenderbuffer(render_buffer)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn IsShader(&self, shader: Option<&WebGLShader>) -> bool {
        self.base.IsShader(shader)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn IsTexture(&self, texture: Option<&WebGLTexture>) -> bool {
        self.base.IsTexture(texture)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.17>
    fn IsVertexArray(&self, vertex_array: Option<&WebGLVertexArrayObject>) -> bool {
        self.base.is_vertex_array_webgl2(vertex_array)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn LineWidth(&self, width: f32) {
        self.base.LineWidth(width)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.2>
    fn PixelStorei(&self, param_name: u32, param_value: i32) {
        if param_value < 0 {
            return self.base.webgl_error(InvalidValue);
        }

        match param_name {
            constants::PACK_ROW_LENGTH => self.texture_pack_row_length.set(param_value as _),
            constants::PACK_SKIP_PIXELS => self.texture_pack_skip_pixels.set(param_value as _),
            constants::PACK_SKIP_ROWS => self.texture_pack_skip_rows.set(param_value as _),
            constants::UNPACK_ROW_LENGTH |
            constants::UNPACK_IMAGE_HEIGHT |
            constants::UNPACK_SKIP_PIXELS |
            constants::UNPACK_SKIP_ROWS |
            constants::UNPACK_SKIP_IMAGES => {
                let mut params = self.unpack_params.get();
                let value = param_value as u32;
                match param_name {
                    constants::UNPACK_ROW_LENGTH => params.row_length = value,
                    constants::UNPACK_IMAGE_HEIGHT => params.image_height = value,
                    constants::UNPACK_SKIP_PIXELS => params.skip_pixels = value,
                    constants::UNPACK_SKIP_ROWS => params.skip_rows = value,
                    _ => params.skip_images = value,
                }
                self.unpack_params.set(params);
            },
            _ => self.base.PixelStorei(param_name, param_value),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn PolygonOffset(&self, factor: f32, units: f32) {
        self.base.PolygonOffset(factor, units)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.12>
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
        let pixels =
            handle_potential_webgl_error!(self.base, pixels.as_mut().ok_or(InvalidValue), return);

        self.read_pixels_into(no_gc, x, y, width, height, format, pixel_type, pixels, 0)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.10>
    fn ReadPixels_(
        &self,
        _no_gc: &NoGC,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        format: u32,
        pixel_type: u32,
        dst_byte_offset: i64,
    ) {
        handle_potential_webgl_error!(self.base, self.base.validate_framebuffer(), return);

        let dst = match self.bound_pixel_pack_buffer.get() {
            Some(buffer) => buffer,
            None => return self.base.webgl_error(InvalidOperation),
        };
        if self.is_double_bound_with_transform_feedback(&dst) {
            return self.base.webgl_error(InvalidOperation);
        }

        if dst_byte_offset < 0 {
            return self.base.webgl_error(InvalidValue);
        }
        let dst_byte_offset = dst_byte_offset as usize;
        if dst_byte_offset > dst.capacity() {
            return self.base.webgl_error(InvalidOperation);
        }

        let ReadPixelsAllowedFormats {
            array_types,
            channels,
        } = match self.calc_read_pixel_formats(pixel_type, format) {
            Ok(result) => result,
            Err(error) => return self.base.webgl_error(error),
        };
        let bytes_per_pixel = channels * array_types[0].byte_size().unwrap_or(1);
        // RGBA plus the type of the read buffer's class, or the implementation's pair
        // (WebGL 2.0, 3.7.10 readPixels; GL ES 3.0, 4.3.1).
        let (primary, implementation) = self.read_pixels_formats();
        if (format, pixel_type) != primary && (format, pixel_type) != implementation {
            return self.base.webgl_error(InvalidOperation);
        }

        let ReadPixelsSizes {
            row_stride: _,
            skipped_bytes,
            size,
        } = match self.calc_read_pixel_sizes(width, height, bytes_per_pixel) {
            Ok(result) => result,
            Err(error) => return self.base.webgl_error(error),
        };
        let dst_end = dst_byte_offset + skipped_bytes + size;
        if dst.capacity() < dst_end {
            return self.base.webgl_error(InvalidOperation);
        }

        {
            let (fb_width, fb_height) = handle_potential_webgl_error!(
                self.base,
                self.base
                    .get_current_framebuffer_size()
                    .ok_or(InvalidOperation),
                return
            );
            let src_origin = Point2D::new(x, y);
            let src_size = Size2D::new(width as u32, height as u32);
            let fb_size = Size2D::new(fb_width as u32, fb_height as u32);
            if pixels::clip(src_origin, src_size.to_u32(), fb_size.to_u32()).is_none() {
                return;
            }
        }
        let src_rect = Rect::new(Point2D::new(x, y), Size2D::new(width, height));

        self.base.send_command(WebGLCommand::PixelStorei(
            constants::PACK_ALIGNMENT,
            self.base.get_texture_packing_alignment() as _,
        ));
        self.base.send_command(WebGLCommand::PixelStorei(
            constants::PACK_ROW_LENGTH,
            self.texture_pack_row_length.get() as _,
        ));
        self.base.send_command(WebGLCommand::PixelStorei(
            constants::PACK_SKIP_ROWS,
            self.texture_pack_skip_rows.get() as _,
        ));
        self.base.send_command(WebGLCommand::PixelStorei(
            constants::PACK_SKIP_PIXELS,
            self.texture_pack_skip_pixels.get() as _,
        ));
        self.base.send_command(WebGLCommand::ReadPixelsPP(
            src_rect,
            format,
            pixel_type,
            dst_byte_offset,
        ));
        // Client-memory readPixels on the WebGL thread assumes the GL defaults.
        for name in [
            constants::PACK_ROW_LENGTH,
            constants::PACK_SKIP_ROWS,
            constants::PACK_SKIP_PIXELS,
        ] {
            self.base.send_command(WebGLCommand::PixelStorei(name, 0));
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.10>
    fn ReadPixels__(
        &self,
        no_gc: &NoGC,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        format: u32,
        pixel_type: u32,
        mut dst: CustomAutoRooterGuard<ArrayBufferView>,
        dst_elem_offset: u32,
    ) {
        self.read_pixels_into(
            no_gc,
            x,
            y,
            width,
            height,
            format,
            pixel_type,
            &mut dst,
            dst_elem_offset,
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn SampleCoverage(&self, value: f32, invert: bool) {
        self.base.SampleCoverage(value, invert)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.4>
    fn Scissor(&self, x: i32, y: i32, width: i32, height: i32) {
        self.base.Scissor(x, y, width, height)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn StencilFunc(&self, func: u32, ref_: i32, mask: u32) {
        self.base.StencilFunc(func, ref_, mask)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn StencilFuncSeparate(&self, face: u32, func: u32, ref_: i32, mask: u32) {
        self.base.StencilFuncSeparate(face, func, ref_, mask)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn StencilMask(&self, mask: u32) {
        self.base.StencilMask(mask)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn StencilMaskSeparate(&self, face: u32, mask: u32) {
        self.base.StencilMaskSeparate(face, mask)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn StencilOp(&self, fail: u32, zfail: u32, zpass: u32) {
        self.base.StencilOp(fail, zfail, zpass)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.3>
    fn StencilOpSeparate(&self, face: u32, fail: u32, zfail: u32, zpass: u32) {
        self.base.StencilOpSeparate(face, fail, zfail, zpass)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn LinkProgram(&self, program: &WebGLProgram) {
        // ES 3.0 2.15.2: relinking the program transform feedback is recording with.
        if self.with_current_transform_feedback(|tf| tf.is_active()) &&
            self.base
                .current_program()
                .is_some_and(|current| &*current == program)
        {
            self.base.webgl_error(InvalidOperation);
            return;
        }
        self.base.LinkProgram(program)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn ShaderSource(&self, shader: &WebGLShader, source: DOMString) {
        self.base.ShaderSource(shader, source)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetShaderSource(&self, shader: &WebGLShader) -> Option<DOMString> {
        self.base.GetShaderSource(shader)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform1f(&self, location: Option<&WebGLUniformLocation>, val: f32) {
        self.base.Uniform1f(location, val)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform1i(&self, location: Option<&WebGLUniformLocation>, val: i32) {
        self.base.Uniform1i(location, val)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform1iv(
        &self,
        location: Option<&WebGLUniformLocation>,
        v: Int32ArrayOrLongSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.uniform1iv(location, v, src_offset, src_length)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn Uniform1ui(&self, location: Option<&WebGLUniformLocation>, val: u32) {
        self.base.with_location(location, |location| {
            match location.type_() {
                constants::BOOL | constants::UNSIGNED_INT => (),
                _ => return Err(InvalidOperation),
            }
            self.base
                .send_command(WebGLCommand::Uniform1ui(location.id(), val));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn Uniform1uiv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Uint32ArrayOrUnsignedLongSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.with_location(location, |location| {
            match location.type_() {
                constants::BOOL |
                constants::UNSIGNED_INT |
                constants::SAMPLER_2D |
                constants::SAMPLER_2D_ARRAY |
                constants::SAMPLER_3D |
                constants::SAMPLER_CUBE => {},
                _ => return Err(InvalidOperation),
            }

            let val = self.uniform_vec_section_uint(val, src_offset, src_length, 1, location)?;

            match location.type_() {
                constants::SAMPLER_2D | constants::SAMPLER_CUBE => {
                    for &v in val
                        .iter()
                        .take(cmp::min(location.size().unwrap_or(1) as usize, val.len()))
                    {
                        if v >= self.base.limits().max_combined_texture_image_units {
                            return Err(InvalidValue);
                        }
                    }
                },
                _ => {},
            }
            self.base
                .send_command(WebGLCommand::Uniform1uiv(location.id(), val));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform1fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        v: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.uniform1fv(location, v, src_offset, src_length);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform2f(&self, location: Option<&WebGLUniformLocation>, x: f32, y: f32) {
        self.base.Uniform2f(location, x, y)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform2fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        v: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.uniform2fv(location, v, src_offset, src_length);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform2i(&self, location: Option<&WebGLUniformLocation>, x: i32, y: i32) {
        self.base.Uniform2i(location, x, y)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform2iv(
        &self,
        location: Option<&WebGLUniformLocation>,
        v: Int32ArrayOrLongSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.uniform2iv(location, v, src_offset, src_length)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn Uniform2ui(&self, location: Option<&WebGLUniformLocation>, x: u32, y: u32) {
        self.base.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC2 | constants::UNSIGNED_INT_VEC2 => {},
                _ => return Err(InvalidOperation),
            }
            self.base
                .send_command(WebGLCommand::Uniform2ui(location.id(), x, y));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn Uniform2uiv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Uint32ArrayOrUnsignedLongSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC2 | constants::UNSIGNED_INT_VEC2 => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.uniform_vec_section_uint(val, src_offset, src_length, 2, location)?;
            self.base
                .send_command(WebGLCommand::Uniform2uiv(location.id(), val));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform3f(&self, location: Option<&WebGLUniformLocation>, x: f32, y: f32, z: f32) {
        self.base.Uniform3f(location, x, y, z)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform3fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        v: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.uniform3fv(location, v, src_offset, src_length);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform3i(&self, location: Option<&WebGLUniformLocation>, x: i32, y: i32, z: i32) {
        self.base.Uniform3i(location, x, y, z)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform3iv(
        &self,
        location: Option<&WebGLUniformLocation>,
        v: Int32ArrayOrLongSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.uniform3iv(location, v, src_offset, src_length)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn Uniform3ui(&self, location: Option<&WebGLUniformLocation>, x: u32, y: u32, z: u32) {
        self.base.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC3 | constants::UNSIGNED_INT_VEC3 => {},
                _ => return Err(InvalidOperation),
            }
            self.base
                .send_command(WebGLCommand::Uniform3ui(location.id(), x, y, z));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn Uniform3uiv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Uint32ArrayOrUnsignedLongSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC3 | constants::UNSIGNED_INT_VEC3 => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.uniform_vec_section_uint(val, src_offset, src_length, 3, location)?;
            self.base
                .send_command(WebGLCommand::Uniform3uiv(location.id(), val));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform4i(&self, location: Option<&WebGLUniformLocation>, x: i32, y: i32, z: i32, w: i32) {
        self.base.Uniform4i(location, x, y, z, w)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform4iv(
        &self,
        location: Option<&WebGLUniformLocation>,
        v: Int32ArrayOrLongSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.uniform4iv(location, v, src_offset, src_length)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn Uniform4ui(&self, location: Option<&WebGLUniformLocation>, x: u32, y: u32, z: u32, w: u32) {
        self.base.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC4 | constants::UNSIGNED_INT_VEC4 => {},
                _ => return Err(InvalidOperation),
            }
            self.base
                .send_command(WebGLCommand::Uniform4ui(location.id(), x, y, z, w));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn Uniform4uiv(
        &self,
        location: Option<&WebGLUniformLocation>,
        val: Uint32ArrayOrUnsignedLongSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.with_location(location, |location| {
            match location.type_() {
                constants::BOOL_VEC4 | constants::UNSIGNED_INT_VEC4 => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.uniform_vec_section_uint(val, src_offset, src_length, 4, location)?;
            self.base
                .send_command(WebGLCommand::Uniform4uiv(location.id(), val));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform4f(&self, location: Option<&WebGLUniformLocation>, x: f32, y: f32, z: f32, w: f32) {
        self.base.Uniform4f(location, x, y, z, w)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn Uniform4fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        v: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.uniform4fv(location, v, src_offset, src_length);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn UniformMatrix2fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        transpose: bool,
        v: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base
            .uniform_matrix_2fv(location, transpose, v, src_offset, src_length)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn UniformMatrix3fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        transpose: bool,
        v: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base
            .uniform_matrix_3fv(location, transpose, v, src_offset, src_length)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn UniformMatrix4fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        transpose: bool,
        v: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base
            .uniform_matrix_4fv(location, transpose, v, src_offset, src_length)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn UniformMatrix3x2fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        transpose: bool,
        val: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.with_location(location, |location| {
            match location.type_() {
                constants::FLOAT_MAT3x2 => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.base.uniform_matrix_section(
                val,
                src_offset,
                src_length,
                transpose,
                3 * 2,
                location,
            )?;
            self.base
                .send_command(WebGLCommand::UniformMatrix3x2fv(location.id(), val));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn UniformMatrix4x2fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        transpose: bool,
        val: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.with_location(location, |location| {
            match location.type_() {
                constants::FLOAT_MAT4x2 => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.base.uniform_matrix_section(
                val,
                src_offset,
                src_length,
                transpose,
                4 * 2,
                location,
            )?;
            self.base
                .send_command(WebGLCommand::UniformMatrix4x2fv(location.id(), val));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn UniformMatrix2x3fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        transpose: bool,
        val: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.with_location(location, |location| {
            match location.type_() {
                constants::FLOAT_MAT2x3 => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.base.uniform_matrix_section(
                val,
                src_offset,
                src_length,
                transpose,
                2 * 3,
                location,
            )?;
            self.base
                .send_command(WebGLCommand::UniformMatrix2x3fv(location.id(), val));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn UniformMatrix4x3fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        transpose: bool,
        val: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.with_location(location, |location| {
            match location.type_() {
                constants::FLOAT_MAT4x3 => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.base.uniform_matrix_section(
                val,
                src_offset,
                src_length,
                transpose,
                4 * 3,
                location,
            )?;
            self.base
                .send_command(WebGLCommand::UniformMatrix4x3fv(location.id(), val));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn UniformMatrix2x4fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        transpose: bool,
        val: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.with_location(location, |location| {
            match location.type_() {
                constants::FLOAT_MAT2x4 => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.base.uniform_matrix_section(
                val,
                src_offset,
                src_length,
                transpose,
                2 * 4,
                location,
            )?;
            self.base
                .send_command(WebGLCommand::UniformMatrix2x4fv(location.id(), val));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn UniformMatrix3x4fv(
        &self,
        location: Option<&WebGLUniformLocation>,
        transpose: bool,
        val: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
        src_length: u32,
    ) {
        self.base.with_location(location, |location| {
            match location.type_() {
                constants::FLOAT_MAT3x4 => {},
                _ => return Err(InvalidOperation),
            }
            let val = self.base.uniform_matrix_section(
                val,
                src_offset,
                src_length,
                transpose,
                3 * 4,
                location,
            )?;
            self.base
                .send_command(WebGLCommand::UniformMatrix3x4fv(location.id(), val));
            Ok(())
        });
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn GetUniform(
        &self,
        cx: &mut JSContext,
        program: &WebGLProgram,
        location: &WebGLUniformLocation,
        mut retval: MutableHandleValue,
    ) {
        handle_potential_webgl_error!(
            self.base,
            self.base.uniform_check_program(program, location),
            return retval.set(NullValue())
        );

        let triple = (&*self.base, program.id(), location.id());

        match location.type_() {
            constants::UNSIGNED_INT => retval.set(UInt32Value(uniform_get(
                triple,
                WebGLCommand::GetUniformUint,
            ))),
            constants::UNSIGNED_INT_VEC2 => uniform_typed::<Uint32>(
                cx,
                &uniform_get(triple, WebGLCommand::GetUniformUint2),
                retval,
            ),
            constants::UNSIGNED_INT_VEC3 => uniform_typed::<Uint32>(
                cx,
                &uniform_get(triple, WebGLCommand::GetUniformUint3),
                retval,
            ),
            constants::UNSIGNED_INT_VEC4 => uniform_typed::<Uint32>(
                cx,
                &uniform_get(triple, WebGLCommand::GetUniformUint4),
                retval,
            ),
            constants::FLOAT_MAT2x3 => uniform_typed::<Float32>(
                cx,
                &uniform_get(triple, WebGLCommand::GetUniformFloat2x3),
                retval,
            ),
            constants::FLOAT_MAT2x4 => uniform_typed::<Float32>(
                cx,
                &uniform_get(triple, WebGLCommand::GetUniformFloat2x4),
                retval,
            ),
            constants::FLOAT_MAT3x2 => uniform_typed::<Float32>(
                cx,
                &uniform_get(triple, WebGLCommand::GetUniformFloat3x2),
                retval,
            ),
            constants::FLOAT_MAT3x4 => uniform_typed::<Float32>(
                cx,
                &uniform_get(triple, WebGLCommand::GetUniformFloat3x4),
                retval,
            ),
            constants::FLOAT_MAT4x2 => uniform_typed::<Float32>(
                cx,
                &uniform_get(triple, WebGLCommand::GetUniformFloat4x2),
                retval,
            ),
            constants::FLOAT_MAT4x3 => uniform_typed::<Float32>(
                cx,
                &uniform_get(triple, WebGLCommand::GetUniformFloat4x3),
                retval,
            ),
            sampler if is_sampler_type(sampler) => {
                retval.set(Int32Value(uniform_get(triple, WebGLCommand::GetUniformInt)))
            },
            _ => self.base.GetUniform(cx, program, location, retval),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn UseProgram(&self, program: Option<&WebGLProgram>) {
        // ES 3.0 2.15.2: no program changes while transform feedback is recording.
        if self.with_current_transform_feedback(|tf| tf.is_active_unpaused()) {
            self.base.webgl_error(InvalidOperation);
            return;
        }
        self.base.UseProgram(program)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn ValidateProgram(&self, program: &WebGLProgram) {
        self.base.ValidateProgram(program)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib1f(&self, cx: &mut JSContext, indx: u32, x: f32) {
        self.base.VertexAttrib1f(cx, indx, x)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib1fv(
        &self,
        cx: &mut JSContext,
        indx: u32,
        v: Float32ArrayOrUnrestrictedFloatSequence,
    ) {
        self.base.VertexAttrib1fv(cx, indx, v)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib2f(&self, cx: &mut JSContext, indx: u32, x: f32, y: f32) {
        self.base.VertexAttrib2f(cx, indx, x, y)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib2fv(
        &self,
        cx: &mut JSContext,
        indx: u32,
        v: Float32ArrayOrUnrestrictedFloatSequence,
    ) {
        self.base.VertexAttrib2fv(cx, indx, v)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib3f(&self, cx: &mut JSContext, indx: u32, x: f32, y: f32, z: f32) {
        self.base.VertexAttrib3f(cx, indx, x, y, z)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib3fv(
        &self,
        cx: &mut JSContext,
        indx: u32,
        v: Float32ArrayOrUnrestrictedFloatSequence,
    ) {
        self.base.VertexAttrib3fv(cx, indx, v)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib4f(&self, cx: &mut JSContext, indx: u32, x: f32, y: f32, z: f32, w: f32) {
        self.base.VertexAttrib4f(cx, indx, x, y, z, w)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttrib4fv(
        &self,
        cx: &mut JSContext,
        indx: u32,
        v: Float32ArrayOrUnrestrictedFloatSequence,
    ) {
        self.base.VertexAttrib4fv(cx, indx, v)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn VertexAttribI4i(&self, cx: &mut JSContext, index: u32, x: i32, y: i32, z: i32, w: i32) {
        self.vertex_attrib_i(cx, index, x, y, z, w)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn VertexAttribI4iv(&self, cx: &mut JSContext, index: u32, v: Int32ArrayOrLongSequence) {
        let values = match v {
            Int32ArrayOrLongSequence::Int32Array(v) => typed_array_to_vec(&*v),
            Int32ArrayOrLongSequence::LongSequence(v) => v,
        };
        if values.len() < 4 {
            return self.base.webgl_error(InvalidValue);
        }
        self.vertex_attrib_i(cx, index, values[0], values[1], values[2], values[3]);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn VertexAttribI4ui(&self, cx: &mut JSContext, index: u32, x: u32, y: u32, z: u32, w: u32) {
        self.vertex_attrib_u(cx, index, x, y, z, w)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn VertexAttribI4uiv(
        &self,
        cx: &mut JSContext,
        index: u32,
        v: Uint32ArrayOrUnsignedLongSequence,
    ) {
        let values = match v {
            Uint32ArrayOrUnsignedLongSequence::Uint32Array(v) => typed_array_to_vec(&*v),
            Uint32ArrayOrUnsignedLongSequence::UnsignedLongSequence(v) => v,
        };
        if values.len() < 4 {
            return self.base.webgl_error(InvalidValue);
        }
        self.vertex_attrib_u(cx, index, values[0], values[1], values[2], values[3]);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.10>
    fn VertexAttribPointer(
        &self,
        cx: &mut JSContext,
        attrib_id: u32,
        size: i32,
        data_type: u32,
        normalized: bool,
        stride: i32,
        offset: i64,
    ) {
        self.base
            .VertexAttribPointer(cx, attrib_id, size, data_type, normalized, stride, offset)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.8>
    fn VertexAttribIPointer(
        &self,
        cx: &mut JSContext,
        index: u32,
        size: i32,
        type_: u32,
        stride: i32,
        offset: i64,
    ) {
        match type_ {
            constants::BYTE |
            constants::UNSIGNED_BYTE |
            constants::SHORT |
            constants::UNSIGNED_SHORT |
            constants::INT |
            constants::UNSIGNED_INT => {},
            _ => return self.base.webgl_error(InvalidEnum),
        };
        let res = self
            .current_vao(cx)
            .vertex_attrib_i_pointer(index, size, type_, stride, offset);
        handle_potential_webgl_error!(self.base, res);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.4>
    fn Viewport(&self, x: i32, y: i32, width: i32, height: i32) {
        self.base.Viewport(x, y, width, height)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.6>
    ///
    /// Allocates and initializes the specified mipmap level of a three-dimensional or
    /// two-dimensional array texture.
    fn TexImage3D(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        internal_format: i32,
        width: i32,
        height: i32,
        depth: i32,
        border: i32,
        format: u32,
        type_: u32,
        src_data: CustomAutoRooterGuard<Option<ArrayBufferView>>,
    ) -> Fallible<()> {
        // If a WebGLBuffer is bound to the PIXEL_UNPACK_BUFFER target,
        // generates an INVALID_OPERATION error.
        if self.bound_pixel_unpack_buffer.get().is_some() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }

        // If type is specified as FLOAT_32_UNSIGNED_INT_24_8_REV, srcData must be null;
        // otherwise, generates an INVALID_OPERATION error.
        if type_ == constants::FLOAT_32_UNSIGNED_INT_24_8_REV && src_data.is_some() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }

        if border != 0 {
            self.base.webgl_error(InvalidValue);
            return Ok(());
        }

        let Ok(TexImage3DValidatorResult {
            width,
            height,
            depth,
            level,
            border,
            texture,
            target,
            internal_format,
            format,
            data_type,
        }) = TexImage3DValidator::new(
            &self.base,
            target,
            level,
            internal_format as u32,
            width,
            height,
            depth,
            border,
            format,
            type_,
            src_data.as_ref(),
        )
        .validate()
        else {
            return Ok(());
        };

        let unpacking_alignment = self.base.texture_unpacking_alignment();
        let (alpha_treatment, y_axis_treatment) =
            self.base.get_current_unpack_state(Alpha::NotPremultiplied);
        // If UNPACK_FLIP_Y_WEBGL or UNPACK_PREMULTIPLY_ALPHA_WEBGL is set to true, texImage3D and texSubImage3D
        // generate an INVALID_OPERATION error if they upload data from a PIXEL_UNPACK_BUFFER or a non-null client
        // side ArrayBufferView.
        if (alpha_treatment.is_some() || y_axis_treatment == YAxisTreatment::Flipped) &&
            src_data.is_some()
        {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        // If srcData is null, a buffer of sufficient size initialized to 0 is passed.
        let buff = match *src_data {
            Some(ref data) => {
                let Some(buff) = self.unpack_client_bytes(
                    array_buffer_view_bytes(&data, no_gc),
                    width,
                    height,
                    depth,
                    format,
                    data_type,
                    unpacking_alignment,
                    true,
                ) else {
                    return Ok(());
                };
                buff
            },
            None => GenericSharedMemory::from_byte(
                0,
                Self::unpack_byte_len(width, height, depth, format, data_type, unpacking_alignment)
                    as usize,
            ),
        };
        let tex_source = TexPixels::from_array(
            buff,
            Size2D::new(width, height),
            alpha_treatment,
            y_axis_treatment,
        );

        self.tex_image_3d(
            &texture,
            target,
            data_type,
            internal_format,
            format,
            level,
            width,
            height,
            depth,
            border,
            unpacking_alignment,
            tex_source,
        );
        Ok(())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#3.7.6>
    ///
    /// texImage3D from the bound PIXEL_UNPACK_BUFFER.
    fn TexImage3D_(
        &self,
        _no_gc: &NoGC,
        target: u32,
        level: i32,
        internal_format: i32,
        width: i32,
        height: i32,
        depth: i32,
        border: i32,
        format: u32,
        type_: u32,
        pbo_offset: i64,
    ) -> Fallible<()> {
        if self.bound_pixel_unpack_buffer.get().is_none() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        if self.unpack_flip_or_premultiply() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        if border != 0 {
            self.base.webgl_error(InvalidValue);
            return Ok(());
        }
        let Ok(TexImage3DValidatorResult {
            width,
            height,
            depth,
            level,
            texture,
            target,
            internal_format,
            format,
            data_type,
            ..
        }) = TexImage3DValidator::new(
            &self.base,
            target,
            level,
            internal_format as u32,
            width,
            height,
            depth,
            border,
            format,
            type_,
            None,
        )
        .validate()
        else {
            return Ok(());
        };
        let unpacking_alignment = self.base.texture_unpacking_alignment();
        let cpp =
            data_type.element_size() * format.components() / data_type.components_per_element();
        let Some(layout) = self.unpack_layout(width, height, depth, cpp, unpacking_alignment, true)
        else {
            return Ok(());
        };
        if !self.validate_pixel_unpack_buffer(pbo_offset, layout.end) {
            return Ok(());
        }

        handle_potential_webgl_error!(
            self.base,
            texture.initialize(
                target,
                width,
                height,
                depth,
                internal_format,
                level,
                Some(data_type)
            ),
            return Ok(())
        );
        let internal_format = self
            .base
            .extension_manager()
            .get_effective_tex_internal_format(internal_format, data_type.as_gl_constant());
        let effective_data_type = self
            .base
            .extension_manager()
            .effective_type(data_type.as_gl_constant());
        self.with_unpack_params(true, || {
            self.base.send_command(WebGLCommand::TexImage3DPBO {
                target: target.as_gl_constant(),
                level,
                internal_format,
                size: Size2D::new(width, height),
                depth,
                format,
                effective_data_type,
                unpacking_alignment,
                offset: pbo_offset,
            })
        });
        if let Some(fb) = self.base.bound_draw_framebuffer() {
            fb.invalidate_texture(&texture);
        }
        Ok(())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#3.7.6>
    ///
    /// texImage3D from an image source: `depth` slices of `width` x `height`, stacked vertically.
    fn TexImage3D__(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        internal_format: i32,
        width: i32,
        height: i32,
        depth: i32,
        border: i32,
        format: u32,
        type_: u32,
        source: TexImageSource,
    ) -> Fallible<()> {
        if self.forbid_pixel_unpack_buffer() {
            return Ok(());
        }
        if border != 0 {
            self.base.webgl_error(InvalidValue);
            return Ok(());
        }
        let Ok(TexImage3DValidatorResult {
            width,
            height,
            depth,
            level,
            border,
            texture,
            target,
            internal_format,
            format,
            data_type,
        }) = TexImage3DValidator::new(
            &self.base,
            target,
            level,
            internal_format as u32,
            width,
            height,
            depth,
            border,
            format,
            type_,
            None,
        )
        .validate()
        else {
            return Ok(());
        };
        if !Self::can_convert_image_source(internal_format, data_type) {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        let Some(pixels) = self.base.get_image_pixels(no_gc, source)? else {
            return Ok(());
        };
        let Some(pixels) = self.image_source_window(pixels, width, height, depth, true) else {
            return Ok(());
        };
        self.tex_image_3d(
            &texture,
            target,
            data_type,
            internal_format,
            format,
            level,
            width,
            height,
            depth,
            border,
            // UNPACK_ALIGNMENT does not apply to TexImageSource uploads, whose rows are packed.
            1,
            pixels,
        );
        Ok(())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#3.7.6>
    fn TexImage3D___(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        internal_format: i32,
        width: i32,
        height: i32,
        depth: i32,
        border: i32,
        format: u32,
        type_: u32,
        src_data: CustomAutoRooterGuard<ArrayBufferView>,
        src_offset: u32,
    ) -> Fallible<()> {
        if self.forbid_pixel_unpack_buffer() {
            return Ok(());
        }
        if type_ == constants::FLOAT_32_UNSIGNED_INT_24_8_REV {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        if self.unpack_flip_or_premultiply() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        if border != 0 {
            self.base.webgl_error(InvalidValue);
            return Ok(());
        }
        let Some(elem_size) = src_data.get_array_type().byte_size() else {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        };
        let src_byte_offset = src_offset as usize * elem_size;
        let bytes = array_buffer_view_bytes(&src_data, no_gc);
        if src_byte_offset > bytes.len() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        let Ok(TexImage3DValidatorResult {
            width,
            height,
            depth,
            level,
            border,
            texture,
            target,
            internal_format,
            format,
            data_type,
        }) = TexImage3DValidator::new(
            &self.base,
            target,
            level,
            internal_format as u32,
            width,
            height,
            depth,
            border,
            format,
            type_,
            Some(&*src_data),
        )
        .validate()
        else {
            return Ok(());
        };
        let unpacking_alignment = self.base.texture_unpacking_alignment();
        let Some(buff) = self.unpack_client_bytes(
            &bytes[src_byte_offset..],
            width,
            height,
            depth,
            format,
            data_type,
            unpacking_alignment,
            true,
        ) else {
            return Ok(());
        };
        let pixels =
            TexPixels::from_array(buff, Size2D::new(width, height), None, YAxisTreatment::AsIs);
        self.tex_image_3d(
            &texture,
            target,
            data_type,
            internal_format,
            format,
            level,
            width,
            height,
            depth,
            border,
            unpacking_alignment,
            pixels,
        );
        Ok(())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#3.7.6>
    ///
    /// texSubImage3D from the bound PIXEL_UNPACK_BUFFER.
    fn TexSubImage3D(
        &self,
        _no_gc: &NoGC,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        width: i32,
        height: i32,
        depth: i32,
        format: u32,
        type_: u32,
        pbo_offset: i64,
    ) -> Fallible<()> {
        if self.bound_pixel_unpack_buffer.get().is_none() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        if self.unpack_flip_or_premultiply() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        let Some(args) = self.validate_tex_sub_image_3d(
            target, level, xoffset, yoffset, zoffset, width, height, depth, format, type_,
        ) else {
            return Ok(());
        };
        let unpacking_alignment = self.base.texture_unpacking_alignment();
        let cpp = args.data_type.element_size() * args.format.components() /
            args.data_type.components_per_element();
        let Some(layout) = self.unpack_layout(
            args.width,
            args.height,
            args.depth,
            cpp,
            unpacking_alignment,
            true,
        ) else {
            return Ok(());
        };
        if !self.validate_pixel_unpack_buffer(pbo_offset, layout.end) {
            return Ok(());
        }
        self.tex_sub_image_3d(
            args,
            unpacking_alignment,
            TexSource::BufferOffset(pbo_offset),
        );
        Ok(())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#3.7.6>
    ///
    /// texSubImage3D from an image source: `depth` slices of `width` x `height`, stacked vertically.
    fn TexSubImage3D_(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        width: i32,
        height: i32,
        depth: i32,
        format: u32,
        type_: u32,
        source: TexImageSource,
    ) -> Fallible<()> {
        if self.forbid_pixel_unpack_buffer() {
            return Ok(());
        }
        let Some(args) = self.validate_tex_sub_image_3d(
            target, level, xoffset, yoffset, zoffset, width, height, depth, format, type_,
        ) else {
            return Ok(());
        };
        if !Self::can_convert_image_source(args.format, args.data_type) {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        let Some(pixels) = self.base.get_image_pixels(no_gc, source)? else {
            return Ok(());
        };
        let Some(pixels) =
            self.image_source_window(pixels, args.width, args.height, args.depth, true)
        else {
            return Ok(());
        };
        self.tex_sub_image_3d(args, 1, TexSource::Pixels(pixels));
        Ok(())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#3.7.6>
    fn TexSubImage3D__(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        width: i32,
        height: i32,
        depth: i32,
        format: u32,
        type_: u32,
        src_data: CustomAutoRooterGuard<Option<ArrayBufferView>>,
        src_offset: u32,
    ) -> Fallible<()> {
        if self.forbid_pixel_unpack_buffer() {
            return Ok(());
        }
        if self.unpack_flip_or_premultiply() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        let Some(args) = self.validate_tex_sub_image_3d(
            target, level, xoffset, yoffset, zoffset, width, height, depth, format, type_,
        ) else {
            return Ok(());
        };
        let Some(ref src_data) = *src_data else {
            self.base.webgl_error(InvalidValue);
            return Ok(());
        };
        // The view's element type must match `type`.
        if array_buffer_type_to_sized_type(src_data.get_array_type()) !=
            Some(args.data_type.sized_data_type())
        {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        let elem_size = src_data.get_array_type().byte_size().unwrap_or(1);
        let src_byte_offset = src_offset as usize * elem_size;
        let bytes = array_buffer_view_bytes(&src_data, no_gc);
        let unpacking_alignment = self.base.texture_unpacking_alignment();
        if src_byte_offset > bytes.len() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        let Some(buff) = self.unpack_client_bytes(
            &bytes[src_byte_offset..],
            args.width,
            args.height,
            args.depth,
            args.format,
            args.data_type,
            unpacking_alignment,
            true,
        ) else {
            return Ok(());
        };
        let pixels = TexPixels::from_array(
            buff,
            Size2D::new(args.width, args.height),
            None,
            YAxisTreatment::AsIs,
        );
        self.tex_sub_image_3d(args, unpacking_alignment, TexSource::Pixels(pixels));
        Ok(())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#3.7.6>
    fn CopyTexSubImage3D(
        &self,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) {
        handle_potential_webgl_error!(self.base, self.base.validate_framebuffer(), return);
        handle_potential_webgl_error!(self.base, self.base.validate_read_buffer_image(), return);
        let target = match TexImageTarget::from_gl_constant(target) {
            Some(target) if target.dimensions() == 3 => target,
            _ => return self.base.webgl_error(InvalidEnum),
        };
        let Some(texture) = self.base.textures().active_texture_for_image_target(target) else {
            return self.base.webgl_error(InvalidOperation);
        };
        if level < 0 || width < 0 || height < 0 {
            return self.base.webgl_error(InvalidValue);
        }
        let Some(image_info) = texture.image_info_for_target(&target, level as u32) else {
            return self.base.webgl_error(InvalidOperation);
        };
        let fits = |offset: i32, size: u32, max: u32| {
            offset >= 0 && offset as u64 + size as u64 <= max as u64
        };
        if !fits(xoffset, width as u32, image_info.width()) ||
            !fits(yoffset, height as u32, image_info.height()) ||
            !fits(zoffset, 1, image_info.depth())
        {
            return self.base.webgl_error(InvalidValue);
        }
        if image_info.internal_format().is_compressed() {
            return self.base.webgl_error(InvalidOperation);
        }
        // Copying from the layer the read buffer is into itself is a feedback loop.
        if self.base.copy_reads_layer(&texture, level as u32, zoffset) {
            return self.base.webgl_error(InvalidOperation);
        }
        if !self
            .base
            .copy_format_compatible(image_info.internal_format(), true)
        {
            return self.base.webgl_error(InvalidOperation);
        }
        let Some((xoffset, yoffset, x, y, width, height)) = clip_copy_to_read_framebuffer(
            self.base.read_framebuffer_size(),
            xoffset,
            yoffset,
            x,
            y,
            width,
            height,
        ) else {
            return;
        };
        self.base.send_command(WebGLCommand::CopyTexSubImage3D(
            target.as_gl_constant(),
            level,
            xoffset,
            yoffset,
            zoffset,
            x,
            y,
            width,
            height,
        ));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
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
    ) -> Fallible<()> {
        if self.forbid_pixel_unpack_buffer() {
            return Ok(());
        }
        // With WebGL 2 unpack parameters set, client data goes through the repacking path.
        if let Some(ref view) = *pixels &&
            self.unpack_params.get() != UnpackParams::default()
        {
            return self.tex_image_2d_from_view(
                no_gc,
                target,
                level,
                internal_format,
                width,
                height,
                border,
                format,
                data_type,
                view,
                0,
            );
        }
        self.base.TexImage2D(
            no_gc,
            target,
            level,
            internal_format,
            width,
            height,
            border,
            format,
            data_type,
            pixels,
        )
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
        if self.forbid_pixel_unpack_buffer() {
            return Ok(());
        }
        // A DOM source converts only to colour formats and types (an integer format such as
        // RGB10_A2UI can't take it); the WebGL 1 path below doesn't know the WebGL 2 ones.
        // INVALID_VALUE, as Chromium reports it (tex-image-with-different-data-source).
        if let (Some(format), Some(data_type)) = (
            TexFormat::from_gl_constant(internal_format as u32),
            TexDataType::from_webgl_constant(data_type, WebGLVersion::WebGL2),
        ) && !Self::can_convert_image_source(format, data_type)
        {
            self.base.webgl_error(InvalidValue);
            return Ok(());
        }
        self.base.TexImage2D_(
            no_gc,
            target,
            level,
            internal_format,
            format,
            data_type,
            source,
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.6>
    fn TexImage2D__(
        &self,
        _no_gc: &NoGC,
        target: u32,
        level: i32,
        internalformat: i32,
        width: i32,
        height: i32,
        border: i32,
        format: u32,
        type_: u32,
        pbo_offset: i64,
    ) -> Fallible<()> {
        if self.bound_pixel_unpack_buffer.get().is_none() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        let Ok(TexImage2DValidatorResult {
            texture,
            target,
            width,
            height,
            level,
            border,
            internal_format,
            format,
            data_type,
        }) = TexImage2DValidator::new(
            &self.base,
            target,
            level,
            internalformat as u32,
            width,
            height,
            border,
            format,
            type_,
        )
        .validate()
        else {
            return Ok(());
        };
        let unpacking_alignment = self.base.texture_unpacking_alignment();
        let cpp =
            data_type.element_size() * format.components() / data_type.components_per_element();
        let Some(layout) = self.unpack_layout(width, height, 1, cpp, unpacking_alignment, false)
        else {
            return Ok(());
        };
        // WebGL 2.0 5.35: FLIP_Y / PREMULTIPLY_ALPHA can't apply to a PIXEL_UNPACK_BUFFER upload.
        if self.unpack_flip_or_premultiply() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        if !self.validate_pixel_unpack_buffer(pbo_offset, layout.end) {
            return Ok(());
        }
        self.with_unpack_params(false, || {
            self.base.tex_image_2d(
                &texture,
                target,
                data_type,
                internal_format,
                format,
                level,
                border,
                unpacking_alignment,
                Size2D::new(width, height),
                TexSource::BufferOffset(pbo_offset),
            )
        });
        Ok(())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.6>
    fn TexImage2D___(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        internalformat: i32,
        width: i32,
        height: i32,
        border: i32,
        format: u32,
        type_: u32,
        source: TexImageSource,
    ) -> Fallible<()> {
        if self.forbid_pixel_unpack_buffer() {
            return Ok(());
        }
        let Ok(TexImage2DValidatorResult {
            texture,
            target,
            width,
            height,
            level,
            border,
            internal_format,
            format,
            data_type,
        }) = TexImage2DValidator::new(
            &self.base,
            target,
            level,
            internalformat as u32,
            width,
            height,
            border,
            format,
            type_,
        )
        .validate()
        else {
            return Ok(());
        };
        if !Self::can_convert_image_source(internal_format, data_type) {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        let Some(pixels) = self.base.get_image_pixels(no_gc, source)? else {
            return Ok(());
        };
        let Some(pixels) = self.image_source_window(pixels, width, height, 1, false) else {
            return Ok(());
        };
        self.base.tex_image_2d(
            &texture,
            target,
            data_type,
            internal_format,
            format,
            level,
            border,
            // UNPACK_ALIGNMENT does not apply to TexImageSource uploads, whose rows are packed.
            1,
            Size2D::new(width, height),
            TexSource::Pixels(pixels),
        );
        Ok(())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.6>
    fn TexImage2D____(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        internalformat: i32,
        width: i32,
        height: i32,
        border: i32,
        format: u32,
        type_: u32,
        src_data: CustomAutoRooterGuard<ArrayBufferView>,
        src_offset: u32,
    ) -> Fallible<()> {
        if self.forbid_pixel_unpack_buffer() {
            return Ok(());
        }
        self.tex_image_2d_from_view(
            no_gc,
            target,
            level,
            internalformat,
            width,
            height,
            border,
            format,
            type_,
            &src_data,
            src_offset,
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
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
    ) -> Fallible<()> {
        if self.forbid_pixel_unpack_buffer() {
            return Ok(());
        }
        match *pixels {
            Some(ref view) => self.tex_sub_image_2d_from_view(
                no_gc, target, level, xoffset, yoffset, width, height, format, data_type, view, 0,
            ),
            None => {
                if self
                    .validate_tex_sub_image_2d(
                        target, level, xoffset, yoffset, width, height, format, data_type,
                    )
                    .is_some()
                {
                    self.base.webgl_error(InvalidValue);
                }
                Ok(())
            },
        }
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
        if self.forbid_pixel_unpack_buffer() {
            return Ok(());
        }
        let Some(pixels) = self.base.get_image_pixels(no_gc, source)? else {
            return Ok(());
        };
        let size = pixels.size();
        let Some(TexSubImage2DArgs {
            texture,
            target,
            width,
            height,
            level,
            format,
            data_type,
        }) = self.validate_tex_sub_image_2d(
            target,
            level,
            xoffset,
            yoffset,
            size.width as i32,
            size.height as i32,
            format,
            data_type,
        )
        else {
            return Ok(());
        };
        if !Self::can_convert_image_source(format, data_type) {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        let Some(pixels) = self.image_source_window(pixels, width, height, 1, false) else {
            return Ok(());
        };
        self.base.tex_sub_image_2d_from_source(
            texture,
            target,
            level,
            xoffset,
            yoffset,
            Size2D::new(width, height),
            format,
            data_type,
            1,
            TexSource::Pixels(pixels),
        );
        Ok(())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#3.7.6>
    ///
    /// texSubImage2D from the bound PIXEL_UNPACK_BUFFER.
    fn TexSubImage2D__(
        &self,
        _no_gc: &NoGC,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
        pbo_offset: i64,
    ) -> Fallible<()> {
        if self.bound_pixel_unpack_buffer.get().is_none() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        let Some(TexSubImage2DArgs {
            texture,
            target,
            width,
            height,
            level,
            format,
            data_type,
        }) = self.validate_tex_sub_image_2d(
            target, level, xoffset, yoffset, width, height, format, type_,
        )
        else {
            return Ok(());
        };
        let unpacking_alignment = self.base.texture_unpacking_alignment();
        let cpp =
            data_type.element_size() * format.components() / data_type.components_per_element();
        let Some(layout) = self.unpack_layout(width, height, 1, cpp, unpacking_alignment, false)
        else {
            return Ok(());
        };
        // WebGL 2.0 5.35: FLIP_Y / PREMULTIPLY_ALPHA can't apply to a PIXEL_UNPACK_BUFFER upload.
        if self.unpack_flip_or_premultiply() {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        if !self.validate_pixel_unpack_buffer(pbo_offset, layout.end) {
            return Ok(());
        }
        self.with_unpack_params(false, || {
            self.base.tex_sub_image_2d_from_source(
                texture,
                target,
                level,
                xoffset,
                yoffset,
                Size2D::new(width, height),
                format,
                data_type,
                unpacking_alignment,
                TexSource::BufferOffset(pbo_offset),
            )
        });
        Ok(())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#3.7.6>
    ///
    /// texSubImage2D from a `width` x `height` window of an image source.
    fn TexSubImage2D___(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
        source: TexImageSource,
    ) -> Fallible<()> {
        if self.forbid_pixel_unpack_buffer() {
            return Ok(());
        }
        let Some(TexSubImage2DArgs {
            texture,
            target,
            width,
            height,
            level,
            format,
            data_type,
        }) = self.validate_tex_sub_image_2d(
            target, level, xoffset, yoffset, width, height, format, type_,
        )
        else {
            return Ok(());
        };
        if !Self::can_convert_image_source(format, data_type) {
            self.base.webgl_error(InvalidOperation);
            return Ok(());
        }
        let Some(pixels) = self.base.get_image_pixels(no_gc, source)? else {
            return Ok(());
        };
        let Some(pixels) = self.image_source_window(pixels, width, height, 1, false) else {
            return Ok(());
        };
        self.base.tex_sub_image_2d_from_source(
            texture,
            target,
            level,
            xoffset,
            yoffset,
            Size2D::new(width, height),
            format,
            data_type,
            1,
            TexSource::Pixels(pixels),
        );
        Ok(())
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#3.7.6>
    fn TexSubImage2D____(
        &self,
        no_gc: &NoGC,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
        src_data: CustomAutoRooterGuard<ArrayBufferView>,
        src_offset: u32,
    ) -> Fallible<()> {
        if self.forbid_pixel_unpack_buffer() {
            return Ok(());
        }
        self.tex_sub_image_2d_from_view(
            no_gc, target, level, xoffset, yoffset, width, height, format, type_, &src_data,
            src_offset,
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn TexParameterf(&self, target: u32, name: u32, value: f32) {
        self.base.TexParameterf(target, name, value)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.8>
    fn TexParameteri(&self, target: u32, name: u32, value: i32) {
        self.base.TexParameteri(target, name, value)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.6>
    fn CheckFramebufferStatus(&self, target: u32) -> u32 {
        let fb_slot = match target {
            constants::FRAMEBUFFER | constants::DRAW_FRAMEBUFFER => {
                self.base.get_draw_framebuffer_slot()
            },
            constants::READ_FRAMEBUFFER => self.base.get_read_framebuffer_slot(),
            _ => {
                self.base.webgl_error(InvalidEnum);
                return 0;
            },
        };
        match fb_slot.get() {
            Some(fb) => fb.check_status(),
            None => constants::FRAMEBUFFER_COMPLETE,
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.7>
    fn RenderbufferStorage(&self, target: u32, internal_format: u32, width: i32, height: i32) {
        self.base
            .RenderbufferStorage(target, internal_format, width, height)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.4S>
    fn BlitFramebuffer(
        &self,
        src_x0: i32,
        src_y0: i32,
        src_x1: i32,
        src_y1: i32,
        dst_x0: i32,
        dst_y0: i32,
        dst_x1: i32,
        dst_y1: i32,
        mask: u32,
        filter: u32,
    ) {
        bitflags! {
            struct BlitFrameBufferFlags: u32 {
                const DEPTH = constants::DEPTH_BUFFER_BIT;
                const COLOR = constants::COLOR_BUFFER_BIT;
                const STENCIL = constants::STENCIL_BUFFER_BIT;
                const DEPTH_STENCIL = constants::DEPTH_BUFFER_BIT | constants::STENCIL_BUFFER_BIT;
            }
        };
        let Some(bits) = BlitFrameBufferFlags::from_bits(mask) else {
            return self.base.webgl_error(InvalidValue);
        };
        let attributes = self.base.GetContextAttributes().unwrap();

        if bits.intersects(BlitFrameBufferFlags::DEPTH_STENCIL) {
            match filter {
                constants::LINEAR => return self.base.webgl_error(InvalidOperation),
                constants::NEAREST => {},
                _ => return self.base.webgl_error(InvalidOperation),
            }
        }

        let src_fb = self.base.get_read_framebuffer_slot().get();
        let dst_fb = self.base.get_draw_framebuffer_slot().get();

        // Zero-initialize the destination's attachments now if they haven't been yet (WebGL
        // initializes lazily, on first use). Otherwise the first draw or read after the blit
        // does it and wipes what the blit wrote: an MSAA resolve read back as zeros.
        if let Some(ref fb) = dst_fb &&
            let CompleteForRendering::Incomplete = fb.check_status_for_rendering()
        {
            return self.base.webgl_error(InvalidFramebufferOperation);
        }
        // The source too: an untouched depth buffer must blit as 1.0, not as whatever GL left
        // there (multisampled-depth-renderbuffer-initialization). Initializing clears the draw
        // framebuffer, so bind the source there for it.
        if let Some(ref fb) = src_fb &&
            !fb.is_initialized() &&
            dst_fb.as_ref().is_none_or(|dst| dst.id() != fb.id())
        {
            self.base.send_command(WebGLCommand::BindFramebuffer(
                constants::DRAW_FRAMEBUFFER,
                WebGLFramebufferBindingRequest::Explicit(fb.id()),
            ));
            let status = fb.check_status_for_rendering();
            self.base.send_command(WebGLCommand::BindFramebuffer(
                constants::DRAW_FRAMEBUFFER,
                match dst_fb {
                    Some(ref dst) => WebGLFramebufferBindingRequest::Explicit(dst.id()),
                    None => WebGLFramebufferBindingRequest::Default,
                },
            ));
            if let CompleteForRendering::Incomplete = status {
                return self.base.webgl_error(InvalidFramebufferOperation);
            }
        }

        let get_default_formats = || -> WebGLResult<(Option<u32>, Option<u32>, Option<u32>)> {
            // All attempts to blit to an antialiased back buffer should fail.
            if attributes.antialias {
                return Err(InvalidOperation);
            };
            let color = if attributes.alpha {
                Some(constants::RGBA8)
            } else {
                Some(constants::RGB8)
            };
            let (depth, stencil) = match (attributes.depth, attributes.stencil) {
                (true, true) => (
                    Some(constants::DEPTH24_STENCIL8),
                    Some(constants::DEPTH24_STENCIL8),
                ),
                (true, false) => (Some(constants::DEPTH_COMPONENT16), None),
                (false, true) => (None, Some(constants::STENCIL_INDEX8)),
                _ => (None, None),
            };
            Ok((color, depth, stencil))
        };

        let (src_color, src_depth, src_stencil) = match &src_fb {
            Some(fb) => handle_potential_webgl_error!(
                self.base,
                fb.get_attachment_formats(fb.read_buffer()),
                return
            ),
            None => handle_potential_webgl_error!(self.base, get_default_formats(), return),
        };
        let (dst_color, dst_depth, dst_stencil) = match &dst_fb {
            Some(fb) => {
                let draw_buffer = match fb.draw_buffer_i(0) {
                    constants::NONE => constants::COLOR_ATTACHMENT0,
                    buffer => buffer,
                };
                handle_potential_webgl_error!(
                    self.base,
                    fb.get_attachment_formats(draw_buffer),
                    return
                )
            },
            None => handle_potential_webgl_error!(self.base, get_default_formats(), return),
        };

        // Reading and writing the same image is INVALID_OPERATION (GL ES 3.0, 4.3.3), for each
        // buffer the mask copies; both bound to the default framebuffer counts as the same.
        let same_image = |src_attachment: u32, dst_attachment: u32| match (&src_fb, &dst_fb) {
            (None, None) => true,
            (Some(src), Some(dst)) => src
                .attached_image(src_attachment)
                .is_some_and(|image| dst.attached_image(dst_attachment) == Some(image)),
            _ => false,
        };
        let src_color_attachment = src_fb
            .as_ref()
            .map_or(constants::BACK, |fb| fb.read_buffer());
        // A colour blit writes every draw buffer (GL ES 3.0 4.3.3).
        let dst_color_attachments: Vec<u32> = match &dst_fb {
            None => vec![constants::BACK],
            Some(fb) => (0..self.base.limits().max_draw_buffers as usize)
                .map(|i| fb.draw_buffer_i(i))
                .filter(|&buffer| buffer != constants::NONE)
                .collect(),
        };
        if (bits.intersects(BlitFrameBufferFlags::COLOR) &&
            dst_color_attachments
                .iter()
                .any(|&dst| same_image(src_color_attachment, dst))) ||
            (bits.intersects(BlitFrameBufferFlags::DEPTH) &&
                (same_image(constants::DEPTH_ATTACHMENT, constants::DEPTH_ATTACHMENT) ||
                    same_image(
                        constants::DEPTH_STENCIL_ATTACHMENT,
                        constants::DEPTH_STENCIL_ATTACHMENT,
                    ))) ||
            (bits.intersects(BlitFrameBufferFlags::STENCIL) &&
                (same_image(constants::STENCIL_ATTACHMENT, constants::STENCIL_ATTACHMENT) ||
                    same_image(
                        constants::DEPTH_STENCIL_ATTACHMENT,
                        constants::DEPTH_STENCIL_ATTACHMENT,
                    )))
        {
            return self.base.webgl_error(InvalidOperation);
        }

        // Colour blits need matching component types (GL ES 3.0 4.3.3): fixed-point and float
        // mix freely, signed and unsigned integer only with themselves. GL itself rejects format
        // mismatches out of multisampled buffers.
        let blit_class = |format| match color_component_class(format) {
            1 => 0,
            class => class,
        };
        if bits.intersects(BlitFrameBufferFlags::COLOR) &&
            let Some(src) = src_color
        {
            let mismatch =
                |dst: Option<u32>| dst.is_some_and(|dst| blit_class(src) != blit_class(dst));
            let any_mismatch = match &dst_fb {
                None => mismatch(dst_color),
                Some(fb) => dst_color_attachments
                    .iter()
                    .any(|&dst| mismatch(fb.attachment_internal_format(dst))),
            };
            if any_mismatch {
                return self.base.webgl_error(InvalidOperation);
            }
        }
        // WebGL 2.0 5.38: a colour blit from a read buffer with no image into draw buffers
        // that have one is INVALID_OPERATION.
        // (A read buffer of NONE is not a missing image: that blit copies no colour.)
        if bits.intersects(BlitFrameBufferFlags::COLOR) &&
            src_color.is_none() &&
            src_fb
                .as_ref()
                .is_some_and(|fb| fb.read_buffer() != constants::NONE)
        {
            let any_dst_image = match &dst_fb {
                None => true,
                Some(fb) => dst_color_attachments
                    .iter()
                    .any(|&dst| fb.attachment_internal_format(dst).is_some()),
            };
            if any_dst_image {
                return self.base.webgl_error(InvalidOperation);
            }
        }
        if bits.intersects(BlitFrameBufferFlags::DEPTH) && src_depth != dst_depth {
            return self.base.webgl_error(InvalidOperation);
        }
        if bits.intersects(BlitFrameBufferFlags::STENCIL) && src_stencil != dst_stencil {
            return self.base.webgl_error(InvalidOperation);
        }

        // A width or height (or its magnitude) that overflows a GLint is INVALID_VALUE, as in
        // ANGLE; the driver's own arithmetic would overflow.
        let extent = |a: i32, b: i32| b.checked_sub(a).and_then(i32::checked_abs);
        if extent(src_x0, src_x1).is_none() ||
            extent(dst_x0, dst_x1).is_none() ||
            extent(src_y0, src_y1).is_none() ||
            extent(dst_y0, dst_y1).is_none()
        {
            return self.base.webgl_error(InvalidValue);
        }

        // GL ES 3.0 4.3.3: nothing blits into a multisampled framebuffer, and a blit out of one
        // (a resolve) needs identical rectangles and identical colour formats. The canvas is
        // RGBA8 or, with alpha: false, RGB8.
        if dst_fb.as_ref().is_some_and(|fb| fb.is_multisampled()) {
            return self.base.webgl_error(InvalidOperation);
        }
        if let Some(src) = src_fb.as_ref().filter(|fb| fb.is_multisampled()) {
            if (src_x0, src_y0, src_x1, src_y1) != (dst_x0, dst_y0, dst_x1, dst_y1) {
                return self.base.webgl_error(InvalidOperation);
            }
            if bits.intersects(BlitFrameBufferFlags::COLOR) &&
                let Some(src_format) = src.attachment_sized_format(src.read_buffer())
            {
                let differs = |dst: Option<u32>| dst.is_some_and(|dst| dst != src_format);
                let any_differs = match &dst_fb {
                    None => differs(dst_color),
                    Some(fb) => dst_color_attachments
                        .iter()
                        .any(|&dst| differs(fb.attachment_sized_format(dst))),
                };
                if any_differs {
                    return self.base.webgl_error(InvalidOperation);
                }
            }
        }

        // Clip out-of-bounds rectangles here (a resolve's are identical and need no clipping).
        let (src_x0, src_y0, src_x1, src_y1, dst_x0, dst_y0, dst_x1, dst_y1) = if src_fb
            .as_ref()
            .is_some_and(|fb| fb.is_multisampled())
        {
            (src_x0, src_y0, src_x1, src_y1, dst_x0, dst_y0, dst_x1, dst_y1)
        } else {
            let canvas_size = self.base.size();
            let canvas_size = (canvas_size.width as i32, canvas_size.height as i32);
            let src_size = src_fb.as_ref().map_or(Some(canvas_size), |fb| fb.size());
            let dst_size = dst_fb.as_ref().map_or(Some(canvas_size), |fb| fb.size());
            match (src_size, dst_size) {
                (Some(src_size), Some(dst_size)) => {
                    let x = clip_blit_axis(src_x0, src_x1, dst_x0, dst_x1, src_size.0, dst_size.0);
                    let y = clip_blit_axis(src_y0, src_y1, dst_y0, dst_y1, src_size.1, dst_size.1);
                    let (Some((sx0, sx1, dx0, dx1)), Some((sy0, sy1, dy0, dy1))) = (x, y) else {
                        // No destination pixel maps inside the source: nothing is written.
                        return;
                    };
                    (sx0, sy0, sx1, sy1, dx0, dy0, dx1, dy1)
                },
                _ => (src_x0, src_y0, src_x1, src_y1, dst_x0, dst_y0, dst_x1, dst_y1),
            }
        };

        self.base.send_command(WebGLCommand::BlitFrameBuffer(
            src_x0, src_y0, src_x1, src_y1, dst_x0, dst_y0, dst_x1, dst_y1, mask, filter,
        ));
        self.mark_as_dirty();
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
            handle_potential_webgl_error!(self.base, self.base.validate_ownership(rb), return);
        }

        let fb_slot = match target {
            constants::FRAMEBUFFER | constants::DRAW_FRAMEBUFFER => {
                self.base.get_draw_framebuffer_slot()
            },
            constants::READ_FRAMEBUFFER => self.base.get_read_framebuffer_slot(),
            _ => return self.base.webgl_error(InvalidEnum),
        };

        if renderbuffertarget != constants::RENDERBUFFER {
            return self.base.webgl_error(InvalidEnum);
        }

        match fb_slot.get() {
            Some(fb) => match attachment {
                constants::DEPTH_STENCIL_ATTACHMENT => {
                    handle_potential_webgl_error!(
                        self.base,
                        fb.renderbuffer(constants::DEPTH_ATTACHMENT, rb)
                    );
                    handle_potential_webgl_error!(
                        self.base,
                        fb.renderbuffer(constants::STENCIL_ATTACHMENT, rb)
                    );
                },
                _ => handle_potential_webgl_error!(self.base, fb.renderbuffer(attachment, rb)),
            },
            None => self.base.webgl_error(InvalidOperation),
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
            handle_potential_webgl_error!(self.base, self.base.validate_ownership(texture), return);
        }

        let fb_slot = match target {
            constants::FRAMEBUFFER | constants::DRAW_FRAMEBUFFER => {
                self.base.get_draw_framebuffer_slot()
            },
            constants::READ_FRAMEBUFFER => self.base.get_read_framebuffer_slot(),
            _ => return self.base.webgl_error(InvalidEnum),
        };
        match fb_slot.get() {
            // DEPTH_STENCIL_ATTACHMENT is DEPTH_ATTACHMENT + STENCIL_ATTACHMENT in WebGL 2, as for
            // renderbuffers (a separate slot made the depth and stencil queries miss the texture).
            Some(fb) => match attachment {
                constants::DEPTH_STENCIL_ATTACHMENT => {
                    handle_potential_webgl_error!(
                        self.base,
                        fb.texture2d(constants::DEPTH_ATTACHMENT, textarget, texture, level),
                        return
                    );
                    handle_potential_webgl_error!(
                        self.base,
                        fb.texture2d(constants::STENCIL_ATTACHMENT, textarget, texture, level)
                    );
                },
                _ => handle_potential_webgl_error!(
                    self.base,
                    fb.texture2d(attachment, textarget, texture, level)
                ),
            },
            None => self.base.webgl_error(InvalidOperation),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/1.0/#5.14.9>
    fn GetAttachedShaders(&self, program: &WebGLProgram) -> Option<Vec<DomRoot<WebGLShader>>> {
        self.base.GetAttachedShaders(program)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.9>
    fn DrawArraysInstanced(
        &self,
        cx: &mut JSContext,
        mode: u32,
        first: i32,
        count: i32,
        primcount: i32,
    ) {
        handle_potential_webgl_error!(
            self.base,
            self.draw_arrays_checked(cx, mode, first, count, primcount)
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.9>
    fn DrawElementsInstanced(
        &self,
        cx: &mut JSContext,
        mode: u32,
        count: i32,
        type_: u32,
        offset: i64,
        primcount: i32,
    ) {
        handle_potential_webgl_error!(
            self.base,
            self.draw_elements_checked(cx, mode, count, type_, offset, primcount)
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.9>
    fn DrawRangeElements(
        &self,
        cx: &mut JSContext,
        mode: u32,
        start: u32,
        end: u32,
        count: i32,
        type_: u32,
        offset: i64,
    ) {
        if end < start {
            self.base.webgl_error(InvalidValue);
            return;
        }
        handle_potential_webgl_error!(
            self.base,
            self.draw_elements_checked(cx, mode, count, type_, offset, 1)
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.9>
    fn VertexAttribDivisor(&self, cx: &mut JSContext, index: u32, divisor: u32) {
        self.base.vertex_attrib_divisor(cx, index, divisor);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.12>
    fn CreateQuery(&self, cx: &mut JSContext) -> Option<DomRoot<WebGLQuery>> {
        Some(WebGLQuery::new(cx, &self.base))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.12>
    fn DeleteQuery(&self, query: Option<&WebGLQuery>) {
        if let Some(query) = query {
            handle_potential_webgl_error!(self.base, self.base.validate_ownership(query), return);

            if let Some(query_target) = query.target() {
                let slot = match query_target {
                    constants::ANY_SAMPLES_PASSED | constants::ANY_SAMPLES_PASSED_CONSERVATIVE => {
                        &self.occlusion_query
                    },
                    constants::TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN => &self.primitives_query,
                    _ => unreachable!(),
                };
                if let Some(stored_query) = slot.get() &&
                    stored_query.target() == query.target()
                {
                    slot.set(None);
                }
            }

            query.delete(Operation::Infallible);
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.12>
    fn IsQuery(&self, query: Option<&WebGLQuery>) -> bool {
        match query {
            Some(query) => self.base.validate_ownership(query).is_ok() && query.is_valid(),
            None => false,
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.13>
    fn CreateSampler(&self, cx: &mut JSContext) -> Option<DomRoot<WebGLSampler>> {
        Some(WebGLSampler::new(cx, &self.base))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.13>
    fn DeleteSampler(&self, sampler: Option<&WebGLSampler>) {
        if let Some(sampler) = sampler {
            handle_potential_webgl_error!(self.base, self.base.validate_ownership(sampler), return);
            for slot in self.samplers.iter() {
                if slot.get().is_some_and(|s| sampler == &*s) {
                    slot.set(None);
                }
            }
            sampler.delete(Operation::Infallible);
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.13>
    fn IsSampler(&self, sampler: Option<&WebGLSampler>) -> bool {
        match sampler {
            Some(sampler) => self.base.validate_ownership(sampler).is_ok() && sampler.is_valid(),
            None => false,
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.12>
    fn BeginQuery(&self, target: u32, query: &WebGLQuery) {
        handle_potential_webgl_error!(self.base, self.base.validate_ownership(query), return);

        let active_query = match target {
            constants::ANY_SAMPLES_PASSED | constants::ANY_SAMPLES_PASSED_CONSERVATIVE => {
                &self.occlusion_query
            },
            constants::TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN => &self.primitives_query,
            _ => {
                self.base.webgl_error(InvalidEnum);
                return;
            },
        };
        if active_query.get().is_some() {
            self.base.webgl_error(InvalidOperation);
            return;
        }
        let result = query.begin(&self.base, target);
        match result {
            Ok(_) => active_query.set(Some(query)),
            Err(error) => self.base.webgl_error(error),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.12>
    fn EndQuery(&self, target: u32) {
        let active_query = match target {
            constants::ANY_SAMPLES_PASSED | constants::ANY_SAMPLES_PASSED_CONSERVATIVE => {
                self.occlusion_query.take()
            },
            constants::TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN => self.primitives_query.take(),
            _ => {
                self.base.webgl_error(InvalidEnum);
                return;
            },
        };
        match active_query {
            None => self.base.webgl_error(InvalidOperation),
            Some(query) => {
                let result = query.end(&self.base, target);
                if let Err(error) = result {
                    self.base.webgl_error(error);
                }
            },
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.12>
    fn GetQuery(&self, target: u32, pname: u32) -> Option<DomRoot<WebGLQuery>> {
        if pname != constants::CURRENT_QUERY {
            self.base.webgl_error(InvalidEnum);
            return None;
        }
        let active_query = match target {
            constants::ANY_SAMPLES_PASSED | constants::ANY_SAMPLES_PASSED_CONSERVATIVE => {
                self.occlusion_query.get()
            },
            constants::TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN => self.primitives_query.get(),
            _ => {
                self.base.webgl_error(InvalidEnum);
                None
            },
        };
        if let Some(query) = active_query.as_ref() &&
            query.target() != Some(target)
        {
            return None;
        }
        active_query
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.12>
    fn GetQueryParameter(
        &self,
        _cx: &mut JSContext,
        query: &WebGLQuery,
        pname: u32,
        mut retval: MutableHandleValue,
    ) {
        handle_potential_webgl_error!(
            self.base,
            self.base.validate_ownership(query),
            return retval.set(NullValue())
        );
        match query.get_parameter(&self.base, pname) {
            Ok(value) => match pname {
                constants::QUERY_RESULT => retval.set(UInt32Value(value)),
                constants::QUERY_RESULT_AVAILABLE => retval.set(BooleanValue(value != 0)),
                _ => unreachable!(),
            },
            Err(error) => {
                self.base.webgl_error(error);
                retval.set(NullValue())
            },
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.14>
    fn FenceSync(
        &self,
        cx: &mut JSContext,
        condition: u32,
        flags: u32,
    ) -> Option<DomRoot<WebGLSync>> {
        if flags != 0 {
            self.base.webgl_error(InvalidValue);
            return None;
        }
        if condition != constants::SYNC_GPU_COMMANDS_COMPLETE {
            self.base.webgl_error(InvalidEnum);
            return None;
        }

        Some(WebGLSync::new(cx, &self.base))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.14>
    fn IsSync(&self, sync: Option<&WebGLSync>) -> bool {
        match sync {
            Some(sync) => {
                // Returns false if the sync was generated by a different
                // WebGL2RenderingContext than this one.
                if self.base.validate_ownership(sync).is_err() {
                    return false;
                }

                // Returns false if the sync's invalidated flag is set.
                if !sync.is_valid() {
                    return false;
                }

                // Return true if the passed WebGLSync is valid and false otherwise.
                handle_potential_webgl_error!(
                    self.base,
                    self.base.validate_ownership(sync),
                    return false
                );
                let (sender, receiver) = webgl_channel().unwrap();
                self.base
                    .send_command(WebGLCommand::IsSync(sync.id(), sender));
                receiver.recv().unwrap()
            },
            None => false,
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.14>
    fn ClientWaitSync(&self, sync: &WebGLSync, flags: u32, timeout: u64) -> u32 {
        if !sync.is_valid() {
            self.base.webgl_error(InvalidOperation);
            return constants::WAIT_FAILED;
        }
        handle_potential_webgl_error!(
            self.base,
            self.base.validate_ownership(sync),
            return constants::WAIT_FAILED
        );
        if flags != 0 && flags != constants::SYNC_FLUSH_COMMANDS_BIT {
            self.base.webgl_error(InvalidValue);
            return constants::WAIT_FAILED;
        }
        if timeout > self.base.limits().max_client_wait_timeout_webgl.as_nanos() as u64 {
            self.base.webgl_error(InvalidOperation);
            return constants::WAIT_FAILED;
        }

        match sync.client_wait_sync(&self.base, flags, timeout) {
            Some(status) => status,
            None => constants::WAIT_FAILED,
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.14>
    fn WaitSync(&self, sync: &WebGLSync, flags: u32, timeout: i64) {
        if !sync.is_valid() {
            self.base.webgl_error(InvalidOperation);
            return;
        }
        handle_potential_webgl_error!(self.base, self.base.validate_ownership(sync), return);
        if flags != 0 {
            self.base.webgl_error(InvalidValue);
            return;
        }
        if timeout != constants::TIMEOUT_IGNORED {
            self.base.webgl_error(InvalidValue);
            return;
        }

        self.base
            .send_command(WebGLCommand::WaitSync(sync.id(), flags, timeout));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.14>
    fn GetSyncParameter(
        &self,
        _cx: &mut JSContext,
        sync: &WebGLSync,
        pname: u32,
        mut retval: MutableHandleValue,
    ) {
        if !sync.is_valid() {
            self.base.webgl_error(InvalidOperation);
            return retval.set(NullValue());
        }
        handle_potential_webgl_error!(
            self.base,
            self.base.validate_ownership(sync),
            return retval.set(NullValue())
        );
        match pname {
            constants::OBJECT_TYPE | constants::SYNC_CONDITION | constants::SYNC_FLAGS => {
                let (sender, receiver) = webgl_channel().unwrap();
                self.base
                    .send_command(WebGLCommand::GetSyncParameter(sync.id(), pname, sender));
                retval.set(UInt32Value(receiver.recv().unwrap()))
            },
            constants::SYNC_STATUS => match sync.get_sync_status(pname, &self.base) {
                Some(status) => retval.set(UInt32Value(status)),
                None => retval.set(UInt32Value(constants::UNSIGNALED)),
            },
            _ => {
                self.base.webgl_error(InvalidEnum);
                retval.set(NullValue())
            },
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.14>
    fn DeleteSync(&self, sync: Option<&WebGLSync>) {
        if let Some(sync) = sync {
            handle_potential_webgl_error!(self.base, self.base.validate_ownership(sync), return);
            sync.delete(Operation::Infallible);
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.13>
    fn BindSampler(&self, unit: u32, sampler: Option<&WebGLSampler>) {
        if let Some(sampler) = sampler {
            handle_potential_webgl_error!(self.base, self.base.validate_ownership(sampler), return);

            if unit as usize >= self.samplers.len() {
                self.base.webgl_error(InvalidValue);
                return;
            }

            let result = sampler.bind(&self.base, unit);
            match result {
                Ok(_) => {
                    // Partii (perf): bindSampler doesn't move the draw-state generation; the unit's
                    // binding sequence number covers it (draw_state_cached).
                    if self.samplers[unit as usize]
                        .get()
                        .is_none_or(|bound| !std::ptr::eq(&*bound, sampler))
                    {
                        self.base.textures().note_unit_changed(unit);
                    }
                    self.samplers[unit as usize].set(Some(sampler))
                },
                Err(error) => self.base.webgl_error(error),
            }
        } else {
            if unit as usize >= self.samplers.len() {
                self.base.webgl_error(InvalidValue);
                return;
            }
            self.base
                .send_command(WebGLCommand::BindSampler(unit, None));
            if self.samplers[unit as usize].get().is_some() {
                self.base.textures().note_unit_changed(unit);
            }
            self.samplers[unit as usize].set(None);
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.17>
    fn BindVertexArray(&self, array: Option<&WebGLVertexArrayObject>) {
        self.base.bind_vertex_array_webgl2(array);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.13>
    fn SamplerParameteri(&self, sampler: &WebGLSampler, pname: u32, param: i32) {
        handle_potential_webgl_error!(self.base, self.base.validate_ownership(sampler), return);
        let param = WebGLSamplerValue::GLenum(param as u32);
        let result = sampler.set_parameter(&self.base, pname, param);
        if let Err(error) = result {
            self.base.webgl_error(error);
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.13>
    fn SamplerParameterf(&self, sampler: &WebGLSampler, pname: u32, param: f32) {
        handle_potential_webgl_error!(self.base, self.base.validate_ownership(sampler), return);
        let param = WebGLSamplerValue::Float(param);
        let result = sampler.set_parameter(&self.base, pname, param);
        if let Err(error) = result {
            self.base.webgl_error(error);
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.13>
    fn GetSamplerParameter(
        &self,
        _cx: &mut JSContext,
        sampler: &WebGLSampler,
        pname: u32,
        mut retval: MutableHandleValue,
    ) {
        handle_potential_webgl_error!(
            self.base,
            self.base.validate_ownership(sampler),
            return retval.set(NullValue())
        );
        match sampler.get_parameter(&self.base, pname) {
            Ok(value) => match value {
                WebGLSamplerValue::GLenum(value) => retval.set(UInt32Value(value)),
                WebGLSamplerValue::Float(value) => retval.set(DoubleValue(value as f64)),
            },
            Err(error) => {
                self.base.webgl_error(error);
                retval.set(NullValue())
            },
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.15>
    fn CreateTransformFeedback(
        &self,
        cx: &mut JSContext,
    ) -> Option<DomRoot<WebGLTransformFeedback>> {
        Some(WebGLTransformFeedback::new(cx, &self.base))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.15>
    fn DeleteTransformFeedback(&self, tf: Option<&WebGLTransformFeedback>) {
        if let Some(tf) = tf {
            handle_potential_webgl_error!(self.base, self.base.validate_ownership(tf), return);
            if tf.is_active() {
                self.base.webgl_error(InvalidOperation);
                return;
            }
            tf.delete(Operation::Infallible);
            // Deleting the bound object reverts the binding to the default object.
            if self
                .current_transform_feedback
                .get()
                .is_some_and(|current| &*current == tf)
            {
                self.switch_transform_feedback(None, false);
            }
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.15>
    fn IsTransformFeedback(&self, tf: Option<&WebGLTransformFeedback>) -> bool {
        match tf {
            Some(tf) => {
                // Returns false if the transform feedback was generated by a different
                // WebGL2RenderingContext than this one.
                if self.base.validate_ownership(tf).is_err() {
                    return false;
                }

                // Returns false if the transform feedback's invalidated flag is set.
                if !tf.is_valid() {
                    return false;
                }

                // Return true if the passed WebGLTransformFeedback is valid and false otherwise.
                handle_potential_webgl_error!(
                    self.base,
                    self.base.validate_ownership(tf),
                    return false
                );
                let (sender, receiver) = webgl_channel().unwrap();
                self.base
                    .send_command(WebGLCommand::IsTransformFeedback(tf.id(), sender));
                receiver.recv().unwrap()
            },
            // Returns false if `transformFeedback` is null.
            None => false,
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.15>
    fn BindTransformFeedback(&self, target: u32, tf: Option<&WebGLTransformFeedback>) {
        if target != constants::TRANSFORM_FEEDBACK {
            self.base.webgl_error(InvalidEnum);
            return;
        }
        match tf {
            Some(transform_feedback) => {
                handle_potential_webgl_error!(
                    self.base,
                    self.base.validate_ownership(transform_feedback),
                    return
                );
                if !transform_feedback.is_valid() {
                    self.base.webgl_error(InvalidOperation);
                    return;
                }
                if self.with_current_transform_feedback(|tf| tf.is_active_unpaused()) {
                    self.base.webgl_error(InvalidOperation);
                    return;
                }
                transform_feedback.bind(&self.base, target);
                self.switch_transform_feedback(Some(transform_feedback), true);
            },
            None => {
                if self.with_current_transform_feedback(|tf| tf.is_active_unpaused()) {
                    self.base.webgl_error(InvalidOperation);
                    return;
                }
                self.base
                    .send_command(WebGLCommand::BindTransformFeedback(target, 0));
                self.switch_transform_feedback(None, true);
            },
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.15>
    #[expect(non_snake_case)]
    fn BeginTransformFeedback(&self, primitiveMode: u32) {
        match primitiveMode {
            constants::POINTS | constants::LINES | constants::TRIANGLES => {},
            _ => {
                self.base.webgl_error(InvalidEnum);
                return;
            },
        };
        if self.with_current_transform_feedback(|tf| tf.is_active()) {
            self.base.webgl_error(InvalidOperation);
            return;
        };
        let program = match self.base.current_program() {
            Some(program) => program,
            None => {
                self.base.webgl_error(InvalidOperation);
                return;
            },
        };
        if !program.is_linked() || program.transform_feedback_varyings_length() == 0 {
            self.base.webgl_error(InvalidOperation);
            return;
        };
        // Every buffer the program records into must be bound (ES 3.0 2.15.2).
        let buffers_needed =
            if program.transform_feedback_buffer_mode() == constants::INTERLEAVED_ATTRIBS as i32 {
                1
            } else {
                program.transform_feedback_varyings_length() as usize
            };
        if self
            .indexed_transform_feedback_buffer_bindings
            .iter()
            .take(buffers_needed)
            .any(|binding| binding.buffer.get().is_none())
        {
            self.base.webgl_error(InvalidOperation);
            return;
        }
        // WebGL 2.0 5.36: nor may one buffer be bound to two of the object's binding points,
        // including ones the program does not record into.
        let mut buffers = Vec::new();
        for buffer in self
            .indexed_transform_feedback_buffer_bindings
            .iter()
            .filter_map(|binding| binding.buffer.get())
        {
            if buffers.contains(&buffer.id()) {
                self.base.webgl_error(InvalidOperation);
                return;
            }
            buffers.push(buffer.id());
        }
        self.with_current_transform_feedback(|tf| tf.begin(&self.base, primitiveMode));
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.15>
    fn EndTransformFeedback(&self) {
        self.with_current_transform_feedback(|tf| {
            if !tf.is_active() {
                self.base.webgl_error(InvalidOperation);
                return;
            }
            tf.end(&self.base);
        })
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.15>
    fn ResumeTransformFeedback(&self) {
        self.with_current_transform_feedback(|tf| {
            if !tf.is_active() || !tf.is_paused() {
                self.base.webgl_error(InvalidOperation);
                return;
            }
            tf.resume(&self.base);
        })
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.15>
    fn PauseTransformFeedback(&self) {
        self.with_current_transform_feedback(|tf| {
            if !tf.is_active() || tf.is_paused() {
                self.base.webgl_error(InvalidOperation);
                return;
            }
            tf.pause(&self.base);
        })
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.15>
    #[expect(non_snake_case)]
    fn TransformFeedbackVaryings(
        &self,
        program: &WebGLProgram,
        varyings: Vec<DOMString>,
        bufferMode: u32,
    ) {
        // Spec: If program was generated by a different WebGL2RenderingContext
        // than this one, generates an INVALID_OPERATION error.
        if let Err(error) = self.base.validate_ownership(program) {
            self.base.webgl_error(error);
            return;
        }
        handle_potential_webgl_error!(self.base, program.validate(), return);
        let strs = varyings
            .iter()
            .map(|name| String::from(name.to_owned()))
            .collect::<Vec<String>>();
        match bufferMode {
            constants::INTERLEAVED_ATTRIBS => {
                self.base
                    .send_command(WebGLCommand::TransformFeedbackVaryings(
                        program.id(),
                        strs,
                        bufferMode,
                    ));
            },
            constants::SEPARATE_ATTRIBS => {
                let max_tf_sp_att =
                    self.base.limits().max_transform_feedback_separate_attribs as usize;
                if strs.len() >= max_tf_sp_att {
                    self.base.webgl_error(InvalidValue);
                    return;
                }
                self.base
                    .send_command(WebGLCommand::TransformFeedbackVaryings(
                        program.id(),
                        strs,
                        bufferMode,
                    ));
            },
            _ => self.base.webgl_error(InvalidEnum),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.15>
    fn GetTransformFeedbackVarying(
        &self,
        cx: &mut JSContext,
        program: &WebGLProgram,
        index: u32,
    ) -> Option<DomRoot<WebGLActiveInfo>> {
        // Spec: If program was generated by a different WebGL2RenderingContext,
        // generate INVALID_OPERATION and return null.
        if let Err(error) = self.base.validate_ownership(program) {
            self.base.webgl_error(error);
            return None;
        }

        handle_potential_webgl_error!(self.base, program.validate(), return None);

        if index >= program.transform_feedback_varyings_length() as u32 {
            self.base.webgl_error(InvalidValue);
            return None;
        }

        let (sender, receiver) = webgl_channel().unwrap();
        self.base
            .send_command(WebGLCommand::GetTransformFeedbackVarying(
                program.id(),
                index,
                sender,
            ));
        let (size, ty, name) = receiver.recv().unwrap();
        Some(WebGLActiveInfo::new(
            cx,
            &self.base.global(),
            size,
            ty,
            DOMString::from(name),
        ))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.16>
    fn BindBufferBase(&self, target: u32, index: u32, buffer: Option<&WebGLBuffer>) {
        let (generic_slot, indexed_bindings) = match target {
            constants::TRANSFORM_FEEDBACK_BUFFER => (
                &self.bound_transform_feedback_buffer,
                &self.indexed_transform_feedback_buffer_bindings,
            ),
            constants::UNIFORM_BUFFER => (
                &self.bound_uniform_buffer,
                &self.indexed_uniform_buffer_bindings,
            ),
            _ => return self.base.webgl_error(InvalidEnum),
        };
        if target == constants::TRANSFORM_FEEDBACK_BUFFER &&
            self.with_current_transform_feedback(|tf| tf.is_active())
        {
            return self.base.webgl_error(InvalidOperation);
        }
        let indexed_binding = match indexed_bindings.get(index as usize) {
            Some(slot) => slot,
            None => return self.base.webgl_error(InvalidValue),
        };

        if let Some(buffer) = buffer {
            handle_potential_webgl_error!(self.base, self.base.validate_ownership(buffer), return);

            if buffer.is_marked_for_deletion() {
                return self.base.webgl_error(InvalidOperation);
            }
            handle_potential_webgl_error!(self.base, buffer.set_target_maybe(target), return);

            // for both the generic and the indexed bindings
            buffer.increment_attached_counter();
            buffer.increment_attached_counter();
        }

        self.base.send_command(WebGLCommand::BindBufferBase(
            target,
            index,
            buffer.map(|b| b.id()),
        ));

        for slot in &[generic_slot, &indexed_binding.buffer] {
            if let Some(old) = slot.get() {
                old.decrement_attached_counter(Operation::Infallible);
            }
            slot.set(buffer);
        }
        indexed_binding.start.set(0);
        indexed_binding.size.set(0);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.16>
    fn BindBufferRange(
        &self,
        target: u32,
        index: u32,
        buffer: Option<&WebGLBuffer>,
        offset: i64,
        size: i64,
    ) {
        let (generic_slot, indexed_bindings) = match target {
            constants::TRANSFORM_FEEDBACK_BUFFER => (
                &self.bound_transform_feedback_buffer,
                &self.indexed_transform_feedback_buffer_bindings,
            ),
            constants::UNIFORM_BUFFER => (
                &self.bound_uniform_buffer,
                &self.indexed_uniform_buffer_bindings,
            ),
            _ => return self.base.webgl_error(InvalidEnum),
        };
        if target == constants::TRANSFORM_FEEDBACK_BUFFER &&
            self.with_current_transform_feedback(|tf| tf.is_active())
        {
            return self.base.webgl_error(InvalidOperation);
        }
        let indexed_binding = match indexed_bindings.get(index as usize) {
            Some(slot) => slot,
            None => return self.base.webgl_error(InvalidValue),
        };

        if offset < 0 || size < 0 {
            return self.base.webgl_error(InvalidValue);
        }
        if buffer.is_some() && size == 0 {
            return self.base.webgl_error(InvalidValue);
        }

        match target {
            constants::TRANSFORM_FEEDBACK_BUFFER => {
                if size % 4 != 0 && offset % 4 != 0 {
                    return self.base.webgl_error(InvalidValue);
                }
            },
            constants::UNIFORM_BUFFER => {
                let offset_alignment = self.base.limits().uniform_buffer_offset_alignment;
                if offset % offset_alignment as i64 != 0 {
                    return self.base.webgl_error(InvalidValue);
                }
            },
            _ => unreachable!(),
        }

        if let Some(buffer) = buffer {
            handle_potential_webgl_error!(self.base, self.base.validate_ownership(buffer), return);

            if buffer.is_marked_for_deletion() {
                return self.base.webgl_error(InvalidOperation);
            }
            handle_potential_webgl_error!(self.base, buffer.set_target_maybe(target), return);

            // for both the generic and the indexed bindings
            buffer.increment_attached_counter();
            buffer.increment_attached_counter();
        }

        self.base.send_command(WebGLCommand::BindBufferRange(
            target,
            index,
            buffer.map(|b| b.id()),
            offset,
            size,
        ));

        for slot in &[generic_slot, &indexed_binding.buffer] {
            if let Some(old) = slot.get() {
                old.decrement_attached_counter(Operation::Infallible);
            }
            slot.set(buffer);
        }
        indexed_binding.start.set(offset);
        indexed_binding.size.set(size);
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.16>
    fn GetUniformIndices(&self, program: &WebGLProgram, names: Vec<DOMString>) -> Option<Vec<u32>> {
        handle_potential_webgl_error!(
            self.base,
            self.base.validate_ownership(program),
            return None
        );
        let indices = handle_potential_webgl_error!(
            self.base,
            program.get_uniform_indices(names),
            return None
        );
        Some(indices)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.16>
    fn GetActiveUniforms(
        &self,
        cx: &mut JSContext,
        program: &WebGLProgram,
        indices: Vec<u32>,
        pname: u32,
        mut rval: MutableHandleValue,
    ) {
        handle_potential_webgl_error!(
            self.base,
            self.base.validate_ownership(program),
            return rval.set(NullValue())
        );
        let values = handle_potential_webgl_error!(
            self.base,
            program.get_active_uniforms(indices, pname),
            return rval.set(NullValue())
        );

        match pname {
            constants::UNIFORM_SIZE |
            constants::UNIFORM_TYPE |
            constants::UNIFORM_BLOCK_INDEX |
            constants::UNIFORM_OFFSET |
            constants::UNIFORM_ARRAY_STRIDE |
            constants::UNIFORM_MATRIX_STRIDE => {
                values.to_jsval(cx, rval);
            },
            constants::UNIFORM_IS_ROW_MAJOR => {
                let values = values.iter().map(|&v| v != 0).collect::<Vec<_>>();
                values.to_jsval(cx, rval);
            },
            _ => unreachable!(),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.16>
    fn GetUniformBlockIndex(&self, program: &WebGLProgram, block_name: DOMString) -> u32 {
        handle_potential_webgl_error!(
            self.base,
            self.base.validate_ownership(program),
            return constants::INVALID_INDEX
        );
        handle_potential_webgl_error!(
            self.base,
            program.get_uniform_block_index(block_name),
            constants::INVALID_INDEX
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.16>
    fn GetActiveUniformBlockParameter(
        &self,
        cx: &mut JSContext,
        program: &WebGLProgram,
        block_index: u32,
        pname: u32,
        mut retval: MutableHandleValue,
    ) {
        handle_potential_webgl_error!(
            self.base,
            self.base.validate_ownership(program),
            return retval.set(NullValue())
        );
        let values = handle_potential_webgl_error!(
            self.base,
            program.get_active_uniform_block_parameter(block_index, pname),
            return retval.set(NullValue())
        );
        match pname {
            constants::UNIFORM_BLOCK_BINDING |
            constants::UNIFORM_BLOCK_DATA_SIZE |
            constants::UNIFORM_BLOCK_ACTIVE_UNIFORMS => {
                assert!(values.len() == 1);
                retval.set(UInt32Value(values[0] as u32))
            },
            constants::UNIFORM_BLOCK_ACTIVE_UNIFORM_INDICES => {
                let values = values.iter().map(|&v| v as u32).collect::<Vec<_>>();
                rooted!(&in(cx) let mut result = ptr::null_mut::<JSObject>());
                create_buffer_source::<Uint32>(cx, &values, result.handle_mut()).unwrap();
                retval.set(ObjectValue(result.get()))
            },
            constants::UNIFORM_BLOCK_REFERENCED_BY_VERTEX_SHADER |
            constants::UNIFORM_BLOCK_REFERENCED_BY_FRAGMENT_SHADER => {
                assert!(values.len() == 1);
                retval.set(BooleanValue(values[0] != 0))
            },
            _ => unreachable!(),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.16>
    fn GetActiveUniformBlockName(
        &self,
        program: &WebGLProgram,
        block_index: u32,
    ) -> Option<DOMString> {
        handle_potential_webgl_error!(
            self.base,
            self.base.validate_ownership(program),
            return None
        );
        let name = handle_potential_webgl_error!(
            self.base,
            program.get_active_uniform_block_name(block_index),
            return None
        );
        Some(DOMString::from(name))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.16>
    fn UniformBlockBinding(&self, program: &WebGLProgram, block_index: u32, block_binding: u32) {
        handle_potential_webgl_error!(self.base, self.base.validate_ownership(program), return);

        if block_binding >= self.base.limits().max_uniform_buffer_bindings {
            return self.base.webgl_error(InvalidValue);
        }

        handle_potential_webgl_error!(
            self.base,
            program.bind_uniform_block(block_index, block_binding)
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.11>
    fn ClearBufferfv(
        &self,
        buffer: u32,
        draw_buffer: i32,
        values: Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u32,
    ) {
        let array = match values {
            Float32ArrayOrUnrestrictedFloatSequence::Float32Array(v) => typed_array_to_vec(&*v),
            Float32ArrayOrUnrestrictedFloatSequence::UnrestrictedFloatSequence(v) => v,
        };
        self.clear_buffer::<f32>(
            buffer,
            draw_buffer,
            &[constants::COLOR, constants::DEPTH],
            &[0, 1],
            src_offset,
            array,
            WebGLCommand::ClearBufferfv,
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.11>
    fn ClearBufferiv(
        &self,
        buffer: u32,
        draw_buffer: i32,
        values: Int32ArrayOrLongSequence,
        src_offset: u32,
    ) {
        let array = match values {
            Int32ArrayOrLongSequence::Int32Array(v) => typed_array_to_vec(&*v),
            Int32ArrayOrLongSequence::LongSequence(v) => v,
        };
        self.clear_buffer::<i32>(
            buffer,
            draw_buffer,
            &[constants::COLOR, constants::STENCIL],
            &[2],
            src_offset,
            array,
            WebGLCommand::ClearBufferiv,
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.11>
    fn ClearBufferuiv(
        &self,
        buffer: u32,
        draw_buffer: i32,
        values: Uint32ArrayOrUnsignedLongSequence,
        src_offset: u32,
    ) {
        let array = match values {
            Uint32ArrayOrUnsignedLongSequence::Uint32Array(v) => typed_array_to_vec(&*v),
            Uint32ArrayOrUnsignedLongSequence::UnsignedLongSequence(v) => v,
        };
        self.clear_buffer::<u32>(
            buffer,
            draw_buffer,
            &[constants::COLOR],
            &[3],
            src_offset,
            array,
            WebGLCommand::ClearBufferuiv,
        )
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.11>
    fn ClearBufferfi(&self, buffer: u32, draw_buffer: i32, depth: f32, stencil: i32) {
        if buffer != constants::DEPTH_STENCIL {
            return self.base.webgl_error(InvalidEnum);
        }

        handle_potential_webgl_error!(
            self.base,
            self.clearbuffer_array_size(buffer, draw_buffer),
            return
        );
        // As in clear_buffer: initialize the attachments before this clear, not after it.
        if let Some(fb) = self.base.get_draw_framebuffer_slot().get() &&
            let CompleteForRendering::Incomplete = fb.check_status_for_rendering()
        {
            return self.base.webgl_error(InvalidFramebufferOperation);
        }

        self.base.send_command(WebGLCommand::ClearBufferfi(
            buffer,
            draw_buffer,
            depth,
            stencil,
        ));
        self.mark_as_dirty();
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.4>
    fn InvalidateFramebuffer(&self, target: u32, attachments: Vec<u32>) {
        if !self.valid_fb_attachment_values(target, &attachments) {
            return;
        }

        self.base
            .send_command(WebGLCommand::InvalidateFramebuffer(target, attachments))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.4>
    fn InvalidateSubFramebuffer(
        &self,
        target: u32,
        attachments: Vec<u32>,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) {
        if !self.valid_fb_attachment_values(target, &attachments) {
            return;
        }

        if width < 0 || height < 0 {
            return self.base.webgl_error(InvalidValue);
        }

        self.base
            .send_command(WebGLCommand::InvalidateSubFramebuffer(
                target,
                attachments,
                x,
                y,
                width,
                height,
            ))
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.4>
    fn FramebufferTextureLayer(
        &self,
        target: u32,
        attachment: u32,
        texture: Option<&WebGLTexture>,
        level: i32,
        layer: i32,
    ) {
        if let Some(tex) = texture {
            handle_potential_webgl_error!(self.base, self.base.validate_ownership(tex), return);
        }

        let fb_slot = match target {
            constants::FRAMEBUFFER | constants::DRAW_FRAMEBUFFER => {
                self.base.get_draw_framebuffer_slot()
            },
            constants::READ_FRAMEBUFFER => self.base.get_read_framebuffer_slot(),
            _ => return self.base.webgl_error(InvalidEnum),
        };

        match fb_slot.get() {
            Some(fb) => match attachment {
                constants::DEPTH_STENCIL_ATTACHMENT => {
                    handle_potential_webgl_error!(
                        self.base,
                        fb.texture_layer(constants::DEPTH_ATTACHMENT, texture, level, layer),
                        return
                    );
                    handle_potential_webgl_error!(
                        self.base,
                        fb.texture_layer(constants::STENCIL_ATTACHMENT, texture, level, layer)
                    );
                },
                _ => handle_potential_webgl_error!(
                    self.base,
                    fb.texture_layer(attachment, texture, level, layer)
                ),
            },
            None => self.base.webgl_error(InvalidOperation),
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.5>
    fn GetInternalformatParameter(
        &self,
        cx: &mut JSContext,
        target: u32,
        internal_format: u32,
        pname: u32,
        mut retval: MutableHandleValue,
    ) {
        if target != constants::RENDERBUFFER {
            self.base.webgl_error(InvalidEnum);
            return retval.set(NullValue());
        }
        // An internalformat that isn't one of ES 3.0's sized or base formats is INVALID_ENUM.
        if !matches!(
            internal_format,
            constants::R8 |
                constants::R8_SNORM |
                constants::RG8 |
                constants::RG8_SNORM |
                constants::RGB8 |
                constants::RGB8_SNORM |
                constants::RGB565 |
                constants::RGBA4 |
                constants::RGB5_A1 |
                constants::RGBA8 |
                constants::RGBA8_SNORM |
                constants::RGB10_A2 |
                constants::RGB10_A2UI |
                constants::SRGB8 |
                constants::SRGB8_ALPHA8 |
                constants::R16F |
                constants::RG16F |
                constants::RGB16F |
                constants::RGBA16F |
                constants::R32F |
                constants::RG32F |
                constants::RGB32F |
                constants::RGBA32F |
                constants::R11F_G11F_B10F |
                constants::RGB9_E5 |
                constants::R8I |
                constants::R8UI |
                constants::R16I |
                constants::R16UI |
                constants::R32I |
                constants::R32UI |
                constants::RG8I |
                constants::RG8UI |
                constants::RG16I |
                constants::RG16UI |
                constants::RG32I |
                constants::RG32UI |
                constants::RGB8I |
                constants::RGB8UI |
                constants::RGB16I |
                constants::RGB16UI |
                constants::RGB32I |
                constants::RGB32UI |
                constants::RGBA8I |
                constants::RGBA8UI |
                constants::RGBA16I |
                constants::RGBA16UI |
                constants::RGBA32I |
                constants::RGBA32UI |
                constants::RGB |
                constants::RGBA |
                constants::DEPTH_STENCIL |
                constants::DEPTH_COMPONENT16 |
                constants::DEPTH_COMPONENT24 |
                constants::DEPTH_COMPONENT32F |
                constants::DEPTH24_STENCIL8 |
                constants::DEPTH32F_STENCIL8 |
                constants::STENCIL_INDEX8
        ) {
            self.base.webgl_error(InvalidEnum);
            return retval.set(NullValue());
        }

        match handle_potential_webgl_error!(
            self.base,
            InternalFormatParameter::from_u32(pname),
            return retval.set(NullValue())
        ) {
            InternalFormatParameter::IntVec(param) => {
                let (sender, receiver) = webgl_channel().unwrap();
                self.base
                    .send_command(WebGLCommand::GetInternalFormatIntVec(
                        target,
                        internal_format,
                        param,
                        sender,
                    ));

                rooted!(&in(cx) let mut rval = ptr::null_mut::<JSObject>());
                create_buffer_source::<Int32>(cx, &receiver.recv().unwrap(), rval.handle_mut())
                    .unwrap();
                retval.set(ObjectValue(rval.get()))
            },
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.5>
    fn RenderbufferStorageMultisample(
        &self,
        target: u32,
        samples: i32,
        internal_format: u32,
        width: i32,
        height: i32,
    ) {
        // WebGL 1's DEPTH_STENCIL stays single-sampled (WebGL 2.0 5.36).
        if internal_format == constants::DEPTH_STENCIL && samples > 0 {
            return self.base.webgl_error(InvalidOperation);
        }
        self.base
            .renderbuffer_storage(target, samples, internal_format, width, height)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.4>
    fn ReadBuffer(&self, src: u32) {
        match src {
            constants::BACK | constants::NONE => {},
            _ if self.base.valid_color_attachment_enum(src) => {},
            // COLOR_ATTACHMENTi past MAX_COLOR_ATTACHMENTS (ES 3.0 4.3.1).
            constants::COLOR_ATTACHMENT0..=constants::COLOR_ATTACHMENT15 => {
                return self.base.webgl_error(InvalidOperation);
            },
            _ => return self.base.webgl_error(InvalidEnum),
        }

        if let Some(fb) = self.base.get_read_framebuffer_slot().get() {
            handle_potential_webgl_error!(self.base, fb.set_read_buffer(src))
        } else {
            match src {
                constants::NONE | constants::BACK => {},
                _ => return self.base.webgl_error(InvalidOperation),
            }

            self.default_fb_readbuffer.set(src);
            self.base.send_command(WebGLCommand::ReadBuffer(src));
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.11>
    fn DrawBuffers(&self, buffers: Vec<u32>) {
        if let Some(fb) = self.base.get_draw_framebuffer_slot().get() {
            handle_potential_webgl_error!(self.base, fb.set_draw_buffers(buffers))
        } else {
            if buffers.len() != 1 {
                return self.base.webgl_error(InvalidOperation);
            }

            match buffers[0] {
                constants::NONE | constants::BACK => {},
                _ => return self.base.webgl_error(InvalidOperation),
            }

            self.default_fb_drawbuffer.set(buffers[0]);
            self.base.send_command(WebGLCommand::DrawBuffers(buffers));
        }
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.6>
    fn TexStorage2D(
        &self,
        target: u32,
        levels: i32,
        internal_format: u32,
        width: i32,
        height: i32,
    ) {
        self.tex_storage(2, target, levels, internal_format, width, height, 1)
    }

    /// <https://www.khronos.org/registry/webgl/specs/latest/2.0/#4.7.6>
    fn TexStorage3D(
        &self,
        target: u32,
        levels: i32,
        internal_format: u32,
        width: i32,
        height: i32,
        depth: i32,
    ) {
        self.tex_storage(3, target, levels, internal_format, width, height, depth)
    }

    /// <https://immersive-web.github.io/webxr/#dom-webglrenderingcontextbase-makexrcompatible>
    #[cfg(feature = "webxr")]
    fn MakeXRCompatible(&self, cx: &mut js::context::JSContext) -> RootedPromise {
        // XXXManishearth Fill in with compatibility checks when rust-webxr supports this
        Promise::new_resolved(cx, &self.global(), ())
    }
}

impl WebGL2RenderingContextHelpers for WebGL2RenderingContext {
    fn is_webgl2_enabled(cx: &mut js::context::JSContext, global: HandleObject) -> bool {
        Self::is_webgl2_enabled(cx, global)
    }
}

/// One axis of blitFramebuffer's clipping (GL ES 3.0 4.3.3, as WebGL 2's conformance tests
/// read it): a destination pixel is written only if it lies inside the draw buffer and the
/// centre of its source pixel, under the original rectangles' mapping, lies inside the read
/// buffer. Returns rectangles that stay inside both buffers and write exactly those pixels, or
/// None when no pixel is written. Drivers clip with integer steps that change the scale between
/// steps (Mesa writes one pixel too many or too few at a clipped edge), so they are given
/// rectangles that need no clipping. Integer scales (a 1:1 copy) keep their exact mapping;
/// other scales round the source edges to whole pixels.
fn clip_blit_axis(
    s0: i32,
    s1: i32,
    d0: i32,
    d1: i32,
    src_size: i32,
    dst_size: i32,
) -> Option<(i32, i32, i32, i32)> {
    let inside = |a: i32, b: i32, size: i32| a.min(b) >= 0 && a.max(b) <= size;
    if inside(s0, s1, src_size) && inside(d0, d1, dst_size) {
        return Some((s0, s1, d0, d1));
    }
    if s0 == s1 || d0 == d1 || src_size <= 0 || dst_size <= 0 {
        return None;
    }
    let (s0f, d0f) = (s0 as f64, d0 as f64);
    let scale = (s1 as f64 - s0f) / (d1 as f64 - d0f);
    let src_at = |x: f64| s0f + (x - d0f) * scale;
    let written = |x: i32| {
        let s = src_at(x as f64 + 0.5);
        s >= 0.0 && s < src_size as f64
    };
    // The written pixels are one run: the mapping is monotonic.
    let (lo, hi) = (d0.min(d1).max(0), d0.max(d1).min(dst_size));
    let mut first = lo;
    while first < hi && !written(first) {
        first += 1;
    }
    let mut end = hi;
    while end > first && !written(end - 1) {
        end -= 1;
    }
    if first >= end {
        return None;
    }
    let (nd0, nd1) = if d0 < d1 { (first, end) } else { (end, first) };
    let src_edge = |x: i32| src_at(x as f64).round().clamp(0.0, src_size as f64) as i32;
    let (mut ns0, mut ns1) = (src_edge(nd0), src_edge(nd1));
    if ns0 == ns1 {
        // Keep the one source pixel the written centres sample.
        let centre = src_at((first + end) as f64 / 2.0)
            .floor()
            .clamp(0.0, (src_size - 1) as f64) as i32;
        (ns0, ns1) = if s0 < s1 {
            (centre, centre + 1)
        } else {
            (centre + 1, centre)
        };
    }
    Some((ns0, ns1, nd0, nd1))
}
