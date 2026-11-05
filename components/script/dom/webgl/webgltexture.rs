/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://www.khronos.org/registry/webgl/specs/latest/1.0/webgl.idl

use std::cell::Cell;
use std::cmp;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::{DomObject as _, reflect_dom_object};
use script_bindings::weakref::WeakRef;
use servo_canvas_traits::webgl::{
    TexDataType, TexFormat, TexParameter, TexParameterBool, TexParameterInt, WebGLCommand,
    WebGLError, WebGLObjectNameKind, WebGLResult, WebGLTextureId, WebGLVersion,
};

use crate::dom::bindings::codegen::Bindings::EXTTextureFilterAnisotropicBinding::EXTTextureFilterAnisotropicConstants;
use crate::dom::bindings::codegen::Bindings::WebGL2RenderingContextBinding::WebGL2RenderingContextConstants as constants;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::webgl::extensions::oestexturefloatlinear::OESTextureFloatLinear;
use crate::dom::webgl::validations::types::TexImageTarget;
use crate::dom::webgl::webglframebuffer::WebGLFramebuffer;
use crate::dom::webgl::webglobject::WebGLObject;
use crate::dom::webgl::webglrenderingcontext::{Operation, WebGLRenderingContext};
use crate::dom::webglrenderingcontext::capture_webgl_backtrace;
#[cfg(feature = "webxr")]
use crate::dom::xrsession::XRSession;

pub(crate) enum TexParameterValue {
    Float(f32),
    Int(i32),
    Bool(bool),
}

// Textures generated for WebXR are owned by the WebXR device, not by the WebGL thread
// so the GL texture should not be deleted when the texture is garbage collected.
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
#[derive(JSTraceable, MallocSizeOf)]
enum WebGLTextureOwner {
    WebGL,
    #[cfg(feature = "webxr")]
    WebXR(WeakRef<XRSession>),
}

const MAX_LEVEL_COUNT: usize = 31;
const MAX_FACE_COUNT: usize = 6;

#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
#[derive(JSTraceable, MallocSizeOf)]
struct DroppableWebGLTexture {
    context: WeakRef<WebGLRenderingContext>,
    #[no_trace]
    id: WebGLTextureId,
    is_deleted: Cell<bool>,
    owner: WebGLTextureOwner,
}

impl DroppableWebGLTexture {
    fn send_with_fallibility(&self, command: WebGLCommand, fallibility: Operation) {
        if let Some(root) = self.context.root() {
            let result = root.sender().send(command, capture_webgl_backtrace());
            if matches!(fallibility, Operation::Infallible) {
                result.expect("Operation failed");
            }
        }
    }

    fn delete(&self, operation_fallibility: Operation) {
        if !self.is_deleted.get() {
            self.is_deleted.set(true);

            /*
            If a texture object is deleted while its image is attached to one or more attachment
            points in a currently bound framebuffer, then it is as if FramebufferTexture had been
            called, with a texture of zero, for each attachment point to which this im-age was
            attached in that framebuffer. In other words, this texture image is firstdetached from
            all attachment points in a currently bound framebuffer.
            - GLES 3.0, 4.4.2.3, "Attaching Texture Images to a Framebuffer"
            */
            if let Some(context) = self.context.root() {
                if let Some(fb) = context.get_draw_framebuffer_slot().get() {
                    let _ = fb.detach_texture(self.id);
                }
                if let Some(fb) = context.get_read_framebuffer_slot().get() {
                    let _ = fb.detach_texture(self.id);
                }
            }

            // We don't delete textures owned by WebXR
            #[cfg(feature = "webxr")]
            if let WebGLTextureOwner::WebXR(_) = self.owner {
                return;
            }

            self.send_with_fallibility(WebGLCommand::DeleteTexture(self.id), operation_fallibility);
        }
    }
}

impl Drop for DroppableWebGLTexture {
    fn drop(&mut self) {
        self.delete(Operation::Fallible);
    }
}

