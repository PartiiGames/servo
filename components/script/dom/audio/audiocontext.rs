/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use js::rust::HandleObject;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_base::id::PipelineId;
use servo_media::ServoMedia;
use stylo_atoms::Atom;
use servo_media::audio::context::{LatencyCategory, ProcessingState, RealTimeAudioContextOptions};

use crate::conversions::Convert;
use crate::dom::audio::baseaudiocontext::{BaseAudioContext, BaseAudioContextOptions};
use crate::dom::audio::mediaelementaudiosourcenode::MediaElementAudioSourceNode;
use crate::dom::audio::mediastreamaudiodestinationnode::MediaStreamAudioDestinationNode;
use crate::dom::audio::mediastreamaudiosourcenode::MediaStreamAudioSourceNode;
use crate::dom::audio::mediastreamtrackaudiosourcenode::MediaStreamTrackAudioSourceNode;
use crate::dom::audio::audiosinkinfo::AudioSinkInfo;
use crate::dom::bindings::cell::DomRefCell;
use crate::dom::bindings::codegen::Bindings::AudioContextBinding::{
    AudioContextLatencyCategory, AudioContextMethods, AudioContextOptions, AudioSinkInfoMethods,
    AudioSinkType, AudioTimestamp,
};
use crate::dom::bindings::codegen::UnionTypes::{StringOrAudioSinkInfo, StringOrAudioSinkOptions};
use crate::dom::bindings::root::MutNullableDom;
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::bindings::codegen::Bindings::AudioNodeBinding::AudioNodeOptions;
use crate::dom::bindings::codegen::Bindings::BaseAudioContextBinding::AudioContextState;
use crate::dom::bindings::codegen::Bindings::BaseAudioContextBinding::BaseAudioContext_Binding::BaseAudioContextMethods;
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::codegen::UnionTypes::AudioContextLatencyCategoryOrDouble;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::{Trusted, TrustedPromise};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::html::htmlmediaelement::HTMLMediaElement;
use crate::dom::mediastream::MediaStream;
use crate::dom::mediastreamtrack::MediaStreamTrack;
use crate::dom::promise::{Promise, RootedPromise};
use crate::dom::window::Window;

#[dom_struct]
pub(crate) struct AudioContext {
    context: BaseAudioContext,
    latency_hint: AudioContextLatencyCategory,
    /// <https://webaudio.github.io/web-audio-api/#dom-audiocontext-baselatency>
    base_latency: f64,
    /// <https://webaudio.github.io/web-audio-api/#dom-audiocontext-outputlatency>
    output_latency: f64,
    /// <https://webaudio.github.io/web-audio-api/#dom-audiocontext-sink-id-slot> when it is a
    /// device id. Only "" (the default device) is valid: there is no device enumeration.
    sink_id: DomRefCell<DOMString>,
    /// The [[sink ID]] slot when it is an AudioSinkInfo (`{type: "none"}`: render, but play
    /// nothing).
    sink_info: MutNullableDom<AudioSinkInfo>,
}

impl AudioContext {
    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    /// <https://webaudio.github.io/web-audio-api/#AudioContext-constructors>
    fn new_inherited(
        options: &AudioContextOptions,
        pipeline_id: PipelineId,
    ) -> Fallible<AudioContext> {
        // A sampleRate outside the nominal range [3000, 768000] is not supported: throw
        // NotSupportedError instead of building a context (a 0 Hz context used to reach the
        // render thread).
        if let Some(rate) = options.sampleRate
            && !(3000. ..=768000.).contains(&*rate)
        {
            return Err(Error::NotSupported(None));
        }

        // Steps 1-3.
        let context = BaseAudioContext::new_inherited(
            BaseAudioContextOptions::AudioContext(options.convert()),
            pipeline_id,
        )?;

        // Step 4.1.
        let latency_hint = match options.latencyHint {
            AudioContextLatencyCategoryOrDouble::AudioContextLatencyCategory(category) => category,
            AudioContextLatencyCategoryOrDouble::Double(_) => {
                AudioContextLatencyCategory::Interactive
            }, // TODO
        };

        // Step 4.2. The sample rate is set during the creation of the BaseAudioContext.
        // servo-media takes care of setting the default sample rate of the output device
        // and of resampling the audio output if needed.

        // Steps 5 and 6 of the construction algorithm will happen in `resume`,
        // after reflecting dom object.

        // The backend's estimates, taken once at construction.
        let media = ServoMedia::get();
        let base_latency = media.base_latency(*context.SampleRate());
        let output_latency = media.output_latency();

        Ok(AudioContext {
            context,
            latency_hint,
            base_latency,
            output_latency,
            sink_id: Default::default(),
            sink_info: Default::default(),
        })
    }

    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    fn new(
        cx: &mut js::context::JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        options: &AudioContextOptions,
    ) -> Fallible<DomRoot<AudioContext>> {
        let pipeline_id = window.pipeline_id();
        let context = AudioContext::new_inherited(options, pipeline_id)?;
        let context = reflect_dom_object_with_proto(cx, Box::new(context), window, proto);
        // Step 10.1: the sinkId option.
        match options.sinkId.as_ref() {
            Some(StringOrAudioSinkOptions::String(id)) if !id.is_empty() => {
                // Not a known device: fire `error` and keep the default device.
                window
                    .as_global_scope()
                    .task_manager()
                    .dom_manipulation_task_source()
                    .queue_simple_event(context.upcast(), atom!("error"));
            },
            Some(StringOrAudioSinkOptions::AudioSinkOptions(sink)) => {
                context.set_sink_none(cx, window, sink.type_);
            },
            _ => {},
        }
        context.resume(window);
        Ok(context)
    }

