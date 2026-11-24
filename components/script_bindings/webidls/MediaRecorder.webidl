/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/mediacapture-record/#mediarecorder-api
// Partii: video tracks only (VP8 in WebM), servo-media-recorder.

[Exposed=Window]
interface MediaRecorder : EventTarget {
  [Throws] constructor(MediaStream stream, optional MediaRecorderOptions options = {});
  readonly attribute MediaStream stream;
  readonly attribute DOMString mimeType;
  readonly attribute RecordingState state;
  attribute EventHandler onstart;
  attribute EventHandler onstop;
  attribute EventHandler ondataavailable;
  attribute EventHandler onpause;
  attribute EventHandler onresume;
  attribute EventHandler onerror;
  readonly attribute unsigned long videoBitsPerSecond;
  readonly attribute unsigned long audioBitsPerSecond;
  readonly attribute BitrateMode audioBitrateMode;

  [Throws] undefined start(optional unsigned long timeslice);
  [Throws] undefined stop();
  [Throws] undefined pause();
  [Throws] undefined resume();
  [Throws] undefined requestData();

  static boolean isTypeSupported(DOMString type);
};

dictionary MediaRecorderOptions {
  DOMString mimeType = "";
  unsigned long audioBitsPerSecond;
  unsigned long videoBitsPerSecond;
  unsigned long bitsPerSecond;
  BitrateMode audioBitrateMode = "variable";
  DOMHighResTimeStamp videoKeyFrameIntervalDuration;
  unsigned long videoKeyFrameIntervalCount;
};

enum BitrateMode {
  "constant",
  "variable"
};

enum RecordingState {
  "inactive",
  "recording",
  "paused"
};
