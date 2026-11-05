/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://www.khronos.org/registry/webgl/specs/latest/1.0/webgl.idl
use std::cell::Cell;

use dom_struct::dom_struct;
#[cfg(feature = "webxr")]
use euclid::Size2D;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object;
use script_bindings::weakref::WeakRef;
use servo_canvas_traits::webgl::{
    TexDataType, WebGLCommand, WebGLError, WebGLFramebufferBindingRequest, WebGLFramebufferId,
    WebGLRenderbufferId, WebGLResult, WebGLTextureId, WebGLVersion, webgl_channel,
};
#[cfg(feature = "webxr")]
use webxr_api::Viewport;

use crate::dom::bindings::codegen::Bindings::WebGL2RenderingContextBinding::WebGL2RenderingContextConstants as constants;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
#[cfg(feature = "webxr")]
use crate::dom::bindings::root::MutNullableDom;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::webgl::webglobject::WebGLObject;
use crate::dom::webgl::webglrenderbuffer::WebGLRenderbuffer;
use crate::dom::webgl::webglrenderingcontext::{Operation, WebGLRenderingContext};
use crate::dom::webgl::webgltexture::WebGLTexture;
#[cfg(feature = "webxr")]
use crate::dom::xrsession::XRSession;

pub(crate) enum CompleteForRendering {
    Complete,
    Incomplete,
    MissingColorAttachment,
}

/// An image attached to a framebuffer, by identity (see `attached_image`).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum AttachedImage {
    Renderbuffer(WebGLRenderbufferId),
    /// Texture, level, and cube map face target or layer (0 for 2D textures).
    Texture(WebGLTextureId, i32, i32),
}

#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
#[derive(Clone, JSTraceable, MallocSizeOf)]
enum WebGLFramebufferAttachment {
    Renderbuffer(Dom<WebGLRenderbuffer>),
    Texture {
        texture: Dom<WebGLTexture>,
        level: i32,
        /// The cube map face (its TEXTURE_CUBE_MAP_* target) or the layer; 0 for 2D textures.
        face_or_layer: i32,
    },
}

impl WebGLFramebufferAttachment {
    fn needs_initialization(&self) -> bool {
        match *self {
            WebGLFramebufferAttachment::Renderbuffer(ref r) => !r.is_initialized(),
            WebGLFramebufferAttachment::Texture { .. } => false,
        }
    }

    fn mark_initialized(&self) {
        match *self {
            WebGLFramebufferAttachment::Renderbuffer(ref r) => r.mark_initialized(),
            WebGLFramebufferAttachment::Texture { .. } => (),
        }
    }

    fn root(&self) -> WebGLFramebufferAttachmentRoot {
        match self {
            WebGLFramebufferAttachment::Renderbuffer(rb) => {
                WebGLFramebufferAttachmentRoot::Renderbuffer(DomRoot::from_ref(rb))
            },
            WebGLFramebufferAttachment::Texture { texture, .. } => {
                WebGLFramebufferAttachmentRoot::Texture(DomRoot::from_ref(texture))
            },
        }
    }

    fn detach(&self) {
        match self {
            WebGLFramebufferAttachment::Renderbuffer(rb) => rb.detach_from_framebuffer(),
            WebGLFramebufferAttachment::Texture { texture, .. } => {
                texture.detach_from_framebuffer()
            },
        }
    }
}

#[derive(Clone, JSTraceable, MallocSizeOf)]
pub(crate) enum WebGLFramebufferAttachmentRoot {
    Renderbuffer(DomRoot<WebGLRenderbuffer>),
    Texture(DomRoot<WebGLTexture>),
}

#[derive(JSTraceable, MallocSizeOf)]
struct DroppableWebGLFramebuffer {
    #[no_trace]
    id: WebGLFramebufferId,
    is_deleted: Cell<bool>,
    context: WeakRef<WebGLRenderingContext>,
}

impl DroppableWebGLFramebuffer {
    fn new(
        id: WebGLFramebufferId,
        is_deleted: Cell<bool>,
        context: WeakRef<WebGLRenderingContext>,
    ) -> Self {
        Self {
            id,
            is_deleted,
            context,
        }
    }
}

impl DroppableWebGLFramebuffer {
    pub(crate) fn id(&self) -> WebGLFramebufferId {
        self.id
    }
    pub(crate) fn is_deleted(&self) -> bool {
        self.is_deleted.get()
    }

    pub(crate) fn set_deleted(&self, deleted: bool) {
        self.is_deleted.set(deleted);
    }

    pub(crate) fn delete(&self, operation_fallibility: Operation) {
        if !self.is_deleted() {
            self.set_deleted(true);
            if let Some(context) = self.context.root() {
                let cmd = WebGLCommand::DeleteFramebuffer(self.id());
                match operation_fallibility {
                    Operation::Fallible => context.send_command_ignored(cmd),
                    Operation::Infallible => context.send_command(cmd),
                }
            }
        }
    }
}

impl Drop for DroppableWebGLFramebuffer {
    fn drop(&mut self) {
        self.delete(Operation::Fallible);
    }
}

#[dom_struct]
pub(crate) struct WebGLFramebuffer {
    webgl_object: WebGLObject,
    #[no_trace]
    webgl_version: WebGLVersion,
    target: Cell<Option<u32>>,
    size: Cell<Option<(i32, i32)>>,
    status: Cell<u32>,
    // The attachment points for textures and renderbuffers on this
    // FBO.
    colors: Vec<DomRefCell<Option<WebGLFramebufferAttachment>>>,
    depth: DomRefCell<Option<WebGLFramebufferAttachment>>,
    stencil: DomRefCell<Option<WebGLFramebufferAttachment>>,
    depthstencil: DomRefCell<Option<WebGLFramebufferAttachment>>,
    color_read_buffer: Cell<u32>,
    color_draw_buffers: DomRefCell<Vec<u32>>,
    is_initialized: Cell<bool>,
    // Framebuffers for XR keep a reference to the XR session.
    // https://github.com/immersive-web/webxr/issues/856
    #[cfg(feature = "webxr")]
    xr_session: MutNullableDom<XRSession>,
    droppable: DroppableWebGLFramebuffer,
}

