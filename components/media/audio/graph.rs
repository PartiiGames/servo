/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{RefCell, RefMut};
use std::{cmp, fmt, hash};

use malloc_size_of::MallocSizeOf as MallocSizeOfTrait;
use malloc_size_of_derive::MallocSizeOf;
use petgraph::Direction;
use petgraph::algo::tarjan_scc;
use petgraph::graph::DefaultIx;
use petgraph::stable_graph::{NodeIndex, StableGraph};
use petgraph::visit::{EdgeFiltered, EdgeRef};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::audio_node::{
    AudioNodeEngine, AudioNodeType, BlockInfo, ChannelCountMode, ChannelInterpretation,
};
use crate::block::{Block, Chunk, Tick};
use crate::destination_node::DestinationNode;
use crate::listener::AudioListenerNode;
use crate::param::ParamType;

#[derive(Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash, Debug, MallocSizeOf)]
/// A unique identifier for nodes in the graph. Stable
/// under graph mutation.
pub struct NodeId(#[ignore_malloc_size_of = "External Type"] NodeIndex<DefaultIx>);

impl NodeId {
    pub fn input(self, port: u32) -> PortId<InputPort> {
        PortId(self, PortIndex::Port(port))
    }
    pub fn param(self, param: ParamType) -> PortId<InputPort> {
        PortId(self, PortIndex::Param(param))
    }
    pub fn output(self, port: u32) -> PortId<OutputPort> {
        PortId(self, PortIndex::Port(port))
    }
    pub(crate) fn listener(self) -> PortId<InputPort> {
        PortId(self, PortIndex::Listener(()))
    }
}

/// A zero-indexed "port" for a node. Most nodes have one
/// input and one output port, but some may have more.
/// For example, a channel splitter node will have one output
/// port for each channel.
///
/// These are essentially indices into the Chunks
///
/// Kind is a zero sized type and is useful for distinguishing
/// between input and output ports (which may otherwise share indices)
#[derive(Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash, Debug, MallocSizeOf)]
pub enum PortIndex<Kind: PortKind> {
    Port(u32),
    Param(Kind::ParamId),
    /// special variant only used for the implicit connection
    /// from listeners to params
    Listener(Kind::Listener),
}

impl<Kind: PortKind> PortId<Kind> {
    pub fn node(&self) -> NodeId {
        self.0
    }
}

pub trait PortKind {
    type ParamId: Copy
        + Eq
        + PartialEq
        + Ord
        + PartialOrd
        + hash::Hash
        + fmt::Debug
        + MallocSizeOfTrait;
    type Listener: Copy
        + Eq
        + PartialEq
        + Ord
        + PartialOrd
        + hash::Hash
        + fmt::Debug
        + MallocSizeOfTrait;
}

/// An identifier for a port.
#[derive(Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash, Debug, MallocSizeOf)]
pub struct PortId<Kind: PortKind>(NodeId, PortIndex<Kind>);

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug, MallocSizeOf)]
/// Marker type for denoting that the port is an input port
/// of the node it is connected to
pub struct InputPort;
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug, MallocSizeOf)]
/// Marker type for denoting that the port is an output port
/// of the node it is connected to
pub struct OutputPort;

impl PortKind for InputPort {
    type ParamId = ParamType;
    type Listener = ();
}

#[derive(Debug, Hash, PartialOrd, Ord, PartialEq, Eq, Copy, Clone, MallocSizeOf)]
pub enum Void {}

impl PortKind for OutputPort {
    // Params are only a feature of input ports. By using an empty type here
    // we ensure that the PortIndex enum has zero overhead for outputs,
    // taking up no extra discriminant space and eliminating PortIndex::Param
    // branches entirely from the compiled code
    type ParamId = Void;
    type Listener = Void;
}

pub struct AudioGraph {
    graph: StableGraph<Node, Edge>,
    dest_id: NodeId,
    dests: Vec<NodeId>,
    listener_id: NodeId,
    /// Nodes whose DOM objects are gone, waiting to be removable (see `collect_unreferenced`).
    unreferenced: Vec<NodeIndex<DefaultIx>>,
    /// Bumped on every change of nodes or edges; `plan` is valid for one value.
    topology: u64,
    /// The processing order and cycle analysis, recomputed only when the topology changes.
    plan: Option<Plan>,
}

/// What `process` needs that depends only on the graph's topology.
struct Plan {
    topology: u64,
    order: Vec<NodeIndex<DefaultIx>>,
    cycle_breakers: FxHashSet<NodeIndex<DefaultIx>>,
    cycle_nodes: FxHashSet<NodeIndex<DefaultIx>>,
}

pub(crate) struct Node {
    node: RefCell<Box<dyn AudioNodeEngine>>,
}

/// An edge in the graph
///
/// This connects one or more pair of ports between two
/// nodes, each connection represented by a `Connection`.
/// WebAudio allows for multiple connections to/from the same port
/// however it does not allow for duplicate connections between pairs
/// of ports
pub(crate) struct Edge {
    connections: SmallVec<[Connection; 1]>,
}

impl Edge {
    /// Find if there are connections between two given ports, return the index
    fn has_between(
        &self,
        output_idx: PortIndex<OutputPort>,
        input_idx: PortIndex<InputPort>,
    ) -> bool {
        self.connections
            .iter()
            .any(|e| e.input_idx == input_idx && e.output_idx == output_idx)
    }

    fn remove_by_output(&mut self, output_idx: PortIndex<OutputPort>) {
        self.connections.retain(|i| i.output_idx != output_idx)
    }

    fn remove_by_input(&mut self, input_idx: PortIndex<InputPort>) {
        self.connections.retain(|i| i.input_idx != input_idx)
    }

    fn remove_by_pair(
        &mut self,
        output_idx: PortIndex<OutputPort>,
        input_idx: PortIndex<InputPort>,
    ) {
        self.connections
            .retain(|i| i.output_idx != output_idx || i.input_idx != input_idx)
    }
}

