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

/// EXT_float_blend: blending into 32-bit float colour buffers. Desktop GL (3.0+) always blends
/// them; GLES needs GL_EXT_float_blend. Like other browsers, blending float32 buffers is allowed
/// whenever the extension is supported (the conformance tests read getSupportedExtensions), so
/// this only reports it.
#[dom_struct]
pub(crate) struct EXTFloatBlend {
    reflector_: Reflector,
}

impl EXTFloatBlend {
    fn new_inherited() -> EXTFloatBlend {
        Self {
            reflector_: Reflector::new(),
        }
    }
}

impl WebGLExtension for EXTFloatBlend {
    type Extension = EXTFloatBlend;
    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<EXTFloatBlend> {
        reflect_dom_object_with_cx(Box::new(Self::new_inherited()), &*ctx.global(), cx)
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::All
    }

    fn is_supported(ext: &WebGLExtensions) -> bool {
        !ext.is_gles() || ext.supports_gl_extension("GL_EXT_float_blend")
    }

    fn enable(_ext: &WebGLExtensions) {}

    fn name() -> &'static str {
        "EXT_float_blend"
    }
}