impl WebGLFramebuffer {
    fn new_inherited(context: &WebGLRenderingContext, id: WebGLFramebufferId) -> Self {
        Self {
            webgl_object: WebGLObject::new_inherited(context),
            webgl_version: context.webgl_version(),
            target: Cell::new(None),
            size: Cell::new(None),
            status: Cell::new(constants::FRAMEBUFFER_INCOMPLETE_MISSING_ATTACHMENT),
            colors: vec![DomRefCell::new(None); context.limits().max_color_attachments as usize],
            depth: DomRefCell::new(None),
            stencil: DomRefCell::new(None),
            depthstencil: DomRefCell::new(None),
            color_read_buffer: Cell::new(constants::COLOR_ATTACHMENT0),
            color_draw_buffers: DomRefCell::new(vec![constants::COLOR_ATTACHMENT0]),
            is_initialized: Cell::new(false),
            #[cfg(feature = "webxr")]
            xr_session: Default::default(),
            droppable: DroppableWebGLFramebuffer::new(id, Cell::new(false), WeakRef::new(context)),
        }
    }

    pub(crate) fn maybe_new(
        cx: &mut JSContext,
        context: &WebGLRenderingContext,
    ) -> Option<DomRoot<Self>> {
        let (sender, receiver) = webgl_channel().unwrap();
        context.send_command(WebGLCommand::CreateFramebuffer(sender));
        let id = receiver.recv().unwrap()?;
        let framebuffer = WebGLFramebuffer::new(cx, context, id);
        Some(framebuffer)
    }

    // TODO: depth, stencil and alpha
    // https://github.com/servo/servo/issues/24498
    #[cfg(feature = "webxr")]
    pub(crate) fn maybe_new_webxr(
        cx: &mut JSContext,
        session: &XRSession,
        context: &WebGLRenderingContext,
        size: Size2D<i32, Viewport>,
    ) -> Option<DomRoot<Self>> {
        let framebuffer = Self::maybe_new(cx, context)?;
        framebuffer.size.set(Some((size.width, size.height)));
        framebuffer.status.set(constants::FRAMEBUFFER_COMPLETE);
        framebuffer.xr_session.set(Some(session));
        Some(framebuffer)
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        context: &WebGLRenderingContext,
        id: WebGLFramebufferId,
    ) -> DomRoot<Self> {
        reflect_dom_object(
            cx,
            Box::new(WebGLFramebuffer::new_inherited(context, id)),
            &*context.global(),
        )
    }
}

impl WebGLFramebuffer {
    pub(crate) fn id(&self) -> WebGLFramebufferId {
        self.droppable.id()
    }

    #[cfg(feature = "webxr")]
    fn is_in_xr_session(&self) -> bool {
        self.xr_session.get().is_some()
    }

    #[cfg(not(feature = "webxr"))]
    fn is_in_xr_session(&self) -> bool {
        false
    }

    pub(crate) fn validate_transparent(&self) -> WebGLResult<()> {
        if self.is_in_xr_session() {
            Err(WebGLError::InvalidOperation)
        } else {
            Ok(())
        }
    }

    pub(crate) fn bind(&self, target: u32) {
        if !self.is_in_xr_session() {
            // Update the framebuffer status on binding.  It may have
            // changed if its attachments were resized or deleted while
            // we've been unbound.
            self.update_status();
        }

        self.target.set(Some(target));
        self.upcast().send_command(WebGLCommand::BindFramebuffer(
            target,
            WebGLFramebufferBindingRequest::Explicit(self.id()),
        ));
    }

    pub(crate) fn delete(&self, operation_fallibility: Operation) {
        self.droppable.delete(operation_fallibility);
    }

    pub(crate) fn is_deleted(&self) -> bool {
        // TODO: if a framebuffer has an attachment which is invalid due to
        // being outside a webxr rAF, should this make the framebuffer invalid?
        // https://github.com/immersive-web/layers/issues/196
        self.droppable.is_deleted()
    }

    pub(crate) fn size(&self) -> Option<(i32, i32)> {
        self.size.get()
    }

    /// Whether COLOR_ATTACHMENT0 holds float data (a 16/32-bit float renderbuffer, or a texture
    /// uploaded with FLOAT / HALF_FLOAT), for WebGL 1's float readPixels.
    pub(crate) fn color_attachment_is_float(&self) -> bool {
        let binding = self.colors[0].borrow();
        match binding.as_ref() {
            Some(WebGLFramebufferAttachment::Renderbuffer(rb)) => matches!(
                rb.internal_format(),
                constants::RGBA32F | constants::RGBA16F | constants::RGB16F
            ),
            Some(WebGLFramebufferAttachment::Texture { texture, level, .. }) => texture
                .image_info_at_face(0, *level as u32)
                .and_then(|info| info.data_type())
                .is_some_and(|data_type| {
                    matches!(data_type, TexDataType::Float | TexDataType::HalfFloat)
                }),
            None => false,
        }
    }