/// A single connection between ports
struct Connection {
    /// The index of the port on the input node
    /// This is actually the /output/ of this edge
    input_idx: PortIndex<InputPort>,
    /// The index of the port on the output node
    /// This is actually the /input/ of this edge
    output_idx: PortIndex<OutputPort>,
    /// When the from node finishes processing, it will push
    /// its data into this cache for the input node to read
    cache: RefCell<Option<Block>>,
}

impl AudioGraph {
    pub fn new(channel_count: u8) -> Self {
        let mut graph = StableGraph::new();
        let dest_id =
            NodeId(graph.add_node(Node::new(Box::new(DestinationNode::new(channel_count)))));
        let listener_id = NodeId(graph.add_node(Node::new(Box::new(AudioListenerNode::new()))));
        AudioGraph {
            graph,
            dest_id,
            dests: vec![dest_id],
            listener_id,
            unreferenced: Vec::new(),
            topology: 0,
            plan: None,
        }
    }

    /// Create a node, obtain its id
    pub(crate) fn add_node(&mut self, node: Box<dyn AudioNodeEngine>) -> NodeId {
        self.topology += 1;
        NodeId(self.graph.add_node(Node::new(node)))
    }

    /// Connect an output port to an input port
    ///
    /// The edge goes *from* the output port *to* the input port, connecting two nodes
    pub fn add_edge(&mut self, out: PortId<OutputPort>, inp: PortId<InputPort>) {
        self.topology += 1;
        let edge = self
            .graph
            .edges(out.node().0)
            .find(|e| e.target() == inp.node().0)
            .map(|e| e.id());
        if let Some(e) = edge {
            // .find(|e| e.weight().has_between(out.1, inp.1));
            let w = self
                .graph
                .edge_weight_mut(e)
                .expect("This edge is known to exist");
            if w.has_between(out.1, inp.1) {
                return;
            }
            w.connections.push(Connection::new(inp.1, out.1))
        } else {
            // add a new edge
            self.graph
                .add_edge(out.node().0, inp.node().0, Edge::new(inp.1, out.1));
        }
    }

    /// Disconnect all outgoing connections from a node
    ///
    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-disconnect>
    pub fn disconnect_all_from(&mut self, node: NodeId) {
        self.topology += 1;
        let edges = self.graph.edges(node.0).map(|e| e.id()).collect::<Vec<_>>();
        for edge in edges {
            self.graph.remove_edge(edge);
        }
    }

    /// Disconnect all outgoing connections from a node's output
    ///
    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-disconnect-output>
    pub fn disconnect_output(&mut self, out: PortId<OutputPort>) {
        self.topology += 1;
        let candidates: Vec<_> = self
            .graph
            .edges(out.node().0)
            .map(|e| (e.id(), e.target()))
            .collect();
        for (edge, to) in candidates {
            let mut e = self
                .graph
                .remove_edge(edge)
                .expect("Edge index is known to exist");
            e.remove_by_output(out.1);
            if !e.connections.is_empty() {
                self.graph.add_edge(out.node().0, to, e);
            }
        }
    }

    /// Disconnect connections from a node to another node
    ///
    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-disconnect-destinationnode>
    pub fn disconnect_between(&mut self, from: NodeId, to: NodeId) {
        self.topology += 1;
        let edge = self
            .graph
            .edges(from.0)
            .find(|e| e.target() == to.0)
            .map(|e| e.id());
        if let Some(i) = edge {
            self.graph.remove_edge(i);
        }
    }

    /// Disconnect all outgoing connections from a node's output to another node
    ///
    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-disconnect-destinationnode-output>
    pub fn disconnect_output_between(&mut self, out: PortId<OutputPort>, to: NodeId) {
        self.topology += 1;
        let edge = self
            .graph
            .edges(out.node().0)
            .find(|e| e.target() == to.0)
            .map(|e| e.id());
        if let Some(edge) = edge {
            let mut e = self
                .graph
                .remove_edge(edge)
                .expect("Edge index is known to exist");
            e.remove_by_output(out.1);
            if !e.connections.is_empty() {
                self.graph.add_edge(out.node().0, to.0, e);
            }
        }
    }

    /// Disconnect all outgoing connections from a node to another node's input
    ///
    /// Only used in WebAudio for disconnecting audio params
    ///
    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-disconnect-destinationparam>
    pub fn disconnect_to(&mut self, node: NodeId, inp: PortId<InputPort>) {
        self.topology += 1;
        let edge = self
            .graph
            .edges(node.0)
            .find(|e| e.target() == inp.node().0)
            .map(|e| e.id());
        if let Some(edge) = edge {
            let mut e = self
                .graph
                .remove_edge(edge)
                .expect("Edge index is known to exist");
            e.remove_by_input(inp.1);
            if !e.connections.is_empty() {
                self.graph.add_edge(node.0, inp.node().0, e);
            }
        }
    }

    /// Disconnect all outgoing connections from a node's output to another node's input
    ///
    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-disconnect-destinationnode-output-input>
    /// <https://webaudio.github.io/web-audio-api/#dom-audionode-disconnect-destinationparam-output>
    pub fn disconnect_output_between_to(
        &mut self,
        out: PortId<OutputPort>,
        inp: PortId<InputPort>,
    ) {
        self.topology += 1;
        let edge = self
            .graph
            .edges(out.node().0)
            .find(|e| e.target() == inp.node().0)
            .map(|e| e.id());
        if let Some(edge) = edge {
            let mut e = self
                .graph
                .remove_edge(edge)
                .expect("Edge index is known to exist");
            e.remove_by_pair(out.1, inp.1);
            if !e.connections.is_empty() {
                self.graph.add_edge(out.node().0, inp.node().0, e);
            }
        }
    }

    /// Get the id of the destination node in this graph
    ///
    /// All graphs have a destination node, with one input port
    pub fn dest_id(&self) -> NodeId {
        self.dest_id
    }

