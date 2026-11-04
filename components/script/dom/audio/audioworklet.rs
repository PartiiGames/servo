/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! <https://webaudio.github.io/web-audio-api/#AudioWorklet>

use std::rc::Rc;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::reflect_dom_object;

use crate::dom::audio::baseaudiocontext::BaseAudioContext;
use crate::dom::audioworkletglobalscope::AudioWorkletThreadPool;
use crate::dom::bindings::codegen::Bindings::AudioWorkletBinding::AudioWorkletMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::messageport::MessagePort;
use crate::dom::window::Window;
use crate::dom::worklet::Worklet;
use crate::dom::workletglobalscope::WorkletGlobalScopeType;

#[dom_struct]
pub(crate) struct AudioWorklet {
    worklet: Worklet,
    port: MutNullableDom<MessagePort>,
}

impl AudioWorklet {
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        context: &BaseAudioContext,
    ) -> DomRoot<AudioWorklet> {
        let pool = AudioWorkletThreadPool::new(context, window.into());
        let worklet = Worklet::new_inherited(
            window,
            WorkletGlobalScopeType::Audio,
            Box::new(move || Rc::new(pool)),
        );
        reflect_dom_object(
            cx,
            Box::new(AudioWorklet {
                worklet,
                port: Default::default(),
            }),
            window,
        )
    }
}

impl AudioWorkletMethods<crate::DomTypeHolder> for AudioWorklet {
    /// <https://webaudio.github.io/web-audio-api/#dom-audioworklet-port>
    fn Port(&self, cx: &mut JSContext) -> DomRoot<MessagePort> {
        self.port.or_init(|| {
            // Not yet entangled with AudioWorkletGlobalScope.port: a v1 gap (see DESIGN.md).
            let global = self.upcast::<Worklet>().global();
            let port = MessagePort::new(cx, &global);
            global.track_message_port(&port, None);
            port
        })
    }
}

