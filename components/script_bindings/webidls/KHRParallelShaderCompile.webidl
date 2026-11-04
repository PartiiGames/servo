/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
// skip-unless CARGO_FEATURE_WEBGL
/*
 * WebGL IDL definitions from the Khronos specification:
 * https://registry.khronos.org/webgl/extensions/KHR_parallel_shader_compile/
 */

[LegacyNoInterfaceObject, Exposed=(Window,Worker)]
interface KHRParallelShaderCompile {
  const GLenum COMPLETION_STATUS_KHR = 0x91B1;
}; // interface KHR_parallel_shader_compile