    /// Add additional terminator nodes
    pub fn add_extra_dest(&mut self, dest: NodeId) {
        self.topology += 1;
        self.dests.push(dest);
    }

    /// Get the id of the AudioListener in this graph
    ///
    /// All graphs have a single listener, with no ports (but nine AudioParams)
    ///
    /// N.B. The listener actually has a single output port containing
    /// its position data for the block, however this should
    /// not be exposed to the DOM.
    pub fn listener_id(&self) -> NodeId {
        self.listener_id
    }

    /// For a given block, process all the data on this graph
    ///
    /// This implements steps 4.2 and parts of 4.4 from
    /// <https://webaudio.github.io/web-audio-api/#rendering-loop>
    pub fn process(&mut self, info: &BlockInfo) -> Chunk {
        // Step 4.2. Order the AudioNodes of the BaseAudioContext to be processed. The order
        // only depends on the topology: reuse it until nodes or edges change.
        if self.plan.as_ref().is_none_or(|plan| plan.topology != self.topology) {
            self.plan = Some(self.make_plan());
        }
        let plan = self.plan.take().expect("planned above");
        let out = self.process_with_plan(&plan, info);
        self.plan = Some(plan);
        out
    }

    fn make_plan(&self) -> Plan {
        let (cycle_breakers, cycle_nodes, scc_of) = self.detect_cycles();

        // Post order: a node comes after every node it reads from, except that the inputs of
        // a cycle-breaking DelayNode are not followed from it: its DelayReader half has no
        // inputs. Its DelayWriter half is a sink, so its inputs are extra roots, visited after
        // the destinations.
        let mut roots: SmallVec<[NodeIndex<DefaultIx>; 4]> =
            self.dests.iter().map(|d| d.0).collect();
        // AnalyserNodes are processed even when nothing downstream pulls from them: a page may
        // connect a source to an analyser only to read it (Tone.js meters, the j9 probes). The
        // rendering loop orders all of the context's nodes, not only those that reach the
        // destination. So are AudioWorkletNodes: an actively processing processor runs whether
        // or not its outputs reach the destination ("automatic pull"), and one with no outputs
        // is a sink. And so is any other node whose output goes nowhere (a source never
        // connected, a chain left dangling): a scheduled source must still play out, fire
        // `ended` and become removable (node lifetime). Idle ones are skipped (`is_inactive`).
        // ScriptProcessorNodes too: `audioprocess` fires whether or not the output is used.
        // Media element and stream sources are left out: they pull from outside the graph.
        for ix in self.graph.node_indices() {
            let node_type = self.graph[ix].node.borrow().node_type();
            let root = match node_type {
                AudioNodeType::AnalyserNode |
                AudioNodeType::AudioWorkletNode |
                AudioNodeType::ScriptProcessorNode => true,
                AudioNodeType::AudioListenerNode |
                AudioNodeType::MediaElementSourceNode |
                AudioNodeType::MediaStreamSourceNode => false,
                _ => self.graph.edges(ix).next().is_none(),
            };
            if root {
                roots.push(ix);
            }
        }
        for breaker in &cycle_breakers {
            for edge in self.graph.edges_directed(*breaker, Direction::Incoming) {
                roots.push(edge.source());
            }
        }
        let order = self.processing_order(&roots, &cycle_breakers, &scc_of);
        Plan {
            topology: self.topology,
            order,
            cycle_breakers,
            cycle_nodes,
        }
    }

    fn process_with_plan(&self, plan: &Plan, info: &BlockInfo) -> Chunk {
        let Plan {
            order,
            cycle_breakers,
            cycle_nodes,
            ..
        } = plan;
        for &ix in order {
            let mut curr = self.graph[ix].node.borrow_mut();

            if cycle_nodes.contains(&ix) {
                // Step 4.2.7. If nodes contains cycles, mute all the AudioNodes
                // that are part of this cycle, and remove them from nodes.
                let chunk = curr.as_mut().mute_node();
                if curr.output_count() == 0 {
                    // Step 4.4.5. If this AudioNode is a destination node, record
                    // the input of this AudioNode.
                    curr.process(chunk, info);
                } else {
                    // Step 4.4.6. Else, process the input buffer, and make
                    // available for reading the resulting buffer.
                    self.fill_node_cache_with_silence(ix, curr.output_count());
                }
                continue;
            }

            if cycle_breakers.contains(&ix) {
                // Step 4.2.6: the DelayReader. Its output comes only from frames written in
                // earlier render quanta. AudioParam inputs from outside its cycle have run
                // already (see `processing_order`): use them for this quantum's delayTime.
                self.gather_ready_param_inputs(ix, &mut curr, info.frame);
                let out = curr
                    .as_delay_node()
                    .expect("cycle breakers are DelayNodes")
                    .process_reader(info);
                self.distribute_outputs(ix, out);
                continue;
            }

            if self.is_inactive(ix, &curr, info.frame) {
                // Nothing to do this quantum (a finished source, a node with silent inputs and
                // no tail): skip it. Its consumers find no block and read silence.
                for edge in self.graph.edges_directed(ix, Direction::Incoming) {
                    for connection in &edge.weight().connections {
                        connection.cache.borrow_mut().take();
                    }
                }
                continue;
            }

            let chunk = self.gather_inputs(ix, &mut curr, info.frame);

            // actually run the node engine
            let out = curr.process(chunk, info);

            assert_eq!(out.len(), curr.output_count() as usize);
            if curr.output_count() == 0 {
                continue;
            }
            self.distribute_outputs(ix, out);
        }

        // The DelayWriters: every node that feeds a cycle breaker has run by now.
        for ix in cycle_breakers {
            let mut curr = self.graph[*ix].node.borrow_mut();
            let chunk = self.gather_inputs(*ix, &mut curr, info.frame);
            curr.as_delay_node()
                .expect("cycle breakers are DelayNodes")
                .process_writer(chunk, info);
        }

        // The destination node stores its output on itself, extract it.
        self.graph[self.dest_id.0]
            .node
            .borrow_mut()
            .destination_data()
            .expect("Destination node should have data cached")
    }

