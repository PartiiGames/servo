/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use servo_canvas_traits::webgl::{WebGLSLVersion, WebGLVersion};

use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::codegen::Bindings::OESDrawBuffersIndexedBinding::OESDrawBuffersIndexedMethods;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;

/// <https://registry.khronos.org/webgl/extensions/OES_draw_buffers_indexed/>: per-draw-buffer
/// blend enables, equations, functions and colour write masks. WebGL 2 only, on desktop GL 4.0
/// (or ARB_draw_buffers_blend) and GLES 3.2 (or EXT_/OES_draw_buffers_indexed).
#[dom_struct]
pub(crate) struct OESDrawBuffersIndexed {
    reflector_: Reflector,
    ctx: Dom<WebGLRenderingContext>,
}

impl OESDrawBuffersIndexed {
    fn new_inherited(ctx: &WebGLRenderingContext) -> Self {
        Self {
            reflector_: Reflector::new(),
            ctx: Dom::from_ref(ctx),
        }
    }
}

impl WebGLExtension for OESDrawBuffersIndexed {
    type Extension = Self;
    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<Self> {
        reflect_dom_object(cx, Box::new(Self::new_inherited(ctx)), &*ctx.global())
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::Specific(WebGLVersion::WebGL2)
    }

    fn is_supported(ext: &WebGLExtensions) -> bool {
        ext.supports_any_gl_extension(&[
            "GL_ARB_draw_buffers_blend",
            "GL_EXT_draw_buffers_indexed",
            "GL_OES_draw_buffers_indexed",
        ]) || (!ext.is_gles()
            && ext.is_min_glsl_version_satisfied(WebGLSLVersion { major: 4, minor: 0 }))
    }

    fn enable(_ext: &WebGLExtensions) {}

    fn name() -> &'static str {
        "OES_draw_buffers_indexed"
    }
}

impl OESDrawBuffersIndexedMethods<crate::DomTypeHolder> for OESDrawBuffersIndexed {
    fn EnableiOES(&self, target: u32, index: u32) {
        self.ctx.set_blend_indexed(target, index, true)
    }

    fn DisableiOES(&self, target: u32, index: u32) {
        self.ctx.set_blend_indexed(target, index, false)
    }

    fn BlendEquationiOES(&self, buf: u32, mode: u32) {
        self.ctx.blend_equation_indexed(buf, mode, None)
    }

    fn BlendEquationSeparateiOES(&self, buf: u32, mode_rgb: u32, mode_alpha: u32) {
        self.ctx
            .blend_equation_indexed(buf, mode_rgb, Some(mode_alpha))
    }

    fn BlendFunciOES(&self, buf: u32, src: u32, dst: u32) {
        self.ctx.blend_func_indexed(buf, src, dst, None)
    }

    fn BlendFuncSeparateiOES(
        &self,
        buf: u32,
        src_rgb: u32,
        dst_rgb: u32,
        src_alpha: u32,
        dst_alpha: u32,
    ) {
        self.ctx
            .blend_func_indexed(buf, src_rgb, dst_rgb, Some((src_alpha, dst_alpha)))
    }

    fn ColorMaskiOES(&self, buf: u32, r: bool, g: bool, b: bool, a: bool) {
        match self.ctx.webgl2() {
            Some(webgl2) => webgl2.color_mask_indexed(buf, [r, g, b, a]),
            None => {
                self.ctx.color_mask_indexed(buf, [r, g, b, a]);
            },
        }
    }
}
