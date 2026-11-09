/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * The origin of this IDL file is
 * https://webaudio.github.io/web-audio-api/#dom-audiocontext
 */

enum AudioContextLatencyCategory {
  "balanced",
  "interactive",
  "playback"
};

enum AudioSinkType {
  "none"
};

dictionary AudioSinkOptions {
  required AudioSinkType type;
};

// https://webaudio.github.io/web-audio-api/#AudioSinkInfo
[Exposed=Window]
interface AudioSinkInfo {
  readonly attribute AudioSinkType type;
};

dictionary AudioContextOptions {
  (AudioContextLatencyCategory or double) latencyHint = "interactive";
  float sampleRate;
  (DOMString or AudioSinkOptions) sinkId;
};

dictionary AudioTimestamp {
  double contextTime;
  DOMHighResTimeStamp performanceTime;
};

[Exposed=Window]
interface AudioContext : BaseAudioContext {
  [Throws] constructor(optional AudioContextOptions contextOptions = {});
  readonly attribute double baseLatency;
  readonly attribute double outputLatency;
  [SecureContext] readonly attribute (DOMString or AudioSinkInfo) sinkId;
  attribute EventHandler onsinkchange;
  attribute EventHandler onerror;

  AudioTimestamp getOutputTimestamp();

  Promise<undefined> suspend();
  Promise<undefined> close();
  [SecureContext] Promise<undefined> setSinkId((DOMString or AudioSinkOptions) sinkId);

  [Throws] MediaElementAudioSourceNode createMediaElementSource(HTMLMediaElement mediaElement);
  [Throws] MediaStreamAudioSourceNode createMediaStreamSource(MediaStream mediaStream);
  [Throws] MediaStreamTrackAudioSourceNode createMediaStreamTrackSource(MediaStreamTrack mediaStreamTrack);
  [Throws] MediaStreamAudioDestinationNode createMediaStreamDestination();
};
