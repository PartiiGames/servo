/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
// skip-unless CARGO_FEATURE_WEBGL
/*
 * WebGL IDL definitions from the Khronos specification:
 * https://registry.khronos.org/webgl/extensions/WEBGL_compressed_texture_astc/
 */

[LegacyNoInterfaceObject, Exposed=(Window,Worker)]
interface WEBGLCompressedTextureASTC {
  const GLenum COMPRESSED_RGBA_ASTC_4x4_KHR = 0x93B0;
  const GLenum COMPRESSED_RGBA_ASTC_5x4_KHR = 0x93B1;
  const GLenum COMPRESSED_RGBA_ASTC_5x5_KHR = 0x93B2;
  const GLenum COMPRESSED_RGBA_ASTC_6x5_KHR = 0x93B3;
  const GLenum COMPRESSED_RGBA_ASTC_6x6_KHR = 0x93B4;
  const GLenum COMPRESSED_RGBA_ASTC_8x5_KHR = 0x93B5;
  const GLenum COMPRESSED_RGBA_ASTC_8x6_KHR = 0x93B6;
  const GLenum COMPRESSED_RGBA_ASTC_8x8_KHR = 0x93B7;
  const GLenum COMPRESSED_RGBA_ASTC_10x5_KHR = 0x93B8;
  const GLenum COMPRESSED_RGBA_ASTC_10x6_KHR = 0x93B9;
  const GLenum COMPRESSED_RGBA_ASTC_10x8_KHR = 0x93BA;
  const GLenum COMPRESSED_RGBA_ASTC_10x10_KHR = 0x93BB;
  const GLenum COMPRESSED_RGBA_ASTC_12x10_KHR = 0x93BC;
  const GLenum COMPRESSED_RGBA_ASTC_12x12_KHR = 0x93BD;
  const GLenum COMPRESSED_SRGB8_ALPHA8_ASTC_4x4_KHR = 0x93D0;
  const GLenum COMPRESSED_SRGB8_ALPHA8_ASTC_5x4_KHR = 0x93D1;
  const GLenum COMPRESSED_SRGB8_ALPHA8_ASTC_5x5_KHR = 0x93D2;
  const GLenum COMPRESSED_SRGB8_ALPHA8_ASTC_6x5_KHR = 0x93D3;
  const GLenum COMPRESSED_SRGB8_ALPHA8_ASTC_6x6_KHR = 0x93D4;
  const GLenum COMPRESSED_SRGB8_ALPHA8_ASTC_8x5_KHR = 0x93D5;
  const GLenum COMPRESSED_SRGB8_ALPHA8_ASTC_8x6_KHR = 0x93D6;
  const GLenum COMPRESSED_SRGB8_ALPHA8_ASTC_8x8_KHR = 0x93D7;
  const GLenum COMPRESSED_SRGB8_ALPHA8_ASTC_10x5_KHR = 0x93D8;
  const GLenum COMPRESSED_SRGB8_ALPHA8_ASTC_10x6_KHR = 0x93D9;
  const GLenum COMPRESSED_SRGB8_ALPHA8_ASTC_10x8_KHR = 0x93DA;
  const GLenum COMPRESSED_SRGB8_ALPHA8_ASTC_10x10_KHR = 0x93DB;
  const GLenum COMPRESSED_SRGB8_ALPHA8_ASTC_12x10_KHR = 0x93DC;
  const GLenum COMPRESSED_SRGB8_ALPHA8_ASTC_12x12_KHR = 0x93DD;

  // Profiles: "ldr", plus "hdr" where the GL supports it.
  sequence<DOMString> getSupportedProfiles();
}; // interface WEBGL_compressed_texture_astc