#[dom_struct]
pub(crate) struct WebGLTexture {
    webgl_object: WebGLObject,
    /// The target to which this texture was bound the first time
    target: Cell<Option<u32>>,
    /// Stores information about mipmap levels and cubemap faces, at `level * face_count + face`.
    /// Partii (perf): grown as images are defined, rather than an inline array of every possible
    /// level and face (31 x 6, ~4.5 KB built and copied per createTexture).
    #[ignore_malloc_size_of = "Arrays are cumbersome"]
    image_info_array: DomRefCell<Vec<Option<ImageInfo>>>,
    /// Face count can only be 1 or 6
    face_count: Cell<u8>,
    // Store information for min and mag filters
    min_filter: Cell<u32>,
    mag_filter: Cell<u32>,
    /// Framebuffers this texture is attached to (pruned as they let go of it), whose
    /// completeness changes when it is re-specified.
    attached_framebuffers: DomRefCell<Vec<Dom<WebGLFramebuffer>>>,
    /// Number of immutable levels.
    immutable_levels: Cell<Option<u32>>,
    /// TEXTURE_COMPARE_MODE, for the draw-time sampler type check.
    compare_mode: Cell<u32>,
    /// TEXTURE_BASE_LEVEL and TEXTURE_MAX_LEVEL (WebGL 2), for the draw-time feedback-loop check.
    base_level: Cell<u32>,
    max_level: Cell<u32>,
    droppable: DroppableWebGLTexture,
}

impl WebGLTexture {
    fn new_inherited(
        context: &WebGLRenderingContext,
        id: WebGLTextureId,
        #[cfg(feature = "webxr")] owner: Option<&XRSession>,
    ) -> Self {
        Self {
            webgl_object: WebGLObject::new_inherited(context),
            target: Cell::new(None),
            immutable_levels: Cell::new(None),
            compare_mode: Cell::new(constants::NONE),
            base_level: Cell::new(0),
            max_level: Cell::new(1000),
            face_count: Cell::new(0),
            min_filter: Cell::new(constants::NEAREST_MIPMAP_LINEAR),
            mag_filter: Cell::new(constants::LINEAR),
            image_info_array: DomRefCell::new(Vec::new()),
            attached_framebuffers: Default::default(),
            droppable: DroppableWebGLTexture {
                context: WeakRef::new(context),
                id,
                is_deleted: Cell::new(false),
                #[cfg(feature = "webxr")]
                owner: owner
                    .map(|session| WebGLTextureOwner::WebXR(WeakRef::new(session)))
                    .unwrap_or(WebGLTextureOwner::WebGL),
                #[cfg(not(feature = "webxr"))]
                owner: WebGLTextureOwner::WebGL,
            },
        }
    }

    pub(crate) fn maybe_new(
        cx: &mut JSContext,
        context: &WebGLRenderingContext,
    ) -> Option<DomRoot<Self>> {
        let id = context.take_object_name(WebGLObjectNameKind::Texture)?;
        Some(WebGLTexture::new(cx, context, WebGLTextureId::new(id)))
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        context: &WebGLRenderingContext,
        id: WebGLTextureId,
    ) -> DomRoot<Self> {
        reflect_dom_object(
            cx,
            Box::new(WebGLTexture::new_inherited(
                context,
                id,
                #[cfg(feature = "webxr")]
                None,
            )),
            &*context.global(),
        )
    }

    #[cfg(feature = "webxr")]
    pub(crate) fn new_webxr(
        cx: &mut JSContext,
        context: &WebGLRenderingContext,
        id: WebGLTextureId,
        session: &XRSession,
    ) -> DomRoot<Self> {
        reflect_dom_object(
            cx,
            Box::new(WebGLTexture::new_inherited(context, id, Some(session))),
            &*context.global(),
        )
    }
}

impl WebGLTexture {
    pub(crate) fn id(&self) -> WebGLTextureId {
        self.droppable.id
    }

