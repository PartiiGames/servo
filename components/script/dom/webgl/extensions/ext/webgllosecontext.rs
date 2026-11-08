/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::codegen::Bindings::WEBGLLoseContextBinding::WEBGLLoseContextMethods;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;

/// <https://registry.khronos.org/webgl/extensions/WEBGL_lose_context/>
#[dom_struct]
pub(crate) struct WEBGLLoseContext {
    reflector_: Reflector,
    ctx: Dom<WebGLRenderingContext>,
}

impl WEBGLLoseContext {
    fn new_inherited(ctx: &WebGLRenderingContext) -> WEBGLLoseContext {
        Self {
            reflector_: Reflector::new(),
            ctx: Dom::from_ref(ctx),
        }
    }
}

impl WebGLExtension for WEBGLLoseContext {
    type Extension = WEBGLLoseContext;
    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<WEBGLLoseContext> {
        reflect_dom_object(
            cx,
            Box::new(WEBGLLoseContext::new_inherited(ctx)),
            &*ctx.global(),
        )
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::All
    }

    fn is_supported(_ext: &WebGLExtensions) -> bool {
        true
    }

    fn enable(_ext: &WebGLExtensions) {}

    fn name() -> &'static str {
        "WEBGL_lose_context"
    }
}

impl WEBGLLoseContextMethods<crate::DomTypeHolder> for WEBGLLoseContext {
    /// <https://registry.khronos.org/webgl/extensions/WEBGL_lose_context/>
    fn LoseContext(&self) {
        self.ctx.lose_context_from_extension();
    }

    /// <https://registry.khronos.org/webgl/extensions/WEBGL_lose_context/>
    fn RestoreContext(&self) {
        self.ctx.restore_context_from_extension();
    }
}
