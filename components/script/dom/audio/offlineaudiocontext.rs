/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::Builder;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use js::rust::HandleObject;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_base::id::PipelineId;
use servo_media::audio::block::FRAMES_PER_BLOCK_USIZE;
use servo_media::audio::context::{
    OfflineAudioContextOptions as ServoMediaOfflineAudioContextOptions, ProcessingState,
};

use crate::dom::audio::audiobuffer::{AudioBuffer, MAX_SAMPLE_RATE, MIN_SAMPLE_RATE};
use crate::dom::audio::audionode::MAX_CHANNEL_COUNT;
use crate::dom::audio::baseaudiocontext::{BaseAudioContext, BaseAudioContextOptions};
use crate::dom::audio::offlineaudiocompletionevent::OfflineAudioCompletionEvent;
use crate::dom::bindings::codegen::Bindings::BaseAudioContextBinding::AudioContextState;
use crate::dom::bindings::codegen::Bindings::BaseAudioContextBinding::BaseAudioContext_Binding::BaseAudioContextMethods;
use crate::dom::bindings::codegen::Bindings::OfflineAudioContextBinding::{
    OfflineAudioContextMethods, OfflineAudioContextOptions,
};
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::{Trusted, TrustedPromise};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::promise::{Promise, RootedPromise, TracedPromise};
use crate::dom::window::Window;

#[dom_struct]
pub(crate) struct OfflineAudioContext {
    context: BaseAudioContext,
    channel_count: u32,
    length: u32,
    rendering_started: Cell<bool>,
    pending_rendering_promise: DomRefCell<Option<TracedPromise>>,
    /// Frames with a scheduled suspend() (no two at the same quantized frame).
    suspend_frames: DomRefCell<Vec<u64>>,
}

impl OfflineAudioContext {
    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    fn new_inherited(
        channel_count: u32,
        length: u32,
        sample_rate: f32,
        pipeline_id: PipelineId,
    ) -> Fallible<OfflineAudioContext> {
        let options = ServoMediaOfflineAudioContextOptions {
            channels: channel_count as u8,
            length: length as usize,
            sample_rate,
        };
        let context = BaseAudioContext::new_inherited(
            BaseAudioContextOptions::OfflineAudioContext(options),
            pipeline_id,
        )?;
        Ok(OfflineAudioContext {
            context,
            channel_count,
            length,
            rendering_started: Cell::new(false),
            pending_rendering_promise: Default::default(),
            suspend_frames: Default::default(),
        })
    }

    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    fn new(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        channel_count: u32,
        length: u32,
        sample_rate: f32,
    ) -> Fallible<DomRoot<OfflineAudioContext>> {
        if channel_count > MAX_CHANNEL_COUNT ||
            channel_count == 0 ||
            length == 0 ||
            !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate)
        {
            return Err(Error::NotSupported(None));
        }
        let pipeline_id = window.pipeline_id();
        let context =
            OfflineAudioContext::new_inherited(channel_count, length, sample_rate, pipeline_id)?;
        Ok(reflect_dom_object_with_proto(
            cx,
            Box::new(context),
            window,
            proto,
        ))
    }
}

