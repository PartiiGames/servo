/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! macOS cameras through AVFoundation (to come: AVCaptureSession + AVCaptureVideoDataOutput in 32BGRA).
//! Until then macOS has only the fake camera.

use std::sync::Arc;

use super::{Camera, CameraInfo, CaptureError, FrameSink, Want};

pub(super) fn list() -> Vec<CameraInfo> {
    vec![]
}

pub(super) fn open(_: CameraInfo, _: &Want, _: Arc<dyn FrameSink>) -> Result<Camera, CaptureError> {
    Err(CaptureError::NotFound)
}
