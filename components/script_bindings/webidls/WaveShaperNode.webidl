/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * The origin of this IDL file is
 * https://webaudio.github.io/web-audio-api/#WaveShaperNode
 */

enum OverSampleType {
  "none",
  "2x",
  "4x"
};

dictionary WaveShaperOptions : AudioNodeOptions {
  sequence<float> curve;
  OverSampleType oversample = "none";
};

[Exposed=Window]
interface WaveShaperNode : AudioNode {
  [Throws] constructor(BaseAudioContext context, optional WaveShaperOptions options = {});
  [SetterThrows] attribute Float32Array? curve;
  attribute OverSampleType oversample;
};
