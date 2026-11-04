/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::sync::{Arc, mpsc};

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::cformat;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use servo_media::audio::context::NodeUnreferencedOnDrop;
use servo_media::audio::audio_node::{AudioNodeMessage, AudioNodeType};
use servo_media::audio::graph::NodeId;
use servo_media::audio::param::{ParamRate, ParamType, RampKind, UserAutomationEvent};

use crate::conversions::Convert;
use crate::dom::audio::baseaudiocontext::BaseAudioContext;
use crate::dom::bindings::codegen::Bindings::AudioParamBinding::{
    AudioParamMethods, AutomationRate,
};
use crate::dom::bindings::codegen::Bindings::BaseAudioContextBinding::BaseAudioContextMethods;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::window::Window;

#[dom_struct]
pub(crate) struct AudioParam {
    reflector_: Reflector,
    context: Dom<BaseAudioContext>,
    #[no_trace]
    node: Option<NodeId>,
    #[no_trace]
    node_type: AudioNodeType,
    #[no_trace]
    param: ParamType,
    automation_rate: Cell<AutomationRate>,
    default_value: f32,
    min_value: f32,
    max_value: f32,
    /// Keeps the owner node's engine node alive while the page holds this AudioParam.
    #[no_trace]
    #[ignore_malloc_size_of = "servo_media"]
    _node_lifetime: Option<Arc<NodeUnreferencedOnDrop>>,
    /// The scheduled automation events, (time, Some(duration) for a SetValueCurve), so that an
    /// event overlapping a curve throws NotSupportedError (see `check_overlap`). Events that
    /// ended before currentTime are pruned as the list grows.
    events: DomRefCell<Vec<(f64, Option<f64>)>>,
}

impl AudioParam {
    #[expect(clippy::too_many_arguments)]
    pub(crate) fn new_inherited(
        context: &BaseAudioContext,
        node: Option<NodeId>,
        node_type: AudioNodeType,
        param: ParamType,
        automation_rate: AutomationRate,
        default_value: f32,
        min_value: f32,
        max_value: f32,
    ) -> AudioParam {
        AudioParam {
            reflector_: Reflector::new(),
            context: Dom::from_ref(context),
            node,
            node_type,
            param,
            automation_rate: Cell::new(automation_rate),
            default_value,
            min_value,
            max_value,
            _node_lifetime: node.and_then(|id| context.node_lifetime(id)),
            events: Default::default(),
        }
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioparam-setvaluecurveattime>: a curve
    /// [T, T + D) may not contain another event's time (strictly after T) nor overlap another
    /// curve; any other event may not fall in a curve's [T, T + D). NotSupportedError if it
    /// would; else the event is recorded. `duration` is Some for a curve.
    fn check_overlap(&self, time: f64, duration: Option<f64>) -> ErrorResult {
        let mut events = self.events.borrow_mut();
        let overlaps = events.iter().any(|&(t, d)| match (duration, d) {
            (Some(duration), Some(d)) => time < t + d && t < time + duration,
            (Some(duration), None) => t > time && t < time + duration,
            (None, Some(d)) => time >= t && time < t + d,
            (None, None) => false,
        });
        if overlaps {
            return Err(Error::NotSupported(None));
        }
        if events.len() >= 64 && events.len().is_power_of_two() {
            let now = *self.context.CurrentTime();
            events.retain(|&(t, d)| t + d.unwrap_or(0.) >= now);
        }
        events.push((time, duration));
        Ok(())
    }

    #[expect(clippy::too_many_arguments)]
    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        context: &BaseAudioContext,
        node: Option<NodeId>,
        node_type: AudioNodeType,
        param: ParamType,
        automation_rate: AutomationRate,
        default_value: f32,
        min_value: f32,
        max_value: f32,
    ) -> DomRoot<AudioParam> {
        let audio_param = AudioParam::new_inherited(
            context,
            node,
            node_type,
            param,
            automation_rate,
            default_value,
            min_value,
            max_value,
        );
        // Update the value range
        audio_param.message_node(AudioNodeMessage::SetParamRange(
            audio_param.param,
            (min_value, max_value),
        ));
        reflect_dom_object(cx, Box::new(audio_param), window)
    }

    fn message_node(&self, message: AudioNodeMessage) {
        if let Some(node_id) = self.node {
            self.context
                .audio_context_impl()
                .lock()
                .unwrap()
                .message_node(node_id, message);
        }
    }

    pub(crate) fn context(&self) -> &BaseAudioContext {
        &self.context
    }

