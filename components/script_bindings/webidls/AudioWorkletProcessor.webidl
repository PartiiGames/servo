/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * The origin of this IDL file is
 * https://webaudio.github.io/web-audio-api/#AudioWorkletProcessor
 */

[Exposed=AudioWorklet, Pref="dom_audioworklet_enabled"]
interface AudioWorkletProcessor {
  [Throws] constructor();
  readonly attribute MessagePort port;
};

dictionary AudioParamDescriptor {
  required DOMString name;
  float defaultValue = 0;
  float minValue = -3.4028235e38;
  float maxValue = 3.4028235e38;
  AutomationRate automationRate = "a-rate";
};
