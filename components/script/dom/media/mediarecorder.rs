/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii: MediaRecorder, <https://w3c.github.io/mediacapture-record/>. Records the stream's first video
//! track (canvas.captureStream, a camera) as VP8 in WebM through servo-media-recorder, on its own thread.
//! Audio tracks are not recorded. The encoder's bytes come back as tasks on the DOM manipulation task
//! source: dataavailable (a Blob) at each timeslice, on requestData and at the end, then stop.

use std::cell::Cell;
use std::time::Duration;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_constellation_traits::BlobImpl;
use servo_media::streams::MediaStreamType;
use stylo_atoms::Atom;

use self::encoder::{Recorder, RecorderEvent, RecorderOptions};
use crate::dom::bindings::codegen::Bindings::MediaRecorderBinding::{
    BitrateMode, MediaRecorderMethods, MediaRecorderOptions, RecordingState,
};
use crate::dom::bindings::codegen::Bindings::PerformanceBinding::PerformanceMethods;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::blob::Blob;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::media::blobevent::BlobEvent;
use crate::dom::media::mediastream::MediaStream;
use crate::dom::window::Window;

#[cfg(feature = "media-recorder")]
mod encoder {
    pub(super) use servo_media_recorder::{
        Recorder, RecorderEvent, RecorderOptions, is_type_supported, recorded_type,
    };
}

/// A build without the encoder: MediaRecorder exists, supports no type and can't start.
#[cfg(not(feature = "media-recorder"))]
mod encoder {
    use servo_media::streams::MediaStreamId;

    pub(crate) enum RecorderEvent {
        Data(Vec<u8>),
        Error(String),
        Stopped {
            #[expect(dead_code)]
            by_track: bool,
        },
    }
    pub(crate) struct RecorderOptions {
        pub(crate) video_bits_per_second: u32,
        pub(crate) timeslice: Option<std::time::Duration>,
    }
    pub(crate) struct Recorder;
    impl Recorder {
        pub(crate) fn start(
            _: &MediaStreamId,
            _: RecorderOptions,
            _: Box<dyn FnMut(RecorderEvent) + Send>,
        ) -> Result<Recorder, String> {
            Err("this build can't record video".into())
        }
        pub(crate) fn request_data(&self) {}
        pub(crate) fn pause(&self) {}
        pub(crate) fn resume(&self) {}
        pub(crate) fn stop(&self) {}
    }
    pub(crate) fn is_type_supported(_: &str) -> bool {
        false
    }
    pub(crate) fn recorded_type(mime: &str) -> String {
        mime.to_owned()
    }
}

#[derive(Clone, Copy, JSTraceable, MallocSizeOf, PartialEq)]
enum State {
    Inactive,
    Recording,
    Paused,
}

/// Chrome's default when the page gives no bit rate.
const DEFAULT_VIDEO_BPS: u32 = 2_500_000;
const DEFAULT_AUDIO_BPS: u32 = 128_000;

#[dom_struct]
pub(crate) struct MediaRecorder {
    eventtarget: EventTarget,
    stream: Dom<MediaStream>,
    mime_type: DomRefCell<DOMString>,
    state: Cell<State>,
    video_bps: u32,
    audio_bps: u32,
    /// Counts start()s, so the end of an earlier recording doesn't touch a newer one.
    generation: Cell<u32>,
    #[no_trace]
    #[ignore_malloc_size_of = "an encoder thread (its buffers are its own)"]
    recorder: DomRefCell<Option<(u32, Recorder)>>,
}

impl MediaRecorder {
    fn new_inherited(stream: &MediaStream, mime: DOMString, video_bps: u32, audio_bps: u32) -> Self {
        MediaRecorder {
            eventtarget: EventTarget::new_inherited(),
            stream: Dom::from_ref(stream),
            mime_type: DomRefCell::new(mime),
            state: Cell::new(State::Inactive),
            video_bps,
            audio_bps,
            generation: Cell::new(0),
            recorder: DomRefCell::new(None),
        }
    }

    fn queue_event(&self, name: &str) {
        self.global()
            .task_manager()
            .dom_manipulation_task_source()
            .queue_simple_event(self.upcast(), Atom::from(name));
    }

    fn with_recorder(&self, f: impl FnOnce(&Recorder)) {
        if let Some((generation, recorder)) = self.recorder.borrow().as_ref() {
            if *generation == self.generation.get() {
                f(recorder);
            }
        }
    }

    /// An encoder event, on the script thread.
    fn handle(&self, cx: &mut JSContext, generation: u32, event: RecorderEvent) {
        match event {
            RecorderEvent::Data(bytes) => {
                if bytes.is_empty() {
                    return;
                }
                let global = self.global();
                let type_string = self.mime_type.borrow().to_string();
                let blob = Blob::new(cx, &global, BlobImpl::new_from_bytes(bytes, type_string));
                let timecode = global.performance(cx).Now();
                let event = BlobEvent::new(cx, &global, Atom::from("dataavailable"), &blob, *timecode);
                event.upcast::<crate::dom::event::Event>().fire(cx, self.upcast());
            },
            RecorderEvent::Error(message) => {
                log::warn!("MediaRecorder: {message}");
                self.upcast::<EventTarget>().fire_event(cx, Atom::from("error"));
            },
            RecorderEvent::Stopped { .. } => {
                if generation == self.generation.get() {
                    self.state.set(State::Inactive);
                    self.recorder.borrow_mut().take();
                }
                self.upcast::<EventTarget>().fire_event(cx, Atom::from("stop"));
            },
        }
    }
}