    // NB: Only valid texture targets come here
    pub(crate) fn bind(&self, target: u32) -> WebGLResult<()> {
        if self.is_invalid() {
            return Err(WebGLError::InvalidOperation);
        }

        if let Some(previous_target) = self.target.get() {
            if target != previous_target {
                return Err(WebGLError::InvalidOperation);
            }
        } else {
            // This is the first time binding
            let face_count = match target {
                constants::TEXTURE_2D | constants::TEXTURE_2D_ARRAY | constants::TEXTURE_3D => 1,
                constants::TEXTURE_CUBE_MAP => 6,
                _ => return Err(WebGLError::InvalidEnum),
            };
            self.face_count.set(face_count);
            self.target.set(Some(target));
        }

        self.upcast()
            .send_command(WebGLCommand::BindTexture(target, Some(self.id())));

        Ok(())
    }

    #[expect(clippy::too_many_arguments)]
    pub(crate) fn initialize(
        &self,
        target: TexImageTarget,
        width: u32,
        height: u32,
        depth: u32,
        internal_format: TexFormat,
        level: u32,
        data_type: Option<TexDataType>,
    ) -> WebGLResult<()> {
        let image_info = ImageInfo {
            width,
            height,
            depth,
            internal_format,
            data_type,
        };

        let face_index = self.face_index_for_target(&target);
        self.set_image_infos_at_level_and_face(level, face_index, image_info);

        self.update_attached_framebuffers();

        self.update_size();

        Ok(())
    }

    pub(crate) fn generate_mipmap(&self) -> WebGLResult<()> {
        let target = match self.target.get() {
            Some(target) => target,
            None => {
                error!("Cannot generate mipmap on texture that has no target!");
                return Err(WebGLError::InvalidOperation);
            },
        };

        let base_image_info = self.base_image_info().ok_or(WebGLError::InvalidOperation)?;

        let is_cubic = target == constants::TEXTURE_CUBE_MAP;
        if is_cubic && !self.is_cube_complete() {
            return Err(WebGLError::InvalidOperation);
        }

        let context = self.upcast().context().ok_or(WebGLError::ContextLost)?;
        match context.webgl_version() {
            // WebGL 1 needs power-of-two sizes.
            WebGLVersion::WebGL1 => {
                if !base_image_info.is_power_of_two() {
                    return Err(WebGLError::InvalidOperation);
                }
            },
            // WebGL 2: any size, but the base level must be colour-renderable and filterable
            // (GL ES 3.0, glGenerateMipmap): no integer, depth or unfilterable 32-bit float formats.
            // 32-bit float is filterable only once content has enabled OES_texture_float_linear
            // (being supported is not enough).
            WebGLVersion::WebGL2 => {
                let format = base_image_info.internal_format;
                let unsized_format = format.to_unsized();
                let is_float32 = matches!(
                    format,
                    TexFormat::R32f | TexFormat::RG32f | TexFormat::RGB32f | TexFormat::RGBA32f
                );
                if matches!(
                    unsized_format,
                    TexFormat::RedInteger |
                        TexFormat::RGInteger |
                        TexFormat::RGBInteger |
                        TexFormat::RGBAInteger |
                        TexFormat::DepthComponent |
                        TexFormat::DepthStencil
                ) || (is_float32 &&
                    !context
                        .extension_manager()
                        .is_enabled::<OESTextureFloatLinear>())
                {
                    return Err(WebGLError::InvalidOperation);
                }
            },
        }

        if base_image_info.is_compressed_format() {
            return Err(WebGLError::InvalidOperation);
        }

        self.upcast()
            .send_command(WebGLCommand::GenerateMipmap(target));

        let base = self.effective_base_level();
        if base + base_image_info.get_max_mimap_levels() == 0 {
            return Err(WebGLError::InvalidOperation);
        }

        // GL ES 3.0 3.8.11: levels level_base + 1 to q, so no further than TEXTURE_MAX_LEVEL.
        let last_level =
            (base + base_image_info.get_max_mimap_levels() - 1).min(self.max_level.get().max(base));
        self.populate_mip_chain(base, last_level)
    }

