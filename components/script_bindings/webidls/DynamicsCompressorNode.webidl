/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * The origin of this IDL file is
 * https://webaudio.github.io/web-audio-api/#DynamicsCompressorNode
 */

dictionary DynamicsCompressorOptions : AudioNodeOptions {
  float attack = 0.003;
  float knee = 30;
  float ratio = 12;
  float release = 0.25;
  float threshold = -24;
};

[Exposed=Window]
interface DynamicsCompressorNode : AudioNode {
  [Throws] constructor(BaseAudioContext context,
                       optional DynamicsCompressorOptions options = {});
  readonly attribute AudioParam threshold;
  readonly attribute AudioParam knee;
  readonly attribute AudioParam ratio;
  readonly attribute float reduction;
  readonly attribute AudioParam attack;
  readonly attribute AudioParam release;
};