    /// [[sink ID]] becomes an AudioSinkInfo: keep rendering, send nothing to the device.
    fn set_sink_none(&self, cx: &mut JSContext, window: &Window, sink_type: AudioSinkType) {
        self.sink_info
            .set(Some(&AudioSinkInfo::new(cx, window, sink_type)));
        *self.sink_id.borrow_mut() = DOMString::new();
        self.context
            .audio_context_impl()
            .lock()
            .unwrap()
            .set_output_none(true);
    }

    /// [[sink ID]] becomes the default device ("").
    fn set_sink_default(&self) {
        self.sink_info.set(None);
        *self.sink_id.borrow_mut() = DOMString::new();
        self.context
            .audio_context_impl()
            .lock()
            .unwrap()
            .set_output_none(false);
    }

    /// Whether `sink` names the current [[sink ID]].
    fn is_current_sink(&self, sink: &StringOrAudioSinkOptions) -> bool {
        match (sink, self.sink_info.get()) {
            (StringOrAudioSinkOptions::String(id), None) => *id == *self.sink_id.borrow(),
            (StringOrAudioSinkOptions::AudioSinkOptions(options), Some(info)) => {
                options.type_ == info.Type()
            },
            _ => false,
        }
    }

    fn resume(&self, window: &Window) {
        // Step 5.
        if self.context.is_allowed_to_start() {
            // Step 6.
            self.context.resume();
            // DEVIATION, as in Chrome: once the page has had a user gesture (sticky activation),
            // a context that is allowed to start reads "running" straight from the constructor;
            // statechange still fires, from a task. Per spec it reads "suspended" until a task
            // updates it, and Phaser, which marks its sound manager locked when the new context is
            // "suspended" and waits for another mousedown/keydown to unlock it, stayed locked when
            // its game was made after the first click (probes/audio-lifecycle phaser.sound).
            // Without a gesture (every WPT page) the state starts "suspended", as specified.
            if window.has_sticky_activation() {
                self.context.set_state_and_notify(AudioContextState::Running);
            }
        }
    }

    pub(crate) fn base(&self) -> DomRoot<BaseAudioContext> {
        DomRoot::from_ref(&self.context)
    }
}

