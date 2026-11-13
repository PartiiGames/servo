/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_canvas_traits::webgl::WebGLError;

use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::codegen::Bindings::WEBGLLoseContextBinding::WEBGLLoseContextMethods;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;

/// Partii: <https://registry.khronos.org/webgl/extensions/WEBGL_lose_context/>.
///
/// loseContext() frees the GL context and everything made in it at once (see
/// `WebGLRenderingContext::lose_context`): engines call it when they throw a context away (Phaser's
/// and three.js's teardown), and without it each dead context's textures waited for a GC, which
/// doesn't see that memory (rocket-to-heaven: +70-90 MB per round until the next GC that found it
/// unreachable). restoreContext() generates INVALID_OPERATION when the context isn't lost, as the
/// spec says; restoring a lost context isn't implemented (the context stays lost, and no
/// `webglcontextrestored` is fired). `PARTII_WEBGL_LOSE_CONTEXT=0` hides the extension.
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

impl WEBGLLoseContextMethods<crate::DomTypeHolder> for WEBGLLoseContext {
    /// <https://registry.khronos.org/webgl/extensions/WEBGL_lose_context/>
    fn LoseContext(&self) {
        self.ctx.lose_context();
    }

    /// <https://registry.khronos.org/webgl/extensions/WEBGL_lose_context/>
    fn RestoreContext(&self) {
        if !self.ctx.is_lost() {
            self.ctx.webgl_error(WebGLError::InvalidOperation);
        }
    }
}

impl WebGLExtension for WEBGLLoseContext {
    type Extension = WEBGLLoseContext;
    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<WEBGLLoseContext> {
        reflect_dom_object_with_cx(Box::new(Self::new_inherited(ctx)), &*ctx.global(), cx)
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::All
    }

    fn is_supported(_ext: &WebGLExtensions) -> bool {
        static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
            std::env::var("PARTII_WEBGL_LOSE_CONTEXT").as_deref() != Ok("0")
        });
        *ON
    }

    fn enable(_ext: &WebGLExtensions) {}

    fn name() -> &'static str {
        "WEBGL_lose_context"
    }
}
