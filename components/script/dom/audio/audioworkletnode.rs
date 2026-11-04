/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! <https://webaudio.github.io/web-audio-api/#AudioWorkletNode>

use dom_struct::dom_struct;
use js::context::JSContext;
use js::jsval::{ObjectValue, UInt32Value};
use js::rust::wrappers2::{JS_DefineProperty, JS_NewPlainObject};
use js::rust::{CustomAutoRooter, HandleObject};
use script_bindings::reflector::{DomObject, reflect_dom_object_with_proto};
use servo_media::audio::audio_node::{AudioNodeInit, AudioNodeType};
use servo_media::audio::audio_worklet_node::{AudioWorkletNodeOptions as MediaOptions, AudioWorkletParamOptions};
use servo_media::audio::param::{ParamRate, ParamType};

use crate::dom::audio::audionode::{AudioNode, AudioNodeOptionsHelper, MAX_CHANNEL_COUNT};
use crate::dom::bindings::codegen::Bindings::MessageChannelBinding::MessageChannelMethods;
use crate::dom::audio::audioparam::AudioParam;
use crate::dom::audio::audioparammap::AudioParamMap;
use crate::dom::audio::baseaudiocontext::BaseAudioContext;
use crate::dom::audioworkletglobalscope::{NodeSlot, ProcessorHandle};
use crate::dom::bindings::codegen::Bindings::AudioNodeBinding::{
    ChannelCountMode, ChannelInterpretation,
};
use crate::dom::bindings::codegen::Bindings::AudioParamBinding::AutomationRate;
use crate::dom::bindings::codegen::Bindings::AudioWorkletNodeBinding::{
    AudioWorkletNodeMethods, AudioWorkletNodeOptions,
};
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::structuredclone;
use crate::dom::event::{EventBubbles, EventCancelable, EventComposed};
use stylo_atoms::Atom;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::messagechannel::MessageChannel;
use crate::dom::messageport::MessagePort;
use crate::dom::window::Window;

#[dom_struct]
pub(crate) struct AudioWorkletNode {
    node: AudioNode,
    parameters: Dom<AudioParamMap>,
    port: Dom<MessagePort>,
}

