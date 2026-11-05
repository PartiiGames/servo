/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use servo_canvas_traits::webgl::WebGLError::InvalidOperation;
use servo_canvas_traits::webgl::{WebGLSLVersion, WebGLVersion};

use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::codegen::Bindings::WEBGLDrawInstancedBaseVertexBaseInstanceBinding::WEBGLDrawInstancedBaseVertexBaseInstanceMethods;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;

/// <https://registry.khronos.org/webgl/extensions/WEBGL_draw_instanced_base_vertex_base_instance/>
#[dom_struct]
pub(crate) struct WEBGLDrawInstancedBaseVertexBaseInstance {
    reflector_: Reflector,
    ctx: Dom<WebGLRenderingContext>,
}

impl WEBGLDrawInstancedBaseVertexBaseInstance {
    fn new_inherited(ctx: &WebGLRenderingContext) -> Self {
        Self {
            reflector_: Reflector::new(),
            ctx: Dom::from_ref(ctx),
        }
    }
}

/// glDrawArraysInstancedBaseInstance / glDrawElementsInstancedBaseVertexBaseInstance: GL 4.2,
/// ARB_base_instance, or ANGLE's / EXT's GLES extensions.
pub(crate) fn base_vertex_base_instance_supported(ext: &WebGLExtensions) -> bool {
    ext.supports_any_gl_extension(&[
        "GL_ARB_base_instance",
        "GL_EXT_base_instance",
        "GL_ANGLE_base_vertex_base_instance",
    ]) ||
        (!ext.is_gles() &&
            ext.is_min_glsl_version_satisfied(WebGLSLVersion {
                major: 4,
                minor: 20,
            }))
}

impl WebGLExtension for WEBGLDrawInstancedBaseVertexBaseInstance {
    type Extension = Self;
    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<Self> {
        reflect_dom_object(cx, Box::new(Self::new_inherited(ctx)), &*ctx.global())
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::Specific(WebGLVersion::WebGL2)
    }

    fn is_supported(ext: &WebGLExtensions) -> bool {
        base_vertex_base_instance_supported(ext)
    }

    fn enable(_ext: &WebGLExtensions) {}

    fn name() -> &'static str {
        "WEBGL_draw_instanced_base_vertex_base_instance"
    }
}

impl WEBGLDrawInstancedBaseVertexBaseInstanceMethods<crate::DomTypeHolder>
    for WEBGLDrawInstancedBaseVertexBaseInstance
{
    fn DrawArraysInstancedBaseInstanceWEBGL(
        &self,
        cx: &mut JSContext,
        mode: u32,
        first: i32,
        count: i32,
        instance_count: i32,
        base_instance: u32,
    ) {
        let result = match self.ctx.webgl2() {
            Some(webgl2) => webgl2.draw_arrays_checked_base_instance(
                cx,
                mode,
                first,
                count,
                instance_count,
                base_instance,
            ),
            None => Err(InvalidOperation),
        };
        handle_potential_webgl_error!(self.ctx, result);
    }

    #[allow(clippy::too_many_arguments)]
    fn DrawElementsInstancedBaseVertexBaseInstanceWEBGL(
        &self,
        cx: &mut JSContext,
        mode: u32,
        count: i32,
        type_: u32,
        offset: i64,
        instance_count: i32,
        base_vertex: i32,
        base_instance: u32,
    ) {
        let result = match self.ctx.webgl2() {
            Some(webgl2) => webgl2.draw_elements_checked_base_vertex_base_instance(
                cx,
                mode,
                count,
                type_,
                offset,
                instance_count,
                base_vertex,
                base_instance,
            ),
            None => Err(InvalidOperation),
        };
        handle_potential_webgl_error!(self.ctx, result);
    }
}
