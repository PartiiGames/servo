/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::codegen::GenericBindings::AudioBufferBinding::AudioBufferMethods;
use script_bindings::codegen::GenericBindings::BaseAudioContextBinding::BaseAudioContextMethods;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_media::audio::audio_node::{AudioNodeInit, AudioNodeMessage};
use servo_media::audio::convolver_node::{
    ConvolverBuffer, ConvolverNodeMessage, ConvolverNodeOptions,
};

use crate::dom::audio::audiobuffer::AudioBuffer;
use crate::dom::audio::audionode::{AudioNode, AudioNodeOptionsHelper};
use crate::dom::audio::baseaudiocontext::BaseAudioContext;
use crate::dom::bindings::codegen::Bindings::AudioNodeBinding::{
    AudioNodeMethods, ChannelCountMode, ChannelInterpretation,
};
use crate::dom::bindings::codegen::Bindings::ConvolverNodeBinding::{
    ConvolverNodeMethods, ConvolverOptions,
};
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::window::Window;

/// <https://webaudio.github.io/web-audio-api/#ConvolverNode>
#[dom_struct]
pub(crate) struct ConvolverNode {
    node: AudioNode,
    buffer: MutNullableDom<AudioBuffer>,
    normalize: Cell<bool>,
}

impl ConvolverNode {
    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    pub(crate) fn new_inherited(
        cx: &mut JSContext,
        context: &BaseAudioContext,
        options: &ConvolverOptions,
    ) -> Fallible<ConvolverNode> {
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
        let node = AudioNode::new_inherited(
            cx,
            AudioNodeInit::ConvolverNode(ConvolverNodeOptions::default()),
            context,
            node_options,
            1, // inputs
            1, // outputs
        )?;
        Ok(ConvolverNode {
            node,
            buffer: Default::default(),
            // > 1. Set the attributes normalize to the inverse of the value of
            // > disableNormalization.
            normalize: Cell::new(!options.disableNormalization),
        })
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        context: &BaseAudioContext,
        options: &ConvolverOptions,
    ) -> Fallible<DomRoot<ConvolverNode>> {
        Self::new_with_proto(cx, window, None, context, options)
    }

    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    fn new_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        context: &BaseAudioContext,
        options: &ConvolverOptions,
    ) -> Fallible<DomRoot<ConvolverNode>> {
        let node = ConvolverNode::new_inherited(cx, context, options)?;
        let node = reflect_dom_object_with_proto(cx, Box::new(node), window, proto);
        // > 2. If buffer exists, set the buffer attribute to its value.
        if let Some(Some(buffer)) = &options.buffer {
            node.SetBuffer(cx, Some(buffer))?;
        }
        Ok(node)
    }
}

impl ConvolverNodeMethods<crate::DomTypeHolder> for ConvolverNode {
    /// <https://webaudio.github.io/web-audio-api/#dom-convolvernode-convolvernode>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        context: &BaseAudioContext,
        options: &ConvolverOptions,
    ) -> Fallible<DomRoot<ConvolverNode>> {
        ConvolverNode::new_with_proto(cx, window, proto, context, options)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-convolvernode-buffer>
    fn GetBuffer(&self) -> Option<DomRoot<AudioBuffer>> {
        self.buffer.get()
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-convolvernode-buffer>
    fn SetBuffer(&self, cx: &mut JSContext, new_buffer: Option<&AudioBuffer>) -> ErrorResult {
        let message = match new_buffer {
            Some(buffer) => {
                // > 1. If the buffer number of channels is not 1, 2, 4, or if the sample-rate of
                // > the buffer is not the same as the sample-rate of its associated
                // > BaseAudioContext, a NotSupportedError MUST be thrown.
                let channels = buffer.NumberOfChannels();
                if !matches!(channels, 1 | 2 | 4) ||
                    *buffer.SampleRate() != *self.node.Context().SampleRate()
                {
                    return Err(Error::NotSupported(None));
                }
                // > 2. Acquire the content of the AudioBuffer.
                let Some(content) = buffer.get_channels(cx) else {
                    return Err(Error::NotSupported(None));
                };
                // The buffer and the normalize attribute at this time configure the node.
                Some(ConvolverBuffer {
                    channels: content.buffers.clone(),
                    sample_rate: content.sample_rate,
                    normalize: self.normalize.get(),
                })
            },
            None => None,
        };
        self.buffer.set(new_buffer);
        self.node
            .message(AudioNodeMessage::ConvolverNode(ConvolverNodeMessage::SetBuffer(
                message,
            )));
        Ok(())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-convolvernode-normalize>
    fn Normalize(&self) -> bool {
        self.normalize.get()
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-convolvernode-normalize>
    ///
    /// > Changes to this value do not take effect until the next time the buffer attribute
    /// > is set.
    fn SetNormalize(&self, normalize: bool) {
        self.normalize.set(normalize);
    }
}