impl MediaRecorderMethods<crate::DomTypeHolder> for MediaRecorder {
    /// <https://w3c.github.io/mediacapture-record/#dom-mediarecorder-constructor>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        stream: &MediaStream,
        options: &MediaRecorderOptions,
    ) -> Fallible<DomRoot<MediaRecorder>> {
        let mime = options.mimeType.to_string();
        if !mime.is_empty() && !encoder::is_type_supported(&mime) {
            return Err(Error::NotSupported(Some(format!("can't record {mime}"))));
        }
        let video_bps = options
            .videoBitsPerSecond
            .or(options.bitsPerSecond)
            .unwrap_or(DEFAULT_VIDEO_BPS);
        let audio_bps = options.audioBitsPerSecond.unwrap_or(DEFAULT_AUDIO_BPS);
        let global: &GlobalScope = window.upcast();
        Ok(reflect_dom_object_with_proto(
            cx,
            Box::new(MediaRecorder::new_inherited(stream, DOMString::from(mime), video_bps, audio_bps)),
            global,
            proto,
        ))
    }

    /// <https://w3c.github.io/mediacapture-record/#dom-mediarecorder-istypesupported>
    fn IsTypeSupported(_window: &Window, type_: DOMString) -> bool {
        encoder::is_type_supported(&type_.to_string())
    }

    fn Stream(&self) -> DomRoot<MediaStream> {
        DomRoot::from_ref(&*self.stream)
    }

    fn MimeType(&self) -> DOMString {
        self.mime_type.borrow().clone()
    }

    fn State(&self) -> RecordingState {
        match self.state.get() {
            State::Inactive => RecordingState::Inactive,
            State::Recording => RecordingState::Recording,
            State::Paused => RecordingState::Paused,
        }
    }

    fn VideoBitsPerSecond(&self) -> u32 {
        self.video_bps
    }

    fn AudioBitsPerSecond(&self) -> u32 {
        self.audio_bps
    }

    fn AudioBitrateMode(&self) -> BitrateMode {
        BitrateMode::Variable
    }

    /// <https://w3c.github.io/mediacapture-record/#dom-mediarecorder-start>
    fn Start(&self, timeslice: Option<u32>) -> Fallible<()> {
        if self.state.get() != State::Inactive {
            return Err(Error::InvalidState(None));
        }
        let track = self
            .stream
            .get_tracks()
            .iter()
            .find(|t| t.ty() == MediaStreamType::Video)
            .map(|t| t.id());
        let Some(track) = track else {
            return Err(Error::NotSupported(Some("the stream has no video track".into())));
        };
        let mime = encoder::recorded_type(&self.mime_type.borrow().to_string());
        *self.mime_type.borrow_mut() = DOMString::from(mime);

        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        let task_source = self
            .global()
            .task_manager()
            .dom_manipulation_task_source()
            .to_sendable();
        let this = Trusted::new(self);
        let sink = Box::new(move |event: RecorderEvent| {
            let this = this.clone();
            task_source.queue(task!(media_recorder_event: move |cx| {
                this.root().handle(cx, generation, event);
            }));
        });
        let options = RecorderOptions {
            video_bits_per_second: self.video_bps,
            timeslice: timeslice.map(|ms| Duration::from_millis(ms.max(10) as u64)),
        };
        let recorder = Recorder::start(&track, options, sink).map_err(|e| Error::NotSupported(Some(e)))?;
        *self.recorder.borrow_mut() = Some((generation, recorder));
        self.state.set(State::Recording);
        self.queue_event("start");
        Ok(())
    }

    /// <https://w3c.github.io/mediacapture-record/#dom-mediarecorder-stop>
    fn Stop(&self) -> Fallible<()> {
        if self.state.get() == State::Inactive {
            return Ok(());
        }
        self.state.set(State::Inactive);
        // The encoder answers with its last data and then Stopped (the stop event).
        self.with_recorder(|r| r.stop());
        Ok(())
    }

    /// <https://w3c.github.io/mediacapture-record/#dom-mediarecorder-pause>
    fn Pause(&self) -> Fallible<()> {
        match self.state.get() {
            State::Inactive => Err(Error::InvalidState(None)),
            State::Paused => Ok(()),
            State::Recording => {
                self.state.set(State::Paused);
                self.with_recorder(|r| r.pause());
                self.queue_event("pause");
                Ok(())
            },
        }
    }

    /// <https://w3c.github.io/mediacapture-record/#dom-mediarecorder-resume>
    fn Resume(&self) -> Fallible<()> {
        match self.state.get() {
            State::Inactive => Err(Error::InvalidState(None)),
            State::Recording => Ok(()),
            State::Paused => {
                self.state.set(State::Recording);
                self.with_recorder(|r| r.resume());
                self.queue_event("resume");
                Ok(())
            },
        }
    }

    /// <https://w3c.github.io/mediacapture-record/#dom-mediarecorder-requestdata>
    fn RequestData(&self) -> Fallible<()> {
        if self.state.get() == State::Inactive {
            return Err(Error::InvalidState(None));
        }
        self.with_recorder(|r| r.request_data());
        Ok(())
    }

    // https://w3c.github.io/mediacapture-record/#dom-mediarecorder-onstart
    event_handler!(start, GetOnstart, SetOnstart);
    event_handler!(stop, GetOnstop, SetOnstop);
    event_handler!(dataavailable, GetOndataavailable, SetOndataavailable);
    event_handler!(pause, GetOnpause, SetOnpause);
    event_handler!(resume, GetOnresume, SetOnresume);
    event_handler!(error, GetOnerror, SetOnerror);
}
