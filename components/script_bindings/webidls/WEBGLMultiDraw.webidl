/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
// skip-unless CARGO_FEATURE_WEBGL
/*
 * WebGL IDL definitions from the Khronos specification:
 * https://registry.khronos.org/webgl/extensions/WEBGL_multi_draw/
 */

[LegacyNoInterfaceObject, Exposed=(Window,Worker)]
interface WEBGLMultiDraw {
  undefined multiDrawArraysWEBGL(GLenum mode,
      Int32List firstsList, unsigned long long firstsOffset,
      Int32List countsList, unsigned long long countsOffset,
      GLsizei drawcount);
  undefined multiDrawElementsWEBGL(GLenum mode,
      Int32List countsList, unsigned long long countsOffset,
      GLenum type,
      Int32List offsetsList, unsigned long long offsetsOffset,
      GLsizei drawcount);
  undefined multiDrawArraysInstancedWEBGL(GLenum mode,
      Int32List firstsList, unsigned long long firstsOffset,
      Int32List countsList, unsigned long long countsOffset,
      Int32List instanceCountsList, unsigned long long instanceCountsOffset,
      GLsizei drawcount);
  undefined multiDrawElementsInstancedWEBGL(GLenum mode,
      Int32List countsList, unsigned long long countsOffset,
      GLenum type,
      Int32List offsetsList, unsigned long long offsetsOffset,
      Int32List instanceCountsList, unsigned long long instanceCountsOffset,
      GLsizei drawcount);
};
