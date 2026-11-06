/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this file,
 * You can obtain one at http://mozilla.org/MPL/2.0/. */

// https://webidl.spec.whatwg.org/#BufferSource
typedef (ArrayBufferView or ArrayBuffer) BufferSource;

// https://webidl.spec.whatwg.org/#AllowSharedBufferSource
// (the spec spells the buffer member SharedArrayBuffer; an [AllowShared] ArrayBuffer is the same set here)
typedef ([AllowShared] ArrayBufferView or [AllowShared] ArrayBuffer) AllowSharedBufferSource;
