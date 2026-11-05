/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
// skip-unless CARGO_FEATURE_WEBGL
/*
 * WebGL IDL definitions from the Khronos specification:
 * https://registry.khronos.org/webgl/extensions/WEBGL_draw_instanced_base_vertex_base_instance/
 */

[LegacyNoInterfaceObject, Exposed=(Window,Worker)]
interface WEBGLDrawInstancedBaseVertexBaseInstance {
  undefined drawArraysInstancedBaseInstanceWEBGL(
      GLenum mode, GLint first, GLsizei count,
      GLsizei instanceCount, GLuint baseInstance);
  undefined drawElementsInstancedBaseVertexBaseInstanceWEBGL(
      GLenum mode, GLsizei count, GLenum type, GLintptr offset,
      GLsizei instanceCount, GLint baseVertex, GLuint baseInstance);
};
