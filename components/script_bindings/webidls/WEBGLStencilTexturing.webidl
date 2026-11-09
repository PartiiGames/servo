/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// skip-unless CARGO_FEATURE_WEBGL
/*
 * WebGL IDL definitions from the Khronos specification:
 * https://registry.khronos.org/webgl/extensions/WEBGL_stencil_texturing/
 */

[LegacyNoInterfaceObject, Exposed=(Window,Worker)]
interface WEBGLStencilTexturing {
  const GLenum DEPTH_STENCIL_TEXTURE_MODE_WEBGL = 0x90EA;
  const GLenum STENCIL_INDEX_WEBGL              = 0x1901;
};
