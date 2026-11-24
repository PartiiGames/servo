/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! MediaStreamTrack. Partii (stream "capture"): tracks of the live media registry
//! (`servo_media::streams::live` for audio, `live_video` for video) get label, enabled, muted,
//! readyState, stop, clone, getSettings and mute/unmute/ended events from it; other ids (older
//! dummy streams) behave as before.

use std::cell::Cell;

use dom_struct::dom_struct;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object;
use servo_media::streams::MediaStreamType;
use servo_media::streams::live::{self, TrackEvent};
use servo_media::streams::live_video;
use servo_media::streams::registry::MediaStreamId;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::MediaDevicesBinding::{
    ConstrainDOMStringParameters, MediaTrackConstraints,
};
use crate::dom::bindings::codegen::Bindings::MediaStreamTrackBinding::{
    MediaStreamTrackMethods, MediaStreamTrackState, MediaTrackCapabilities, MediaTrackSettings,
};
use crate::dom::bindings::codegen::UnionTypes::{
    ClampedUnsignedLongOrConstrainULongRange as ConstrainULong,
    DoubleOrConstrainDoubleRange as ConstrainDouble,
    StringOrStringSequenceOrConstrainDOMStringParameters as ConstrainDOMString,
};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::{Promise, RootedPromise};

/// What getUserMedia opened, for getSettings / getConstraints / getCapabilities.
#[derive(Clone, Debug, Default, JSTraceable, MallocSizeOf)]
pub(crate) struct CaptureSettings {
    pub(crate) device_id: String,
    pub(crate) group_id: String,
    pub(crate) facing_mode: String,
    /// Audio: what the page asked for (the voice stream's processing honours them).
    pub(crate) echo_cancellation: Option<bool>,
    pub(crate) noise_suppression: Option<bool>,
    pub(crate) auto_gain_control: Option<bool>,
    /// The constraints as asked: width, height, frameRate (getConstraints).
    pub(crate) want_width: Option<u32>,
    pub(crate) want_height: Option<u32>,
    pub(crate) want_frame_rate: Option<f64>,
}

#[dom_struct]
pub(crate) struct MediaStreamTrack {
    eventtarget: EventTarget,
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    id: MediaStreamId,
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    ty: MediaStreamType,
    /// Set on tracks that aren't in the live registry (their state is kept here).
    enabled: Cell<bool>,
    ended: Cell<bool>,
    muted: Cell<bool>,
    capture: DomRefCell<CaptureSettings>,
    droppable: DroppableTrack,
}

/// Frees the registry entry when the track is collected (and stops the source when no other track
/// uses it). Canvas captureStream tracks (clips) rely on this too.
#[derive(JSTraceable, MallocSizeOf)]
struct DroppableTrack {
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    id: MediaStreamId,
}

impl Drop for DroppableTrack {
    fn drop(&mut self) {
        live::unregister_track(&self.id);
    }
}

