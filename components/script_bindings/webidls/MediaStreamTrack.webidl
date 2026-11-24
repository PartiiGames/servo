/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack

[Exposed=Window]
interface MediaStreamTrack : EventTarget {
    readonly        attribute DOMString kind;
    readonly        attribute DOMString id;
    readonly        attribute DOMString label;
                    attribute boolean enabled;
    readonly        attribute boolean muted;
                    attribute EventHandler onmute;
                    attribute EventHandler onunmute;
    readonly        attribute MediaStreamTrackState readyState;
                    attribute EventHandler onended;
    MediaStreamTrack clone();
    undefined stop();
    MediaTrackCapabilities getCapabilities();
    MediaTrackConstraints getConstraints();
    MediaTrackSettings getSettings();
    Promise<undefined> applyConstraints(optional MediaTrackConstraints constraints = {});
};

enum MediaStreamTrackState {
  "live",
  "ended"
};

dictionary MediaTrackSettings {
             unsigned long width;
             unsigned long height;
             double aspectRatio;
             double frameRate;
             DOMString facingMode;
             DOMString resizeMode;
             unsigned long sampleRate;
             unsigned long sampleSize;
             boolean echoCancellation;
             boolean autoGainControl;
             boolean noiseSuppression;
             double latency;
             unsigned long channelCount;
             DOMString deviceId;
             DOMString groupId;
};

dictionary MediaTrackCapabilities {
             ULongRange width;
             ULongRange height;
             DoubleRange aspectRatio;
             DoubleRange frameRate;
             sequence<DOMString> facingMode;
             sequence<DOMString> resizeMode;
             ULongRange sampleRate;
             ULongRange sampleSize;
             sequence<boolean> echoCancellation;
             sequence<boolean> autoGainControl;
             sequence<boolean> noiseSuppression;
             ULongRange channelCount;
             DOMString deviceId;
             DOMString groupId;
};