    /// What image is attached at `attachment`, to tell when a blit reads and writes the same one.
    pub(crate) fn attached_image(&self, attachment: u32) -> Option<AttachedImage> {
        let binding = self.attachment_binding(attachment)?;
        let binding = binding.borrow();
        match binding.as_ref()? {
            WebGLFramebufferAttachment::Renderbuffer(rb) => {
                Some(AttachedImage::Renderbuffer(rb.id()))
            },
            WebGLFramebufferAttachment::Texture {
                texture,
                level,
                face_or_layer,
            } => Some(AttachedImage::Texture(texture.id(), *level, *face_or_layer)),
        }
    }

    /// The levels at which `texture` is attached to this framebuffer (any attachment point).
    pub(crate) fn texture_attachment_levels(&self, texture: &WebGLTexture) -> Vec<i32> {
        self.colors
            .iter()
            .chain([&self.depth, &self.stencil, &self.depthstencil])
            .filter_map(|attachment| match *attachment.borrow() {
                Some(WebGLFramebufferAttachment::Texture {
                    texture: ref attached,
                    level,
                    ..
                }) if attached.id() == texture.id() => Some(level),
                _ => None,
            })
            .collect()
    }

    /// The internal formats of the colour attachment at `color_attachment` and of the depth and
    /// stencil attachments, renderbuffers and textures alike.
    pub(crate) fn get_attachment_formats(
        &self,
        color_attachment: u32,
    ) -> WebGLResult<(Option<u32>, Option<u32>, Option<u32>)> {
        if self.check_status() != constants::FRAMEBUFFER_COMPLETE {
            return Err(WebGLError::InvalidFramebufferOperation);
        }
        let depth_stencil = self.attachment_internal_format(constants::DEPTH_STENCIL_ATTACHMENT);
        let color = match color_attachment {
            constants::NONE => None,
            attachment => self.attachment_internal_format(attachment),
        };
        let depth = self
            .attachment_internal_format(constants::DEPTH_ATTACHMENT)
            .or(depth_stencil);
        let stencil = self
            .attachment_internal_format(constants::STENCIL_ATTACHMENT)
            .or(depth_stencil);
        Ok((color, depth, stencil))
    }