    /// Depth-first post order from `roots` over the edges into each node (its inputs), not
    /// following the inputs of `cut` nodes. Each node appears once.
    ///
    /// For a cut node, the sources of its AudioParam inputs that are outside its own cycle
    /// (`scc_of`) are still followed: they do not depend on the node, and running them first
    /// lets a cycle-breaking DelayNode read a modulated delayTime in the same quantum.
    fn processing_order(
        &self,
        roots: &[NodeIndex<DefaultIx>],
        cut: &FxHashSet<NodeIndex<DefaultIx>>,
        scc_of: &FxHashMap<NodeIndex<DefaultIx>, usize>,
    ) -> Vec<NodeIndex<DefaultIx>> {
        let mut order = Vec::with_capacity(self.graph.node_count());
        let mut seen: FxHashSet<NodeIndex<DefaultIx>> = FxHashSet::default();
        // (node, inputs already pushed)
        let mut stack: Vec<(NodeIndex<DefaultIx>, bool)> = Vec::new();
        for root in roots {
            stack.push((*root, false));
            while let Some((ix, expanded)) = stack.pop() {
                if expanded {
                    order.push(ix);
                    continue;
                }
                if !seen.insert(ix) {
                    continue;
                }
                stack.push((ix, true));
                let is_cut = cut.contains(&ix);
                for edge in self.graph.edges_directed(ix, Direction::Incoming) {
                    if is_cut {
                        let param_input = edge
                            .weight()
                            .connections
                            .iter()
                            .any(|c| matches!(c.input_idx, PortIndex::Param(_)));
                        let same_cycle = scc_of.get(&edge.source()) == scc_of.get(&ix);
                        if !param_input || same_cycle {
                            continue;
                        }
                    }
                    if !seen.contains(&edge.source()) {
                        stack.push((edge.source(), false));
                    }
                }
            }
        }
        order
    }

    /// Collect, mix and sum the blocks connected to each input of `ix` into a chunk, and hand
    /// AudioParam and listener inputs to the node.
    fn gather_inputs(
        &self,
        ix: NodeIndex<DefaultIx>,
        curr: &mut RefMut<'_, Box<dyn AudioNodeEngine>>,
        frame: Tick,
    ) -> Chunk {
        let mut chunk = Chunk::default();
        chunk
            .blocks
            .resize(curr.input_count() as usize, Default::default());

        // if we have inputs, collect all the computed blocks
        // and construct a Chunk

        // set up scratch space to store all the blocks
        let mut blocks: SmallVec<[SmallVec<[Block; 1]>; 1]> = SmallVec::new();
        blocks.resize(curr.input_count() as usize, Default::default());

        let mode = curr.channel_count_mode();
        let count = curr.channel_count();
        let interpretation = curr.channel_interpretation();

        // all edges to this node are from its dependencies
        for edge in self.graph.edges_directed(ix, Direction::Incoming) {
            let edge = edge.weight();
            for connection in &edge.connections {
                let Some(mut block) = connection.cache.borrow_mut().take() else {
                    // No block: the source was skipped as inactive (silence), or a DelayReader
                    // took an AudioParam input already.
                    continue;
                };

                match connection.input_idx {
                    PortIndex::Port(idx) => {
                        blocks[idx as usize].push(block);
                    },
                    PortIndex::Param(param) => {
                        // param inputs are downmixed to mono
                        // https://webaudio.github.io/web-audio-api/#dom-audionode-connect-destinationparam-output
                        block.mix(1, ChannelInterpretation::Speakers);
                        curr.get_param(param).add_block(block, frame)
                    },
                    PortIndex::Listener(_) => curr.set_listenerdata(block),
                }
            }
        }

        for (i, mut blocks) in blocks.drain(..).enumerate() {
            if blocks.is_empty() {
                if mode == ChannelCountMode::Explicit {
                    // It's silence, but mix it anyway
                    chunk.blocks[i].mix(count, interpretation);
                }
            } else if blocks.len() == 1 {
                chunk.blocks[i] = blocks.pop().expect("`blocks` had length 1");
                match mode {
                    ChannelCountMode::Explicit => {
                        chunk.blocks[i].mix(count, interpretation);
                    },
                    ChannelCountMode::ClampedMax => {
                        if chunk.blocks[i].chan_count() > count {
                            chunk.blocks[i].mix(count, interpretation);
                        }
                    },
                    // It's one channel, it maxes itself
                    ChannelCountMode::Max => (),
                }
            } else {
                let mix_count = match mode {
                    ChannelCountMode::Explicit => count,
                    _ => {
                        let mut max = 0; // max channel count
                        for block in &blocks {
                            max = cmp::max(max, block.chan_count());
                        }
                        if mode == ChannelCountMode::ClampedMax {
                            max = cmp::min(max, count);
                        }
                        max
                    },
                };
                let block = blocks.into_iter().fold(Block::default(), |acc, mut block| {
                    block.mix(mix_count, interpretation);
                    acc.sum(block)
                });
                chunk.blocks[i] = block;
            }
        }
        chunk
    }

    /// A node that need not run this quantum: it says it has nothing more to produce
    /// (`removable`) and every block waiting at its inputs is silence (or absent). Analysers
    /// always run (they record silence too), as do nodes without outputs (destinations).
    fn is_inactive(
        &self,
        ix: NodeIndex<DefaultIx>,
        curr: &RefMut<'_, Box<dyn AudioNodeEngine>>,
        now: Tick,
    ) -> bool {
        if curr.output_count() == 0 ||
            curr.node_type() == AudioNodeType::AnalyserNode ||
            !curr.removable(now)
        {
            return false;
        }
        self.graph.edges_directed(ix, Direction::Incoming).all(|edge| {
            edge.weight().connections.iter().all(|connection| {
                matches!(connection.input_idx, PortIndex::Listener(_)) ||
                    connection
                        .cache
                        .borrow()
                        .as_ref()
                        .is_none_or(|block| block.is_silence())
            })
        })
    }