    pub(crate) fn delete(&self, operation_fallibility: Operation) {
        self.droppable.delete(operation_fallibility);
    }

    pub(crate) fn is_invalid(&self) -> bool {
        // https://immersive-web.github.io/layers/#xrwebglsubimagetype
        #[cfg(feature = "webxr")]
        if let WebGLTextureOwner::WebXR(ref session) = self.droppable.owner &&
            let Some(xr) = session.root() &&
            xr.is_outside_raf()
        {
            return true;
        }
        self.droppable.is_deleted.get()
    }

    /// The mip levels a draw samples, or None when the texture is incomplete (ES 3.0.6 3.8.14;
    /// an incomplete texture samples as zeros, so it can't form a feedback loop). Without a
    /// mipmapping min filter that is level_base alone, otherwise level_base to q. An immutable
    /// texture's base and max levels are clamped to its levels (ES 3.0.6 3.8.10). Only face 0
    /// of a cube map is looked at.
    pub(crate) fn sampled_levels(&self) -> Option<(u32, u32)> {
        let mipmapped = !matches!(
            self.min_filter.get(),
            constants::NEAREST | constants::LINEAR
        );
        let (base, max) = match self.immutable_levels.get() {
            Some(levels) => {
                let last = levels.saturating_sub(1);
                let base = self.base_level.get().min(last);
                (base, self.max_level.get().max(base).min(last))
            },
            None => (self.base_level.get(), self.max_level.get()),
        };
        if base > max {
            return None;
        }
        let base_info = self.image_info_at_face(0, base)?;
        if !mipmapped {
            return Some((base, base));
        }
        let q = self.top_level(base, &base_info).min(max);
        if !self.is_immutable() && !self.mip_chain_matches(base, q, &base_info) {
            return None;
        }
        Some((base, q))
    }

    /// p of ES 3.0.6 3.8.10: level_base plus floor(log2) of the base image's largest dimension.
    fn top_level(&self, base: u32, base_info: &ImageInfo) -> u32 {
        let mut largest = base_info.width().max(base_info.height());
        if self.target.get() == Some(constants::TEXTURE_3D) {
            largest = largest.max(base_info.depth());
        }
        (31 - largest.max(1).leading_zeros()).saturating_add(base)
    }

    /// Whether levels base..=q are all defined with the base level's format and halving sizes
    /// (mipmap completeness of a mutable texture).
    fn mip_chain_matches(&self, base: u32, q: u32, base_info: &ImageInfo) -> bool {
        let is_3d = self.target.get() == Some(constants::TEXTURE_3D);
        (base..=q).all(|l| {
            let shift = l - base;
            self.image_info_at_face(0, l).is_some_and(|info| {
                info.internal_format() == base_info.internal_format() &&
                    info.width() == (base_info.width() >> shift).max(1) &&
                    info.height() == (base_info.height() >> shift).max(1) &&
                    info.depth() ==
                        if is_3d {
                            (base_info.depth() >> shift).max(1)
                        } else {
                            base_info.depth()
                        }
            })
        })
    }

    /// Whether `level` of this texture can be a complete framebuffer attachment (ES 3.0.6
    /// 4.4.4.2): for a mutable texture it must lie in [level_base, q], and a level other
    /// than level_base needs the texture to be mipmap complete. Immutable textures (whose
    /// levels all exist) and WebGL 1 rules are not affected.
    pub(crate) fn level_is_attachment_complete(&self, level: u32) -> bool {
        if self.is_immutable() {
            return true;
        }
        let base = self.base_level.get();
        let Some(base_info) = self.image_info_at_face(0, base) else {
            return false;
        };
        let q = self.top_level(base, &base_info).min(self.max_level.get());
        if level < base || level > q {
            return false;
        }
        level == base || self.mip_chain_matches(base, q, &base_info)
    }

