/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
// skip-unless CARGO_FEATURE_WEBGL
/*
 * WebGL IDL definitions from the Khronos specification:
 * https://registry.khronos.org/webgl/extensions/EXT_texture_compression_rgtc/
 */

[LegacyNoInterfaceObject, Exposed=(Window,Worker)]
interface EXTTextureCompressionRGTC {
  const GLenum COMPRESSED_RED_RGTC1_EXT = 0x8DBB;
  const GLenum COMPRESSED_SIGNED_RED_RGTC1_EXT = 0x8DBC;
  const GLenum COMPRESSED_RED_GREEN_RGTC2_EXT = 0x8DBD;
  const GLenum COMPRESSED_SIGNED_RED_GREEN_RGTC2_EXT = 0x8DBE;
}; // interface EXT_texture_compression_rgtc