    /// Hand the AudioParam inputs of `ix` whose sources have already run to its params.
    fn gather_ready_param_inputs(
        &self,
        ix: NodeIndex<DefaultIx>,
        curr: &mut RefMut<'_, Box<dyn AudioNodeEngine>>,
        frame: Tick,
    ) {
        for edge in self.graph.edges_directed(ix, Direction::Incoming) {
            for connection in &edge.weight().connections {
                if let PortIndex::Param(param) = connection.input_idx &&
                    let Some(mut block) = connection.cache.borrow_mut().take()
                {
                    block.mix(1, ChannelInterpretation::Speakers);
                    curr.get_param(param).add_block(block, frame);
                }
            }
        }
    }

    /// Make the output blocks of `ix` available for reading by the nodes it connects to.
    fn distribute_outputs(&self, ix: NodeIndex<DefaultIx>, mut out: Chunk) {
        // Count how many output connections fan out from each port
        // This is so that we don't have to needlessly clone audio buffers
        let mut output_counts: SmallVec<[u32; 1]> = SmallVec::new();
        output_counts.resize(out.len(), 0);
        for edge in self.graph.edges(ix) {
            let edge = edge.weight();
            for conn in &edge.connections {
                if let PortIndex::Port(idx) = conn.output_idx {
                    output_counts[idx as usize] += 1;
                } else {
                    unreachable!()
                }
            }
        }

        // all the edges from this node go to nodes which depend on it,
        // i.e. the nodes it outputs to. Store the blocks for retrieval.
        for edge in self.graph.edges(ix) {
            let edge = edge.weight();
            for conn in &edge.connections {
                if let PortIndex::Port(idx) = conn.output_idx {
                    output_counts[idx as usize] -= 1;
                    // if there are no consumers left after this, take the data
                    let block = if output_counts[idx as usize] == 0 {
                        out[conn.output_idx].take()
                    } else {
                        out[conn.output_idx].clone()
                    };
                    *conn.cache.borrow_mut() = Some(block);
                } else {
                    unreachable!()
                }
            }
        }
    }