    pub(crate) fn compare_mode(&self) -> u32 {
        self.compare_mode.get()
    }

    /// The internal format of the base level image (face 0 for cube maps), if defined.
    pub(crate) fn base_internal_format(&self) -> Option<TexFormat> {
        self.base_image_info().map(|info| info.internal_format)
    }

    pub(crate) fn is_immutable(&self) -> bool {
        self.immutable_levels.get().is_some()
    }

    pub(crate) fn target(&self) -> Option<u32> {
        self.target.get()
    }

    pub(crate) fn maybe_get_tex_parameter(&self, param: TexParameter) -> Option<TexParameterValue> {
        match param {
            TexParameter::Int(TexParameterInt::TextureImmutableLevels) => Some(
                TexParameterValue::Int(self.immutable_levels.get().unwrap_or(0) as i32),
            ),
            TexParameter::Bool(TexParameterBool::TextureImmutableFormat) => {
                Some(TexParameterValue::Bool(self.is_immutable()))
            },
            _ => None,
        }
    }

    /// We have to follow the conversion rules for GLES 2.0. See:
    /// <https://www.khronos.org/webgl/public-mailing-list/archives/1008/msg00014.html>
    pub(crate) fn tex_parameter(&self, param: u32, value: TexParameterValue) -> WebGLResult<()> {
        let target = self.target().unwrap();

        let (int_value, float_value) = match value {
            TexParameterValue::Int(int_value) => (int_value, int_value as f32),
            TexParameterValue::Float(float_value) => (float_value as i32, float_value),
            TexParameterValue::Bool(_) => unreachable!("no settable tex params should be booleans"),
        };

        let Some(context) = self.upcast().context() else {
            return Err(WebGLError::ContextLost);
        };
        let is_webgl2 = context.webgl_version() == WebGLVersion::WebGL2;

        let update_filter = |filter: &Cell<u32>| {
            if filter.get() == int_value as u32 {
                return Ok(());
            }
            filter.set(int_value as u32);
            context.send_command(WebGLCommand::TexParameteri(target, param, int_value));
            Ok(())
        };
        if is_webgl2 {
            match param {
                constants::TEXTURE_BASE_LEVEL | constants::TEXTURE_MAX_LEVEL => {
                    if int_value < 0 {
                        return Err(WebGLError::InvalidValue);
                    }
                    if param == constants::TEXTURE_BASE_LEVEL {
                        self.base_level.set(int_value as u32);
                    } else {
                        self.max_level.set(int_value as u32);
                    }
                    context.send_command(WebGLCommand::TexParameteri(target, param, int_value));
                    // The levels a framebuffer may attach depend on these.
                    self.update_attached_framebuffers();
                    return Ok(());
                },
                constants::TEXTURE_COMPARE_FUNC => match int_value as u32 {
                    constants::LEQUAL |
                    constants::GEQUAL |
                    constants::LESS |
                    constants::GREATER |
                    constants::EQUAL |
                    constants::NOTEQUAL |
                    constants::ALWAYS |
                    constants::NEVER => {
                        context.send_command(WebGLCommand::TexParameteri(target, param, int_value));
                        return Ok(());
                    },
                    _ => return Err(WebGLError::InvalidEnum),
                },
                constants::TEXTURE_COMPARE_MODE => match int_value as u32 {
                    mode @ (constants::COMPARE_REF_TO_TEXTURE | constants::NONE) => {
                        self.compare_mode.set(mode);
                        context.send_command(WebGLCommand::TexParameteri(target, param, int_value));
                        return Ok(());
                    },
                    _ => return Err(WebGLError::InvalidEnum),
                },
                constants::TEXTURE_MAX_LOD | constants::TEXTURE_MIN_LOD => {
                    context.send_command(WebGLCommand::TexParameterf(target, param, float_value));
                    return Ok(());
                },
                constants::TEXTURE_WRAP_R => match int_value as u32 {
                    constants::CLAMP_TO_EDGE | constants::MIRRORED_REPEAT | constants::REPEAT => {
                        self.upcast()
                            .send_command(WebGLCommand::TexParameteri(target, param, int_value));
                        return Ok(());
                    },
                    _ => return Err(WebGLError::InvalidEnum),
                },
                _ => {},
            }
        }
        match param {
            constants::TEXTURE_MIN_FILTER => match int_value as u32 {
                constants::NEAREST |
                constants::LINEAR |
                constants::NEAREST_MIPMAP_NEAREST |
                constants::LINEAR_MIPMAP_NEAREST |
                constants::NEAREST_MIPMAP_LINEAR |
                constants::LINEAR_MIPMAP_LINEAR => update_filter(&self.min_filter),
                _ => Err(WebGLError::InvalidEnum),
            },
            constants::TEXTURE_MAG_FILTER => match int_value as u32 {
                constants::NEAREST | constants::LINEAR => update_filter(&self.mag_filter),
                _ => Err(WebGLError::InvalidEnum),
            },
            constants::TEXTURE_WRAP_S | constants::TEXTURE_WRAP_T => match int_value as u32 {
                constants::CLAMP_TO_EDGE | constants::MIRRORED_REPEAT | constants::REPEAT => {
                    context.send_command(WebGLCommand::TexParameteri(target, param, int_value));
                    Ok(())
                },
                _ => Err(WebGLError::InvalidEnum),
            },
            EXTTextureFilterAnisotropicConstants::TEXTURE_MAX_ANISOTROPY_EXT => {
                // NaN is not less than 1., what a time to be alive.
                if float_value < 1. || !float_value.is_normal() {
                    return Err(WebGLError::InvalidValue);
                }
                context.send_command(WebGLCommand::TexParameterf(target, param, float_value));
                Ok(())
            },
            _ => Err(WebGLError::InvalidEnum),
        }
    }

