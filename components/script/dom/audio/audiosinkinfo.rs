/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! <https://webaudio.github.io/web-audio-api/#AudioSinkInfo>: AudioContext.sinkId when the
//! context renders to no device (`{type: "none"}`).

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::AudioContextBinding::{
    AudioSinkInfoMethods, AudioSinkType,
};
use crate::dom::bindings::root::DomRoot;
use crate::dom::window::Window;

#[dom_struct]
pub(crate) struct AudioSinkInfo {
    reflector_: Reflector,
    sink_type: AudioSinkType,
}

impl AudioSinkInfo {
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        sink_type: AudioSinkType,
    ) -> DomRoot<AudioSinkInfo> {
        reflect_dom_object(
            cx,
            Box::new(AudioSinkInfo {
                reflector_: Reflector::new(),
                sink_type,
            }),
            window,
        )
    }
}

impl AudioSinkInfoMethods<crate::DomTypeHolder> for AudioSinkInfo {
    /// <https://webaudio.github.io/web-audio-api/#dom-audiosinkinfo-type>
    fn Type(&self) -> AudioSinkType {
        self.sink_type
    }
}
