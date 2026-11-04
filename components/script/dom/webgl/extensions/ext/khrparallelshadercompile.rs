/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};

use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;

/// <https://registry.khronos.org/webgl/extensions/KHR_parallel_shader_compile/>
///
/// compileShader and linkProgram wait for the WebGL thread's answer, so by the time a page can
/// ask, COMPLETION_STATUS_KHR is always true. The extension spec allows exactly that; what pages
/// (Babylon, three's `compileAsync`) need is the extension object and the query to exist.
#[dom_struct]
pub(crate) struct KHRParallelShaderCompile {
    reflector_: Reflector,
}

impl KHRParallelShaderCompile {
    fn new_inherited() -> KHRParallelShaderCompile {
        Self {
            reflector_: Reflector::new(),
        }
    }
}

impl WebGLExtension for KHRParallelShaderCompile {
    type Extension = KHRParallelShaderCompile;
    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<KHRParallelShaderCompile> {
        reflect_dom_object_with_cx(Box::new(Self::new_inherited()), &*ctx.global(), cx)
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::All
    }

    fn is_supported(_ext: &WebGLExtensions) -> bool {
        true
    }

    fn enable(_ext: &WebGLExtensions) {}

    fn name() -> &'static str {
        "KHR_parallel_shader_compile"
    }
}