impl OfflineAudioContextMethods<crate::DomTypeHolder> for OfflineAudioContext {
    /// <https://webaudio.github.io/web-audio-api/#dom-offlineaudiocontext-offlineaudiocontext>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        options: &OfflineAudioContextOptions,
    ) -> Fallible<DomRoot<OfflineAudioContext>> {
        OfflineAudioContext::new(
            cx,
            window,
            proto,
            options.numberOfChannels,
            options.length,
            *options.sampleRate,
        )
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-offlineaudiocontext-offlineaudiocontext-numberofchannels-length-samplerate>
    fn Constructor_(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        number_of_channels: u32,
        length: u32,
        sample_rate: Finite<f32>,
    ) -> Fallible<DomRoot<OfflineAudioContext>> {
        OfflineAudioContext::new(cx, window, proto, number_of_channels, length, *sample_rate)
    }

    // https://webaudio.github.io/web-audio-api/#dom-offlineaudiocontext-oncomplete
    event_handler!(complete, GetOncomplete, SetOncomplete);

    /// <https://webaudio.github.io/web-audio-api/#dom-offlineaudiocontext-length>
    fn Length(&self) -> u32 {
        self.length
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-offlineaudiocontext-startrendering>
    fn StartRendering(&self, cx: &mut CurrentRealm) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        // > If the [[control thread state]] on the OfflineAudioContext is closed, reject
        // > promise with InvalidStateError.
        if self.rendering_started.get() || self.context.State() == AudioContextState::Closed {
            promise.reject_error(cx, Error::InvalidState(None));
            return promise;
        }
        self.rendering_started.set(true);
        self.context.set_state_and_notify(AudioContextState::Running);

        *self.pending_rendering_promise.safe_borrow_mut(cx.no_gc()) = Some(promise.to_traced());

        let processed_audio = Arc::new(Mutex::new(Vec::new()));
        let processed_audio_ = processed_audio.clone();
        let (sender, receiver) = mpsc::channel();
        let sender = Mutex::new(sender);
        self.context
            .audio_context_impl()
            .lock()
            .unwrap()
            .set_eos_callback(Box::new(move |buffer| {
                processed_audio_
                    .lock()
                    .unwrap()
                    .extend_from_slice((*buffer).as_ref());
                let _ = sender.lock().unwrap().send(());
            }));

        let this = Trusted::new(self);
        let task_source = self
            .global()
            .task_manager()
            .dom_manipulation_task_source()
            .to_sendable();
        Builder::new()
            .name("OfflineACResolver".to_owned())
            .spawn(move || {
                let _ = receiver.recv();
                task_source.queue(task!(resolve: move |cx| {
                    let this = this.root();
                    let processed_audio = processed_audio.lock().unwrap();
                    let mut processed_audio: Vec<_> = processed_audio
                        .chunks(this.length as usize)
                        .map(|channel| channel.to_vec())
                        .collect();
                    // it can end up being empty if the task failed
                    if processed_audio.len() != this.length as usize {
                        processed_audio.resize(this.length as usize, Vec::new())
                    }
                    let buffer = AudioBuffer::new(
                        cx,
                        this.global().as_window(),
                        this.channel_count,
                        this.length,
                        *this.context.SampleRate(),
                        Some(processed_audio.as_slice()),
                    );
                    let promise = {
                        (*this
                            .pending_rendering_promise
                            .safe_borrow_mut(cx.no_gc()))
                            .take()
                            .unwrap()
                            .root(cx)
                    };
                    // Rendering is over: the context is closed (statechange) by the time
                    // the promise resolves.
                    if this.context.State() != AudioContextState::Closed {
                        this.context.set_state_and_notify(AudioContextState::Closed);
                    }
                    promise.resolve_native(cx, &buffer);
                    let global = &this.global();
                    let window = global.as_window();
                    let event = OfflineAudioCompletionEvent::new(cx, window,
                                                                 atom!("complete"),
                                                                 EventBubbles::DoesNotBubble,
                                                                 EventCancelable::NotCancelable,
                                                                 &buffer);
                    event.upcast::<Event>().fire(cx, this.upcast());
                }));
            })
            .unwrap();

        if self
            .context
            .audio_context_impl()
            .lock()
            .unwrap()
            .resume()
            .is_none()
        {
            promise.reject_error(
                cx,
                Error::Type(c"Could not start offline rendering".to_owned()),
            );
        }

        promise
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-offlineaudiocontext-resume>
    fn Resume(&self, cx: &mut CurrentRealm) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        // > Abort these steps and reject promise with InvalidStateError when any of following
        // > conditions is true: the [[control thread state]] is closed; [[rendering started]]
        // > is false.
        if self.context.State() == AudioContextState::Closed || !self.rendering_started.get() {
            promise.reject_error(cx, Error::InvalidState(None));
            return promise;
        }
        if self.context.State() == AudioContextState::Running {
            promise.resolve_native(cx, &());
            return promise;
        }
        // > Set the [[control thread state]] to running. Queue a control message to resume.
        self.context.push_pending_resume_promise(&promise);
        self.context.resume();
        promise
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-offlineaudiocontext-close>
    fn Close(&self, cx: &mut CurrentRealm) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        // > If the [[control thread state]] flag on the OfflineAudioContext is closed reject the
        // > promise with InvalidStateError, abort these steps, returning promise.
        if self.context.State() == AudioContextState::Closed ||
            self.context.control_thread_state() == ProcessingState::Closed
        {
            promise.reject_error(cx, Error::InvalidState(None));
            return promise;
        }
        // > Set the [[control thread state]] flag to closed. Queue a control message to close
        // > the OfflineAudioContext. (The rendering thread stops rendering.)
        let _ = self.context.audio_context_impl().lock().unwrap().close();
        let trusted_promise = TrustedPromise::new(&promise);
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .dom_manipulation_task_source()
            .queue(task!(offline_close: move |cx| {
                let this = this.root();
                // > Resolve promise. If the state attribute is not already "closed": set it to
                // > "closed" and fire statechange.
                trusted_promise.root(cx).resolve_native(cx, &());
                this.context.set_state_and_notify(AudioContextState::Closed);
            }));
        promise
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-offlineaudiocontext-suspend>
    fn Suspend(&self, cx: &mut CurrentRealm, suspend_time: Finite<f64>) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        // > the specified suspension time will be rounded up to the nearest render quantum
        // > boundary.
        let quantum = FRAMES_PER_BLOCK_USIZE as f64;
        let frame = (*suspend_time * *self.context.SampleRate() as f64 / quantum).ceil() * quantum;
        let current_frame = *self.context.CurrentTime() * *self.context.SampleRate() as f64;
        // > If the quantized frame number is negative or is less than or equal to the current
        // > time or is greater than or equal to the total render duration or is scheduled by
        // > another suspend for the same time, then the promise is rejected with
        // > InvalidStateError.
        //
        // Deviation, as in Chrome and Firefox (and WPT, e.g. audioworkletglobalscope-timing-info):
        // a suspend at the current frame is allowed when that quantum hasn't been rendered yet,
        // e.g. suspend(0) before startRendering(); only frames already rendered are rejected.
        if *suspend_time < 0. ||
            frame < current_frame.round() ||
            frame >= self.length as f64 ||
            self.suspend_frames.borrow().contains(&(frame as u64)) ||
            self.context.State() == AudioContextState::Closed
        {
            promise.reject_error(cx, Error::InvalidState(None));
            return promise;
        }
        let frame = frame as u64;
        self.suspend_frames.borrow_mut().push(frame);

        let trusted_promise = TrustedPromise::new(&promise);
        let this = Trusted::new(self);
        let task_source = self
            .global()
            .task_manager()
            .dom_manipulation_task_source()
            .to_sendable();
        self.context.audio_context_impl().lock().unwrap().suspend_at(
            frame,
            Box::new(move || {
                task_source.queue(task!(offline_suspend: move |cx| {
                    let this = this.root();
                    this.suspend_frames.borrow_mut().retain(|f| *f != frame);
                    // The rendering thread is suspended: the control thread state follows,
                    // then the promise resolves.
                    this.context.set_state_and_notify(AudioContextState::Suspended);
                    trusted_promise.root(cx).resolve_native(cx, &());
                }));
            }),
        );
        promise
    }
}
