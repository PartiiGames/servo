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
use crate::dom::bindings::codegen::Bindings::MessageChannelBinding::MessageChannelMethods;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::structuredclone;
use crate::dom::globalscope::GlobalScope;
use crate::dom::messagechannel::MessageChannel;
use js::rust::CustomAutoRooter;
use script_bindings::reflector::DomObject;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::messageport::MessagePort;
use crate::dom::window::Window;
use crate::dom::worklet::Worklet;
use crate::dom::workletglobalscope::WorkletGlobalScopeType;

#[dom_struct]
pub(crate) struct AudioWorklet {
    worklet: Worklet,
    /// Entangled with the AudioWorkletGlobalScope's `port` (its end is transferred to the render
    /// thread with the first addModule()).
    port: Dom<MessagePort>,
}

impl AudioWorklet {
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        context: &BaseAudioContext,
    ) -> DomRoot<AudioWorklet> {
        // <https://webaudio.github.io/web-audio-api/#dom-audioworklet-port>: a MessageChannel; the
        // other end becomes AudioWorkletGlobalScope.port.
        let global = window.upcast::<GlobalScope>();
        let channel = MessageChannel::new(cx, global, None);
        let port1 = channel.Port1();
        let port2 = channel.Port2();
        rooted!(&in(cx) let undefined = js::jsval::UndefinedValue());
        let mut transfer = CustomAutoRooter::new(vec![port2.reflector().get_jsobject().get()]);
        #[expect(unsafe_code)]
        let transfer = transfer.root(unsafe { cx.raw_cx() });
        let global_port = structuredclone::write(cx, undefined.handle(), Some(transfer)).ok();
        let pool = AudioWorkletThreadPool::new(context, window.into(), global_port);
        let worklet = Worklet::new_inherited(
            window,
            WorkletGlobalScopeType::Audio,
            Box::new(move || Rc::new(pool)),
        );
        reflect_dom_object(
            cx,
            Box::new(AudioWorklet {
                worklet,
                port: Dom::from_ref(&*port1),
            }),
            window,
        )
    }
}

impl AudioWorkletMethods<crate::DomTypeHolder> for AudioWorklet {
    /// <https://webaudio.github.io/web-audio-api/#dom-audioworklet-port>
    fn Port(&self, _cx: &mut JSContext) -> DomRoot<MessagePort> {
        DomRoot::from_ref(&self.port)
    }
}

