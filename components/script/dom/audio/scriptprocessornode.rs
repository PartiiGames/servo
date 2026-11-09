/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! <https://webaudio.github.io/web-audio-api/#ScriptProcessorNode> (deprecated, but how
//! Emscripten's SDL2 and OpenAL and older engines play sound). The render side
//! (servo-media's script_processor_node.rs) hands each full input buffer here as a task; this
//! fires `audioprocess` and sends the page's output buffer back.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_media::audio::audio_node::AudioNodeInit;
use stylo_atoms::Atom;
use servo_media::audio::script_processor_node::{
    ScriptProcessorHandle, ScriptProcessorNodeOptions,
};

use crate::dom::audio::audiobuffer::AudioBuffer;
use crate::dom::audio::audionode::{AudioNode, MAX_CHANNEL_COUNT, UnwrappedAudioNodeOptions};
use crate::dom::audio::audioprocessingevent::AudioProcessingEvent;
use crate::dom::audio::baseaudiocontext::BaseAudioContext;
use crate::dom::bindings::codegen::Bindings::AudioNodeBinding::{
    AudioNodeMethods, ChannelCountMode, ChannelInterpretation,
};
use crate::dom::bindings::codegen::Bindings::BaseAudioContextBinding::BaseAudioContext_Binding::BaseAudioContextMethods;
use crate::dom::bindings::codegen::Bindings::ScriptProcessorNodeBinding::ScriptProcessorNodeMethods;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::window::Window;
use crate::tasks::task_source::SendableTaskSource;

/// Output buffers the page filled, waiting for the render thread.
type Outputs = Arc<Mutex<VecDeque<Vec<Vec<f32>>>>>;

/// The buffer size createScriptProcessor(0) picks: Blink's choice for a real-time context
/// (1024 frames, ~21 ms at 48 kHz).
const DEFAULT_BUFFER_SIZE: u32 = 1024;
/// Buffers queued for the render thread beyond this are dropped (a page that can't keep up
/// shouldn't build up latency).
const MAX_QUEUED_OUTPUTS: usize = 4;

#[dom_struct]
pub(crate) struct ScriptProcessorNode {
    node: AudioNode,
    buffer_size: u32,
    input_channels: u32,
    output_channels: u32,
    #[ignore_malloc_size_of = "Arc"]
    #[no_trace]
    outputs: Outputs,
}

/// The render thread's handle on the node.
struct Handle {
    node: Arc<Mutex<Option<Trusted<ScriptProcessorNode>>>>,
    task_source: SendableTaskSource,
    outputs: Outputs,
}

impl ScriptProcessorHandle for Handle {
    fn audio_process(&mut self, input: Vec<Vec<f32>>, playback_time: f64) {
        let Some(node) = self.node.lock().unwrap().clone() else {
            return;
        };
        self.task_source
            .queue(task!(script_processor_audio_process: move |cx| {
                node.root().audio_process(cx, input, playback_time);
            }));
    }

    fn take_output(&mut self) -> Option<Vec<Vec<f32>>> {
        self.outputs.lock().unwrap().pop_front()
    }
}

impl ScriptProcessorNode {
    /// <https://webaudio.github.io/web-audio-api/#dom-baseaudiocontext-createscriptprocessor>
    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        context: &BaseAudioContext,
        buffer_size: u32,
        input_channels: u32,
        output_channels: u32,
    ) -> Fallible<DomRoot<ScriptProcessorNode>> {
        let buffer_size = match buffer_size {
            0 => DEFAULT_BUFFER_SIZE,
            256 | 512 | 1024 | 2048 | 4096 | 8192 | 16384 => buffer_size,
            _ => return Err(Error::IndexSize(None)),
        };
        if (input_channels == 0 && output_channels == 0) ||
            input_channels > MAX_CHANNEL_COUNT ||
            output_channels > MAX_CHANNEL_COUNT
        {
            return Err(Error::IndexSize(None));
        }

        let outputs: Outputs = Default::default();
        let trusted = Arc::new(Mutex::new(None));
        let task_source = window
            .as_global_scope()
            .task_manager()
            .dom_manipulation_task_source()
            .to_sendable();
        let handle = Handle {
            node: trusted.clone(),
            task_source,
            outputs: outputs.clone(),
        };
        let init = AudioNodeInit::ScriptProcessorNode(ScriptProcessorNodeOptions {
            buffer_size,
            input_channels: input_channels as u8,
            output_channels: output_channels as u8,
            handle: Box::new(handle),
        });
        // channelCount is numberOfInputChannels and channelCountMode "explicit", both fixed.
        let options = UnwrappedAudioNodeOptions {
            count: input_channels.max(1),
            mode: ChannelCountMode::Explicit,
            interpretation: ChannelInterpretation::Speakers,
        };
        let node = AudioNode::new_inherited(cx, init, context, options, 1, 1)?;
        let node = ScriptProcessorNode {
            node,
            buffer_size,
            input_channels,
            output_channels,
            outputs,
        };
        let object = reflect_dom_object_with_proto(cx, Box::new(node), window, None);
        *trusted.lock().unwrap() = Some(Trusted::new(&*object));
        Ok(object)
    }

    pub(crate) fn input_channels(&self) -> u32 {
        self.input_channels
    }

    /// One input buffer from the render thread: fire `audioprocess`, then send the output.
    fn audio_process(&self, cx: &mut JSContext, input: Vec<Vec<f32>>, playback_time: f64) {
        let context = self.node.Context();
        let global = self.global();
        let window = global.as_window();
        let rate = *context.SampleRate();
        let length = self.buffer_size;
        // A node with no input (or no output) channels still gets a one-channel buffer of
        // silence, as in Blink.
        let input_buffer = if input.is_empty() {
            AudioBuffer::new(cx, window, 1, length, rate, None)
        } else {
            AudioBuffer::new(cx, window, input.len() as u32, length, rate, Some(&input))
        };
        let output_buffer =
            AudioBuffer::new(cx, window, self.output_channels.max(1), length, rate, None);
        let event = AudioProcessingEvent::new(
            cx,
            window,
            Atom::from("audioprocess"),
            EventBubbles::DoesNotBubble,
            EventCancelable::NotCancelable,
            playback_time,
            &input_buffer,
            &output_buffer,
        );
        event.upcast::<Event>().fire(cx, self.upcast());

        let Some(output) = output_buffer.get_channels(cx) else {
            return;
        };
        let output: Vec<Vec<f32>> = output
            .buffers
            .iter()
            .take(self.output_channels as usize)
            .cloned()
            .collect();
        let mut queue = self.outputs.lock().unwrap();
        queue.push_back(output);
        while queue.len() > MAX_QUEUED_OUTPUTS {
            queue.pop_front();
        }
    }
}

impl ScriptProcessorNodeMethods<crate::DomTypeHolder> for ScriptProcessorNode {
    // <https://webaudio.github.io/web-audio-api/#dom-scriptprocessornode-onaudioprocess>
    event_handler!(audioprocess, GetOnaudioprocess, SetOnaudioprocess);

    /// <https://webaudio.github.io/web-audio-api/#dom-scriptprocessornode-buffersize>
    fn BufferSize(&self) -> i32 {
        self.buffer_size as i32
    }
}