    fn check_attachment_constraints<'a>(
        &self,
        attachment: &Option<WebGLFramebufferAttachment>,
        mut constraints: impl Iterator<Item = &'a u32>,
        fb_size: &mut Option<(i32, i32)>,
    ) -> Result<(), u32> {
        // Get the size of this attachment.
        let (format, size) = match attachment {
            Some(WebGLFramebufferAttachment::Renderbuffer(att_rb)) => {
                (Some(att_rb.internal_format()), att_rb.size())
            },
            Some(WebGLFramebufferAttachment::Texture {
                texture: att_tex,
                level,
                ..
            }) => match att_tex.image_info_at_face(0, *level as u32) {
                Some(_)
                    if self.webgl_version == WebGLVersion::WebGL2 &&
                        !att_tex.level_is_attachment_complete(*level as u32) =>
                {
                    return Err(constants::FRAMEBUFFER_INCOMPLETE_ATTACHMENT);
                },
                Some(info) => (
                    Some(info.internal_format().as_gl_constant()),
                    Some((info.width() as i32, info.height() as i32)),
                ),
                None => return Err(constants::FRAMEBUFFER_INCOMPLETE_ATTACHMENT),
            },
            None => (None, None),
        };

        // Make sure that, if we've found any other attachment,
        // that the size matches.
        if size.is_some() {
            if fb_size.is_some() && size != *fb_size {
                return Err(constants::FRAMEBUFFER_INCOMPLETE_DIMENSIONS);
            } else {
                *fb_size = size;
            }
        }

        if let Some(format) = format &&
            constraints.all(|c| *c != format)
        {
            return Err(constants::FRAMEBUFFER_INCOMPLETE_ATTACHMENT);
        }

        Ok(())
    }

    pub(crate) fn update_status(&self) {
        let z = self.depth.borrow();
        let s = self.stencil.borrow();
        let zs = self.depthstencil.borrow();
        let has_z = z.is_some();
        let has_s = s.is_some();
        let has_zs = zs.is_some();

        let is_supported = match self.webgl_version {
            // From the WebGL 1.0 spec, 6.6 ("Framebuffer Object Attachments"):
            //
            //    "In the WebGL API, it is an error to concurrently attach
            //     renderbuffers to the following combinations of
            //     attachment points:
            //
            //     DEPTH_ATTACHMENT + DEPTH_STENCIL_ATTACHMENT
            //     STENCIL_ATTACHMENT + DEPTH_STENCIL_ATTACHMENT
            //     DEPTH_ATTACHMENT + STENCIL_ATTACHMENT
            //
            //     If any of the constraints above are violated, then:
            //
            //     checkFramebufferStatus must return FRAMEBUFFER_UNSUPPORTED."
            WebGLVersion::WebGL1 => !(has_zs && (has_z || has_s)) && !(has_z && has_s),

            // In WebGL 2.0, DEPTH_STENCIL_ATTACHMENT is considered an alias for
            // DEPTH_ATTACHMENT + STENCIL_ATTACHMENT, i.e., the same image is attached to both DEPTH_ATTACHMENT
            // and STENCIL_ATTACHMENT, overwriting the original images attached to the two attachment points.
            // If different images are bound to the depth and stencil attachment points, checkFramebufferStatus
            // returns FRAMEBUFFER_UNSUPPORTED, and getFramebufferAttachmentParameter with attachment of
            // DEPTH_STENCIL_ATTACHMENT generates an INVALID_OPERATION error.
            // -- WebGL 2.0 spec, 4.1.5 Framebuffer Object Attachments
            WebGLVersion::WebGL2 => {
                use WebGLFramebufferAttachment::{Renderbuffer, Texture};
                match (&*z, &*s) {
                    (Some(Renderbuffer(a)), Some(Renderbuffer(b))) => a.id() == b.id(),
                    (Some(Texture { texture: a, .. }), Some(Texture { texture: b, .. })) => {
                        a.id() == b.id()
                    },
                    _ => !has_z || !has_s,
                }
            },
        };
        if !is_supported {
            return self.status.set(constants::FRAMEBUFFER_UNSUPPORTED);
        }
        // WebGL 2.0 5.x: one image attached at two colour attachment points is
        // FRAMEBUFFER_UNSUPPORTED.
        if self.webgl_version == WebGLVersion::WebGL2 {
            let mut images = Vec::new();
            for index in 0..self.colors.len() as u32 {
                if let Some(image) = self.attached_image(constants::COLOR_ATTACHMENT0 + index) {
                    if images.contains(&image) {
                        return self.status.set(constants::FRAMEBUFFER_UNSUPPORTED);
                    }
                    images.push(image);
                }
            }
        }

        let mut fb_size = None;

        let attachments = [&*z, &*s, &*zs];
        let webgl1_attachment_constraints = &[
            &[
                constants::DEPTH_COMPONENT16,
                constants::DEPTH_COMPONENT24,
                constants::DEPTH_COMPONENT32F,
                constants::DEPTH24_STENCIL8,
                constants::DEPTH32F_STENCIL8,
            ][..],
            &[
                constants::STENCIL_INDEX8,
                constants::DEPTH24_STENCIL8,
                constants::DEPTH32F_STENCIL8,
            ][..],
            // Textures record their sized format (DEPTH24_STENCIL8 from a WebGL 2
            // texImage2D), renderbuffers the unsized one.
            &[
                constants::DEPTH_STENCIL,
                constants::DEPTH24_STENCIL8,
                constants::DEPTH32F_STENCIL8,
            ][..],
        ];
        let webgl2_attachment_constraints = &[
            &[constants::DEPTH_STENCIL, constants::DEPTH_COMPONENT][..],
            &[constants::DEPTH_STENCIL][..],
            &[][..],
        ];
        let empty_attachment_constrains = &[&[][..], &[][..], &[][..]];
        let extra_attachment_constraints = match self.webgl_version {
            WebGLVersion::WebGL1 => empty_attachment_constrains,
            WebGLVersion::WebGL2 => webgl2_attachment_constraints,
        };
        let attachment_constraints = webgl1_attachment_constraints
            .iter()
            .zip(extra_attachment_constraints.iter())
            .map(|(a, b)| a.iter().chain(b.iter()));

        for (attachment, constraints) in attachments.iter().zip(attachment_constraints) {
            if let Err(errnum) =
                self.check_attachment_constraints(attachment, constraints, &mut fb_size)
            {
                return self.status.set(errnum);
            }
        }

        let webgl1_color_constraints = &[
            constants::RGB,
            constants::RGB565,
            constants::RGB5_A1,
            constants::RGBA,
            constants::RGBA4,
        ][..];
        let webgl2_color_constraints = &[
            constants::ALPHA,
            constants::LUMINANCE,
            constants::LUMINANCE_ALPHA,
            constants::R11F_G11F_B10F,
            constants::R16F,
            constants::R16I,
            constants::R16UI,
            constants::R32F,
            constants::R32I,
            constants::R32UI,
            constants::R8,
            constants::R8_SNORM,
            constants::R8I,
            constants::R8UI,
            constants::RG16F,
            constants::RG16I,
            constants::RG16UI,
            constants::RG32F,
            constants::RG32I,
            constants::RG32UI,
            constants::RG8,
            constants::RG8_SNORM,
            constants::RG8I,
            constants::RG8UI,
            constants::RGB10_A2,
            constants::RGB10_A2UI,
            constants::RGB16F,
            constants::RGB16I,
            constants::RGB16UI,
            constants::RGB32F,
            constants::RGB32I,
            constants::RGB32UI,
            constants::RGB8,
            constants::RGB8_SNORM,
            constants::RGB8I,
            constants::RGB8UI,
            constants::RGB9_E5,
            constants::RGBA16F,
            constants::RGBA16I,
            constants::RGBA16UI,
            constants::RGBA32F,
            constants::RGBA32I,
            constants::RGBA32UI,
            constants::RGBA8,
            constants::RGBA8_SNORM,
            constants::RGBA8I,
            constants::RGBA8UI,
            constants::SRGB8,
            constants::SRGB8_ALPHA8,
        ][..];
        let empty_color_constrains = &[][..];
        let extra_color_constraints = match self.webgl_version {
            WebGLVersion::WebGL1 => empty_color_constrains,
            WebGLVersion::WebGL2 => webgl2_color_constraints,
        };
        // Float color formats are renderable in WebGL 2 only with EXT_color_buffer_float
        // (or EXT_color_buffer_half_float, for the 16-bit ones).
        let (half_float_renderable, float_renderable) = match self.upcast().context() {
            Some(context) => {
                let extensions = context.extension_manager();
                (
                    extensions.is_half_float_buffer_renderable(),
                    extensions.is_float_buffer_renderable(),
                )
            },
            None => (false, false),
        };
        let color_constraints = webgl1_color_constraints
            .iter()
            .chain(extra_color_constraints.iter())
            .filter(move |format| match **format {
                constants::R16F | constants::RG16F | constants::RGBA16F => half_float_renderable,
                constants::R32F |
                constants::RG32F |
                constants::RGBA32F |
                constants::R11F_G11F_B10F => float_renderable,
                _ => true,
            });

        let has_c = self.colors.iter().any(|att| att.borrow().is_some());
        for attachment in self.colors.iter() {
            let attachment = attachment.borrow();
            let constraints = color_constraints.clone();
            if let Err(errnum) =
                self.check_attachment_constraints(&attachment, constraints, &mut fb_size)
            {
                return self.status.set(errnum);
            }
        }

        self.size.set(fb_size);

        if has_c || has_z || has_zs || has_s {
            if self.size.get().is_some_and(|(w, h)| w != 0 && h != 0) {
                self.status.set(constants::FRAMEBUFFER_COMPLETE);
            } else {
                self.status
                    .set(constants::FRAMEBUFFER_INCOMPLETE_ATTACHMENT);
            }
        } else {
            self.status
                .set(constants::FRAMEBUFFER_INCOMPLETE_MISSING_ATTACHMENT);
        }
    }

    pub(crate) fn check_status(&self) -> u32 {
        // For opaque framebuffers, check to see if the XR session is currently processing an rAF
        // https://immersive-web.github.io/webxr/#opaque-framebuffer
        #[cfg(feature = "webxr")]
        if let Some(xr_session) = self.xr_session.get() {
            return if xr_session.is_outside_raf() {
                constants::FRAMEBUFFER_UNSUPPORTED
            } else {
                constants::FRAMEBUFFER_COMPLETE
            };
        }

        self.status.get()
        // TODO: if a framebuffer has an attachment which is invalid due to
        // being outside a webxr rAF, should this make the framebuffer incomplete?
        // https://github.com/immersive-web/layers/issues/196
    }

    pub(crate) fn check_status_for_rendering(&self) -> CompleteForRendering {
        let result = self.check_status();
        if result != constants::FRAMEBUFFER_COMPLETE {
            return CompleteForRendering::Incomplete;
        }

        // XR framebuffers are complete inside an rAF
        // https://github.com/immersive-web/webxr/issues/854
        #[cfg(feature = "webxr")]
        if self.xr_session.get().is_some() {
            return CompleteForRendering::Complete;
        }

        // GL ES 3.0 framebuffers need no colour attachment (depth-only passes such as shadow
        // maps); WebGL 1 keeps rejecting them.
        if self.webgl_version == WebGLVersion::WebGL1 &&
            self.colors.iter().all(|att| att.borrow().is_none())
        {
            return CompleteForRendering::MissingColorAttachment;
        }

        if !self.is_initialized.get() {
            let attachments = [
                (&self.depth, constants::DEPTH_BUFFER_BIT),
                (&self.stencil, constants::STENCIL_BUFFER_BIT),
                (
                    &self.depthstencil,
                    constants::DEPTH_BUFFER_BIT | constants::STENCIL_BUFFER_BIT,
                ),
            ];
            let mut clear_bits = 0;
            for &(attachment, bits) in &attachments {
                if let Some(ref att) = *attachment.borrow() &&
                    att.needs_initialization()
                {
                    att.mark_initialized();
                    clear_bits |= bits;
                }
            }
            for attachment in self.colors.iter() {
                if let Some(ref att) = *attachment.borrow() &&
                    att.needs_initialization()
                {
                    att.mark_initialized();
                    clear_bits |= constants::COLOR_BUFFER_BIT;
                }
            }

            if let Some(context) = self.upcast().context() {
                context.initialize_framebuffer(clear_bits);
            }

            self.is_initialized.set(true);
        }

        // TODO: if a framebuffer has an attachment which is invalid due to
        // being outside a webxr rAF, should this make the framebuffer incomplete?
        // https://github.com/immersive-web/layers/issues/196

        CompleteForRendering::Complete
    }

    pub(crate) fn renderbuffer(
        &self,
        attachment: u32,
        rb: Option<&WebGLRenderbuffer>,
    ) -> WebGLResult<()> {
        // Opaque framebuffers cannot have their attachments changed
        // https://immersive-web.github.io/webxr/#opaque-framebuffer
        self.validate_transparent()?;

        let binding = self
            .attachment_binding(attachment)
            .ok_or(WebGLError::InvalidEnum)?;

        let rb_id = match rb {
            Some(rb) => {
                if !rb.ever_bound() {
                    return Err(WebGLError::InvalidOperation);
                }
                *binding.borrow_mut() =
                    Some(WebGLFramebufferAttachment::Renderbuffer(Dom::from_ref(rb)));
                rb.attach_to_framebuffer(self);
                Some(rb.id())
            },

            _ => None,
        };

        self.upcast()
            .send_command(WebGLCommand::FramebufferRenderbuffer(
                self.target.get().unwrap(),
                attachment,
                constants::RENDERBUFFER,
                rb_id,
            ));

        if rb.is_none() {
            self.detach_binding(binding, attachment)?;
        }

        self.update_status();
        self.is_initialized.set(false);
        Ok(())
    }

    fn detach_binding(
        &self,
        binding: &DomRefCell<Option<WebGLFramebufferAttachment>>,
        attachment: u32,
    ) -> WebGLResult<()> {
        // Opaque framebuffers cannot have their attachments changed
        // https://immersive-web.github.io/webxr/#opaque-framebuffer
        self.validate_transparent()?;

        if let Some(att) = &*binding.borrow() {
            att.detach();
        }
        *binding.borrow_mut() = None;
        if INTERESTING_ATTACHMENT_POINTS.contains(&attachment) {
            self.reattach_depth_stencil()?;
        }
        Ok(())
    }

    fn attachment_binding(
        &self,
        attachment: u32,
    ) -> Option<&DomRefCell<Option<WebGLFramebufferAttachment>>> {
        match attachment {
            constants::COLOR_ATTACHMENT0..=constants::COLOR_ATTACHMENT15 => {
                let idx = attachment - constants::COLOR_ATTACHMENT0;
                self.colors.get(idx as usize)
            },
            constants::DEPTH_ATTACHMENT => Some(&self.depth),
            constants::STENCIL_ATTACHMENT => Some(&self.stencil),
            constants::DEPTH_STENCIL_ATTACHMENT => Some(&self.depthstencil),
            _ => None,
        }
    }

    fn reattach_depth_stencil(&self) -> WebGLResult<()> {
        // Opaque framebuffers cannot have their attachments changed
        // https://immersive-web.github.io/webxr/#opaque-framebuffer
        self.validate_transparent()?;

        let reattach = |attachment: &WebGLFramebufferAttachment, attachment_point| {
            let webgl_object = self.upcast();
            match *attachment {
                WebGLFramebufferAttachment::Renderbuffer(ref rb) => {
                    rb.attach_to_framebuffer(self);
                    webgl_object.send_command(WebGLCommand::FramebufferRenderbuffer(
                        self.target.get().unwrap(),
                        attachment_point,
                        constants::RENDERBUFFER,
                        Some(rb.id()),
                    ));
                },
                WebGLFramebufferAttachment::Texture {
                    ref texture, level, ..
                } => {
                    texture.attach_to_framebuffer(self);
                    webgl_object.send_command(WebGLCommand::FramebufferTexture2D(
                        self.target.get().unwrap(),
                        attachment_point,
                        texture.target().expect("missing texture target"),
                        Some(texture.id()),
                        level,
                    ));
                },
            }
        };

        // Since the DEPTH_STENCIL attachment causes both the DEPTH and STENCIL
        // attachments to be overwritten, we need to ensure that we reattach
        // the DEPTH and STENCIL attachments when any of those attachments
        // is cleared.
        if let Some(ref depth) = *self.depth.borrow() {
            reattach(depth, constants::DEPTH_ATTACHMENT);
        }
        if let Some(ref stencil) = *self.stencil.borrow() {
            reattach(stencil, constants::STENCIL_ATTACHMENT);
        }
        if let Some(ref depth_stencil) = *self.depthstencil.borrow() {
            reattach(depth_stencil, constants::DEPTH_STENCIL_ATTACHMENT);
        }
        Ok(())
    }

    pub(crate) fn attachment(&self, attachment: u32) -> Option<WebGLFramebufferAttachmentRoot> {
        let binding = self.attachment_binding(attachment)?;
        binding
            .borrow()
            .as_ref()
            .map(WebGLFramebufferAttachment::root)
    }

    /// Whether any attachment is a multisampled renderbuffer (SAMPLE_BUFFERS is 1).
    pub(crate) fn is_multisampled(&self) -> bool {
        self.colors
            .iter()
            .chain([&self.depth, &self.stencil, &self.depthstencil])
            .any(|binding| {
                matches!(
                    binding.borrow().as_ref(),
                    Some(WebGLFramebufferAttachment::Renderbuffer(rb)) if rb.samples() > 0
                )
            })
    }

    /// The sized internal format of the colour image at `attachment`: a texture's unsized
    /// format is resolved by its type (RGBA + UNSIGNED_BYTE is RGBA8). None when there is no
    /// image or the pair isn't one of the common ones.
    pub(crate) fn attachment_sized_format(&self, attachment: u32) -> Option<u32> {
        let binding = self.attachment_binding(attachment)?;
        let binding = binding.borrow();
        let (format, data_type) = match binding.as_ref()? {
            WebGLFramebufferAttachment::Renderbuffer(rb) => return Some(rb.internal_format()),
            WebGLFramebufferAttachment::Texture { texture, level, .. } => {
                let info = texture.image_info_at_face(0, *level as u32)?;
                (
                    info.internal_format().as_gl_constant(),
                    info.data_type().map(|data_type| data_type.as_gl_constant()),
                )
            },
        };
        match (format, data_type) {
            (constants::RGBA, Some(constants::UNSIGNED_BYTE)) => Some(constants::RGBA8),
            (constants::RGB, Some(constants::UNSIGNED_BYTE)) => Some(constants::RGB8),
            (constants::RGBA, Some(constants::UNSIGNED_SHORT_4_4_4_4)) => Some(constants::RGBA4),
            (constants::RGBA, Some(constants::UNSIGNED_SHORT_5_5_5_1)) => Some(constants::RGB5_A1),
            (constants::RGB, Some(constants::UNSIGNED_SHORT_5_6_5)) => Some(constants::RGB565),
            (constants::RGBA | constants::RGB | constants::ALPHA | constants::LUMINANCE, _) |
            (constants::LUMINANCE_ALPHA, _) => None,
            (format, _) => Some(format),
        }
    }

    /// The sized or unsized internal format of what is attached at `attachment`, if anything.
    pub(crate) fn attachment_internal_format(&self, attachment: u32) -> Option<u32> {
        let binding = self.attachment_binding(attachment)?;
        let binding = binding.borrow();
        match binding.as_ref()? {
            WebGLFramebufferAttachment::Renderbuffer(rb) => Some(rb.internal_format()),
            WebGLFramebufferAttachment::Texture { texture, level, .. } => texture
                .image_info_at_face(0, *level as u32)
                .map(|info| info.internal_format().as_gl_constant()),
        }
    }

    pub(crate) fn texture2d(
        &self,
        attachment: u32,
        textarget: u32,
        texture: Option<&WebGLTexture>,
        level: i32,
    ) -> WebGLResult<()> {
        // Opaque framebuffers cannot have their attachments changed
        // https://immersive-web.github.io/webxr/#opaque-framebuffer
        self.validate_transparent()?;
        if let Some(texture) = texture {
            //     "If texture is not zero, then texture must either
            //      name an existing texture object with an target of
            //      textarget, or texture must name an existing cube
            //      map texture and textarget must be one of:
            //      TEXTURE_CUBE_MAP_POSITIVE_X,
            //      TEXTURE_CUBE_MAP_POSITIVE_Y,
            //      TEXTURE_CUBE_MAP_POSITIVE_Z,
            //      TEXTURE_CUBE_MAP_NEGATIVE_X,
            //      TEXTURE_CUBE_MAP_NEGATIVE_Y, or
            //      TEXTURE_CUBE_MAP_NEGATIVE_Z. Otherwise,
            //      INVALID_OPERATION is generated."
            let is_cube = match textarget {
                constants::TEXTURE_2D => false,

                constants::TEXTURE_CUBE_MAP_POSITIVE_X => true,
                constants::TEXTURE_CUBE_MAP_POSITIVE_Y => true,
                constants::TEXTURE_CUBE_MAP_POSITIVE_Z => true,
                constants::TEXTURE_CUBE_MAP_NEGATIVE_X => true,
                constants::TEXTURE_CUBE_MAP_NEGATIVE_Y => true,
                constants::TEXTURE_CUBE_MAP_NEGATIVE_Z => true,

                _ => return Err(WebGLError::InvalidEnum),
            };

            match texture.target() {
                Some(constants::TEXTURE_CUBE_MAP) if is_cube => {},
                Some(_) if !is_cube => {},
                _ => return Err(WebGLError::InvalidOperation),
            }

            let Some(context) = self.upcast().context() else {
                return Err(WebGLError::ContextLost);
            };
            let max_tex_size = if is_cube {
                context.limits().max_cube_map_tex_size
            } else {
                context.limits().max_tex_size
            };
            if level < 0 || level as u32 > max_tex_size.ilog2() {
                return Err(WebGLError::InvalidValue);
            }
        }
        self.texture2d_even_if_opaque(attachment, textarget, texture, level)
    }

    pub(crate) fn texture2d_even_if_opaque(
        &self,
        attachment: u32,
        textarget: u32,
        texture: Option<&WebGLTexture>,
        level: i32,
    ) -> WebGLResult<()> {
        let binding = self
            .attachment_binding(attachment)
            .ok_or(WebGLError::InvalidEnum)?;

        let tex_id = match texture {
            // Note, from the GLES 2.0.25 spec, page 113:
            //      "If texture is zero, then textarget and level are ignored."
            Some(texture) => {
                *binding.borrow_mut() = Some(WebGLFramebufferAttachment::Texture {
                    texture: Dom::from_ref(texture),
                    level,
                    face_or_layer: if textarget == constants::TEXTURE_2D {
                        0
                    } else {
                        textarget as i32
                    },
                });
                texture.attach_to_framebuffer(self);

                Some(texture.id())
            },

            _ => None,
        };

        self.upcast()
            .send_command(WebGLCommand::FramebufferTexture2D(
                self.target.get().unwrap(),
                attachment,
                textarget,
                tex_id,
                level,
            ));

        if texture.is_none() {
            self.detach_binding(binding, attachment)?;
        }

        self.update_status();
        self.is_initialized.set(false);
        Ok(())
    }

    pub(crate) fn texture_layer(
        &self,
        attachment: u32,
        texture: Option<&WebGLTexture>,
        level: i32,
        layer: i32,
    ) -> WebGLResult<()> {
        let binding = self
            .attachment_binding(attachment)
            .ok_or(WebGLError::InvalidEnum)?;

        let Some(context) = self.upcast().context() else {
            return Err(WebGLError::ContextLost);
        };

        let tex_id = match texture {
            Some(texture) => {
                // Layers of 3D textures and 2D arrays only (GL ES 3.0, glFramebufferTextureLayer).
                let (max_size, layers) = match texture.target() {
                    Some(constants::TEXTURE_3D) => (
                        context.limits().max_3d_texture_size,
                        context.limits().max_3d_texture_size,
                    ),
                    Some(constants::TEXTURE_2D_ARRAY) => (
                        context.limits().max_tex_size,
                        context.limits().max_array_texture_layers,
                    ),
                    _ => return Err(WebGLError::InvalidOperation),
                };

                if level < 0 || level as u32 > max_size.ilog2() {
                    return Err(WebGLError::InvalidValue);
                }
                if layer < 0 || layer as u32 >= layers {
                    return Err(WebGLError::InvalidValue);
                }

                *binding.borrow_mut() = Some(WebGLFramebufferAttachment::Texture {
                    texture: Dom::from_ref(texture),
                    level,
                    face_or_layer: layer,
                });
                texture.attach_to_framebuffer(self);

                Some(texture.id())
            },
            _ => None,
        };

        context.send_command(WebGLCommand::FramebufferTextureLayer(
            self.target.get().unwrap(),
            attachment,
            tex_id,
            level,
            layer,
        ));

        if texture.is_none() {
            self.detach_binding(binding, attachment)?;
        }

        // As for framebufferTexture2D: completeness (and readPixels / draws) depend on it.
        self.update_status();
        self.is_initialized.set(false);
        Ok(())
    }

    fn with_matching_renderbuffers_id<F>(&self, rb_id: &WebGLRenderbufferId, mut closure: F)
    where
        F: FnMut(&DomRefCell<Option<WebGLFramebufferAttachment>>, u32),
    {
        let attachments = [
            (&self.depth, constants::DEPTH_ATTACHMENT),
            (&self.stencil, constants::STENCIL_ATTACHMENT),
            (&self.depthstencil, constants::DEPTH_STENCIL_ATTACHMENT),
        ];

        fn has_matching_id(
            attachment: &DomRefCell<Option<WebGLFramebufferAttachment>>,
            target: &WebGLRenderbufferId,
        ) -> bool {
            match *attachment.borrow() {
                Some(WebGLFramebufferAttachment::Renderbuffer(ref att_rb)) => {
                    att_rb.id() == *target
                },
                _ => false,
            }
        }

        for (attachment, name) in &attachments {
            if has_matching_id(attachment, rb_id) {
                closure(attachment, *name);
            }
        }

        for (idx, attachment) in self.colors.iter().enumerate() {
            if has_matching_id(attachment, rb_id) {
                let name = constants::COLOR_ATTACHMENT0 + idx as u32;
                closure(attachment, name);
            }
        }
    }

    fn with_matching_textures_id<F>(&self, tex_id: WebGLTextureId, mut closure: F)
    where
        F: FnMut(&DomRefCell<Option<WebGLFramebufferAttachment>>, u32),
    {
        let attachments = [
            (&self.depth, constants::DEPTH_ATTACHMENT),
            (&self.stencil, constants::STENCIL_ATTACHMENT),
            (&self.depthstencil, constants::DEPTH_STENCIL_ATTACHMENT),
        ];

        fn has_matching_id(
            attachment: &DomRefCell<Option<WebGLFramebufferAttachment>>,
            target: &WebGLTextureId,
        ) -> bool {
            matches!(*attachment.borrow(), Some(WebGLFramebufferAttachment::Texture {
                                     texture: ref att_texture,
                                     ..
                                }) if att_texture.id() == *target)
        }

        for (attachment, name) in &attachments {
            if has_matching_id(attachment, &tex_id) {
                closure(attachment, *name);
            }
        }

        for (idx, attachment) in self.colors.iter().enumerate() {
            if has_matching_id(attachment, &tex_id) {
                let name = constants::COLOR_ATTACHMENT0 + idx as u32;
                closure(attachment, name);
            }
        }
    }

    pub(crate) fn detach_renderbuffer_by_id(&self, rb_id: &WebGLRenderbufferId) -> WebGLResult<()> {
        // Opaque framebuffers cannot have their attachments changed
        // https://immersive-web.github.io/webxr/#opaque-framebuffer
        self.validate_transparent()?;

        let mut depth_or_stencil_updated = false;
        self.with_matching_renderbuffers_id(rb_id, |att, name| {
            depth_or_stencil_updated |= INTERESTING_ATTACHMENT_POINTS.contains(&name);
            if let Some(att) = &*att.borrow() {
                att.detach();
            }
            *att.borrow_mut() = None;
            self.update_status();
        });

        if depth_or_stencil_updated {
            self.reattach_depth_stencil()?;
        }
        Ok(())
    }

    pub(crate) fn detach_texture(&self, tex_id: WebGLTextureId) -> WebGLResult<()> {
        // Opaque framebuffers cannot have their attachments changed
        // https://immersive-web.github.io/webxr/#opaque-framebuffer
        self.validate_transparent()?;

        let mut depth_or_stencil_updated = false;
        self.with_matching_textures_id(tex_id, |att, name| {
            depth_or_stencil_updated |= INTERESTING_ATTACHMENT_POINTS.contains(&name);
            if let Some(att) = &*att.borrow() {
                att.detach();
            }
            *att.borrow_mut() = None;
            self.update_status();
        });

        if depth_or_stencil_updated {
            self.reattach_depth_stencil()?;
        }
        Ok(())
    }

    pub(crate) fn invalidate_renderbuffer(&self, rb: &WebGLRenderbuffer) {
        self.with_matching_renderbuffers_id(&rb.id(), |_att, _| {
            self.is_initialized.set(false);
            self.update_status();
        });
    }

    pub(crate) fn invalidate_texture(&self, texture: &WebGLTexture) {
        self.with_matching_textures_id(texture.id(), |_att, _name| {
            self.update_status();
        });
    }

    pub(crate) fn set_read_buffer(&self, buffer: u32) -> WebGLResult<()> {
        let Some(context) = self.upcast().context() else {
            return Err(WebGLError::ContextLost);
        };

        match buffer {
            constants::NONE => {},
            _ if context.valid_color_attachment_enum(buffer) => {},
            _ => return Err(WebGLError::InvalidOperation),
        };

        self.color_read_buffer.set(buffer);
        context.send_command(WebGLCommand::ReadBuffer(buffer));
        Ok(())
    }

    pub(crate) fn set_draw_buffers(&self, buffers: Vec<u32>) -> WebGLResult<()> {
        let Some(context) = self.upcast().context() else {
            return Err(WebGLError::ContextLost);
        };

        if buffers.len() > context.limits().max_draw_buffers as usize {
            return Err(WebGLError::InvalidValue);
        }

        let enums_valid = buffers
            .iter()
            .all(|&val| val == constants::NONE || context.valid_color_attachment_enum(val));
        if !enums_valid {
            return Err(WebGLError::InvalidEnum);
        }

        let values_valid = buffers.iter().enumerate().all(|(i, &val)| {
            val == constants::NONE || val == (constants::COLOR_ATTACHMENT0 + i as u32)
        });
        if !values_valid {
            return Err(WebGLError::InvalidOperation);
        }

        self.color_draw_buffers.borrow_mut().clone_from(&buffers);
        context.send_command(WebGLCommand::DrawBuffers(buffers));
        Ok(())
    }

    pub(crate) fn read_buffer(&self) -> u32 {
        self.color_read_buffer.get()
    }

    pub(crate) fn draw_buffer_i(&self, index: usize) -> u32 {
        let buffers = &*self.color_draw_buffers.borrow();
        *buffers.get(index).unwrap_or(&constants::NONE)
    }

    pub(crate) fn target(&self) -> Option<u32> {
        self.target.get()
    }
}

static INTERESTING_ATTACHMENT_POINTS: &[u32] = &[
    constants::DEPTH_ATTACHMENT,
    constants::STENCIL_ATTACHMENT,
    constants::DEPTH_STENCIL_ATTACHMENT,
];