    pub(crate) fn min_filter(&self) -> u32 {
        self.min_filter.get()
    }

    pub(crate) fn mag_filter(&self) -> u32 {
        self.mag_filter.get()
    }

    pub(crate) fn is_using_linear_filtering(&self) -> bool {
        let filters = [self.min_filter.get(), self.mag_filter.get()];
        filters.iter().any(|filter| {
            matches!(
                *filter,
                constants::LINEAR |
                    constants::NEAREST_MIPMAP_LINEAR |
                    constants::LINEAR_MIPMAP_NEAREST |
                    constants::LINEAR_MIPMAP_LINEAR
            )
        })
    }

    pub(crate) fn populate_mip_chain(&self, first_level: u32, last_level: u32) -> WebGLResult<()> {
        let base_image_info = self
            .image_info_at_face(0, first_level)
            .ok_or(WebGLError::InvalidOperation)?;

        let mut ref_width = base_image_info.width;
        let mut ref_height = base_image_info.height;
        let mut ref_depth = base_image_info.depth;
        // Only 3D textures shrink in depth; a 2D array keeps its layers.
        let is_3d = self.target.get() == Some(constants::TEXTURE_3D);

        if ref_width == 0 || ref_height == 0 {
            return Err(WebGLError::InvalidOperation);
        }

        for level in (first_level + 1)..=last_level {
            if ref_width == 1 && ref_height == 1 && (!is_3d || ref_depth <= 1) {
                break;
            }

            ref_width = cmp::max(1, ref_width / 2);
            ref_height = cmp::max(1, ref_height / 2);
            if is_3d {
                ref_depth = cmp::max(1, ref_depth / 2);
            }

            let image_info = ImageInfo {
                width: ref_width,
                height: ref_height,
                depth: ref_depth,
                internal_format: base_image_info.internal_format,
                data_type: base_image_info.data_type,
            };

            self.set_image_infos_at_level(level, image_info);
        }

        self.update_size();
        Ok(())
    }

