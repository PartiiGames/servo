/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * The origin of this IDL file is
 * https://webaudio.github.io/web-audio-api/#AudioWorkletNode
 */

[Exposed=Window, SecureContext, Pref="dom_audioworklet_enabled"]
interface AudioWorkletNode : AudioNode {
  [Throws] constructor(BaseAudioContext context, DOMString name,
                       optional AudioWorkletNodeOptions options = {});
  readonly attribute AudioParamMap parameters;
  readonly attribute MessagePort port;
  attribute EventHandler onprocessorerror;
};

dictionary AudioWorkletNodeOptions : AudioNodeOptions {
  unsigned long numberOfInputs = 1;
  unsigned long numberOfOutputs = 1;
  sequence<unsigned long> outputChannelCount;
  record<DOMString, double> parameterData;
  object processorOptions;
};