impl MediaStreamTrack {
    pub(crate) fn new_inherited(id: MediaStreamId, ty: MediaStreamType) -> MediaStreamTrack {
        MediaStreamTrack {
            eventtarget: EventTarget::new_inherited(),
            id,
            ty,
            enabled: Cell::new(true),
            ended: Cell::new(false),
            muted: Cell::new(false),
            capture: DomRefCell::new(CaptureSettings::default()),
            droppable: DroppableTrack { id },
        }
    }

    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        global: &GlobalScope,
        id: MediaStreamId,
        ty: MediaStreamType,
    ) -> DomRoot<MediaStreamTrack> {
        reflect_dom_object(
            cx,
            Box::new(MediaStreamTrack::new_inherited(id, ty)),
            global,
        )
    }

    /// A track of getUserMedia, with what it opened.
    pub(crate) fn new_capture(
        cx: &mut js::context::JSContext,
        global: &GlobalScope,
        id: MediaStreamId,
        ty: MediaStreamType,
        capture: CaptureSettings,
    ) -> DomRoot<MediaStreamTrack> {
        let track = Self::new(cx, global, id, ty);
        *track.capture.borrow_mut() = capture;
        track.watch();
        track
    }

    pub(crate) fn id(&self) -> MediaStreamId {
        self.id
    }

    pub(crate) fn ty(&self) -> MediaStreamType {
        self.ty
    }

    /// Whether this track can still produce media.
    pub(crate) fn is_live(&self) -> bool {
        match live::track_info(&self.id) {
            Some(info) => !info.ended,
            None => !self.ended.get(),
        }
    }

    /// mute / unmute / ended from the registry, as events on the script thread. Only getUserMedia
    /// tracks (and their clones) watch: the watcher keeps the track alive until it ends or stops,
    /// as a live capture track should be.
    fn watch(&self) {
        if !live::is_live(&self.id) {
            return;
        }
        let this = Trusted::new(self);
        let task_source = self
            .global()
            .task_manager()
            .media_element_task_source()
            .to_sendable();
        let f = Box::new(move |event: TrackEvent| {
            let this = this.clone();
            task_source.queue(task!(media_stream_track_event: move |cx| {
                let this = this.root();
                let name = match event {
                    TrackEvent::Muted => {
                        this.muted.set(true);
                        "mute"
                    },
                    TrackEvent::Unmuted => {
                        this.muted.set(false);
                        "unmute"
                    },
                    TrackEvent::Ended => {
                        // stop() ends a track without an event (the spec's "ended" is for the source).
                        if this.ended.replace(true) {
                            return;
                        }
                        "ended"
                    },
                };
                this.upcast::<EventTarget>().fire_event(cx, Atom::from(name));
            }));
        });
        if live_video::is_video_track(&self.id) {
            live_video::watch_video_track(&self.id, f);
        } else {
            live::watch_track(&self.id, f);
        }
    }
}