impl AudioContextMethods<crate::DomTypeHolder> for AudioContext {
    /// <https://webaudio.github.io/web-audio-api/#AudioContext-constructors>
    fn Constructor(
        cx: &mut js::context::JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        options: &AudioContextOptions,
    ) -> Fallible<DomRoot<AudioContext>> {
        // Step 1: a document that is not fully active (a removed or navigated-away frame)
        // can't make a context.
        if !window.Document().is_fully_active() {
            return Err(Error::InvalidState(Some(
                "The document is not fully active.".into(),
            )));
        }
        AudioContext::new(cx, window, proto, options)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audiocontext-baselatency>
    fn BaseLatency(&self) -> Finite<f64> {
        Finite::wrap(self.base_latency)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audiocontext-outputlatency>
    fn OutputLatency(&self) -> Finite<f64> {
        Finite::wrap(self.output_latency)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audiocontext-sinkid>
    fn SinkId(&self) -> StringOrAudioSinkInfo {
        match self.sink_info.get() {
            Some(info) => StringOrAudioSinkInfo::AudioSinkInfo(info),
            None => StringOrAudioSinkInfo::String(self.sink_id.borrow().clone()),
        }
    }

    // <https://webaudio.github.io/web-audio-api/#dom-audiocontext-onsinkchange>
    event_handler!(sinkchange, GetOnsinkchange, SetOnsinkchange);

    // <https://webaudio.github.io/web-audio-api/#dom-audiocontext-onerror>
    event_handler!(error, GetOnerror, SetOnerror);

    /// <https://webaudio.github.io/web-audio-api/#dom-audiocontext-setsinkid>
    fn SetSinkId(&self, cx: &mut CurrentRealm, sink: StringOrAudioSinkOptions) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        // Step 2.
        if self.context.reject_if_not_fully_active(cx, &promise) {
            return promise;
        }
        if self.context.control_thread_state() == ProcessingState::Closed {
            promise.reject_error(cx, Error::InvalidState(None));
            return promise;
        }
        // Step 3.
        if self.is_current_sink(&sink) {
            promise.resolve_native(cx, &());
            return promise;
        }
        // Step 4, validating sinkId: "" (the default device) and {type: "none"} only; there is
        // no device enumeration to match other ids against.
        if let StringOrAudioSinkOptions::String(ref id) = sink &&
            !id.is_empty()
        {
            promise.reject_error(cx, Error::NotFound(None));
            return promise;
        }
        // Steps 5-6: switch, then (from a task) fire sinkchange and resolve.
        match sink {
            StringOrAudioSinkOptions::AudioSinkOptions(options) => {
                let global = self.global();
                self.set_sink_none(cx, global.as_window(), options.type_);
            },
            StringOrAudioSinkOptions::String(_) => self.set_sink_default(),
        }
        let context = Trusted::new(self);
        let trusted_promise = TrustedPromise::from(&promise);
        self.global()
            .task_manager()
            .dom_manipulation_task_source()
            .queue(task!(sink_change: move |cx| {
                let context = context.root();
                context
                    .upcast::<EventTarget>()
                    .fire_event(cx, Atom::from("sinkchange"));
                trusted_promise.root(cx).resolve_native(cx, &());
            }));
        promise
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audiocontext-outputlatency>
    fn GetOutputTimestamp(&self) -> AudioTimestamp {
        // TODO
        AudioTimestamp {
            contextTime: Some(Finite::wrap(0.)),
            performanceTime: Some(Finite::wrap(0.)),
        }
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audiocontext-suspend>
    fn Suspend(&self, cx: &mut CurrentRealm) -> RootedPromise {
        // Step 1.
        let promise = Promise::new_in_realm(cx);
        if self.context.reject_if_not_fully_active(cx, &promise) {
            return promise;
        }

        // Step 2.
        if self.context.control_thread_state() == ProcessingState::Closed {
            promise.reject_error(cx, Error::InvalidState(None));
            return promise;
        }

        // Step 3. The [[control thread state]] (set synchronously by suspend()/resume()), not
        // the state attribute, which a queued task updates later: with the attribute,
        // `suspend(); resume();` saw "running", skipped the resume, and the context stayed
        // suspended.
        if self.context.control_thread_state() == ProcessingState::Suspended {
            promise.resolve_native(cx, &());
            return promise;
        }

        // Steps 4 and 5.
        let trusted_promise = TrustedPromise::from(&promise);
        match self.context.audio_context_impl().lock().unwrap().suspend() {
            Some(_) => {
                let base_context = Trusted::new(&self.context);
                let context = Trusted::new(self);
                self.global().task_manager().dom_manipulation_task_source().queue(
                    task!(suspend_ok: move |cx| {
                        let base_context = base_context.root();
                        let context = context.root();
                        let promise = trusted_promise.root(cx);
                        promise.resolve_native(cx, &());
                        if base_context.State() != AudioContextState::Suspended {
                            base_context.set_state_attribute(AudioContextState::Suspended);
                            context.global().task_manager().dom_manipulation_task_source().queue_simple_event(
                                context.upcast(),
                                atom!("statechange"),
                            );
                        }
                    })
                );
            },
            None => {
                // The spec does not define the error case and `suspend` should
                // never fail, but we handle the case here for completion.
                self.global()
                    .task_manager()
                    .dom_manipulation_task_source()
                    .queue(task!(suspend_error: move |cx| {
                        let promise = trusted_promise.root(cx);
                        promise.reject_error(cx, Error::Type(c"Something went wrong".to_owned()));
                    }));
            },
        };

        // Step 6.
        promise
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audiocontext-close>
    fn Close(&self, cx: &mut CurrentRealm) -> RootedPromise {
        // Step 1.
        let promise = Promise::new_in_realm(cx);
        if self.context.reject_if_not_fully_active(cx, &promise) {
            return promise;
        }

        // Step 2.
        if self.context.control_thread_state() == ProcessingState::Closed {
            promise.reject_error(cx, Error::InvalidState(None));
            return promise;
        }

        // Step 3.
        if self.context.State() == AudioContextState::Closed {
            promise.resolve_native(cx, &());
            return promise;
        }

        // Steps 4 and 5.
        let trusted_promise = TrustedPromise::from(&promise);
        match self.context.audio_context_impl().lock().unwrap().close() {
            Some(_) => {
                let base_context = Trusted::new(&self.context);
                let context = Trusted::new(self);
                self.global().task_manager().dom_manipulation_task_source().queue(
                    task!(suspend_ok: move |cx| {
                        let base_context = base_context.root();
                        let context = context.root();
                        let promise = trusted_promise.root(cx);
                        promise.resolve_native(cx, &());
                        if base_context.State() != AudioContextState::Closed {
                            base_context.set_state_attribute(AudioContextState::Closed);
                            context.global().task_manager().dom_manipulation_task_source().queue_simple_event(
                                context.upcast(),
                                atom!("statechange"),
                            );
                        }
                    })
                );
            },
            None => {
                // The spec does not define the error case and `suspend` should
                // never fail, but we handle the case here for completion.
                self.global()
                    .task_manager()
                    .dom_manipulation_task_source()
                    .queue(task!(suspend_error: move |cx| {
                        let promise = trusted_promise.root(cx);
                        promise.reject_error(cx, Error::Type(c"Something went wrong".to_owned()));
                    }));
            },
        };

        // Step 6.
        promise
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audiocontext-createmediaelementsource>
    fn CreateMediaElementSource(
        &self,
        cx: &mut JSContext,
        media_element: &HTMLMediaElement,
    ) -> Fallible<DomRoot<MediaElementAudioSourceNode>> {
        let global = self.global();
        let window = global.as_window();
        MediaElementAudioSourceNode::new(cx, window, self, media_element)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audiocontext-createmediastreamsource>
    fn CreateMediaStreamSource(
        &self,
        cx: &mut JSContext,
        stream: &MediaStream,
    ) -> Fallible<DomRoot<MediaStreamAudioSourceNode>> {
        let global = self.global();
        let window = global.as_window();
        MediaStreamAudioSourceNode::new(cx, window, self, stream)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audiocontext-createmediastreamtracksource>
    fn CreateMediaStreamTrackSource(
        &self,
        cx: &mut JSContext,
        track: &MediaStreamTrack,
    ) -> Fallible<DomRoot<MediaStreamTrackAudioSourceNode>> {
        let global = self.global();
        let window = global.as_window();
        MediaStreamTrackAudioSourceNode::new(cx, window, self, track)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audiocontext-createmediastreamdestination>
    fn CreateMediaStreamDestination(
        &self,
        cx: &mut JSContext,
    ) -> Fallible<DomRoot<MediaStreamAudioDestinationNode>> {
        let global = self.global();
        let window = global.as_window();
        MediaStreamAudioDestinationNode::new(cx, window, self, &AudioNodeOptions::empty())
    }
}

impl Convert<LatencyCategory> for AudioContextLatencyCategory {
    fn convert(self) -> LatencyCategory {
        match self {
            AudioContextLatencyCategory::Balanced => LatencyCategory::Balanced,
            AudioContextLatencyCategory::Interactive => LatencyCategory::Interactive,
            AudioContextLatencyCategory::Playback => LatencyCategory::Playback,
        }
    }
}

impl Convert<RealTimeAudioContextOptions> for &AudioContextOptions {
    fn convert(self) -> RealTimeAudioContextOptions {
        RealTimeAudioContextOptions {
            // Default: the output device's rate (the backend's preferred rate).
            sample_rate: self
                .sampleRate
                .map(|rate| *rate)
                .unwrap_or_else(|| ServoMedia::get().preferred_sample_rate()),
            latency_hint: match self.latencyHint {
                AudioContextLatencyCategoryOrDouble::AudioContextLatencyCategory(category) => {
                    category.convert()
                },
                AudioContextLatencyCategoryOrDouble::Double(_) => LatencyCategory::Interactive, // TODO
            },
        }
    }
}
