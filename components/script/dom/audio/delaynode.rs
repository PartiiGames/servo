/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_media::audio::audio_node::{AudioNodeInit, AudioNodeType};
use servo_media::audio::delay_node::DelayNodeOptions;
use servo_media::audio::param::ParamType;

use crate::dom::audio::audionode::{AudioNode, AudioNodeOptionsHelper};
use crate::dom::audio::audioparam::AudioParam;
use crate::dom::audio::baseaudiocontext::BaseAudioContext;
use crate::dom::bindings::codegen::Bindings::AudioNodeBinding::{
    ChannelCountMode, ChannelInterpretation,
};
use crate::dom::bindings::codegen::Bindings::AudioParamBinding::AutomationRate;
use crate::dom::bindings::codegen::Bindings::DelayNodeBinding::{DelayNodeMethods, DelayOptions};
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::window::Window;

/// <https://webaudio.github.io/web-audio-api/#DelayNode>
#[dom_struct]
pub(crate) struct DelayNode {
    node: AudioNode,
    delay_time: Dom<AudioParam>,
}

impl DelayNode {
    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    pub(crate) fn new_inherited(
        cx: &mut JSContext,
        window: &Window,
        context: &BaseAudioContext,
        options: &DelayOptions,
    ) -> Fallible<DelayNode> {
        // > maxDelayTime: [...] If specified, this value MUST be greater than zero and less
        // > than three minutes or a NotSupportedError exception MUST be thrown.
        let max_delay_time = *options.maxDelayTime;
        if max_delay_time <= 0. || max_delay_time >= 180. {
            return Err(Error::NotSupported(None));
        }
        let node_options =
            options
                .parent
                .unwrap_or(2, ChannelCountMode::Max, ChannelInterpretation::Speakers);
        let delay_time = *options.delayTime as f32;
        let node = AudioNode::new_inherited(
            cx,
            AudioNodeInit::DelayNode(DelayNodeOptions {
                max_delay_time,
                delay_time,
            }),
            context,
            node_options,
            1, // inputs
            1, // outputs
        )?;
        // <https://webaudio.github.io/web-audio-api/#dom-delaynode-delaytime>
        let delay_time = AudioParam::new(
            cx,
            window,
            context,
            node.node_id(),
            AudioNodeType::DelayNode,
            ParamType::DelayTime,
            AutomationRate::A_rate,
            delay_time,            // default value
            0.,                    // min value
            max_delay_time as f32, // max value
        );
        Ok(DelayNode {
            node,
            delay_time: Dom::from_ref(&delay_time),
        })
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        context: &BaseAudioContext,
        options: &DelayOptions,
    ) -> Fallible<DomRoot<DelayNode>> {
        Self::new_with_proto(cx, window, None, context, options)
    }

    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    fn new_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        context: &BaseAudioContext,
        options: &DelayOptions,
    ) -> Fallible<DomRoot<DelayNode>> {
        let node = DelayNode::new_inherited(cx, window, context, options)?;
        Ok(reflect_dom_object_with_proto(
            cx,
            Box::new(node),
            window,
            proto,
        ))
    }
}

impl DelayNodeMethods<crate::DomTypeHolder> for DelayNode {
    /// <https://webaudio.github.io/web-audio-api/#dom-delaynode-delaynode>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        context: &BaseAudioContext,
        options: &DelayOptions,
    ) -> Fallible<DomRoot<DelayNode>> {
        DelayNode::new_with_proto(cx, window, proto, context, options)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-delaynode-delaytime>
    fn DelayTime(&self) -> DomRoot<AudioParam> {
        DomRoot::from_ref(&self.delay_time)
    }
}
