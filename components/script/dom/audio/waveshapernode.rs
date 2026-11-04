/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::ptr;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::jsapi::JSObject;
use js::rust::{CustomAutoRooterGuard, HandleObject};
use js::typedarray::{Float32, Float32Array, HeapFloat32Array};
use script_bindings::buffer_source::create_buffer_source;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_media::audio::audio_node::{AudioNodeInit, AudioNodeMessage};
use servo_media::audio::wave_shaper_node::{
    OverSampleType as ServoMediaOverSampleType, WaveShaperNodeMessage, WaveShaperNodeOptions,
};

use crate::conversions::Convert;
use crate::dom::audio::audionode::{AudioNode, AudioNodeOptionsHelper};
use crate::dom::audio::baseaudiocontext::BaseAudioContext;
use script_bindings::cell::DomRefCell;
use crate::dom::bindings::codegen::Bindings::AudioNodeBinding::{
    ChannelCountMode, ChannelInterpretation,
};
use crate::dom::bindings::codegen::Bindings::WaveShaperNodeBinding::{
    OverSampleType, WaveShaperNodeMethods, WaveShaperOptions,
};
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::trace::RootedTraceableBox;
use crate::dom::window::Window;

/// <https://webaudio.github.io/web-audio-api/#WaveShaperNode>
#[dom_struct]
pub(crate) struct WaveShaperNode {
    node: AudioNode,
    /// The internal copy of the curve; `None` is the null curve.
    curve: DomRefCell<Option<Vec<f32>>>,
    /// <https://webaudio.github.io/web-audio-api/#dom-waveshapernode-curve-set-slot>
    curve_set: Cell<bool>,
    oversample: Cell<OverSampleType>,
}

impl WaveShaperNode {
    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    pub(crate) fn new_inherited(
        cx: &mut JSContext,
        context: &BaseAudioContext,
        options: &WaveShaperOptions,
    ) -> Fallible<WaveShaperNode> {
        let node_options =
            options
                .parent
                .unwrap_or(2, ChannelCountMode::Max, ChannelInterpretation::Speakers);
        // Setting the curve from the options follows the attribute's setter: a curve shorter
        // than 2 is an InvalidStateError.
        let curve: Option<Vec<f32>> = options
            .curve
            .as_ref()
            .map(|c| c.iter().map(|v| **v).collect());
        if curve.as_ref().is_some_and(|c| c.len() < 2) {
            return Err(Error::InvalidState(None));
        }
        let oversample = options.oversample;
        let node = AudioNode::new_inherited(
            cx,
            AudioNodeInit::WaveShaperNode(WaveShaperNodeOptions {
                curve: curve.clone(),
                oversample: oversample.convert(),
            }),
            context,
            node_options,
            1, // inputs
            1, // outputs
        )?;
        Ok(WaveShaperNode {
            node,
            // > If options is given and specifies a curve, set [[curve set]] to true.
            curve_set: Cell::new(curve.is_some()),
            curve: DomRefCell::new(curve),
            oversample: Cell::new(oversample),
        })
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        context: &BaseAudioContext,
        options: &WaveShaperOptions,
    ) -> Fallible<DomRoot<WaveShaperNode>> {
        Self::new_with_proto(cx, window, None, context, options)
    }

    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    fn new_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        context: &BaseAudioContext,
        options: &WaveShaperOptions,
    ) -> Fallible<DomRoot<WaveShaperNode>> {
        let node = WaveShaperNode::new_inherited(cx, context, options)?;
        Ok(reflect_dom_object_with_proto(
            cx,
            Box::new(node),
            window,
            proto,
        ))
    }
}

impl WaveShaperNodeMethods<crate::DomTypeHolder> for WaveShaperNode {
    /// <https://webaudio.github.io/web-audio-api/#dom-waveshapernode-waveshapernode>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        context: &BaseAudioContext,
        options: &WaveShaperOptions,
    ) -> Fallible<DomRoot<WaveShaperNode>> {
        WaveShaperNode::new_with_proto(cx, window, proto, context, options)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-waveshapernode-curve>
    fn GetCurve(&self, cx: &mut JSContext) -> Option<RootedTraceableBox<HeapFloat32Array>> {
        let curve = self.curve.borrow();
        let data = curve.as_ref()?;
        rooted!(&in(cx) let mut array = ptr::null_mut::<JSObject>());
        create_buffer_source::<Float32>(cx, data, array.handle_mut()).ok()
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-waveshapernode-curve>
    fn SetCurve(&self, new_curve: CustomAutoRooterGuard<Option<Float32Array>>) -> ErrorResult {
        // > 1. Let new curve be a Float32Array to be assigned to curve or null.
        let new_curve: Option<Vec<f32>> = match &*new_curve {
            // A detached array has length 0, which the length check below rejects.
            Some(curve) => Some(curve.to_vec().unwrap_or_default()),
            None => None,
        };
        if let Some(c) = &new_curve {
            // > A InvalidStateError MUST be thrown if this attribute is set with a
            // > Float32Array that has a length less than 2.
            if c.len() < 2 {
                return Err(Error::InvalidState(None));
            }
            // > 2. If new curve is not null and [[curve set]] is true, throw an
            // > InvalidStateError and abort these steps.
            if self.curve_set.get() {
                return Err(Error::InvalidState(None));
            }
            // > 3. If new curve is not null, set [[curve set]] to true.
            self.curve_set.set(true);
        }
        // > 4. Assign new curve to the curve attribute. (An internal copy: later changes to
        // > the array have no effect.)
        *self.curve.borrow_mut() = new_curve.clone();
        self.node
            .message(AudioNodeMessage::WaveShaperNode(WaveShaperNodeMessage::SetCurve(
                new_curve,
            )));
        Ok(())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-waveshapernode-oversample>
    fn Oversample(&self) -> OverSampleType {
        self.oversample.get()
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-waveshapernode-oversample>
    fn SetOversample(&self, value: OverSampleType) {
        self.oversample.set(value);
        self.node
            .message(AudioNodeMessage::WaveShaperNode(
                WaveShaperNodeMessage::SetOversample(value.convert()),
            ));
    }
}

impl Convert<ServoMediaOverSampleType> for OverSampleType {
    fn convert(self) -> ServoMediaOverSampleType {
        match self {
            OverSampleType::None => ServoMediaOverSampleType::None,
            OverSampleType::_2x => ServoMediaOverSampleType::Double,
            OverSampleType::_4x => ServoMediaOverSampleType::Quadruple,
        }
    }
}
