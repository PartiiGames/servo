/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::sync::Arc;

use dom_struct::dom_struct;
use js::context::JSContext;
use log::warn;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::InheritTypes::{
    AudioNodeTypeId, AudioScheduledSourceNodeTypeId, EventTargetTypeId,
};
use servo_media::audio::context::NodeUnreferencedOnDrop;
use servo_media::audio::audio_node::{
    AudioNodeInit, AudioNodeMessage, ChannelCountMode as ServoMediaChannelCountMode, ChannelInfo,
    ChannelInterpretation as ServoMediaChannelInterpretation,
};
use servo_media::audio::graph::NodeId;
use servo_media::audio::param::ParamType;

use crate::conversions::Convert;
use crate::dom::audio::audioparam::AudioParam;
use crate::dom::audio::baseaudiocontext::BaseAudioContext;
use crate::dom::bindings::codegen::Bindings::AudioNodeBinding::{
    AudioNodeMethods, AudioNodeOptions, ChannelCountMode, ChannelInterpretation,
};
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::console::Console;
use crate::dom::eventtarget::EventTarget;

/// Where one of this node's outputs goes: an input of a node, or an AudioParam.
#[derive(Clone, Copy, PartialEq)]
enum ConnectionTarget {
    Input(NodeId, u32),
    Param(NodeId, ParamType),
}

// 32 is the minimum required by the spec for createBuffer() and the deprecated
// createScriptProcessor() and matches what is used by Blink and Gecko.
// The limit protects against large memory allocations.
pub(crate) const MAX_CHANNEL_COUNT: u32 = 32;

#[dom_struct]
pub(crate) struct AudioNode {
    eventtarget: EventTarget,
    #[no_trace]
    node_id: Option<NodeId>,
    context: Dom<BaseAudioContext>,
    number_of_inputs: u32,
    number_of_outputs: u32,
    channel_count: Cell<u32>,
    channel_count_mode: Cell<ChannelCountMode>,
    channel_interpretation: Cell<ChannelInterpretation>,
    /// Node lifetime: tells the engine this node is unreferenced when this AudioNode (and its
    /// AudioParams, which share it) are finalized. None for the destination node.
    #[no_trace]
    #[ignore_malloc_size_of = "servo_media"]
    lifetime: Option<Arc<NodeUnreferencedOnDrop>>,
    /// This node's connections, (output, target), so that disconnect() can throw
    /// InvalidAccessError for one that doesn't exist. By NodeId, so they keep no node alive; a
    /// target's engine node (and so its id) lives on while it has this input.
    #[no_trace]
    #[ignore_malloc_size_of = "servo_media"]
    connections: DomRefCell<Vec<(u32, ConnectionTarget)>>,
}

impl AudioNode {
    pub(crate) fn new_inherited(
        cx: &mut JSContext,
        node_type: AudioNodeInit,
        context: &BaseAudioContext,
        options: UnwrappedAudioNodeOptions,
        number_of_inputs: u32,
        number_of_outputs: u32,
    ) -> Fallible<AudioNode> {
        if options.count == 0 || options.count > MAX_CHANNEL_COUNT {
            return Err(Error::NotSupported(None));
        }
        let ch = ChannelInfo {
            count: options.count as u8,
            mode: options.mode.convert(),
            interpretation: options.interpretation.convert(),
            context_channel_count: context.channel_count() as u8,
        };
        let node_id = context
            .audio_context_impl()
            .lock()
            .unwrap()
            .create_node(node_type, ch);

        if node_id.is_none() {
            // Follow Chromuim and Gecko, we just warn and create an inert AudioNode.
            const MESSAGE: &str =
                "Failed to create an AudioNode backend. The constructed AudioNode will be inert.";
            warn!("{MESSAGE}");
            Console::internal_warn(cx, &context.global(), MESSAGE.to_string());
        }

        let mut node = AudioNode::new_inherited_for_id(
            node_id,
            context,
            options,
            number_of_inputs,
            number_of_outputs,
        );
        node.lifetime = node_id.map(|id| context.register_node_lifetime(id));
        Ok(node)
    }