impl MediaStreamTrackMethods<crate::DomTypeHolder> for MediaStreamTrack {
    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-kind>
    fn Kind(&self) -> DOMString {
        match self.ty {
            MediaStreamType::Video => DOMString::from_static("video"),
            MediaStreamType::Audio => DOMString::from_static("audio"),
        }
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-id>
    fn Id(&self) -> DOMString {
        self.id.id().to_string().into()
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-label>
    fn Label(&self) -> DOMString {
        live::track_info(&self.id)
            .map(|i| DOMString::from(i.label))
            .unwrap_or_default()
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-enabled>
    fn Enabled(&self) -> bool {
        self.enabled.get()
    }

    fn SetEnabled(&self, enabled: bool) {
        self.enabled.set(enabled);
        live::set_enabled(&self.id, enabled);
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-muted>
    fn Muted(&self) -> bool {
        live::track_info(&self.id).map_or(self.muted.get(), |i| i.muted)
    }

    event_handler!(mute, GetOnmute, SetOnmute);
    event_handler!(unmute, GetOnunmute, SetOnunmute);
    event_handler!(ended, GetOnended, SetOnended);

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-readystate>
    fn ReadyState(&self) -> MediaStreamTrackState {
        if self.ended.get() || !self.is_live() {
            MediaStreamTrackState::Ended
        } else {
            MediaStreamTrackState::Live
        }
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-clone>
    fn Clone(&self, cx: &mut js::context::JSContext) -> DomRoot<MediaStreamTrack> {
        let id = if live::is_live(&self.id) {
            live::clone_track(&self.id).unwrap_or_else(MediaStreamId::new)
        } else {
            MediaStreamId::new()
        };
        let track = MediaStreamTrack::new(cx, &self.global(), id, self.ty);
        *track.capture.borrow_mut() = self.capture.borrow().clone();
        if !self.capture.borrow().device_id.is_empty() {
            track.watch();
        }
        track.SetEnabled(self.enabled.get());
        if self.ended.get() {
            track.Stop();
        }
        track
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-stop>
    fn Stop(&self) {
        self.ended.set(true);
        live::stop_track(&self.id);
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-getcapabilities>
    fn GetCapabilities(&self) -> MediaTrackCapabilities {
        let c = self.capture.borrow();
        let mut caps = MediaTrackCapabilities::empty();
        if !c.device_id.is_empty() {
            caps.deviceId = Some(DOMString::from(c.device_id.clone()));
            caps.groupId = Some(DOMString::from(c.group_id.clone()));
        }
        if self.ty == MediaStreamType::Video && !c.facing_mode.is_empty() {
            caps.facingMode = Some(vec![DOMString::from(c.facing_mode.clone())]);
        }
        caps
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-getconstraints>
    fn GetConstraints(&self) -> MediaTrackConstraints {
        let c = self.capture.borrow();
        let mut out = MediaTrackConstraints::empty();
        out.parent.width = c.want_width.map(ConstrainULong::ClampedUnsignedLong);
        out.parent.height = c.want_height.map(ConstrainULong::ClampedUnsignedLong);
        out.parent.frameRate = c
            .want_frame_rate
            .map(|f| ConstrainDouble::Double(Finite::wrap(f)));
        if !c.device_id.is_empty() {
            let mut exact = ConstrainDOMStringParameters::empty();
            exact.exact = Some(
                crate::dom::bindings::codegen::UnionTypes::StringOrStringSequence::String(
                    DOMString::from(c.device_id.clone()),
                ),
            );
            out.parent.deviceId = Some(ConstrainDOMString::ConstrainDOMStringParameters(exact));
        }
        out
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-getsettings>
    fn GetSettings(&self) -> MediaTrackSettings {
        let c = self.capture.borrow();
        let info = live::track_info(&self.id);
        let mut s = MediaTrackSettings::empty();
        if !c.device_id.is_empty() {
            s.deviceId = Some(DOMString::from(c.device_id.clone()));
            s.groupId = Some(DOMString::from(c.group_id.clone()));
        }
        match self.ty {
            MediaStreamType::Video => {
                if let Some(info) = info {
                    if info.width > 0 && info.height > 0 {
                        s.width = Some(info.width);
                        s.height = Some(info.height);
                        s.aspectRatio = Some(Finite::wrap(info.width as f64 / info.height as f64));
                    }
                    if info.frame_rate > 0. {
                        s.frameRate = Some(Finite::wrap(info.frame_rate));
                    }
                }
                if !c.facing_mode.is_empty() {
                    s.facingMode = Some(DOMString::from(c.facing_mode.clone()));
                }
                s.resizeMode = Some(DOMString::from_static("none"));
            },
            MediaStreamType::Audio => {
                if let Some(format) = info.and_then(|i| i.format) {
                    s.sampleRate = Some(format.sample_rate);
                    s.channelCount = Some(format.channels as u32);
                    s.sampleSize = Some(16);
                }
                s.echoCancellation = c.echo_cancellation;
                s.noiseSuppression = c.noise_suppression;
                s.autoGainControl = c.auto_gain_control;
            },
        }
        s
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-constrainablepattern-applyconstraints>
    ///
    /// Partii: the new constraints are kept (getConstraints); the device keeps its mode (a camera
    /// isn't reopened at another size yet).
    fn ApplyConstraints(
        &self,
        cx: &mut js::context::JSContext,
        constraints: &MediaTrackConstraints,
    ) -> RootedPromise {
        let p = Promise::new(cx, &self.global());
        {
            let mut c = self.capture.borrow_mut();
            let pick = |v: &Option<ConstrainULong>| match v {
                Some(ConstrainULong::ClampedUnsignedLong(v)) => Some(*v),
                Some(ConstrainULong::ConstrainULongRange(r)) => {
                    r.ideal.or(r.exact).or(r.parent.min).or(r.parent.max)
                },
                None => None,
            };
            c.want_width = pick(&constraints.parent.width);
            c.want_height = pick(&constraints.parent.height);
            c.want_frame_rate = match &constraints.parent.frameRate {
                Some(ConstrainDouble::Double(v)) => Some(**v),
                Some(ConstrainDouble::ConstrainDoubleRange(r)) => r
                    .ideal
                    .or(r.exact)
                    .or(r.parent.max)
                    .or(r.parent.min)
                    .map(|v| *v),
                None => None,
            };
        }
        p.resolve_native(cx, &());
        p
    }
}