impl AudioWorkletNode {
    #[expect(unsafe_code)]
    fn new_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        context: &BaseAudioContext,
        name: DOMString,
        options: &AudioWorkletNodeOptions,
    ) -> Fallible<DomRoot<AudioWorkletNode>> {
        let name = String::from(name);
        // Step 1-2. The name must be registered in this context's AudioWorkletGlobalScope.
        let Some(descriptors) = context.audio_worklet_processor(&name) else {
            return Err(Error::InvalidState(None));
        };
        let number_of_inputs = options.numberOfInputs;
        let number_of_outputs = options.numberOfOutputs;
        // Step 3-4. Validate the counts.
        if number_of_inputs == 0 && number_of_outputs == 0 {
            return Err(Error::NotSupported(None));
        }
        if number_of_inputs > 32 || number_of_outputs > 32 {
            return Err(Error::NotSupported(None));
        }
        let output_channel_count = match &options.outputChannelCount {
            Some(counts) => {
                if counts.len() != number_of_outputs as usize {
                    return Err(Error::IndexSize(None));
                }
                if counts.iter().any(|c| *c == 0 || *c > MAX_CHANNEL_COUNT) {
                    return Err(Error::NotSupported(None));
                }
                Some(counts.iter().map(|c| *c as u8).collect::<Vec<u8>>())
            },
            None => None,
        };

        // Step 5-6. A MessageChannel: port1 is the node's, port2 goes to the processor.
        let global = window.upcast::<GlobalScope>();
        let channel = MessageChannel::new(cx, global, None);
        let port1 = channel.Port1();
        let port2 = channel.Port2();

        // Step 7-8. Serialize the options, transferring port2 with them.
        rooted!(&in(cx) let options_obj = unsafe { JS_NewPlainObject(cx) });
        if options_obj.get().is_null() {
            return Err(Error::JSFailed);
        }
        unsafe {
            rooted!(&in(cx) let v = UInt32Value(number_of_inputs));
            JS_DefineProperty(cx, options_obj.handle(), c"numberOfInputs".as_ptr(), v.handle(), js::jsapi::JSPROP_ENUMERATE as u32);
            rooted!(&in(cx) let v = UInt32Value(number_of_outputs));
            JS_DefineProperty(cx, options_obj.handle(), c"numberOfOutputs".as_ptr(), v.handle(), js::jsapi::JSPROP_ENUMERATE as u32);
            if let Some(processor_options) = options.processorOptions.as_ref() {
                rooted!(&in(cx) let v = ObjectValue(processor_options.get()));
                JS_DefineProperty(cx, options_obj.handle(), c"processorOptions".as_ptr(), v.handle(), js::jsapi::JSPROP_ENUMERATE as u32);
            }
        }
        rooted!(&in(cx) let options_value = ObjectValue(options_obj.get()));
        let mut transfer = CustomAutoRooter::new(vec![port2.reflector().get_jsobject().get()]);
        let transfer = transfer.root(unsafe { cx.raw_cx() });
        let serialized = structuredclone::write(cx, options_value.handle(), Some(transfer))?;

        // Params: initial values from parameterData.
        let param_options: Vec<AudioWorkletParamOptions> = descriptors
            .iter()
            .map(|d| {
                let initial = options
                    .parameterData
                    .as_ref()
                    .and_then(|data| data.get(&DOMString::from(d.name.as_str())))
                    .map(|v| **v as f32)
                    .unwrap_or(d.default_value);
                AudioWorkletParamOptions {
                    default_value: d.default_value,
                    min_value: d.min_value,
                    max_value: d.max_value,
                    rate: if d.a_rate { ParamRate::ARate } else { ParamRate::KRate },
                    initial_value: initial.clamp(d.min_value, d.max_value),
                }
            })
            .collect();

        let node_options =
            options
                .parent
                .unwrap_or(2, ChannelCountMode::Max, ChannelInterpretation::Speakers);

        // The processor reports errors to the node, which doesn't exist until the engine does:
        // it gets the node through this slot, filled below (errors are queued tasks, so they
        // always run after that).
        let node_slot: NodeSlot = Default::default();
        let handle = ProcessorHandle::new(
            name,
            serialized,
            node_slot.clone(),
            number_of_inputs as usize,
            number_of_outputs as usize,
        );
        let init = AudioNodeInit::AudioWorkletNode(MediaOptions {
            number_of_inputs,
            number_of_outputs,
            output_channel_count,
            params: param_options.clone(),
            processor: Box::new(handle),
        });
        let audio_node = AudioNode::new_inherited(
            cx,
            init,
            context,
            node_options,
            number_of_inputs,
            number_of_outputs,
        )?;
        let node_id = audio_node.node_id();
        let parameters = AudioParamMap::new(cx, window);
        let node = reflect_dom_object_with_proto(
            cx,
            Box::new(AudioWorkletNode {
                node: audio_node,
                parameters: Dom::from_ref(&*parameters),
                port: Dom::from_ref(&*port1),
            }),
            window,
            proto,
        );
        *node_slot.lock().unwrap() = Some(Trusted::new(&*node));

        // Step 9. parameters: an AudioParam per descriptor.
        for (index, descriptor) in descriptors.iter().enumerate() {
            let audio_param = AudioParam::new(
                cx,
                window,
                context,
                node_id,
                AudioNodeType::AudioWorkletNode,
                ParamType::Worklet(index as u16),
                if descriptor.a_rate {
                    AutomationRate::A_rate
                } else {
                    AutomationRate::K_rate
                },
                descriptor.default_value,
                descriptor.min_value,
                descriptor.max_value,
            );
            parameters.insert(cx, DOMString::from(descriptor.name.as_str()), &audio_param);
        }

        Ok(node)
    }

    /// Fire `processorerror` (the processor's constructor or `process()` threw).
    pub(crate) fn fire_processor_error(&self, cx: &mut JSContext) {
        self.upcast::<EventTarget>().fire_event_with_params(
            cx,
            Atom::from("processorerror"),
            EventBubbles::DoesNotBubble,
            EventCancelable::NotCancelable,
            EventComposed::NotComposed,
        );
    }
}

impl AudioWorkletNodeMethods<crate::DomTypeHolder> for AudioWorkletNode {
    /// <https://webaudio.github.io/web-audio-api/#dom-audioworkletnode-audioworkletnode>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        context: &BaseAudioContext,
        name: DOMString,
        options: &AudioWorkletNodeOptions,
    ) -> Fallible<DomRoot<AudioWorkletNode>> {
        AudioWorkletNode::new_with_proto(cx, window, proto, context, name, options)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioworkletnode-parameters>
    fn Parameters(&self, _cx: &mut JSContext) -> DomRoot<AudioParamMap> {
        DomRoot::from_ref(&self.parameters)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioworkletnode-port>
    fn Port(&self) -> DomRoot<MessagePort> {
        DomRoot::from_ref(&self.port)
    }

    // https://webaudio.github.io/web-audio-api/#dom-audioworkletnode-onprocessorerror
    event_handler!(processorerror, GetOnprocessorerror, SetOnprocessorerror);
}