    pub(crate) fn node_id(&self) -> Option<NodeId> {
        self.node
    }

    pub(crate) fn param_type(&self) -> ParamType {
        self.param
    }
}

impl AudioParamMethods<crate::DomTypeHolder> for AudioParam {
    /// <https://webaudio.github.io/web-audio-api/#dom-audioparam-automationrate>
    fn AutomationRate(&self) -> AutomationRate {
        self.automation_rate.get()
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioparam-automationrate>
    fn SetAutomationRate(&self, automation_rate: AutomationRate) -> Fallible<()> {
        // > AudioBufferSourceNode
        // > The AudioParams playbackRate and detune MUST be "k-rate". An InvalidStateError must be
        // > thrown if the rate is changed to "a-rate".
        if automation_rate == AutomationRate::A_rate &&
            self.node_type == AudioNodeType::AudioBufferSourceNode &&
            (self.param == ParamType::Detune || self.param == ParamType::PlaybackRate)
        {
            return Err(Error::InvalidState(None));
        }
        // > DynamicsCompressorNode: [all AudioParams have] automation rate constraints
        // <https://webaudio.github.io/web-audio-api/#audioparam-automation-rate-constraints>
        if automation_rate == AutomationRate::A_rate &&
            self.node_type == AudioNodeType::DynamicsCompressionNode
        {
            return Err(Error::InvalidState(None));
        }

        self.automation_rate.set(automation_rate);
        self.message_node(AudioNodeMessage::SetParamRate(
            self.param,
            automation_rate.convert(),
        ));

        Ok(())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioparam-value>
    fn Value(&self) -> Finite<f32> {
        if self.node.is_none() {
            return Finite::wrap(self.default_value);
        }
        let (tx, rx) = mpsc::channel();
        self.message_node(AudioNodeMessage::GetParamValue(self.param, tx));
        Finite::wrap(rx.recv().unwrap())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioparam-value>
    fn SetValue(&self, value: Finite<f32>) -> ErrorResult {
        // > Setting this attribute has the effect of [...] calling setValueAtTime() with the
        // > current AudioContext's currentTime: it throws during a SetValueCurve.
        let now = *self.context.CurrentTime();
        if self
            .events
            .borrow()
            .iter()
            .any(|&(t, d)| d.is_some_and(|d| now >= t && now < t + d))
        {
            return Err(Error::NotSupported(None));
        }
        self.message_node(AudioNodeMessage::SetParam(
            self.param,
            UserAutomationEvent::SetValue(*value),
        ));
        Ok(())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioparam-defaultvalue>
    fn DefaultValue(&self) -> Finite<f32> {
        Finite::wrap(self.default_value)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioparam-minvalue>
    fn MinValue(&self) -> Finite<f32> {
        Finite::wrap(self.min_value)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioparam-maxvalue>
    fn MaxValue(&self) -> Finite<f32> {
        Finite::wrap(self.max_value)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioparam-setvalueattime>
    fn SetValueAtTime(
        &self,
        value: Finite<f32>,
        start_time: Finite<f64>,
    ) -> Fallible<DomRoot<AudioParam>> {
        if *start_time < 0. {
            return Err(Error::Range(cformat!(
                "start time {} should not be negative",
                *start_time
            )));
        }
        self.check_overlap(*start_time, None)?;
        self.message_node(AudioNodeMessage::SetParam(
            self.param,
            UserAutomationEvent::SetValueAtTime(*value, *start_time),
        ));
        Ok(DomRoot::from_ref(self))
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioparam-linearramptovalueattime>
    fn LinearRampToValueAtTime(
        &self,
        value: Finite<f32>,
        end_time: Finite<f64>,
    ) -> Fallible<DomRoot<AudioParam>> {
        if *end_time < 0. {
            return Err(Error::Range(cformat!(
                "end time {} should not be negative",
                *end_time
            )));
        }
        self.check_overlap(*end_time, None)?;
        self.message_node(AudioNodeMessage::SetParam(
            self.param,
            UserAutomationEvent::RampToValueAtTime(RampKind::Linear, *value, *end_time),
        ));
        Ok(DomRoot::from_ref(self))
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioparam-exponentialramptovalueattime>
    fn ExponentialRampToValueAtTime(
        &self,
        value: Finite<f32>,
        end_time: Finite<f64>,
    ) -> Fallible<DomRoot<AudioParam>> {
        if *end_time < 0. {
            return Err(Error::Range(cformat!(
                "end time {} should not be negative",
                *end_time
            )));
        }
        if *value == 0. {
            return Err(Error::Range(cformat!(
                "target value {} should not be 0",
                *value
            )));
        }
        self.check_overlap(*end_time, None)?;
        self.message_node(AudioNodeMessage::SetParam(
            self.param,
            UserAutomationEvent::RampToValueAtTime(RampKind::Exponential, *value, *end_time),
        ));
        Ok(DomRoot::from_ref(self))
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioparam-settargetattime>
    fn SetTargetAtTime(
        &self,
        target: Finite<f32>,
        start_time: Finite<f64>,
        time_constant: Finite<f32>,
    ) -> Fallible<DomRoot<AudioParam>> {
        if *start_time < 0. {
            return Err(Error::Range(cformat!(
                "start time {} should not be negative",
                *start_time
            )));
        }
        if *time_constant < 0. {
            return Err(Error::Range(cformat!(
                "time constant {} should not be negative",
                *time_constant
            )));
        }
        self.check_overlap(*start_time, None)?;
        self.message_node(AudioNodeMessage::SetParam(
            self.param,
            UserAutomationEvent::SetTargetAtTime(*target, *start_time, (*time_constant).into()),
        ));
        Ok(DomRoot::from_ref(self))
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioparam-setvaluecurveattime>
    fn SetValueCurveAtTime(
        &self,
        values: Vec<Finite<f32>>,
        start_time: Finite<f64>,
        end_time: Finite<f64>,
    ) -> Fallible<DomRoot<AudioParam>> {
        if *start_time < 0. {
            return Err(Error::Range(cformat!(
                "start time {} should not be negative",
                *start_time
            )));
        }
        if values.len() < 2. as usize {
            return Err(Error::InvalidState(None));
        }

        if *end_time <= 0. {
            return Err(Error::Range(cformat!(
                "duration {} should be positive",
                *end_time
            )));
        }
        self.check_overlap(*start_time, Some(*end_time))?;
        self.message_node(AudioNodeMessage::SetParam(
            self.param,
            UserAutomationEvent::SetValueCurveAtTime(
                values.into_iter().map(|v| *v).collect(),
                *start_time,
                *end_time,
            ),
        ));
        Ok(DomRoot::from_ref(self))
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioparam-cancelscheduledvalues>
    fn CancelScheduledValues(&self, cancel_time: Finite<f64>) -> Fallible<DomRoot<AudioParam>> {
        if *cancel_time < 0. {
            return Err(Error::Range(cformat!(
                "cancel time {} should not be negative",
                *cancel_time
            )));
        }
        // > Cancels all scheduled parameter changes with times greater than or equal to
        // > cancelTime. [...] Any active automations whose automation event time is less than
        // > cancelTime are also cancelled: so is a curve still running at cancelTime.
        let cancel_time = *cancel_time;
        self.events
            .borrow_mut()
            .retain(|&(t, d)| t < cancel_time && d.is_none_or(|d| t + d <= cancel_time));
        self.message_node(AudioNodeMessage::SetParam(
            self.param,
            UserAutomationEvent::CancelScheduledValues(cancel_time),
        ));
        Ok(DomRoot::from_ref(self))
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioparam-cancelandholdattime>
    fn CancelAndHoldAtTime(&self, cancel_time: Finite<f64>) -> Fallible<DomRoot<AudioParam>> {
        if *cancel_time < 0. {
            return Err(Error::Range(cformat!(
                "cancel time {} should not be negative",
                *cancel_time
            )));
        }
        // Events after cancelTime are removed and a curve running at cancelTime ends there.
        let cancel_time = *cancel_time;
        let mut events = self.events.borrow_mut();
        events.retain(|&(t, _)| t <= cancel_time);
        for (t, d) in events.iter_mut() {
            if let Some(duration) = d &&
                *t + *duration > cancel_time
            {
                *duration = cancel_time - *t;
            }
        }
        drop(events);
        self.message_node(AudioNodeMessage::SetParam(
            self.param,
            UserAutomationEvent::CancelAndHoldAtTime(cancel_time),
        ));
        Ok(DomRoot::from_ref(self))
    }
}

// https://webaudio.github.io/web-audio-api/#enumdef-automationrate
impl Convert<ParamRate> for AutomationRate {
    fn convert(self) -> ParamRate {
        match self {
            AutomationRate::A_rate => ParamRate::ARate,
            AutomationRate::K_rate => ParamRate::KRate,
        }
    }
}