    pub(crate) fn new_inherited_for_id(
        node_id: Option<NodeId>,
        context: &BaseAudioContext,
        options: UnwrappedAudioNodeOptions,
        number_of_inputs: u32,
        number_of_outputs: u32,
    ) -> AudioNode {
        AudioNode {
            eventtarget: EventTarget::new_inherited(),
            node_id,
            context: Dom::from_ref(context),
            number_of_inputs,
            number_of_outputs,
            channel_count: Cell::new(options.count),
            channel_count_mode: Cell::new(options.mode),
            channel_interpretation: Cell::new(options.interpretation),
            lifetime: None,
            connections: Default::default(),
        }
    }

    fn add_connection(&self, output: u32, target: ConnectionTarget) {
        let mut connections = self.connections.borrow_mut();
        if !connections.contains(&(output, target)) {
            connections.push((output, target));
        }
    }

    /// Forget the connections that match; false (InvalidAccessError) if none did.
    fn remove_connections(&self, matches: impl Fn(u32, ConnectionTarget) -> bool) -> bool {
        let mut connections = self.connections.borrow_mut();
        let before = connections.len();
        connections.retain(|&(output, target)| !matches(output, target));
        connections.len() != before
    }

    pub(crate) fn message(&self, message: AudioNodeMessage) {
        if let Some(node_id) = self.node_id {
            self.context
                .audio_context_impl()
                .lock()
                .unwrap()
                .message_node(node_id, message);
        }
    }

    pub(crate) fn node_id(&self) -> Option<NodeId> {
        self.node_id
    }
}