    /// Obtain a mutable reference to a node
    pub(crate) fn node_mut(&self, ix: NodeId) -> RefMut<'_, Box<dyn AudioNodeEngine>> {
        self.graph[ix.0].node.borrow_mut()
    }

    /// Whether `id` is (still) a node of this graph.
    pub fn has_node(&self, id: NodeId) -> bool {
        self.graph.contains_node(id.0)
    }

    /// Unreferenced nodes waiting to become removable.
    pub fn unreferenced_count(&self) -> usize {
        self.unreferenced.len()
    }

    /// The number of nodes in the graph (including the destination and listener).
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// The node's DOM object (and its AudioParams) are gone: it can't be connected, started or
    /// changed any more. It is removed once it stops mattering (`collect_unreferenced`).
    pub fn node_unreferenced(&mut self, id: NodeId) {
        if !self.graph.contains_node(id.0) ||
            id == self.dest_id ||
            id == self.listener_id ||
            self.dests.contains(&id) ||
            self.unreferenced.contains(&id.0)
        {
            return;
        }
        self.unreferenced.push(id.0);
    }

    /// Node lifetime (<https://webaudio.github.io/web-audio-api/#lifetime-AudioNode>): remove
    /// the unreferenced nodes that can't affect the output any more: no inputs left (audio or
    /// AudioParam; the listener's feed to a panner doesn't count) and either no outputs or
    /// nothing more to produce (`AudioNodeEngine::removable`). Removing a node drops its edges,
    /// which can free the next node downstream in a later quantum. Runs after each render
    /// quantum, only over the candidates; it does not allocate.
    pub(crate) fn collect_unreferenced(&mut self, now: Tick) {
        let mut i = 0;
        while i < self.unreferenced.len() {
            let ix = self.unreferenced[i];
            if self.is_removable(ix, now) {
                // The engine is dropped here, on the render thread.
                self.graph.remove_node(ix);
                self.topology += 1;
                self.unreferenced.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    fn is_removable(&self, ix: NodeIndex<DefaultIx>, now: Tick) -> bool {
        let has_inputs = self.graph.edges_directed(ix, Direction::Incoming).any(|edge| {
            edge.weight()
                .connections
                .iter()
                .any(|c| !matches!(c.input_idx, PortIndex::Listener(_)))
        });
        if has_inputs {
            return false;
        }
        let has_outputs = self.graph.edges(ix).next().is_some();
        !has_outputs || self.graph[ix].node.borrow().removable(now)
    }

    /// Steps 4.2.4 to 4.2.7 of <https://webaudio.github.io/web-audio-api/#rendering-loop>:
    /// returns the cycle breakers (DelayNodes that are part of a cycle) and the nodes of the
    /// cycles that remain once the breakers are split, which must be muted.
    ///
    /// > 4. Let cycle breakers be an empty set of DelayNodes. It will contain all the
    /// >    DelayNodes that are part of a cycle.
    /// > 5. For each AudioNode node in nodes:
    /// >    1. If node is a DelayNode that is part of a cycle, add it to cycle breakers
    /// >       and remove it from nodes.
    /// > 6. For each DelayNode delay in cycle breakers:
    /// >    1. Let delayWriter and delayReader respectively be a DelayWriter and a
    /// >       DelayReader, for delay. Add delayWriter and delayReader to nodes.
    /// >       Disconnect delay from all its input and outputs.
    /// >       Note: This breaks the cycle: if a DelayNode is in a cycle, its two ends can be
    /// >       considered separately, because delay lines cannot be smaller than one render
    /// >       quantum when in a cycle.
    /// > 7. If nodes contains cycles, mute all the AudioNodes that are part of this cycle, and
    /// >    remove them from nodes.
    ///
    /// Also returns, for every node in a cycle, an id of its strongly connected component.
    #[allow(clippy::type_complexity)]
    fn detect_cycles(
        &self,
    ) -> (
        FxHashSet<NodeIndex<DefaultIx>>,
        FxHashSet<NodeIndex<DefaultIx>>,
        FxHashMap<NodeIndex<DefaultIx>, usize>,
    ) {
        let mut breakers = FxHashSet::default();
        let mut scc_of = FxHashMap::default();
        for (id, component) in tarjan_scc(&self.graph).into_iter().enumerate() {
            if self.is_cycle(&component) {
                for ix in component {
                    scc_of.insert(ix, id);
                    if self.graph[ix].node.borrow().node_type() == AudioNodeType::DelayNode {
                        breakers.insert(ix);
                    }
                }
            }
        }
        let mut cycle_nodes = FxHashSet::default();
        if scc_of.is_empty() {
            return (breakers, cycle_nodes, scc_of);
        }
        // The cycles left once the breakers' inputs are cut.
        let cut = EdgeFiltered::from_fn(&self.graph, |edge| !breakers.contains(&edge.target()));
        for component in tarjan_scc(&cut) {
            let is_cycle = component.len() > 1 ||
                component.first().is_some_and(|node_index| {
                    self.graph.edges(*node_index).any(|edge| {
                        edge.target() == *node_index && !breakers.contains(node_index)
                    })
                });
            if is_cycle {
                cycle_nodes.extend(component);
            }
        }
        // Breakers inside a muted cycle are muted with it.
        breakers.retain(|ix| !cycle_nodes.contains(ix));
        (breakers, cycle_nodes, scc_of)
    }

    /// A strongly connected component with several nodes is a cycle; a single node is a cycle
    /// only when it connects to itself.
    fn is_cycle(&self, component: &[NodeIndex<DefaultIx>]) -> bool {
        component.len() > 1 ||
            component.first().is_some_and(|node_index| {
                self.graph
                    .edges(*node_index)
                    .any(|edge| edge.target() == *node_index)
            })
    }
    /// <https://webaudio.github.io/web-audio-api/#available-for-reading>
    ///
    /// Making a buffer available for reading from an AudioNode means putting
    /// it in a state where other AudioNodes connected to this AudioNode can
    /// safely read from it.
    ///
    /// The specification does not require these buffers to be silent. This helper
    /// runs after [`AudioNodeEngine::mute_node`], so it fills the caches with
    /// silence for downstream nodes.
    fn fill_node_cache_with_silence(&self, node_index: NodeIndex<DefaultIx>, output_count: u32) {
        for edge in self.graph.edges(node_index) {
            let edge = edge.weight();
            for connection in &edge.connections {
                if let PortIndex::Port(index) = connection.output_idx {
                    if index < output_count {
                        *connection.cache.borrow_mut() = Some(Block::default());
                    }
                } else {
                    unreachable!()
                }
            }
        }
    }
}

impl Node {
    pub fn new(node: Box<dyn AudioNodeEngine>) -> Self {
        Node {
            node: RefCell::new(node),
        }
    }
}

impl Edge {
    pub fn new(input_idx: PortIndex<InputPort>, output_idx: PortIndex<OutputPort>) -> Self {
        Edge {
            connections: SmallVec::from_buf([Connection::new(input_idx, output_idx)]),
        }
    }
}

impl Connection {
    pub fn new(input_idx: PortIndex<InputPort>, output_idx: PortIndex<OutputPort>) -> Self {
        Connection {
            input_idx,
            output_idx,
            cache: RefCell::new(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_node::{AudioNodeType, ChannelInfo};
    use crate::block::{FRAMES_PER_BLOCK_USIZE, Tick};
    use crate::delay_node::{DelayNode, DelayNodeOptions};
    use crate::gain_node::{GainNode, GainNodeOptions};

    /// A source that plays one unit impulse at frame 0 of the first block.
    #[derive(AudioNodeCommon)]
    struct Impulse {
        channel_info: ChannelInfo,
        done: bool,
    }

    impl AudioNodeEngine for Impulse {
        fn node_type(&self) -> AudioNodeType {
            AudioNodeType::ConstantSourceNode
        }
        fn input_count(&self) -> u32 {
            0
        }
        fn process(&mut self, _: Chunk, _: &BlockInfo) -> Chunk {
            let mut chunk = Chunk::default();
            if self.done {
                chunk.blocks.push(Block::default());
            } else {
                let mut v = vec![0.; FRAMES_PER_BLOCK_USIZE];
                v[0] = 1.;
                chunk.blocks.push(Block::for_vec(v));
                self.done = true;
            }
            chunk
        }
        fn removable(&self, _: Tick) -> bool {
            self.done
        }
    }

    fn impulse() -> Box<dyn AudioNodeEngine> {
        Box::new(Impulse {
            channel_info: ChannelInfo::default(),
            done: false,
        })
    }

    fn render(graph: &mut AudioGraph, blocks: u64) -> Vec<f32> {
        let mut out = Vec::new();
        for n in 0..blocks {
            let info = BlockInfo {
                sample_rate: 48000.,
                frame: Tick(n * FRAMES_PER_BLOCK_USIZE as u64),
                time: 0.,
            };
            let chunk = graph.process(&info);
            let block = &chunk.blocks[0];
            if block.is_silence() {
                out.extend(std::iter::repeat_n(0., FRAMES_PER_BLOCK_USIZE));
            } else {
                out.extend_from_slice(block.data_chan(0));
            }
        }
        out
    }

    #[test]
    fn feedback_delay_cycle_echoes_each_render_quantum() {
        let mut graph = AudioGraph::new(2);
        let src = graph.add_node(impulse());
        // A delay of 0 inside a cycle is clamped to one render quantum.
        let delay = graph.add_node(Box::new(DelayNode::new(
            DelayNodeOptions::default(),
            ChannelInfo::default(),
        )));
        let feedback = graph.add_node(Box::new(GainNode::new(
            GainNodeOptions { gain: 0.5 },
            ChannelInfo::default(),
        )));
        let dest = graph.dest_id();
        graph.add_edge(src.output(0), delay.input(0));
        graph.add_edge(delay.output(0), feedback.input(0));
        graph.add_edge(feedback.output(0), delay.input(0));
        graph.add_edge(delay.output(0), dest.input(0));

        let out = render(&mut graph, 5);
        assert_eq!(out[0], 0.);
        assert_eq!(out[128], 1.);
        assert_eq!(out[256], 0.5);
        assert_eq!(out[384], 0.25);
        let nonzero = out.iter().filter(|v| **v != 0.).count();
        assert_eq!(nonzero, 4, "{:?}", out.iter().enumerate().filter(|(_, v)| **v != 0.).collect::<Vec<_>>());
    }

    /// Stands in for an AnalyserNode: counts the non-silent blocks it sees.
    #[derive(AudioNodeCommon)]
    struct Meter {
        channel_info: ChannelInfo,
        seen: std::rc::Rc<std::cell::Cell<u32>>,
    }

    // The graph is single-threaded in these tests; Rc is fine for the counter.
    unsafe impl Send for Meter {}

    impl AudioNodeEngine for Meter {
        fn node_type(&self) -> AudioNodeType {
            AudioNodeType::AnalyserNode
        }
        fn process(&mut self, inputs: Chunk, _: &BlockInfo) -> Chunk {
            if !inputs.blocks[0].is_silence() {
                self.seen.set(self.seen.get() + 1);
            }
            inputs
        }
    }

    #[test]
    fn analyser_not_connected_to_the_destination_is_processed() {
        let mut graph = AudioGraph::new(2);
        let src = graph.add_node(impulse());
        let seen = std::rc::Rc::new(std::cell::Cell::new(0));
        let meter = graph.add_node(Box::new(Meter {
            channel_info: ChannelInfo::default(),
            seen: seen.clone(),
        }));
        graph.add_edge(src.output(0), meter.input(0));
        render(&mut graph, 2);
        assert_eq!(seen.get(), 1);
    }

    /// A source of a constant value.
    #[derive(AudioNodeCommon)]
    struct Constant {
        channel_info: ChannelInfo,
        value: f32,
    }

    impl AudioNodeEngine for Constant {
        fn node_type(&self) -> AudioNodeType {
            AudioNodeType::ConstantSourceNode
        }
        fn input_count(&self) -> u32 {
            0
        }
        fn process(&mut self, _: Chunk, _: &BlockInfo) -> Chunk {
            let mut chunk = Chunk::default();
            chunk
                .blocks
                .push(Block::for_vec(vec![self.value; FRAMES_PER_BLOCK_USIZE]));
            chunk
        }
        fn removable(&self, _: Tick) -> bool {
            false
        }
    }

    #[test]
    fn modulated_delay_time_in_a_cycle_applies_in_the_same_quantum() {
        let mut graph = AudioGraph::new(2);
        let src = graph.add_node(impulse());
        let delay = graph.add_node(Box::new(DelayNode::new(
            DelayNodeOptions::default(),
            ChannelInfo::default(),
        )));
        let feedback = graph.add_node(Box::new(GainNode::new(
            GainNodeOptions { gain: 0.5 },
            ChannelInfo::default(),
        )));
        // delayTime = 0 + 0.004 s from the modulator: 192 frames at 48 kHz.
        let modulator = graph.add_node(Box::new(Constant {
            channel_info: ChannelInfo::default(),
            value: 0.004,
        }));
        let dest = graph.dest_id();
        graph.add_edge(src.output(0), delay.input(0));
        graph.add_edge(delay.output(0), feedback.input(0));
        graph.add_edge(feedback.output(0), delay.input(0));
        graph.add_edge(modulator.output(0), delay.param(ParamType::DelayTime));
        graph.add_edge(delay.output(0), dest.input(0));
        let out = render(&mut graph, 4);
        let first = out.iter().position(|v| *v != 0.).unwrap();
        assert_eq!(first, 192);
        // 0.004 s is 192.00001 frames in f32: the interpolation is off by ~1e-5.
        assert!((out[192] - 1.).abs() < 1e-4, "{}", out[192]);
        assert!((out[384] - 0.5).abs() < 1e-4, "{}", out[384]);
    }

    /// A source that plays (a constant) until `until`, then is removable.
    #[derive(AudioNodeCommon)]
    struct Voice {
        channel_info: ChannelInfo,
        until: Tick,
    }

    impl AudioNodeEngine for Voice {
        fn node_type(&self) -> AudioNodeType {
            AudioNodeType::AudioBufferSourceNode
        }
        fn input_count(&self) -> u32 {
            0
        }
        fn process(&mut self, _: Chunk, info: &BlockInfo) -> Chunk {
            let mut chunk = Chunk::default();
            if info.frame < self.until {
                chunk.blocks.push(Block::for_vec(vec![0.1; FRAMES_PER_BLOCK_USIZE]));
            } else {
                chunk.blocks.push(Block::default());
            }
            chunk
        }
        fn removable(&self, now: Tick) -> bool {
            now >= self.until
        }
    }

    fn voice(graph: &mut AudioGraph, until: u64) -> (NodeId, NodeId) {
        let src = graph.add_node(Box::new(Voice {
            channel_info: ChannelInfo::default(),
            until: Tick(until),
        }));
        let gain = graph.add_node(Box::new(GainNode::new(
            GainNodeOptions { gain: 0.5 },
            ChannelInfo::default(),
        )));
        let dest = graph.dest_id();
        graph.add_edge(src.output(0), gain.input(0));
        graph.add_edge(gain.output(0), dest.input(0));
        (src, gain)
    }

    fn render_and_collect(graph: &mut AudioGraph, blocks: u64) {
        for n in 0..blocks {
            let frame = Tick(n * FRAMES_PER_BLOCK_USIZE as u64);
            let info = BlockInfo {
                sample_rate: 48000.,
                frame,
                time: 0.,
            };
            graph.process(&info);
            graph.collect_unreferenced(frame + Tick(FRAMES_PER_BLOCK_USIZE as u64));
        }
    }

    #[test]
    fn unreferenced_voices_are_removed_once_finished() {
        let mut graph = AudioGraph::new(2);
        let base = graph.node_count(); // destination + listener
        let (src, gain) = voice(&mut graph, 256);
        graph.node_unreferenced(src);
        graph.node_unreferenced(gain);
        // Still playing: nothing goes.
        render_and_collect(&mut graph, 1);
        assert_eq!(graph.node_count(), base + 2);
        // Finished: the source goes, then the gain (no inputs left).
        render_and_collect(&mut graph, 3);
        assert_eq!(graph.node_count(), base);
        assert!(!graph.has_node(src) && !graph.has_node(gain));
    }

    #[test]
    fn referenced_nodes_and_their_downstream_stay() {
        let mut graph = AudioGraph::new(2);
        let base = graph.node_count();
        let (src, gain) = voice(&mut graph, 128);
        // The page still holds the source, but dropped the gain: the gain has an input.
        graph.node_unreferenced(gain);
        render_and_collect(&mut graph, 4);
        assert!(graph.has_node(src) && graph.has_node(gain));
        assert_eq!(graph.node_count(), base + 2);
    }

    /// A source that counts its process() calls.
    #[derive(AudioNodeCommon)]
    struct Counted {
        channel_info: ChannelInfo,
        calls: std::sync::Arc<std::sync::atomic::AtomicU32>,
    }

    impl AudioNodeEngine for Counted {
        fn node_type(&self) -> AudioNodeType {
            AudioNodeType::ConstantSourceNode
        }
        fn input_count(&self) -> u32 {
            0
        }
        fn process(&mut self, _: Chunk, _: &BlockInfo) -> Chunk {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut chunk = Chunk::default();
            chunk.blocks.push(Block::for_vec(vec![0.1; FRAMES_PER_BLOCK_USIZE]));
            chunk
        }
        fn removable(&self, _: Tick) -> bool {
            false
        }
    }

    #[test]
    fn sources_whose_output_goes_nowhere_still_run() {
        // Never connected (it must still reach its stop time and fire `ended`), and connected
        // only to a dangling gain.
        let mut graph = AudioGraph::new(2);
        let lone: std::sync::Arc<std::sync::atomic::AtomicU32> = Default::default();
        let dangling: std::sync::Arc<std::sync::atomic::AtomicU32> = Default::default();
        graph.add_node(Box::new(Counted {
            channel_info: ChannelInfo::default(),
            calls: lone.clone(),
        }));
        let src = graph.add_node(Box::new(Counted {
            channel_info: ChannelInfo::default(),
            calls: dangling.clone(),
        }));
        let gain = graph.add_node(Box::new(GainNode::new(
            GainNodeOptions { gain: 0.5 },
            ChannelInfo::default(),
        )));
        graph.add_edge(src.output(0), gain.input(0));
        render_and_collect(&mut graph, 3);
        assert_eq!(lone.load(std::sync::atomic::Ordering::Relaxed), 3);
        assert_eq!(dangling.load(std::sync::atomic::Ordering::Relaxed), 3);
    }

    #[test]
    fn destination_and_listener_are_never_removed() {
        let mut graph = AudioGraph::new(2);
        let base = graph.node_count();
        graph.node_unreferenced(graph.dest_id());
        graph.node_unreferenced(graph.listener_id());
        render_and_collect(&mut graph, 2);
        assert_eq!(graph.node_count(), base);
    }

    #[test]
    fn many_voices_keep_the_graph_flat() {
        let mut graph = AudioGraph::new(2);
        let base = graph.node_count();
        let mut frame = 0u64;
        for _ in 0..2000 {
            let (src, gain) = voice(&mut graph, frame + 256);
            graph.node_unreferenced(src);
            graph.node_unreferenced(gain);
            for _ in 0..4 {
                let info = BlockInfo {
                    sample_rate: 48000.,
                    frame: Tick(frame),
                    time: 0.,
                };
                graph.process(&info);
                frame += FRAMES_PER_BLOCK_USIZE as u64;
                graph.collect_unreferenced(Tick(frame));
            }
            assert!(graph.node_count() <= base + 4, "{}", graph.node_count());
        }
        assert_eq!(graph.node_count(), base);
    }

    #[test]
    fn cycle_without_delay_is_muted() {
        let mut graph = AudioGraph::new(2);
        let src = graph.add_node(impulse());
        let a = graph.add_node(Box::new(GainNode::new(
            GainNodeOptions { gain: 1. },
            ChannelInfo::default(),
        )));
        let b = graph.add_node(Box::new(GainNode::new(
            GainNodeOptions { gain: 0.5 },
            ChannelInfo::default(),
        )));
        let dest = graph.dest_id();
        graph.add_edge(src.output(0), a.input(0));
        graph.add_edge(a.output(0), b.input(0));
        graph.add_edge(b.output(0), a.input(0));
        graph.add_edge(a.output(0), dest.input(0));
        assert!(render(&mut graph, 2).iter().all(|v| *v == 0.));
    }

    #[test]
    fn delay_outside_a_cycle_is_a_plain_node() {
        let mut graph = AudioGraph::new(2);
        let src = graph.add_node(impulse());
        let delay = graph.add_node(Box::new(DelayNode::new(
            DelayNodeOptions {
                max_delay_time: 1.,
                delay_time: 5. / 48000.,
            },
            ChannelInfo::default(),
        )));
        let dest = graph.dest_id();
        graph.add_edge(src.output(0), delay.input(0));
        graph.add_edge(delay.output(0), dest.input(0));
        let out = render(&mut graph, 1);
        assert!((out[5] - 1.).abs() < 1e-6);
    }
}
