/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::mpsc::{Receiver, Sender};

use malloc_size_of_derive::MallocSizeOf;
use servo_media_streams::{MediaSocket, MediaStreamId};

use crate::analyser_node::AnalyserNode;
use crate::script_processor_node::ScriptProcessorNode;
use crate::audio_worklet_node::AudioWorkletNode;
use crate::audio_node::{AudioNodeEngine, AudioNodeInit, AudioNodeMessage, BlockInfo, ChannelInfo};
use crate::biquad_filter_node::BiquadFilterNode;
use crate::block::{Chunk, FRAMES_PER_BLOCK, Tick};
use crate::buffer_source_node::AudioBufferSourceNode;
use crate::channel_node::{ChannelMergerNode, ChannelSplitterNode};
use crate::constant_source_node::ConstantSourceNode;
use crate::context::{AudioContextOptions, ProcessingState, StateChangeResult};
use crate::gain_node::GainNode;
use crate::graph::{AudioGraph, InputPort, NodeId, OutputPort, PortId};
use crate::convolver_node::ConvolverNode;
use crate::delay_node::DelayNode;
use crate::dynamics_compressor_node::DynamicsCompressorNode;
use crate::iir_filter_node::IIRFilterNode;
use crate::media_element_source_node::MediaElementSourceNode;
use crate::media_stream_destination_node::MediaStreamDestinationNode;
use crate::media_stream_source_node::MediaStreamSourceNode;
use crate::offline_sink::OfflineAudioSink;
use crate::oscillator_node::OscillatorNode;
use crate::panner_node::PannerNode;
use crate::render_stats::RenderStats;
use crate::sink::{AudioSink, AudioSinkError};
use crate::stereo_panner::StereoPannerNode;
use crate::wave_shaper_node::WaveShaperNode;
use crate::{AudioBackend, AudioStreamReader};

/// A closure the embedder runs on the render thread (AudioWorklet: script tasks, module loading).
pub type RenderThreadTask = Box<dyn FnOnce() + Send + 'static>;

