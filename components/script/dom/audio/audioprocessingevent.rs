/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::reflector::reflect_dom_object_with_proto;
use stylo_atoms::Atom;

use crate::dom::audio::audiobuffer::AudioBuffer;
use crate::dom::bindings::codegen::Bindings::AudioProcessingEventBinding::{
    AudioProcessingEventInit, AudioProcessingEventMethods,
};
use crate::dom::bindings::codegen::Bindings::EventBinding::EventMethods;
use crate::dom::bindings::error::Fallible;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::window::Window;

/// <https://webaudio.github.io/web-audio-api/#AudioProcessingEvent>
#[dom_struct]
pub(crate) struct AudioProcessingEvent {
    event: Event,
    playback_time: f64,
    input_buffer: Dom<AudioBuffer>,
    output_buffer: Dom<AudioBuffer>,
}

impl AudioProcessingEvent {
    fn new_inherited(
        playback_time: f64,
        input_buffer: &AudioBuffer,
        output_buffer: &AudioBuffer,
    ) -> AudioProcessingEvent {
        AudioProcessingEvent {
            event: Event::new_inherited(),
            playback_time,
            input_buffer: Dom::from_ref(input_buffer),
            output_buffer: Dom::from_ref(output_buffer),
        }
    }

    #[expect(clippy::too_many_arguments)]
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        type_: Atom,
        bubbles: EventBubbles,
        cancelable: EventCancelable,
        playback_time: f64,
        input_buffer: &AudioBuffer,
        output_buffer: &AudioBuffer,
    ) -> DomRoot<AudioProcessingEvent> {
        Self::new_with_proto(
            cx,
            window,
            None,
            type_,
            bubbles,
            cancelable,
            playback_time,
            input_buffer,
            output_buffer,
        )
    }

    #[expect(clippy::too_many_arguments)]
    fn new_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        type_: Atom,
        bubbles: EventBubbles,
        cancelable: EventCancelable,
        playback_time: f64,
        input_buffer: &AudioBuffer,
        output_buffer: &AudioBuffer,
    ) -> DomRoot<AudioProcessingEvent> {
        let event = Box::new(AudioProcessingEvent::new_inherited(
            playback_time,
            input_buffer,
            output_buffer,
        ));
        let ev = reflect_dom_object_with_proto(cx, event, window, proto);
        ev.upcast::<Event>()
            .init_event(type_, bool::from(bubbles), bool::from(cancelable));
        ev
    }
}

impl AudioProcessingEventMethods<crate::DomTypeHolder> for AudioProcessingEvent {
    /// <https://webaudio.github.io/web-audio-api/#dom-audioprocessingevent-audioprocessingevent>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        type_: DOMString,
        init: &AudioProcessingEventInit,
    ) -> Fallible<DomRoot<AudioProcessingEvent>> {
        Ok(AudioProcessingEvent::new_with_proto(
            cx,
            window,
            proto,
            Atom::from(type_),
            EventBubbles::from(init.parent.bubbles),
            EventCancelable::from(init.parent.cancelable),
            *init.playbackTime,
            &init.inputBuffer,
            &init.outputBuffer,
        ))
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioprocessingevent-playbacktime>
    fn PlaybackTime(&self) -> Finite<f64> {
        Finite::wrap(self.playback_time)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioprocessingevent-inputbuffer>
    fn InputBuffer(&self) -> DomRoot<AudioBuffer> {
        DomRoot::from_ref(&*self.input_buffer)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioprocessingevent-outputbuffer>
    fn OutputBuffer(&self) -> DomRoot<AudioBuffer> {
        DomRoot::from_ref(&*self.output_buffer)
    }

    /// <https://dom.spec.whatwg.org/#dom-event-istrusted>
    fn IsTrusted(&self) -> bool {
        self.event.IsTrusted()
    }
}
