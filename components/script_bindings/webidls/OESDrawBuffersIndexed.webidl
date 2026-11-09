/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// skip-unless CARGO_FEATURE_WEBGL
/*
 * WebGL IDL definitions from the Khronos specification:
 * https://registry.khronos.org/webgl/extensions/OES_draw_buffers_indexed/
 */

[LegacyNoInterfaceObject, Exposed=(Window,Worker)]
interface OESDrawBuffersIndexed {
  undefined enableiOES(GLenum target, GLuint index);

  undefined disableiOES(GLenum target, GLuint index);

  undefined blendEquationiOES(GLuint buf, GLenum mode);

  undefined blendEquationSeparateiOES(GLuint buf,
                                      GLenum modeRGB, GLenum modeAlpha);

  undefined blendFunciOES(GLuint buf,
                          GLenum src, GLenum dst);

  undefined blendFuncSeparateiOES(GLuint buf,
                                  GLenum srcRGB, GLenum dstRGB,
                                  GLenum srcAlpha, GLenum dstAlpha);

  undefined colorMaskiOES(GLuint buf,
                          GLboolean r, GLboolean g, GLboolean b, GLboolean a);
};
