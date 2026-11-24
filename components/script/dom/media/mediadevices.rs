/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! navigator.mediaDevices. Partii (stream "capture"): getUserMedia picks the device from
//! enumerateDevices (deviceId, facingMode), asks the embedder for the camera / microphone
//! permission, opens it through the media backend (the live media registry), and rejects with the
//! errors pages test for: TypeError (nothing asked), NotFoundError (no such device),
//! OverconstrainedError (an exact deviceId that doesn't exist), NotAllowedError (denied),
//! NotReadableError (the device didn't start).

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::reflector::{reflect_dom_object, reflect_dom_object_with_cx};
use servo_config::pref;
use servo_media::ServoMedia;
use servo_media::streams::MediaStreamType;
use servo_media::streams::capture::{Constrain, ConstrainRange, MediaTrackConstraintSet};
use servo_media::streams::device_monitor::MediaDeviceKind as BackendDeviceKind;

use crate::conversions::Convert;
use crate::dom::bindings::codegen::Bindings::MediaDevicesBinding::{
    MediaDevicesMethods, MediaStreamConstraints, MediaTrackConstraintSet as JsConstraintSet,
    MediaTrackSupportedConstraints,
};
use crate::dom::bindings::codegen::Bindings::MediaStreamTrackBinding::MediaStreamTrackMethods;
use crate::dom::bindings::codegen::Bindings::PermissionStatusBinding::{
    PermissionName, PermissionState,
};
use crate::dom::bindings::codegen::UnionTypes::{
    BooleanOrConstrainBooleanParameters as ConstrainBoolean, BooleanOrMediaTrackConstraints,
    ClampedUnsignedLongOrConstrainULongRange as ConstrainULong,
    DoubleOrConstrainDoubleRange as ConstrainDouble, StringOrStringSequence,
    StringOrStringSequenceOrConstrainDOMStringParameters as ConstrainDOMString,
};
use crate::dom::bindings::error::Error;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::domexception::DOMException;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::media::mediadeviceinfo::MediaDeviceInfo;
use crate::dom::media::mediastream::MediaStream;
use crate::dom::media::mediastreamtrack::{CaptureSettings, MediaStreamTrack};
use crate::dom::permissions::request_permission_to_use;
use crate::dom::promise::{Promise, RootedPromise};

#[dom_struct]
pub(crate) struct MediaDevices {
    eventtarget: EventTarget,
}

