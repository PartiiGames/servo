/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! <https://webaudio.github.io/web-audio-api/#AudioWorkletProcessor>

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_proto};

use crate::dom::audioworkletglobalscope::AudioWorkletGlobalScope;
use crate::dom::bindings::codegen::Bindings::AudioWorkletProcessorBinding::AudioWorkletProcessorMethods;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::globalscope::GlobalScope;
use crate::dom::messageport::MessagePort;

#[dom_struct]
pub(crate) struct AudioWorkletProcessor {
    reflector_: Reflector,
    port: Dom<MessagePort>,
}

impl AudioWorkletProcessorMethods<crate::DomTypeHolder> for AudioWorkletProcessor {
    /// <https://webaudio.github.io/web-audio-api/#dom-audioworkletprocessor-audioworkletprocessor>
    fn Constructor(
        cx: &mut JSContext,
        global: &AudioWorkletGlobalScope,
        proto: Option<HandleObject>,
    ) -> Fallible<DomRoot<AudioWorkletProcessor>> {
        // Step 1-3. Only constructible with pending processor construction data, i.e. by
        // an AudioWorkletNode creating it.
        let Some(port) = global.take_pending_processor_port() else {
            return Err(Error::Type(c"Illegal constructor".to_owned()));
        };
        // Step 4-6. Set the processor's port; clear the construction data (taken above).
        Ok(reflect_dom_object_with_proto(
            cx,
            Box::new(AudioWorkletProcessor {
                reflector_: Reflector::new(),
                port: Dom::from_ref(&*port),
            }),
            global.upcast::<GlobalScope>(),
            proto,
        ))
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioworkletprocessor-port>
    fn Port(&self) -> DomRoot<MessagePort> {
        DomRoot::from_ref(&self.port)
    }
}