    fn is_cube_complete(&self) -> bool {
        debug_assert_eq!(self.face_count.get(), 6);

        let image_info = match self.base_image_info() {
            Some(info) => info,
            None => return false,
        };

        let ref_width = image_info.width;
        let ref_format = image_info.internal_format;

        for face in 0..self.face_count.get() {
            let current_image_info =
                match self.image_info_at_face(face, self.effective_base_level()) {
                    Some(info) => info,
                    None => return false,
                };

            // Compares height with width to enforce square dimensions
            if current_image_info.internal_format != ref_format ||
                current_image_info.width != ref_width ||
                current_image_info.height != ref_width
            {
                return false;
            }
        }

        true
    }

    fn face_index_for_target(&self, target: &TexImageTarget) -> u8 {
        match *target {
            TexImageTarget::CubeMapPositiveX => 0,
            TexImageTarget::CubeMapNegativeX => 1,
            TexImageTarget::CubeMapPositiveY => 2,
            TexImageTarget::CubeMapNegativeY => 3,
            TexImageTarget::CubeMapPositiveZ => 4,
            TexImageTarget::CubeMapNegativeZ => 5,
            _ => 0,
        }
    }

    pub(crate) fn image_info_for_target(
        &self,
        target: &TexImageTarget,
        level: u32,
    ) -> Option<ImageInfo> {
        let face_index = self.face_index_for_target(target);
        self.image_info_at_face(face_index, level)
    }

    /// The image at `level` of `face`; None when undefined or out of range (TEXTURE_BASE_LEVEL
    /// and TEXTURE_MAX_LEVEL may name any level).
    pub(crate) fn image_info_at_face(&self, face: u8, level: u32) -> Option<ImageInfo> {
        let pos = level
            .checked_mul(self.face_count.get() as u32)?
            .checked_add(face as u32)?;
        self.image_info_array
            .borrow()
            .get(pos as usize)
            .copied()
            .flatten()
    }

    fn set_image_infos_at_level(&self, level: u32, image_info: ImageInfo) {
        for face in 0..self.face_count.get() {
            self.set_image_infos_at_level_and_face(level, face, image_info);
        }
    }

    fn set_image_infos_at_level_and_face(&self, level: u32, face: u8, image_info: ImageInfo) {
        debug_assert!(face < self.face_count.get());
        let pos = ((level * self.face_count.get() as u32) + face as u32) as usize;
        debug_assert!(pos < MAX_LEVEL_COUNT * MAX_FACE_COUNT);
        let mut infos = self.image_info_array.borrow_mut();
        if infos.len() <= pos {
            infos.resize(pos + 1, None);
        }
        infos[pos] = Some(image_info);
    }

    fn update_size(&self) {
        let size = self
            .image_info_array
            .borrow()
            .iter()
            .filter_map(|info| *info)
            .map(|info| info.physical_size())
            .sum();
        self.reflector().update_memory_size(self, size);
    }

    /// TEXTURE_BASE_LEVEL (always 0 in WebGL 1), clamped to an immutable texture's last level
    /// (GL ES 3.0 3.8.10).
    fn effective_base_level(&self) -> u32 {
        let base = self.base_level.get();
        match self.immutable_levels.get() {
            Some(levels) => base.min(levels.saturating_sub(1)),
            None => base,
        }
    }

    fn base_image_info(&self) -> Option<ImageInfo> {
        let base = self.effective_base_level();
        if base as usize >= MAX_LEVEL_COUNT {
            return None;
        }
        self.image_info_at_face(0, base)
    }

    pub(crate) fn attach_to_framebuffer(&self, fb: &WebGLFramebuffer) {
        let mut framebuffers = self.attached_framebuffers.borrow_mut();
        if !framebuffers
            .iter()
            .any(|attached| std::ptr::eq(&**attached, fb))
        {
            framebuffers.push(Dom::from_ref(fb));
        }
    }

    /// The framebuffer drops this texture from one attachment point; it may still hold it at
    /// another, so the list is pruned in `update_attached_framebuffers`.
    pub(crate) fn detach_from_framebuffer(&self) {}

