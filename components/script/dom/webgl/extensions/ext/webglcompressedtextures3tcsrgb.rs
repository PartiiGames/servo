/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_canvas_traits::webgl::TexFormat;

use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::webgl::extensions::ext::webglcompressedtextures3tc::WEBGLCompressedTextureS3TC;
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;
use crate::dom::webgl::webgltexture::{TexCompression, TexCompressionValidation};

/// <https://registry.khronos.org/webgl/extensions/WEBGL_compressed_texture_s3tc_srgb/>
#[dom_struct]
pub(crate) struct WEBGLCompressedTextureS3TCsRGB {
    reflector_: Reflector,
}

impl WEBGLCompressedTextureS3TCsRGB {
    fn new_inherited() -> WEBGLCompressedTextureS3TCsRGB {
        Self {
            reflector_: Reflector::new(),
        }
    }
}

impl WebGLExtension for WEBGLCompressedTextureS3TCsRGB {
    type Extension = WEBGLCompressedTextureS3TCsRGB;
    fn new(
        cx: &mut JSContext,
        ctx: &WebGLRenderingContext,
    ) -> DomRoot<WEBGLCompressedTextureS3TCsRGB> {
        reflect_dom_object_with_cx(Box::new(Self::new_inherited()), &*ctx.global(), cx)
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::All
    }

    /// The S3TC formats plus sRGB: desktop GL has the sRGB S3TC formats with EXT_texture_sRGB
    /// (core since 2.1), GLES with EXT_texture_compression_s3tc_srgb.
    fn is_supported(ext: &WebGLExtensions) -> bool {
        <WEBGLCompressedTextureS3TC as WebGLExtension>::is_supported(ext) &&
            ext.supports_any_gl_extension(&[
                "GL_EXT_texture_sRGB",
                "GL_EXT_texture_compression_s3tc_srgb",
            ])
    }

    fn enable(ext: &WebGLExtensions) {
        let s3tc = |format, bytes_per_block| TexCompression {
            format,
            bytes_per_block,
            block_width: 4,
            block_height: 4,
            validation: TexCompressionValidation::S3TC,
        };
        ext.add_tex_compression_formats(&[
            s3tc(TexFormat::CompressedSrgbS3tcDxt1, 8),
            s3tc(TexFormat::CompressedSrgbAlphaS3tcDxt1, 8),
            s3tc(TexFormat::CompressedSrgbAlphaS3tcDxt3, 16),
            s3tc(TexFormat::CompressedSrgbAlphaS3tcDxt5, 16),
        ]);
    }

    fn name() -> &'static str {
        "WEBGL_compressed_texture_s3tc_srgb"
    }
}
