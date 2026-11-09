/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_canvas_traits::webgl::{WebGLSLVersion, WebGLVersion};

use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::codegen::Bindings::WEBGLStencilTexturingBinding::WEBGLStencilTexturingConstants;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;

/// <https://registry.khronos.org/webgl/extensions/WEBGL_stencil_texturing/>: the
/// DEPTH_STENCIL_TEXTURE_MODE texture parameter, so a depth-stencil texture can be sampled as
/// its stencil index (an unsigned integer texture). WebGL 2 only, on desktop GL 4.3 (or
/// ARB_stencil_texturing); GLES 3.1 has it in core.
#[dom_struct]
pub(crate) struct WEBGLStencilTexturing {
    reflector_: Reflector,
}

impl WEBGLStencilTexturing {
    fn new_inherited() -> Self {
        Self {
            reflector_: Reflector::new(),
        }
    }
}

impl WebGLExtension for WEBGLStencilTexturing {
    type Extension = Self;
    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<Self> {
        reflect_dom_object_with_cx(Box::new(Self::new_inherited()), &*ctx.global(), cx)
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::Specific(WebGLVersion::WebGL2)
    }

    fn is_supported(ext: &WebGLExtensions) -> bool {
        if ext.is_gles() {
            return ext.is_min_glsl_version_satisfied(WebGLSLVersion {
                major: 3,
                minor: 10,
            });
        }
        ext.supports_gl_extension("GL_ARB_stencil_texturing")
            || ext.is_min_glsl_version_satisfied(WebGLSLVersion {
                major: 4,
                minor: 30,
            })
    }

    fn enable(ext: &WebGLExtensions) {
        ext.enable_get_tex_parameter_name(
            WEBGLStencilTexturingConstants::DEPTH_STENCIL_TEXTURE_MODE_WEBGL,
        );
    }

    fn name() -> &'static str {
        "WEBGL_stencil_texturing"
    }
}
