/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
// skip-unless CARGO_FEATURE_WEBGL
/*
 * WebGL IDL definitions from the Khronos specification:
 * https://registry.khronos.org/webgl/extensions/EXT_texture_compression_bptc/
 */

[LegacyNoInterfaceObject, Exposed=Window]
interface EXTTextureCompressionBPTC {
  const GLenum COMPRESSED_RGBA_BPTC_UNORM_EXT = 0x8E8C;
  const GLenum COMPRESSED_SRGB_ALPHA_BPTC_UNORM_EXT = 0x8E8D;
  const GLenum COMPRESSED_RGB_BPTC_SIGNED_FLOAT_EXT = 0x8E8E;
  const GLenum COMPRESSED_RGB_BPTC_UNSIGNED_FLOAT_EXT = 0x8E8F;
}; // interface EXT_texture_compression_bptc
