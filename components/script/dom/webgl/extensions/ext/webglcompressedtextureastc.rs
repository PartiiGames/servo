/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_canvas_traits::webgl::TexFormat;

use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::codegen::Bindings::WEBGLCompressedTextureASTCBinding::WEBGLCompressedTextureASTCMethods;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;
use crate::dom::webgl::webgltexture::{TexCompression, TexCompressionValidation};

/// <https://registry.khronos.org/webgl/extensions/WEBGL_compressed_texture_astc/>
#[dom_struct]
pub(crate) struct WEBGLCompressedTextureASTC {
    reflector_: Reflector,
    hdr: bool,
}

impl WEBGLCompressedTextureASTC {
    fn new_inherited(hdr: bool) -> WEBGLCompressedTextureASTC {
        Self {
            reflector_: Reflector::new(),
            hdr,
        }
    }
}

impl WebGLExtension for WEBGLCompressedTextureASTC {
    type Extension = WEBGLCompressedTextureASTC;
    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<WEBGLCompressedTextureASTC> {
        let hdr = ctx
            .extension_manager()
            .supports_gl_extension("GL_KHR_texture_compression_astc_hdr");
        reflect_dom_object_with_cx(Box::new(Self::new_inherited(hdr)), &*ctx.global(), cx)
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::All
    }

    fn is_supported(ext: &WebGLExtensions) -> bool {
        ext.supports_gl_extension("GL_KHR_texture_compression_astc_ldr")
    }

    fn enable(ext: &WebGLExtensions) {
        ext.add_tex_compression_formats(&[
            TexCompression {
                format: TexFormat::CompressedRgbaAstc4x4,
                bytes_per_block: 16,
                block_width: 4,
                block_height: 4,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedRgbaAstc5x4,
                bytes_per_block: 16,
                block_width: 5,
                block_height: 4,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedRgbaAstc5x5,
                bytes_per_block: 16,
                block_width: 5,
                block_height: 5,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedRgbaAstc6x5,
                bytes_per_block: 16,
                block_width: 6,
                block_height: 5,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedRgbaAstc6x6,
                bytes_per_block: 16,
                block_width: 6,
                block_height: 6,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedRgbaAstc8x5,
                bytes_per_block: 16,
                block_width: 8,
                block_height: 5,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedRgbaAstc8x6,
                bytes_per_block: 16,
                block_width: 8,
                block_height: 6,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedRgbaAstc8x8,
                bytes_per_block: 16,
                block_width: 8,
                block_height: 8,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedRgbaAstc10x5,
                bytes_per_block: 16,
                block_width: 10,
                block_height: 5,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedRgbaAstc10x6,
                bytes_per_block: 16,
                block_width: 10,
                block_height: 6,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedRgbaAstc10x8,
                bytes_per_block: 16,
                block_width: 10,
                block_height: 8,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedRgbaAstc10x10,
                bytes_per_block: 16,
                block_width: 10,
                block_height: 10,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedRgbaAstc12x10,
                bytes_per_block: 16,
                block_width: 12,
                block_height: 10,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedRgbaAstc12x12,
                bytes_per_block: 16,
                block_width: 12,
                block_height: 12,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedSrgb8Alpha8Astc4x4,
                bytes_per_block: 16,
                block_width: 4,
                block_height: 4,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedSrgb8Alpha8Astc5x4,
                bytes_per_block: 16,
                block_width: 5,
                block_height: 4,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedSrgb8Alpha8Astc5x5,
                bytes_per_block: 16,
                block_width: 5,
                block_height: 5,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedSrgb8Alpha8Astc6x5,
                bytes_per_block: 16,
                block_width: 6,
                block_height: 5,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedSrgb8Alpha8Astc6x6,
                bytes_per_block: 16,
                block_width: 6,
                block_height: 6,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedSrgb8Alpha8Astc8x5,
                bytes_per_block: 16,
                block_width: 8,
                block_height: 5,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedSrgb8Alpha8Astc8x6,
                bytes_per_block: 16,
                block_width: 8,
                block_height: 6,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedSrgb8Alpha8Astc8x8,
                bytes_per_block: 16,
                block_width: 8,
                block_height: 8,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedSrgb8Alpha8Astc10x5,
                bytes_per_block: 16,
                block_width: 10,
                block_height: 5,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedSrgb8Alpha8Astc10x6,
                bytes_per_block: 16,
                block_width: 10,
                block_height: 6,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedSrgb8Alpha8Astc10x8,
                bytes_per_block: 16,
                block_width: 10,
                block_height: 8,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedSrgb8Alpha8Astc10x10,
                bytes_per_block: 16,
                block_width: 10,
                block_height: 10,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedSrgb8Alpha8Astc12x10,
                bytes_per_block: 16,
                block_width: 12,
                block_height: 10,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
            TexCompression {
                format: TexFormat::CompressedSrgb8Alpha8Astc12x12,
                bytes_per_block: 16,
                block_width: 12,
                block_height: 12,
                validation: TexCompressionValidation::SubImageBlockAligned,
            },
        ]);
    }

    fn name() -> &'static str {
        "WEBGL_compressed_texture_astc"
    }
}

impl WEBGLCompressedTextureASTCMethods<crate::DomTypeHolder> for WEBGLCompressedTextureASTC {
    /// <https://registry.khronos.org/webgl/extensions/WEBGL_compressed_texture_astc/>
    fn GetSupportedProfiles(&self) -> Vec<DOMString> {
        let mut profiles = vec![DOMString::from("ldr")];
        if self.hdr {
            profiles.push(DOMString::from("hdr"));
        }
        profiles
    }
}
