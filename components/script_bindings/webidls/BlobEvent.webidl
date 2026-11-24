/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/mediacapture-record/#blobevent-section

[Exposed=Window]
interface BlobEvent : Event {
  [Throws] constructor(DOMString type, BlobEventInit eventInitDict);
  [SameObject] readonly attribute Blob data;
  readonly attribute DOMHighResTimeStamp timecode;
};

dictionary BlobEventInit : EventInit {
  required Blob data;
  DOMHighResTimeStamp timecode;
};