thread_local! {
    static EXIT_HOOKS: std::cell::RefCell<Vec<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Register `hook` to run on the current render thread when its event loop ends (the context
/// closed), before the thread exits. Call it from a [`RenderThreadTask`]. The AudioWorklet uses it
/// to tear its JS runtime down in order rather than leaving it to thread-local destructors.
pub fn on_render_thread_exit(hook: Box<dyn FnOnce()>) {
    EXIT_HOOKS.with(|hooks| hooks.borrow_mut().push(hook));
}

thread_local! {
    static IDLE_HOOKS: std::cell::RefCell<Vec<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Register `hook` to run on the current render thread whenever it is about to wait with the
/// sink full (so the work is covered by the buffered audio) or while suspended. Call it from a
/// [`RenderThreadTask`]. Hooks must be cheap when they have nothing to do. The AudioWorklet runs
/// its garbage collections here rather than letting them happen inside `process()`.
pub fn on_render_thread_idle(hook: Box<dyn FnMut()>) {
    IDLE_HOOKS.with(|hooks| hooks.borrow_mut().push(hook));
}

fn run_idle_hooks() {
    // Take the hooks out while they run, so a hook may register another.
    let mut hooks = IDLE_HOOKS.with(|hooks| std::mem::take(&mut *hooks.borrow_mut()));
    if hooks.is_empty() {
        return;
    }
    for hook in hooks.iter_mut() {
        hook();
    }
    IDLE_HOOKS.with(|h| {
        let mut h = h.borrow_mut();
        hooks.append(&mut h);
        *h = hooks;
    });
}

thread_local! {
    static QUANTUM_END_HOOKS: std::cell::RefCell<Vec<Box<dyn FnMut(&BlockInfo)>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Register `hook` to run on the current render thread after every rendered quantum, with the
/// context's time now (the start of the next quantum). The AudioWorklet updates its global's
/// currentFrame/currentTime and does its microtask checkpoint there (between render quanta, not
/// between processors).
pub fn on_render_quantum_end(hook: Box<dyn FnMut(&BlockInfo)>) {
    QUANTUM_END_HOOKS.with(|hooks| hooks.borrow_mut().push(hook));
}

fn run_quantum_end_hooks(info: &BlockInfo) {
    let mut hooks = QUANTUM_END_HOOKS.with(|hooks| std::mem::take(&mut *hooks.borrow_mut()));
    if hooks.is_empty() {
        return;
    }
    for hook in hooks.iter_mut() {
        hook(info);
    }
    QUANTUM_END_HOOKS.with(|h| {
        let mut h = h.borrow_mut();
        hooks.append(&mut h);
        *h = hooks;
    });
}

fn run_exit_hooks() {
    loop {
        let hook = EXIT_HOOKS.with(|hooks| hooks.borrow_mut().pop());
        match hook {
            Some(hook) => hook(),
            None => break,
        }
    }
}

pub type SinkEosCallback = Box<dyn Fn(Box<dyn AsRef<[f32]>>) + Send + Sync + 'static>;

/// Called on the render thread when an OfflineAudioContext reaches a scheduled suspension.
pub struct SuspendCallback(pub Box<dyn FnOnce() + Send + 'static>);

#[derive(MallocSizeOf)]
pub enum AudioRenderThreadMsg {
    CreateNode(AudioNodeInit, Sender<Option<NodeId>>, ChannelInfo),
    ConnectPorts(PortId<OutputPort>, PortId<InputPort>),
    MessageNode(NodeId, AudioNodeMessage),
    Resume(Sender<StateChangeResult>),
    Suspend(Sender<StateChangeResult>),
    /// OfflineAudioContext.suspend(): suspend before rendering the quantum that starts at
    /// this frame (a multiple of the render quantum), then call back.
    SuspendAt(u64, #[ignore_malloc_size_of = "Fn"] SuspendCallback),
    /// The node's DOM object and AudioParams are gone (node lifetime): remove the node once it
    /// has no inputs and nothing more to produce.
    NodeUnreferenced(NodeId),
    Close(Sender<StateChangeResult>),
    SinkNeedData,
    GetCurrentTime(Sender<f64>),

    DisconnectAllFrom(NodeId),
    DisconnectOutput(PortId<OutputPort>),
    DisconnectBetween(NodeId, NodeId),
    DisconnectTo(NodeId, PortId<InputPort>),
    DisconnectOutputBetween(PortId<OutputPort>, NodeId),
    DisconnectOutputBetweenTo(PortId<OutputPort>, PortId<InputPort>),

    SetSinkEosCallback(#[ignore_malloc_size_of = "Fn"] SinkEosCallback),

    SetMute(bool),
    /// AudioContext sinkId {type: "none"}: keep rendering (time advances, analysers and
    /// script processors see the graph) but send silence to the device.
    SetOutputNone(bool),

    /// Run an embedder task on the render thread, between render quanta.
    RunTask(#[ignore_malloc_size_of = "Fn"] RenderThreadTask),
}

pub enum Sink {
    RealTime(Box<dyn AudioSink>),
    Offline(OfflineAudioSink),
}

impl AudioSink for Sink {
    fn init(
        &self,
        sample_rate: f32,
        sender: Sender<AudioRenderThreadMsg>,
    ) -> Result<(), AudioSinkError> {
        match *self {
            Sink::RealTime(ref sink) => sink.init(sample_rate, sender),
            Sink::Offline(ref sink) => {
                sink.init(sample_rate, sender).unwrap();
                Ok(())
            },
        }
    }

    fn init_stream(&self, _: u8, _: f32, _: Box<dyn MediaSocket>) -> Result<(), AudioSinkError> {
        unreachable!("Sink should never be used for MediaStreamDestinationNode")
    }

    fn play(&self) -> Result<(), AudioSinkError> {
        match *self {
            Sink::RealTime(ref sink) => sink.play(),
            Sink::Offline(ref sink) => {
                sink.play().unwrap();
                Ok(())
            },
        }
    }

    fn stop(&self) -> Result<(), AudioSinkError> {
        match *self {
            Sink::RealTime(ref sink) => sink.stop(),
            Sink::Offline(ref sink) => {
                sink.stop().unwrap();
                Ok(())
            },
        }
    }

    fn underrun_stats(&self) -> Option<(u64, u64)> {
        match *self {
            Sink::RealTime(ref sink) => sink.underrun_stats(),
            Sink::Offline(_) => None,
        }
    }

    fn has_enough_data(&self) -> bool {
        match *self {
            Sink::RealTime(ref sink) => sink.has_enough_data(),
            Sink::Offline(ref sink) => sink.has_enough_data(),
        }
    }

    fn push_data(&self, chunk: Chunk) -> Result<(), AudioSinkError> {
        match *self {
            Sink::RealTime(ref sink) => sink.push_data(chunk),
            Sink::Offline(ref sink) => {
                sink.push_data(chunk).unwrap();
                Ok(())
            },
        }
    }

    fn set_eos_callback(
        &self,
        callback: Box<dyn Fn(Box<dyn AsRef<[f32]>>) + Send + Sync + 'static>,
    ) {
        match *self {
            Sink::RealTime(ref sink) => sink.set_eos_callback(callback),
            Sink::Offline(ref sink) => sink.set_eos_callback(callback),
        }
    }
}

pub type ReaderFactoryCallback =
    dyn Fn(MediaStreamId, f32) -> Result<Box<dyn AudioStreamReader + Send>, AudioSinkError>;

pub struct AudioRenderThread {
    pub graph: AudioGraph,
    pub sink: Sink,
    pub sink_factory: Box<dyn Fn() -> Result<Box<dyn AudioSink + 'static>, AudioSinkError>>,
    pub reader_factory: Box<ReaderFactoryCallback>,
    pub state: ProcessingState,
    pub sample_rate: f32,
    pub current_time: f64,
    pub current_frame: Tick,
    pub muted: bool,
    /// sinkId {type: "none"}: render, but output silence.
    output_none: bool,
    /// Scheduled offline suspensions, by frame.
    suspend_points: std::collections::BTreeMap<u64, SuspendCallback>,
    /// PARTII_AUDIO_GRAPH_STATS: log the graph's node count about once a second when it
    /// changes (for leak tests).
    graph_stats: bool,
    graph_stats_last: usize,
    graph_stats_quanta: u32,
}

impl AudioRenderThread {
    /// Initializes the AudioRenderThread object
    ///
    /// You must call .event_loop() on this to run it!
    fn prepare_thread<B: AudioBackend>(
        sender: Sender<AudioRenderThreadMsg>,
        sample_rate: f32,
        graph: AudioGraph,
        options: AudioContextOptions,
    ) -> Result<Self, AudioSinkError> {
        let sink_factory = Box::new(|| B::make_sink().map(|s| Box::new(s) as Box<dyn AudioSink>));
        let reader_factory = Box::new(|id, sample_rate| B::make_streamreader(id, sample_rate));
        let sink = match options {
            AudioContextOptions::RealTimeAudioContext(_) => Sink::RealTime(sink_factory()?),
            AudioContextOptions::OfflineAudioContext(options) => Sink::Offline(
                OfflineAudioSink::new(options.channels as usize, options.length),
            ),
        };

        sink.init(sample_rate, sender)?;

        Ok(Self {
            graph,
            sink,
            sink_factory,
            reader_factory,
            state: ProcessingState::Suspended,
            sample_rate,
            current_time: 0.,
            current_frame: Tick(0),
            muted: false,
            output_none: false,
            suspend_points: Default::default(),
            graph_stats: std::env::var_os("PARTII_AUDIO_GRAPH_STATS").is_some(),
            graph_stats_last: 0,
            graph_stats_quanta: 0,
        })
    }

    /// Start the audio render thread
    ///
    /// In case something fails, it will instead start a thread with a dummy backend
    pub fn start<B: AudioBackend>(
        event_queue: Receiver<AudioRenderThreadMsg>,
        sender: Sender<AudioRenderThreadMsg>,
        sample_rate: f32,
        graph: AudioGraph,
        options: AudioContextOptions,
        init_sender: Sender<Result<(), AudioSinkError>>,
    ) {
        let mut thread = match Self::prepare_thread::<B>(sender, sample_rate, graph, options) {
            Ok(thread) => {
                let _ = init_sender.send(Ok(()));
                thread
            },
            Err(e) => {
                let _ = init_sender.send(Err(e));
                return;
            },
        };

        // If the loop panics, embedder state (the AudioWorklet's JS runtime) must still be torn
        // down in order here: left to thread-local destructors, a SpiderMonkey runtime is dropped
        // after mozjs's own thread-locals are gone, which aborts the process.
        let looped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            thread.event_loop(event_queue)
        }));
        if let Err(panic) = looped {
            run_exit_hooks();
            std::panic::resume_unwind(panic);
        }
        // Embedder state (the AudioWorklet's processors, global and JS runtime) goes before the
        // graph, which may still hold processor handles.
        run_exit_hooks();
    }

    make_render_thread_state_change!(resume, Running, play);

    make_render_thread_state_change!(suspend, Suspended, stop);

    fn create_node(&mut self, node_type: AudioNodeInit, ch: ChannelInfo) -> Option<NodeId> {
        let mut needs_listener = false;
        let mut is_dest = false;
        let node: Box<dyn AudioNodeEngine> = match node_type {
            AudioNodeInit::AnalyserNode(sender) => Box::new(AnalyserNode::new(sender, ch)),
            AudioNodeInit::ScriptProcessorNode(options) => {
                Box::new(ScriptProcessorNode::new(options, ch))
            },
            AudioNodeInit::AudioBufferSourceNode(options) => {
                Box::new(AudioBufferSourceNode::new(options, ch))
            },
            AudioNodeInit::BiquadFilterNode(options) => {
                Box::new(BiquadFilterNode::new(options, ch, self.sample_rate))
            },
            AudioNodeInit::GainNode(options) => Box::new(GainNode::new(options, ch)),
            AudioNodeInit::StereoPannerNode(options) => {
                Box::new(StereoPannerNode::new(options, ch))
            },
            AudioNodeInit::PannerNode(options) => {
                needs_listener = true;
                Box::new(PannerNode::new(options, ch))
            },
            AudioNodeInit::MediaStreamSourceNode(id) => {
                let reader = (self.reader_factory)(id, self.sample_rate);
                Box::new(MediaStreamSourceNode::new(reader.ok()?, ch))
            },
            AudioNodeInit::OscillatorNode(options) => Box::new(OscillatorNode::new(options, ch)),
            AudioNodeInit::ChannelMergerNode(options) => {
                Box::new(ChannelMergerNode::new(options, ch))
            },
            AudioNodeInit::ConstantSourceNode(options) => {
                Box::new(ConstantSourceNode::new(options, ch))
            },
            AudioNodeInit::MediaStreamDestinationNode(socket) => {
                is_dest = true;
                Box::new(MediaStreamDestinationNode::new(
                    socket,
                    self.sample_rate,
                    (self.sink_factory)().unwrap(),
                    ch,
                ))
            },
            AudioNodeInit::ChannelSplitterNode => Box::new(ChannelSplitterNode::new(ch)),
            AudioNodeInit::WaveShaperNode(options) => Box::new(WaveShaperNode::new(options, ch)),
            AudioNodeInit::MediaElementSourceNode => Box::new(MediaElementSourceNode::new(ch)),
            AudioNodeInit::IIRFilterNode(options) => Box::new(IIRFilterNode::new(options, ch)),
            AudioNodeInit::ConvolverNode(options) => Box::new(ConvolverNode::new(options, ch)),
            AudioNodeInit::DelayNode(options) => Box::new(DelayNode::new(options, ch)),
            AudioNodeInit::DynamicsCompressionNode(options) => {
                Box::new(DynamicsCompressorNode::new(options, ch))
            },
            AudioNodeInit::AudioWorkletNode(options) => {
                // The processor is constructed now, on this thread, as the spec requires (its
                // constructor may run before the context renders anything).
                let mut node = AudioWorkletNode::new(options, ch);
                node.construct(&BlockInfo {
                    sample_rate: self.sample_rate,
                    frame: self.current_frame,
                    time: self.current_time,
                });
                Box::new(node)
            },
            _ => unimplemented!(),
        };
        let id = self.graph.add_node(node);
        if needs_listener {
            let listener = self.graph.listener_id().output(0);
            self.graph.add_edge(listener, id.listener());
        }
        if is_dest {
            self.graph.add_extra_dest(id);
        }
        Some(id)
    }

    fn connect_ports(&mut self, output: PortId<OutputPort>, input: PortId<InputPort>) {
        self.graph.add_edge(output, input)
    }

    fn process(&mut self) -> Chunk {
        if self.muted {
            return Chunk::explicit_silence();
        }

        let info = BlockInfo {
            sample_rate: self.sample_rate,
            frame: self.current_frame,
            time: self.current_time,
        };
        let data = self.graph.process(&info);
        if self.output_none {
            return Chunk::explicit_silence();
        }
        data
    }

    fn log_graph_stats(&mut self) {
        self.graph_stats_quanta += 1;
        let quanta_per_second = (self.sample_rate / FRAMES_PER_BLOCK.0 as f32) as u32;
        if self.graph_stats_quanta >= quanta_per_second {
            self.graph_stats_quanta = 0;
            let nodes = self.graph.node_count();
            if nodes != self.graph_stats_last {
                self.graph_stats_last = nodes;
                eprintln!(
                    "partii-audio-graph: nodes={nodes} unreferenced={}",
                    self.graph.unreferenced_count()
                );
            }
        }
    }

    fn set_mute(&mut self, val: bool) {
        self.muted = val;
    }

    fn event_loop(&mut self, event_queue: Receiver<AudioRenderThreadMsg>) {
        let sample_rate = self.sample_rate;
        let mut stats = RenderStats::new(sample_rate, matches!(self.sink, Sink::RealTime(_)));
        let handle_msg = move |context: &mut Self, msg: AudioRenderThreadMsg| -> bool {
            let mut break_loop = false;
            match msg {
                AudioRenderThreadMsg::CreateNode(node_type, tx, ch) => {
                    let _ = tx.send(context.create_node(node_type, ch));
                },
                AudioRenderThreadMsg::ConnectPorts(output, input) => {
                    context.connect_ports(output, input);
                },
                AudioRenderThreadMsg::Resume(tx) => {
                    let _ = tx.send(context.resume());
                },
                AudioRenderThreadMsg::Suspend(tx) => {
                    let _ = tx.send(context.suspend());
                },
                AudioRenderThreadMsg::Close(tx) => {
                    let _ = tx.send(context.suspend());
                    break_loop = true;
                },
                AudioRenderThreadMsg::GetCurrentTime(response) => {
                    response.send(context.current_time).unwrap()
                },
                AudioRenderThreadMsg::MessageNode(id, msg) => {
                    // A removed node can't get messages from its (gone) DOM object; be safe.
                    if context.graph.has_node(id) {
                        let msg = msg.clamped_to(context.current_time);
                        context.graph.node_mut(id).message(msg, sample_rate)
                    }
                },
                AudioRenderThreadMsg::NodeUnreferenced(id) => {
                    context.graph.node_unreferenced(id);
                },
                AudioRenderThreadMsg::SinkNeedData => {
                    // Do nothing. This will simply unblock the thread so we
                    // can restart the non-blocking event loop.
                },
                AudioRenderThreadMsg::DisconnectAllFrom(id) => {
                    context.graph.disconnect_all_from(id)
                },
                AudioRenderThreadMsg::DisconnectOutput(out) => context.graph.disconnect_output(out),
                AudioRenderThreadMsg::DisconnectBetween(from, to) => {
                    context.graph.disconnect_between(from, to)
                },
                AudioRenderThreadMsg::DisconnectTo(from, to) => {
                    context.graph.disconnect_to(from, to)
                },
                AudioRenderThreadMsg::DisconnectOutputBetween(from, to) => {
                    context.graph.disconnect_output_between(from, to)
                },
                AudioRenderThreadMsg::DisconnectOutputBetweenTo(from, to) => {
                    context.graph.disconnect_output_between_to(from, to)
                },
                AudioRenderThreadMsg::SetSinkEosCallback(callback) => {
                    context.sink.set_eos_callback(callback);
                },
                AudioRenderThreadMsg::SetMute(val) => {
                    context.set_mute(val);
                },
                AudioRenderThreadMsg::SetOutputNone(val) => {
                    context.output_none = val;
                },
                AudioRenderThreadMsg::RunTask(task) => task(),
                AudioRenderThreadMsg::SuspendAt(frame, callback) => {
                    context.suspend_points.insert(frame, callback);
                },
            };

            break_loop
        };

        loop {
            if self.sink.has_enough_data() || self.state == ProcessingState::Suspended {
                // If we are not processing audio or
                // if we have already pushed enough data into the audio sink
                // we wait for messages coming from the control thread or
                // the audio sink. The audio sink will notify whenever it
                // needs more data.
                // Embedder work that would glitch inside a quantum goes here, with the
                // buffer full, unless a message is already waiting.
                run_idle_hooks();
                if event_queue.recv().is_ok_and(|msg| handle_msg(self, msg)) {
                    break;
                }
            } else {
                // If we have not pushed enough data into the audio sink yet,
                // we process the control message queue
                if event_queue
                    .try_recv()
                    .is_ok_and(|msg| handle_msg(self, msg))
                {
                    break;
                }

                if self.state == ProcessingState::Suspended {
                    // Bail out if we just suspended processing.
                    continue;
                }

                // OfflineAudioContext.suspend(): stop before this quantum, and tell the
                // control thread, which resolves the promise and may change the graph
                // before it resumes us.
                if let Some(callback) = self.suspend_points.remove(&self.current_frame.0) {
                    let _ = self.suspend();
                    (callback.0)();
                    continue;
                }

                // push into the audio sink the result of processing a
                // render quantum.
                let started = stats.enabled().then(std::time::Instant::now);
                let data = self.process();
                if let Some(started) = started {
                    stats.record(started.elapsed(), self.sink.underrun_stats());
                }
                if self.sink.push_data(data).is_ok() {
                    // increment current frame by the render quantum size.
                    self.current_frame += FRAMES_PER_BLOCK;
                    self.current_time = self.current_frame / self.sample_rate as f64;
                    run_quantum_end_hooks(&BlockInfo {
                        sample_rate: self.sample_rate,
                        frame: self.current_frame,
                        time: self.current_time,
                    });
                    self.graph.collect_unreferenced(self.current_frame);
                    if self.graph_stats {
                        self.log_graph_stats();
                    }
                } else {
                    eprintln!("Could not push data to audio sink");
                }
            }
        }
        stats.finish();
    }
}
