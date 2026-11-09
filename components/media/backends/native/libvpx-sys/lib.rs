/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! libvpx's VP8 and VP9 decoders, statically linked (build.rs builds the vendored libvpx/, BSD-3-Clause:
//! libvpx/LICENSE, libvpx/PATENTS). The declarations live with their one user,
//! servo-media-native's webm/vpx.rs; this crate only provides the library.
