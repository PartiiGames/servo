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
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;
use crate::dom::webgl::webgltexture::{TexCompression, TexCompressionValidation};

/// <https://registry.khronos.org/webgl/extensions/EXT_texture_compression_bptc/>
#[dom_struct]
pub(crate) struct EXTTextureCompressionBPTC {
    reflector_: Reflector,
}

impl EXTTextureCompressionBPTC {
    fn new_inherited() -> EXTTextureCompressionBPTC {
        Self {
            reflector_: Reflector::new(),
        }
    }
}

impl WebGLExtension for EXTTextureCompressionBPTC {
    type Extension = EXTTextureCompressionBPTC;
    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<EXTTextureCompressionBPTC> {
        reflect_dom_object_with_cx(Box::new(Self::new_inherited()), &*ctx.global(), cx)
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::All
    }

    fn is_supported(ext: &WebGLExtensions) -> bool {
        ext.supports_any_gl_extension(&[
            "GL_EXT_texture_compression_bptc",
            "GL_ARB_texture_compression_bptc",
        ])
    }

    fn enable(ext: &WebGLExtensions) {
        ext.add_tex_compression_formats(&[
            TexCompression {
                format: TexFormat::CompressedRgbaBptcUnorm,
                bytes_per_block: 16,
                block_width: 4,
                block_height: 4,
                validation: TexCompressionValidation::BaseLevelBlockMultiple,
            },
            TexCompression {
                format: TexFormat::CompressedSrgbAlphaBptcUnorm,
                bytes_per_block: 16,
                block_width: 4,
                block_height: 4,
                validation: TexCompressionValidation::BaseLevelBlockMultiple,
            },
            TexCompression {
                format: TexFormat::CompressedRgbBptcSignedFloat,
                bytes_per_block: 16,
                block_width: 4,
                block_height: 4,
                validation: TexCompressionValidation::BaseLevelBlockMultiple,
            },
            TexCompression {
                format: TexFormat::CompressedRgbBptcUnsignedFloat,
                bytes_per_block: 16,
                block_width: 4,
                block_height: 4,
                validation: TexCompressionValidation::BaseLevelBlockMultiple,
            },
        ]);
    }

    fn name() -> &'static str {
        "EXT_texture_compression_bptc"
    }
}
