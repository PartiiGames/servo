/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_media::audio::audio_node::{AudioNodeInit, AudioNodeType};
use servo_media::audio::dynamics_compressor_node::{
    DynamicsCompressorNodeOptions, ReductionCell,
};
use servo_media::audio::param::ParamType;

use crate::dom::audio::audionode::{AudioNode, AudioNodeOptionsHelper};
use crate::dom::audio::audioparam::AudioParam;
use crate::dom::audio::baseaudiocontext::BaseAudioContext;
use crate::dom::bindings::codegen::Bindings::AudioNodeBinding::{
    ChannelCountMode, ChannelInterpretation,
};
use crate::dom::bindings::codegen::Bindings::AudioParamBinding::AutomationRate;
use crate::dom::bindings::codegen::Bindings::DynamicsCompressorNodeBinding::{
    DynamicsCompressorNodeMethods, DynamicsCompressorOptions,
};
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::window::Window;

/// <https://webaudio.github.io/web-audio-api/#DynamicsCompressorNode>
#[dom_struct]
pub(crate) struct DynamicsCompressorNode {
    node: AudioNode,
    threshold: Dom<AudioParam>,
    knee: Dom<AudioParam>,
    ratio: Dom<AudioParam>,
    attack: Dom<AudioParam>,
    release: Dom<AudioParam>,
    /// <https://webaudio.github.io/web-audio-api/#dom-dynamicscompressornode-internal-reduction-slot>,
    /// written by the render thread once per block.
    #[no_trace]
    #[ignore_malloc_size_of = "shared with the render thread"]
    reduction: ReductionCell,
}

impl DynamicsCompressorNode {
    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    pub(crate) fn new_inherited(
        cx: &mut JSContext,
        window: &Window,
        context: &BaseAudioContext,
        options: &DynamicsCompressorOptions,
    ) -> Fallible<DynamicsCompressorNode> {
        let node_options = options.parent.unwrap_or(
            2,
            ChannelCountMode::Clamped_max,
            ChannelInterpretation::Speakers,
        );
        // Channel count constraints: at most 2. Channel count mode constraints: not "max".
        // <https://webaudio.github.io/web-audio-api/#audionode-channelcount-constraints>
        if node_options.count > 2 || node_options.count == 0 {
            return Err(Error::NotSupported(None));
        }
        if node_options.mode == ChannelCountMode::Max {
            return Err(Error::NotSupported(None));
        }
        let reduction = ReductionCell::default();
        let node = AudioNode::new_inherited(
            cx,
            AudioNodeInit::DynamicsCompressionNode(DynamicsCompressorNodeOptions {
                threshold: *options.threshold,
                knee: *options.knee,
                ratio: *options.ratio,
                attack: *options.attack,
                release: *options.release,
                reduction: reduction.clone(),
            }),
            context,
            node_options,
            1, // inputs
            1, // outputs
        )?;
        let param = |cx: &mut JSContext, param, default, min, max| {
            AudioParam::new(
                cx,
                window,
                context,
                node.node_id(),
                AudioNodeType::DynamicsCompressionNode,
                param,
                AutomationRate::K_rate,
                default,
                min,
                max,
            )
        };
        // <https://webaudio.github.io/web-audio-api/#dom-dynamicscompressornode-threshold>
        let threshold = param(cx, ParamType::Threshold, *options.threshold, -100., 0.);
        // <https://webaudio.github.io/web-audio-api/#dom-dynamicscompressornode-knee>
        let knee = param(cx, ParamType::Knee, *options.knee, 0., 40.);
        // <https://webaudio.github.io/web-audio-api/#dom-dynamicscompressornode-ratio>
        let ratio = param(cx, ParamType::Ratio, *options.ratio, 1., 20.);
        // <https://webaudio.github.io/web-audio-api/#dom-dynamicscompressornode-attack>
        let attack = param(cx, ParamType::Attack, *options.attack, 0., 1.);
        // <https://webaudio.github.io/web-audio-api/#dom-dynamicscompressornode-release>
        let release = param(cx, ParamType::Release, *options.release, 0., 1.);
        Ok(DynamicsCompressorNode {
            node,
            threshold: Dom::from_ref(&threshold),
            knee: Dom::from_ref(&knee),
            ratio: Dom::from_ref(&ratio),
            attack: Dom::from_ref(&attack),
            release: Dom::from_ref(&release),
            reduction,
        })
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        context: &BaseAudioContext,
        options: &DynamicsCompressorOptions,
    ) -> Fallible<DomRoot<DynamicsCompressorNode>> {
        Self::new_with_proto(cx, window, None, context, options)
    }

    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    fn new_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        context: &BaseAudioContext,
        options: &DynamicsCompressorOptions,
    ) -> Fallible<DomRoot<DynamicsCompressorNode>> {
        let node = DynamicsCompressorNode::new_inherited(cx, window, context, options)?;
        Ok(reflect_dom_object_with_proto(
            cx,
            Box::new(node),
            window,
            proto,
        ))
    }
}

impl DynamicsCompressorNodeMethods<crate::DomTypeHolder> for DynamicsCompressorNode {
    /// <https://webaudio.github.io/web-audio-api/#dom-dynamicscompressornode-dynamicscompressornode>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        context: &BaseAudioContext,
        options: &DynamicsCompressorOptions,
    ) -> Fallible<DomRoot<DynamicsCompressorNode>> {
        DynamicsCompressorNode::new_with_proto(cx, window, proto, context, options)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-dynamicscompressornode-threshold>
    fn Threshold(&self) -> DomRoot<AudioParam> {
        DomRoot::from_ref(&self.threshold)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-dynamicscompressornode-knee>
    fn Knee(&self) -> DomRoot<AudioParam> {
        DomRoot::from_ref(&self.knee)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-dynamicscompressornode-ratio>
    fn Ratio(&self) -> DomRoot<AudioParam> {
        DomRoot::from_ref(&self.ratio)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-dynamicscompressornode-reduction>
    ///
    /// > When this attribute is read, return the value of the private slot
    /// > [[internal reduction]].
    fn Reduction(&self) -> Finite<f32> {
        Finite::wrap(self.reduction.get())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-dynamicscompressornode-attack>
    fn Attack(&self) -> DomRoot<AudioParam> {
        DomRoot::from_ref(&self.attack)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-dynamicscompressornode-release>
    fn Release(&self) -> DomRoot<AudioParam> {
        DomRoot::from_ref(&self.release)
    }
}