impl MediaDevices {
    pub(crate) fn new_inherited() -> MediaDevices {
        MediaDevices {
            eventtarget: EventTarget::new_inherited(),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<MediaDevices> {
        reflect_dom_object_with_cx(Box::new(MediaDevices::new_inherited()), global, cx)
    }
}

/// Why getUserMedia failed, as the DOMException name pages check.
enum GumError {
    Type(&'static std::ffi::CStr),
    NotFound(&'static str),
    Overconstrained(&'static str),
    NotAllowed(&'static str),
    NotReadable(&'static str),
}

/// One kind's request, resolved against the devices.
struct Request {
    set: MediaTrackConstraintSet,
    capture: CaptureSettings,
}

fn backend_kind(ty: MediaStreamType) -> BackendDeviceKind {
    match ty {
        MediaStreamType::Audio => BackendDeviceKind::AudioInput,
        MediaStreamType::Video => BackendDeviceKind::VideoInput,
    }
}

/// Chooses the device for one kind (`constraints` is the page's audio or video member).
fn resolve(
    ty: MediaStreamType,
    constraints: &BooleanOrMediaTrackConstraints,
) -> Result<Option<Request>, GumError> {
    let Some((mut set, js)) = convert_constraints(constraints) else {
        return Ok(None);
    };
    let devices: Vec<_> = ServoMedia::get()
        .get_device_monitor()
        .enumerate_devices()
        .unwrap_or_default()
        .into_iter()
        .filter(|d| std::mem::discriminant(&d.kind) == std::mem::discriminant(&backend_kind(ty)))
        .collect();
    let missing = match ty {
        MediaStreamType::Audio => "there is no microphone",
        MediaStreamType::Video => "there is no camera",
    };
    if devices.is_empty() {
        return Err(GumError::NotFound(missing));
    }
    let (exact_id, ideal_id) = js
        .and_then(|c| c.deviceId.as_ref())
        .map(dom_string_constraint)
        .unwrap_or_default();
    let chosen = if let Some(exact) = exact_id {
        devices
            .iter()
            .find(|d| exact.contains(&d.device_id))
            .ok_or(GumError::Overconstrained("no device has that deviceId"))?
    } else {
        ideal_id
            .and_then(|ideal| devices.iter().find(|d| ideal.contains(&d.device_id)))
            .unwrap_or(&devices[0])
    };
    let facing = js
        .and_then(|c| c.facingMode.as_ref())
        .map(dom_string_constraint)
        .and_then(|(exact, ideal)| exact.or(ideal))
        .and_then(|v| v.into_iter().next());
    set.device_id = Some(chosen.device_id.clone());
    set.facing_mode = facing.clone();
    let bool_of = |c: Option<&ConstrainBoolean>| match c? {
        ConstrainBoolean::Boolean(b) => Some(*b),
        ConstrainBoolean::ConstrainBooleanParameters(p) => p.exact.or(p.ideal),
    };
    let ulong_of = |c: &Option<Constrain<u32>>| match c {
        Some(Constrain::Value(v)) => Some(*v),
        Some(Constrain::Range(r)) => r.ideal.or(r.min).or(r.max),
        None => None,
    };
    let capture = CaptureSettings {
        device_id: chosen.device_id.clone(),
        group_id: String::new(),
        facing_mode: if ty == MediaStreamType::Video {
            facing.unwrap_or_default()
        } else {
            String::new()
        },
        echo_cancellation: (ty == MediaStreamType::Audio)
            .then(|| bool_of(js.and_then(|c| c.echoCancellation.as_ref())).unwrap_or(true)),
        noise_suppression: (ty == MediaStreamType::Audio)
            .then(|| bool_of(js.and_then(|c| c.noiseSuppression.as_ref())).unwrap_or(true)),
        auto_gain_control: (ty == MediaStreamType::Audio)
            .then(|| bool_of(js.and_then(|c| c.autoGainControl.as_ref())).unwrap_or(true)),
        want_width: ulong_of(&set.width),
        want_height: ulong_of(&set.height),
        want_frame_rate: match &set.frame_rate {
            Some(Constrain::Value(v)) => Some(*v),
            Some(Constrain::Range(r)) => r.ideal.or(r.max).or(r.min),
            None => None,
        },
    };
    Ok(Some(Request { set, capture }))
}

/// (exact, ideal) values of a ConstrainDOMString.
fn dom_string_constraint(c: &ConstrainDOMString) -> (Option<Vec<String>>, Option<Vec<String>>) {
    let list = |v: &StringOrStringSequence| match v {
        StringOrStringSequence::String(s) => vec![s.to_string()],
        StringOrStringSequence::StringSequence(v) => v.iter().map(|s| s.to_string()).collect(),
    };
    match c {
        ConstrainDOMString::String(s) => (None, Some(vec![s.to_string()])),
        ConstrainDOMString::StringSequence(v) => {
            (None, Some(v.iter().map(|s| s.to_string()).collect()))
        },
        ConstrainDOMString::ConstrainDOMStringParameters(p) => {
            (p.exact.as_ref().map(list), p.ideal.as_ref().map(list))
        },
    }
}

impl MediaDevices {
    fn get_user_media(
        &self,
        cx: &mut JSContext,
        constraints: &MediaStreamConstraints,
    ) -> Result<DomRoot<MediaStream>, GumError> {
        let wants = |c: &BooleanOrMediaTrackConstraints| {
            !matches!(c, BooleanOrMediaTrackConstraints::Boolean(false))
        };
        if !wants(&constraints.audio) && !wants(&constraints.video) {
            return Err(GumError::Type(c"getUserMedia needs audio or video"));
        }
        let media = ServoMedia::get();
        media.set_capture_mocking(pref!(media_fake_video_input));
        let audio = resolve(MediaStreamType::Audio, &constraints.audio)?;
        let video = resolve(MediaStreamType::Video, &constraints.video)?;

        let global = self.global();
        // The embedder decides (the launcher's camera / microphone flow; the OS prompt on macOS).
        let allowed = |name| request_permission_to_use(name, &global) == PermissionState::Granted;
        if audio.is_some() && !allowed(PermissionName::Microphone) {
            return Err(GumError::NotAllowed("the microphone wasn't allowed"));
        }
        if video.is_some() && !allowed(PermissionName::Camera) {
            return Err(GumError::NotAllowed("the camera wasn't allowed"));
        }

        let stream = MediaStream::new(cx, &global);
        let mut opened = vec![];
        if let Some(r) = audio {
            let Some(id) = media.create_audioinput_stream(r.set) else {
                return Err(GumError::NotReadable("the microphone didn't start"));
            };
            let track =
                MediaStreamTrack::new_capture(cx, &global, id, MediaStreamType::Audio, r.capture);
            stream.add_track(&track);
            opened.push(track);
        }
        if let Some(r) = video {
            let Some(id) = media.create_videoinput_stream(r.set) else {
                for t in &opened {
                    t.Stop();
                }
                return Err(GumError::NotReadable("the camera didn't start"));
            };
            let track =
                MediaStreamTrack::new_capture(cx, &global, id, MediaStreamType::Video, r.capture);
            stream.add_track(&track);
        }
        Ok(stream)
    }
}

impl MediaDevicesMethods<crate::DomTypeHolder> for MediaDevices {
    /// <https://w3c.github.io/mediacapture-main/#dom-mediadevices-getusermedia>
    fn GetUserMedia(
        &self,
        cx: &mut CurrentRealm,
        constraints: &MediaStreamConstraints,
    ) -> RootedPromise {
        let p = Promise::new_in_realm(cx);
        match self.get_user_media(cx, constraints) {
            Ok(stream) => p.resolve_native(cx, &stream),
            Err(GumError::Type(msg)) => p.reject_error(cx, Error::Type(msg.to_owned())),
            Err(GumError::NotFound(msg)) => {
                p.reject_error(cx, Error::NotFound(Some(msg.to_owned())))
            },
            Err(GumError::NotAllowed(msg)) => {
                p.reject_error(cx, Error::NotAllowed(Some(msg.to_owned())))
            },
            Err(GumError::NotReadable(msg)) => {
                p.reject_error(cx, Error::NotReadable(Some(msg.to_owned())))
            },
            Err(GumError::Overconstrained(msg)) => {
                // OverconstrainedError: a DOMException by that name (pages check e.name).
                let e = reflect_dom_object(
                    cx,
                    Box::new(DOMException::new_inherited(
                        DOMString::from(msg),
                        DOMString::from_static("OverconstrainedError"),
                    )),
                    &*self.global(),
                );
                p.reject_native(cx, &e);
            },
        }
        p
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediadevices-enumeratedevices>
    fn EnumerateDevices(&self, cx: &mut JSContext) -> RootedPromise {
        // Step 1.
        let mut realm = CurrentRealm::assert(cx);
        let p = Promise::new_in_realm(&mut realm);

        // Step 2.
        // XXX These steps should be run in parallel.
        // XXX Steps 2.1 - 2.4

        // Step 2.5
        let media = ServoMedia::get();
        media.set_capture_mocking(pref!(media_fake_video_input));
        let device_monitor = media.get_device_monitor();
        let result_list = device_monitor
            .enumerate_devices()
            .map(|devices| {
                devices
                    .iter()
                    .map(|device| {
                        // XXX The media backend has no way to group devices yet.
                        MediaDeviceInfo::new(
                            cx,
                            &self.global(),
                            &device.device_id,
                            device.kind.convert(),
                            &device.label,
                            "",
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        p.resolve_native(cx, &result_list);

        // Step 3.
        p
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediadevices-getsupportedconstraints>
    fn GetSupportedConstraints(&self) -> MediaTrackSupportedConstraints {
        MediaTrackSupportedConstraints::empty()
    }

    event_handler!(devicechange, GetOndevicechange, SetOndevicechange);
}

fn convert_constraints(
    js: &BooleanOrMediaTrackConstraints,
) -> Option<(MediaTrackConstraintSet, Option<&JsConstraintSet>)> {
    match js {
        BooleanOrMediaTrackConstraints::Boolean(false) => None,
        BooleanOrMediaTrackConstraints::Boolean(true) => Some((Default::default(), None)),
        BooleanOrMediaTrackConstraints::MediaTrackConstraints(c) => Some((
            MediaTrackConstraintSet {
                height: c.parent.height.as_ref().and_then(convert_culong),
                width: c.parent.width.as_ref().and_then(convert_culong),
                aspect: c.parent.aspectRatio.as_ref().and_then(convert_cdouble),
                frame_rate: c.parent.frameRate.as_ref().and_then(convert_cdouble),
                sample_rate: c.parent.sampleRate.as_ref().and_then(convert_culong),
                device_id: None,
                facing_mode: None,
            },
            Some(&c.parent),
        )),
    }
}

fn convert_culong(js: &ConstrainULong) -> Option<Constrain<u32>> {
    match js {
        ConstrainULong::ClampedUnsignedLong(val) => Some(Constrain::Value(*val)),
        ConstrainULong::ConstrainULongRange(range) => {
            if range.parent.min.is_some() || range.parent.max.is_some() {
                Some(Constrain::Range(ConstrainRange {
                    min: range.parent.min,
                    max: range.parent.max,
                    ideal: range.ideal,
                }))
            } else if range.exact.is_some() {
                range.exact.map(Constrain::Value)
            } else {
                // Partii: { ideal: 640 } (hardware-client.ts) is the wanted size.
                range.ideal.map(Constrain::Value)
            }
        },
    }
}

fn convert_cdouble(js: &ConstrainDouble) -> Option<Constrain<f64>> {
    match js {
        ConstrainDouble::Double(val) => Some(Constrain::Value(**val)),
        ConstrainDouble::ConstrainDoubleRange(range) => {
            if range.parent.min.is_some() || range.parent.max.is_some() {
                Some(Constrain::Range(ConstrainRange {
                    min: range.parent.min.map(|x| *x),
                    max: range.parent.max.map(|x| *x),
                    ideal: range.ideal.map(|x| *x),
                }))
            } else if range.exact.is_some() {
                range.exact.map(|exact| Constrain::Value(*exact))
            } else {
                range.ideal.map(|ideal| Constrain::Value(*ideal))
            }
        },
    }
}