impl AudioNodeMethods<crate::DomTypeHolder> for AudioNode {
    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-connect>
    fn Connect(
        &self,
        destination: &AudioNode,
        output: u32,
        input: u32,
    ) -> Fallible<DomRoot<AudioNode>> {
        if self.context != destination.context {
            return Err(Error::InvalidAccess(None));
        }

        if output >= self.NumberOfOutputs() || input >= destination.NumberOfInputs() {
            return Err(Error::IndexSize(None));
        }

        // servo-media takes care of ignoring duplicated connections.

        let Some(source_id) = self.node_id() else {
            return Ok(DomRoot::from_ref(destination));
        };
        let Some(dest_id) = destination.node_id() else {
            return Ok(DomRoot::from_ref(destination));
        };

        self.context
            .audio_context_impl()
            .lock()
            .unwrap()
            .connect_ports(source_id.output(output), dest_id.input(input));
        self.add_connection(output, ConnectionTarget::Input(dest_id, input));

        Ok(DomRoot::from_ref(destination))
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-connect-destinationparam-output>
    fn Connect_(&self, dest: &AudioParam, output: u32) -> Fallible<()> {
        if self.context != dest.context() {
            return Err(Error::InvalidAccess(None));
        }

        if output >= self.NumberOfOutputs() {
            return Err(Error::IndexSize(None));
        }

        // servo-media takes care of ignoring duplicated connections.

        let Some(source_id) = self.node_id() else {
            return Ok(());
        };
        let Some(param_node) = dest.node_id() else {
            return Ok(());
        };

        self.context
            .audio_context_impl()
            .lock()
            .unwrap()
            .connect_ports(
                source_id.output(output),
                param_node.param(dest.param_type()),
            );
        self.add_connection(
            output,
            ConnectionTarget::Param(param_node, dest.param_type()),
        );

        Ok(())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-disconnect>
    fn Disconnect(&self) -> ErrorResult {
        if let Some(node_id) = self.node_id() {
            self.connections.borrow_mut().clear();
            self.context
                .audio_context_impl()
                .lock()
                .unwrap()
                .disconnect_all_from(node_id);
        }
        Ok(())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-disconnect-output>
    fn Disconnect_(&self, out: u32) -> ErrorResult {
        if out >= self.NumberOfOutputs() {
            return Err(Error::IndexSize(None));
        }
        if let Some(node_id) = self.node_id() {
            self.remove_connections(|output, _| output == out);
            self.context
                .audio_context_impl()
                .lock()
                .unwrap()
                .disconnect_output(node_id.output(out));
        }
        Ok(())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-disconnect-destinationnode>
    fn Disconnect__(&self, to: &AudioNode) -> ErrorResult {
        if let (Some(from_node), Some(to_node)) = (self.node_id(), to.node_id()) {
            if !self.remove_connections(
                |_, target| matches!(target, ConnectionTarget::Input(node, _) if node == to_node),
            ) {
                return Err(Error::InvalidAccess(None));
            }
            self.context
                .audio_context_impl()
                .lock()
                .unwrap()
                .disconnect_between(from_node, to_node);
        }
        Ok(())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-disconnect-destinationnode-output>
    fn Disconnect___(&self, to: &AudioNode, out: u32) -> ErrorResult {
        if out >= self.NumberOfOutputs() {
            return Err(Error::IndexSize(None));
        }
        if let (Some(from_node), Some(to_node)) = (self.node_id(), to.node_id()) {
            if !self.remove_connections(|output, target| {
                output == out &&
                    matches!(target, ConnectionTarget::Input(node, _) if node == to_node)
            }) {
                return Err(Error::InvalidAccess(None));
            }
            self.context
                .audio_context_impl()
                .lock()
                .unwrap()
                .disconnect_output_between(from_node.output(out), to_node);
        }
        Ok(())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-disconnect-destinationnode-output-input>
    fn Disconnect____(&self, to: &AudioNode, out: u32, inp: u32) -> ErrorResult {
        if out >= self.NumberOfOutputs() || inp >= to.NumberOfInputs() {
            return Err(Error::IndexSize(None));
        }
        if let (Some(from_node), Some(to_node)) = (self.node_id(), to.node_id()) {
            let target = ConnectionTarget::Input(to_node, inp);
            if !self.remove_connections(|output, t| output == out && t == target) {
                return Err(Error::InvalidAccess(None));
            }
            self.context
                .audio_context_impl()
                .lock()
                .unwrap()
                .disconnect_output_between_to(from_node.output(out), to_node.input(inp));
        }
        Ok(())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-disconnect-destinationparam>
    fn Disconnect_____(&self, param: &AudioParam) -> ErrorResult {
        if let (Some(from_node), Some(param_node)) = (self.node_id(), param.node_id()) {
            let target = ConnectionTarget::Param(param_node, param.param_type());
            if !self.remove_connections(|_, t| t == target) {
                return Err(Error::InvalidAccess(None));
            }
            self.context
                .audio_context_impl()
                .lock()
                .unwrap()
                .disconnect_to(from_node, param_node.param(param.param_type()));
        }
        Ok(())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-disconnect-destinationparam-output>
    fn Disconnect______(&self, param: &AudioParam, out: u32) -> ErrorResult {
        if out >= self.NumberOfOutputs() {
            return Err(Error::IndexSize(None));
        }
        if let (Some(from_node), Some(param_node)) = (self.node_id(), param.node_id()) {
            let target = ConnectionTarget::Param(param_node, param.param_type());
            if !self.remove_connections(|output, t| output == out && t == target) {
                return Err(Error::InvalidAccess(None));
            }
            self.context
                .audio_context_impl()
                .lock()
                .unwrap()
                .disconnect_output_between_to(
                    from_node.output(out),
                    param_node.param(param.param_type()),
                );
        }
        Ok(())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-context>
    fn Context(&self) -> DomRoot<BaseAudioContext> {
        DomRoot::from_ref(&self.context)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-numberofinputs>
    fn NumberOfInputs(&self) -> u32 {
        self.number_of_inputs
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-numberofoutputs>
    fn NumberOfOutputs(&self) -> u32 {
        self.number_of_outputs
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-channelcount>
    fn ChannelCount(&self) -> u32 {
        self.channel_count.get()
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-channelcount>
    fn SetChannelCount(&self, value: u32) -> ErrorResult {
        match self.upcast::<EventTarget>().type_id() {
            EventTargetTypeId::AudioNode(AudioNodeTypeId::AudioDestinationNode) => {
                if self.context.is_offline() {
                    return Err(Error::InvalidState(None));
                } else if !(1..=MAX_CHANNEL_COUNT).contains(&value) {
                    return Err(Error::IndexSize(None));
                }
            },
            EventTargetTypeId::AudioNode(AudioNodeTypeId::PannerNode) if value > 2 => {
                return Err(Error::NotSupported(None));
            },
            EventTargetTypeId::AudioNode(AudioNodeTypeId::AudioScheduledSourceNode(
                AudioScheduledSourceNodeTypeId::StereoPannerNode,
            )) if value > 2 => {
                return Err(Error::NotSupported(None));
            },
            // These are fixed: setting them to anything else throws, the same value does not.
            // <https://webaudio.github.io/web-audio-api/#ChannelMergerNode>
            EventTargetTypeId::AudioNode(AudioNodeTypeId::ChannelMergerNode) if value != 1 => {
                return Err(Error::InvalidState(None));
            },
            // <https://webaudio.github.io/web-audio-api/#ChannelSplitterNode>
            EventTargetTypeId::AudioNode(AudioNodeTypeId::ChannelSplitterNode)
                if value != self.number_of_outputs =>
            {
                return Err(Error::InvalidState(None));
            },
            // <https://webaudio.github.io/web-audio-api/#dom-dynamicscompressornode-channelcount>
            EventTargetTypeId::AudioNode(AudioNodeTypeId::DynamicsCompressorNode) if value > 2 => {
                return Err(Error::NotSupported(None));
            },
            // <https://webaudio.github.io/web-audio-api/#dom-convolvernode-channelcount>
            EventTargetTypeId::AudioNode(AudioNodeTypeId::ConvolverNode) if value > 2 => {
                return Err(Error::NotSupported(None));
            },
            // XXX We do not support any of the other AudioNodes with
            // constraints yet. Add more cases here as we add support
            // for new AudioNodes.
            _ => (),
        };

        if value == 0 || value > MAX_CHANNEL_COUNT {
            return Err(Error::NotSupported(None));
        }

        // Setting the current value changes nothing (and a splitter's engine refuses any change).
        if value == self.channel_count.get() {
            return Ok(());
        }
        self.channel_count.set(value);
        self.message(AudioNodeMessage::SetChannelCount(value as u8));
        Ok(())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-channelcountmode>
    fn ChannelCountMode(&self) -> ChannelCountMode {
        self.channel_count_mode.get()
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-channelcountmode>
    fn SetChannelCountMode(&self, value: ChannelCountMode) -> ErrorResult {
        // Channel count mode has no effect for nodes with no inputs.
        if self.number_of_inputs == 0 {
            return Ok(());
        }

        match self.upcast::<EventTarget>().type_id() {
            EventTargetTypeId::AudioNode(AudioNodeTypeId::AudioDestinationNode)
                if self.context.is_offline() =>
            {
                return Err(Error::InvalidState(None));
            },
            EventTargetTypeId::AudioNode(AudioNodeTypeId::PannerNode)
                if value == ChannelCountMode::Max =>
            {
                return Err(Error::NotSupported(None));
            },
            EventTargetTypeId::AudioNode(AudioNodeTypeId::AudioScheduledSourceNode(
                AudioScheduledSourceNodeTypeId::StereoPannerNode,
            )) if value == ChannelCountMode::Max => {
                return Err(Error::NotSupported(None));
            },
            EventTargetTypeId::AudioNode(
                AudioNodeTypeId::ChannelMergerNode | AudioNodeTypeId::ChannelSplitterNode,
            ) if value != ChannelCountMode::Explicit => {
                return Err(Error::InvalidState(None));
            },
            // <https://webaudio.github.io/web-audio-api/#dom-dynamicscompressornode-channelcountmode>
            EventTargetTypeId::AudioNode(AudioNodeTypeId::DynamicsCompressorNode)
                if value == ChannelCountMode::Max =>
            {
                return Err(Error::NotSupported(None));
            },
            // <https://webaudio.github.io/web-audio-api/#dom-convolvernode-channelcountmode>
            EventTargetTypeId::AudioNode(AudioNodeTypeId::ConvolverNode)
                if value == ChannelCountMode::Max =>
            {
                return Err(Error::NotSupported(None));
            },
            // XXX We do not support any of the other AudioNodes with
            // constraints yet. Add more cases here as we add support
            // for new AudioNodes.
            _ => (),
        };

        if value == self.channel_count_mode.get() {
            return Ok(());
        }
        self.channel_count_mode.set(value);
        self.message(AudioNodeMessage::SetChannelMode(value.convert()));
        Ok(())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-channelinterpretation>
    fn ChannelInterpretation(&self) -> ChannelInterpretation {
        self.channel_interpretation.get()
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-channelinterpretation>
    fn SetChannelInterpretation(&self, value: ChannelInterpretation) -> ErrorResult {
        // Channel interpretation mode has no effect for nodes with no inputs.
        if self.number_of_inputs == 0 {
            return Ok(());
        }

        if let EventTargetTypeId::AudioNode(AudioNodeTypeId::ChannelSplitterNode) =
            self.upcast::<EventTarget>().type_id() &&
            value != ChannelInterpretation::Discrete
        {
            return Err(Error::InvalidState(None));
        };

        if value == self.channel_interpretation.get() {
            return Ok(());
        }
        self.channel_interpretation.set(value);
        self.message(AudioNodeMessage::SetChannelInterpretation(value.convert()));
        Ok(())
    }
}

impl Convert<ServoMediaChannelCountMode> for ChannelCountMode {
    fn convert(self) -> ServoMediaChannelCountMode {
        match self {
            ChannelCountMode::Max => ServoMediaChannelCountMode::Max,
            ChannelCountMode::Clamped_max => ServoMediaChannelCountMode::ClampedMax,
            ChannelCountMode::Explicit => ServoMediaChannelCountMode::Explicit,
        }
    }
}

impl Convert<ServoMediaChannelInterpretation> for ChannelInterpretation {
    fn convert(self) -> ServoMediaChannelInterpretation {
        match self {
            ChannelInterpretation::Discrete => ServoMediaChannelInterpretation::Discrete,
            ChannelInterpretation::Speakers => ServoMediaChannelInterpretation::Speakers,
        }
    }
}

pub(crate) trait AudioNodeOptionsHelper {
    fn unwrap_or(
        &self,
        count: u32,
        mode: ChannelCountMode,
        interpretation: ChannelInterpretation,
    ) -> UnwrappedAudioNodeOptions;
}

impl AudioNodeOptionsHelper for AudioNodeOptions {
    fn unwrap_or(
        &self,
        count: u32,
        mode: ChannelCountMode,
        interpretation: ChannelInterpretation,
    ) -> UnwrappedAudioNodeOptions {
        UnwrappedAudioNodeOptions {
            count: self.channelCount.unwrap_or(count),
            mode: self.channelCountMode.unwrap_or(mode),
            interpretation: self.channelInterpretation.unwrap_or(interpretation),
        }
    }
}

/// Each node has a set of defaults, so this lets us work with them
/// easily without having to deal with the Options
pub(crate) struct UnwrappedAudioNodeOptions {
    pub(crate) count: u32,
    pub(crate) mode: ChannelCountMode,
    pub(crate) interpretation: ChannelInterpretation,
}

impl Default for UnwrappedAudioNodeOptions {
    fn default() -> Self {
        UnwrappedAudioNodeOptions {
            count: 2,
            mode: ChannelCountMode::Max,
            interpretation: ChannelInterpretation::Speakers,
        }
    }
}