    /// Re-evaluates the completeness of every framebuffer this texture is attached to.
    fn update_attached_framebuffers(&self) {
        let framebuffers: Vec<DomRoot<WebGLFramebuffer>> = self
            .attached_framebuffers
            .borrow()
            .iter()
            .map(|fb| DomRoot::from_ref(&**fb))
            .collect();
        for fb in &framebuffers {
            fb.update_status();
        }
        self.attached_framebuffers
            .borrow_mut()
            .retain(|fb| fb.has_texture(self.id()));
    }

    pub(crate) fn storage(
        &self,
        target: TexImageTarget,
        levels: u32,
        internal_format: TexFormat,
        width: u32,
        height: u32,
        depth: u32,
    ) -> WebGLResult<()> {
        // Handled by the caller
        assert!(!self.is_immutable());
        assert!(self.target().is_some());

        let target_id = target.as_gl_constant();
        let command = match target {
            TexImageTarget::Texture2D | TexImageTarget::CubeMap => {
                WebGLCommand::TexStorage2D(target_id, levels, internal_format, width, height)
            },
            TexImageTarget::Texture3D | TexImageTarget::Texture2DArray => {
                WebGLCommand::TexStorage3D(target_id, levels, internal_format, width, height, depth)
            },
            _ => unreachable!(), // handled by the caller
        };
        self.upcast().send_command(command);

        let mut width = width;
        let mut height = height;
        let mut depth = depth;
        for level in 0..levels {
            let image_info = ImageInfo {
                width,
                height,
                depth,
                internal_format,
                data_type: None,
            };
            self.set_image_infos_at_level(level, image_info);

            width = cmp::max(1, width / 2);
            height = cmp::max(1, height / 2);
            // A 2D array keeps its layers at every level.
            if target == TexImageTarget::Texture3D {
                depth = cmp::max(1, depth / 2);
            }
        }

        self.immutable_levels.set(Some(levels));

        self.update_attached_framebuffers();

        self.update_size();

        Ok(())
    }
}

#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf, PartialEq)]
pub(crate) struct ImageInfo {
    width: u32,
    height: u32,
    depth: u32,
    #[no_trace]
    internal_format: TexFormat,
    #[no_trace]
    data_type: Option<TexDataType>,
}

impl ImageInfo {
    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    pub(crate) fn depth(&self) -> u32 {
        self.depth
    }

    pub(crate) fn internal_format(&self) -> TexFormat {
        self.internal_format
    }

    pub(crate) fn data_type(&self) -> Option<TexDataType> {
        self.data_type
    }

    fn is_power_of_two(&self) -> bool {
        self.width.is_power_of_two() &&
            self.height.is_power_of_two() &&
            self.depth.is_power_of_two()
    }

    fn get_max_mimap_levels(&self) -> u32 {
        let largest = cmp::max(cmp::max(self.width, self.height), self.depth);
        if largest == 0 {
            return 0;
        }
        // FloorLog2(largest) + 1
        (largest as f64).log2() as u32 + 1
    }

    fn is_compressed_format(&self) -> bool {
        self.internal_format.is_compressed()
    }

    /// Returns approximate physical size
    pub(crate) fn physical_size(&self) -> usize {
        self.width as usize *
            self.height as usize *
            self.depth as usize *
            self.internal_format.components() as usize
    }
}

#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf)]
pub(crate) enum TexCompressionValidation {
    None,
    S3TC,
    /// No extra rule for full images; sub-images must be block-aligned (ASTC).
    SubImageBlockAligned,
    /// BPTC and RGTC: the implied base level (size << level) must be a multiple of the 4x4
    /// block; sub-images must be block-aligned.
    BaseLevelBlockMultiple,
}

#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf)]
pub(crate) struct TexCompression {
    #[no_trace]
    pub(crate) format: TexFormat,
    pub(crate) bytes_per_block: u8,
    pub(crate) block_width: u8,
    pub(crate) block_height: u8,
    pub(crate) validation: TexCompressionValidation,
}
