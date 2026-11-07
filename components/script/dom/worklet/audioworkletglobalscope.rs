/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The AudioWorklet: an [`AudioWorkletGlobalScope`] living on servo-media's audio render thread.
//!
//! Each `BaseAudioContext` has one render thread. The first `audioWorklet.addModule()` sends that
//! thread a task which creates an [`AudioWorkletHost`] there: a SpiderMonkey [`Runtime`] and the
//! context's one `AudioWorkletGlobalScope`. Script tasks for that global (module fetches, port
//! messages) reach it as render-thread tasks too ([`RenderTaskSender`]), so they run between render
//! quanta. Processors run inline in the graph walk: the servo-media `AudioWorkletNode` engine calls
//! [`ProcessorHandle::process`], which calls the processor's `process()` in JS. See
//! `~/runtime-work/w3-audioworklet/DESIGN.md`.
//!
//! <https://webaudio.github.io/web-audio-api/#AudioWorkletGlobalScope>

#![cfg_attr(crown, allow(crown::jscontext_first_arg))]

use std::cell::{Cell, RefCell};
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use dom_struct::dom_struct;
use js::context::JSContext;
use js::jsapi::{HandleValueArray, Heap, IsCallable, IsConstructor, JSObject};
use js::jsval::{JSVal, ObjectValue, UndefinedValue};
use js::rust::wrappers2::{
    Call, Construct1, JS_DefineProperty, JS_IsExceptionPending, JS_NewPlainObject, JS_SetElement,
    NewArrayObject,
};
use js::typedarray::{CreateWith, Float32, TypedArray};
use net_traits::policy_container::PolicyContainer;
use net_traits::request::Destination;
use script_bindings::callback::{HasCallbackHolder, RootedCallback};
use script_bindings::cell::DomRefCell;
use script_bindings::interfaces::HasOrigin;
use servo_base::id::PipelineId;
use servo_media::audio::audio_node::BlockInfo;
use servo_media::audio::audio_worklet_node::AudioWorkletProcessorHandle;
use servo_media::audio::block::{Block, FRAMES_PER_BLOCK_USIZE};
use servo_media::audio::context::RenderTaskSender;
use servo_media::audio::render_thread::{
    on_render_quantum_end, on_render_thread_exit, on_render_thread_idle,
};
use servo_url::{ImmutableOrigin, MutableOrigin, ServoUrl};
use style::thread_state::{self, ThreadState};

use crate::dom::RootedPromise;
use crate::dom::audio::baseaudiocontext::{AudioParamDescriptorData, BaseAudioContext};
use crate::dom::bindings::codegen::Bindings::AudioParamBinding::AutomationRate;
use crate::dom::bindings::codegen::Bindings::BaseAudioContextBinding::BaseAudioContextMethods;
use crate::dom::bindings::codegen::Bindings::AudioWorkletGlobalScopeBinding;
use crate::dom::bindings::codegen::Bindings::AudioWorkletGlobalScopeBinding::{
    AudioWorkletGlobalScopeMethods, AudioWorkletProcessorConstructor,
};
use crate::dom::bindings::codegen::Bindings::AudioWorkletProcessorBinding::AudioParamDescriptor;
use crate::dom::bindings::codegen::Bindings::RequestBinding::RequestCredentials;
use crate::dom::bindings::conversions::{get_property, get_property_jsval};
use crate::dom::bindings::error::{
    Error, Fallible, report_pending_exception, report_pending_exception_with_info,
};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::{Trusted, TrustedPromise};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::structuredclone;
use servo_constellation_traits::StructuredSerializedData;
use crate::dom::bindings::trace::{HashMapTracedValues, RootedTraceableBox};
use crate::dom::globalscope::GlobalScope;
use crate::dom::messageport::MessagePort;
use crate::dom::bindings::utils::define_all_exposed_interfaces;
use crate::dom::worklet::{
    PendingTasksStruct, WorkletExecutor, WorkletId, WorkletThreadPool,
    fetch_and_invoke_a_worklet_script,
};
use crate::dom::workletglobalscope::{
    WorkletGlobalScope, WorkletGlobalScopeInit, WorkletGlobalScopeType,
};
use crate::messaging::{CommonScriptMsg, MainThreadScriptMsg};
use crate::realms::enter_auto_realm;
use crate::runtime::script_runtime::{Runtime, ScriptThreadEventCategory};
use crate::tasks::task_source::TaskSourceName;

// ---------------------------------------------------------------------------------------------
// The global scope
// ---------------------------------------------------------------------------------------------

/// A registered processor class.
/// <https://webaudio.github.io/web-audio-api/#node-name-to-processor-constructor-map>
#[derive(JSTraceable)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct ProcessorDefinition {
    constructor: Heap<JSVal>,
    #[no_trace]
    descriptors: Vec<AudioParamDescriptorData>,
}

/// A live processor and the JS arrays it is handed every quantum (kept, not rebuilt).
#[derive(JSTraceable)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct ProcessorState {
    instance: Heap<JSVal>,
    inputs: Heap<JSVal>,
    outputs: Heap<JSVal>,
    parameters: Heap<JSVal>,
    /// `inputs[n][c]`, `outputs[n][c]`: Float32Array(128) objects.
    input_arrays: Vec<Vec<Box<Heap<*mut JSObject>>>>,
    output_arrays: Vec<Vec<Box<Heap<*mut JSObject>>>>,
    /// Per param: a Float32Array(128) and a Float32Array(1); `parameters[name]` is one of them.
    param_full: Vec<Box<Heap<*mut JSObject>>>,
    param_one: Vec<Box<Heap<*mut JSObject>>>,
    param_names: Vec<String>,
    /// Which of the two arrays `parameters[name]` holds now (None: not set yet).
    param_constant: Vec<Cell<Option<bool>>>,
    dead: Cell<bool>,
}

#[dom_struct]
pub(crate) struct AudioWorkletGlobalScope {
    worklet_global: WorkletGlobalScope,
    sample_rate: f32,
    current_frame: Cell<u64>,
    current_time: Cell<f64>,
    /// <https://webaudio.github.io/web-audio-api/#node-name-to-processor-constructor-map>
    #[ignore_malloc_size_of = "mozjs"]
    processor_definitions: DomRefCell<HashMapTracedValues<String, Box<ProcessorDefinition>>>,
    #[ignore_malloc_size_of = "mozjs"]
    processors: DomRefCell<HashMapTracedValues<u64, Box<ProcessorState>>>,
    /// <https://webaudio.github.io/web-audio-api/#pending-processor-construction-data>
    pending_processor_port: MutNullableDom<MessagePort>,
    /// The global's end of `audioWorklet.port`.
    port: MutNullableDom<MessagePort>,
    /// The BaseAudioContext, to record registered processors on the main thread.
    #[ignore_malloc_size_of = "Trusted"]
    context: Trusted<BaseAudioContext>,
}

impl AudioWorkletGlobalScope {
    #[allow(clippy::too_many_arguments)]
    fn new(
        cx: &mut JSContext,
        pipeline_id: PipelineId,
        base_url: ServoUrl,
        inherited_secure_context: Option<bool>,
        executor: WorkletExecutor,
        init: &WorkletGlobalScopeInit,
        closing: Arc<AtomicBool>,
        sample_rate: f32,
        context: Trusted<BaseAudioContext>,
        cross_origin_isolated: bool,
    ) -> DomRoot<AudioWorkletGlobalScope> {
        debug!("Creating audio worklet global scope for pipeline {}.", pipeline_id);
        let global = Box::new(AudioWorkletGlobalScope {
            worklet_global: WorkletGlobalScope::new_inherited(
                pipeline_id,
                base_url,
                inherited_secure_context,
                executor,
                init,
                closing,
            ),
            sample_rate,
            current_frame: Cell::new(0),
            current_time: Cell::new(0.),
            processor_definitions: Default::default(),
            processors: Default::default(),
            pending_processor_port: Default::default(),
            port: Default::default(),
            context,
        });
        // The worklet is in its window's agent cluster: same cross-origin isolation (so a port
        // can carry SharedArrayBuffers and WebAssembly.Modules both ways).
        global
            .upcast::<GlobalScope>()
            .set_cross_origin_isolated_capability(cross_origin_isolated);
        let scope = AudioWorkletGlobalScopeBinding::Wrap::<crate::DomTypeHolder>(
            cx,
            &global.origin(),
            cross_origin_isolated,
            global,
        );
        let mut realm = enter_auto_realm(cx, &*scope);
        let mut realm = realm.current_realm();
        define_all_exposed_interfaces(&mut realm, scope.upcast());
        // Partii console mode: the embedder's prelude (the WebAssembly stand-in over PartiiWasm) runs
        // before any worklet module, as it does on the page.
        if let Some(js) = crate::dom::partiiwasm::worklet_prelude() {
            let _ = scope.upcast::<GlobalScope>().evaluate_js_on_global(
                &mut realm,
                js.into(),
                "partii-worklet-prelude.js",
                None,
                None,
            );
        }
        scope
    }

    fn set_time(&self, info: &BlockInfo) {
        self.current_frame.set(info.frame.0);
        self.current_time.set(info.time);
    }

    /// Take the port for the processor being constructed (its `super()` call).
    pub(crate) fn take_pending_processor_port(&self) -> Option<DomRoot<MessagePort>> {
        self.pending_processor_port.take()
    }

    fn queue_main_thread_task(&self, task: impl crate::tasks::task::TaskOnce + 'static) {
        let msg = CommonScriptMsg::Task(
            ScriptThreadEventCategory::WorkletEvent,
            Box::new(task),
            None,
            TaskSourceName::Networking,
        );
        let _ = self
            .worklet_global
            .to_script_thread_sender()
            .send(MainThreadScriptMsg::Common(msg));
    }

    /// <https://webaudio.github.io/web-audio-api/#AudioWorkletProcessor-instantiation>
    /// Runs on the render thread, from the node's `create_node`.
    #[expect(unsafe_code)]
    fn construct_processor(
        &self,
        cx: &mut JSContext,
        id: u64,
        name: &str,
        options: StructuredSerializedData,
        number_of_inputs: usize,
        number_of_outputs: usize,
    ) {
        let mut realm = enter_auto_realm(cx, self);
        let cx = &mut realm.current_realm();
        let global = self.upcast::<GlobalScope>();

        rooted!(&in(cx) let mut ctor = UndefinedValue());
        let param_names: Vec<String> = match self.processor_definitions.borrow().get(name) {
            Some(definition) => {
                ctor.set(definition.constructor.get());
                definition.descriptors.iter().map(|d| d.name.clone()).collect()
            },
            None => {
                self.processor_error(id);
                return;
            },
        };

        // Step 3-4. Deserialize the options; the node's port comes back as the one transfer.
        // Platform objects that AudioWorkletGlobalScope doesn't expose (a Blob, an ImageBitmap...)
        // can't be deserialized here: that is a processor error.
        if has_objects_unexposed_in_audio_worklet(&options) {
            self.processor_error(id);
            return;
        }
        rooted!(&in(cx) let mut options_value = UndefinedValue());
        let ports = match structuredclone::read(cx, global, options, options_value.handle_mut()) {
            Ok(ports) => ports,
            Err(_) => {
                self.processor_error(id);
                return;
            },
        };
        // Step 5. Set the pending processor construction data.
        if let Some(port) = ports.into_iter().next() {
            self.pending_processor_port.set(Some(&port));
        }

        // Step 6. Construct.
        rooted!(&in(cx) let mut instance = null_mut::<JSObject>());
        let ok = script_bindings::settings_stack::run_a_script::<crate::DomTypeHolder, _, _>(
            cx,
            global,
            |cx| unsafe {
                let args = [options_value.get()];
                let args = value_array(&args);
                Construct1(cx, ctor.handle(), &args, instance.handle_mut()) &&
                    !JS_IsExceptionPending(cx)
            },
        );
        self.pending_processor_port.set(None);
        if !ok || instance.get().is_null() {
            let details = report_pending_exception_with_info(cx);
            self.processor_error_with(id, details);
            return;
        }

        let state = Box::new(ProcessorState {
            instance: Heap::default(),
            inputs: Heap::default(),
            outputs: Heap::default(),
            parameters: Heap::default(),
            input_arrays: (0..number_of_inputs).map(|_| Vec::new()).collect(),
            output_arrays: (0..number_of_outputs).map(|_| Vec::new()).collect(),
            param_full: Vec::new(),
            param_one: Vec::new(),
            param_constant: param_names.iter().map(|_| Cell::new(None)).collect(),
            param_names,
            dead: Cell::new(false),
        });
        state.instance.set(ObjectValue(instance.get()));
        // Into the traced map first, so the arrays built next are rooted from the start.
        self.processors.borrow_mut().insert(id, state);
        let built = {
            let mut processors = self.processors.borrow_mut();
            let state = processors.0.get_mut(&id).expect("just inserted");
            build_processor_arrays(cx, state)
        };
        if built.is_err() {
            self.processors.borrow_mut().0.remove(&id);
            report_pending_exception(cx);
            self.processor_error(id);
        }
    }

    /// <https://webaudio.github.io/web-audio-api/#rendering-loop>, step "process an AudioWorkletNode".
    #[expect(unsafe_code)]
    fn process(
        &self,
        cx: &mut JSContext,
        id: u64,
        inputs: &[Block],
        outputs: &mut [Block],
        params: &[&[f32]],
        info: &BlockInfo,
    ) -> bool {
        self.set_time(info);
        PROCESSED_THIS_QUANTUM.with(|p| p.set(true));
        let mut realm = enter_auto_realm(cx, self);
        let cx = &mut realm.current_realm();
        let global = self.upcast::<GlobalScope>();

        rooted!(&in(cx) let mut instance = UndefinedValue());
        rooted!(&in(cx) let mut args_inputs = UndefinedValue());
        rooted!(&in(cx) let mut args_outputs = UndefinedValue());
        rooted!(&in(cx) let mut args_parameters = UndefinedValue());

        // Phase 1 (no page JS runs): shape the arrays, copy inputs and params in.
        {
            let mut processors = self.processors.borrow_mut();
            let Some(state) = processors.0.get_mut(&id) else {
                return false;
            };
            if state.dead.get() {
                return false;
            }
            let input_channels: Vec<usize> = inputs
                .iter()
                .map(|b| if b.is_silence() { 0 } else { b.chan_count() as usize })
                .collect();
            let output_channels: Vec<usize> =
                outputs.iter().map(|b| b.chan_count() as usize).collect();
            let shape_changed = state
                .input_arrays
                .iter()
                .map(|v| v.len())
                .ne(input_channels.iter().copied()) ||
                state
                    .output_arrays
                    .iter()
                    .map(|v| v.len())
                    .ne(output_channels.iter().copied());
            // The page may transfer a channel's buffer away (it can't transfer the frozen arrays
            // themselves): hand it fresh arrays next time.
            let detached = state
                .input_arrays
                .iter()
                .chain(state.output_arrays.iter())
                .flatten()
                .any(|array| is_detached(array.get()));
            if (shape_changed || detached) &&
                reshape_arrays(cx, state, &input_channels, &output_channels).is_err()
            {
                drop(processors);
                self.fail(id);
                return false;
            }

            for (n, block) in inputs.iter().enumerate() {
                for (c, array) in state.input_arrays[n].iter().enumerate() {
                    write_f32(array.get(), block.data_chan(c as u8));
                }
            }
            // The spec hands process() silent output arrays.
            for arrays in &state.output_arrays {
                for array in arrays {
                    fill_f32(array.get(), 0.);
                }
            }
            for (p, values) in params.iter().enumerate().take(state.param_names.len()) {
                let constant = values.len() == 1;
                let array = if constant {
                    state.param_one[p].get()
                } else {
                    state.param_full[p].get()
                };
                write_f32(array, values);
                if state.param_constant[p].get() != Some(constant) {
                    state.param_constant[p].set(Some(constant));
                    rooted!(&in(cx) let parameters = state.parameters.get().to_object());
                    rooted!(&in(cx) let value = ObjectValue(array));
                    let name = std::ffi::CString::new(state.param_names[p].as_str())
                        .unwrap_or_default();
                    // A plain data property on a plain object: runs no page JS.
                    unsafe {
                        JS_DefineProperty(
                            cx,
                            parameters.handle(),
                            name.as_ptr(),
                            value.handle(),
                            js::jsapi::JSPROP_ENUMERATE as u32,
                        );
                    }
                }
            }
            instance.set(state.instance.get());
            args_inputs.set(state.inputs.get());
            args_outputs.set(state.outputs.get());
            args_parameters.set(state.parameters.get());
        }

        // Phase 2: call process(inputs, outputs, parameters). No borrow is held: page JS runs.
        rooted!(&in(cx) let instance_obj = instance.to_object());
        rooted!(&in(cx) let mut process = UndefinedValue());
        rooted!(&in(cx) let mut rval = UndefinedValue());
        // The outer callback entry keeps the settings stack non-empty, so run_a_script's
        // "clean up after running script" doesn't do a microtask checkpoint after each processor;
        // the checkpoint comes once per render quantum (on_render_quantum_end).
        let ok = script_bindings::settings_stack::run_a_callback::<crate::DomTypeHolder, _>(global, || {
          script_bindings::settings_stack::run_a_script::<crate::DomTypeHolder, _, _>(
            cx,
            global,
            |cx| {
                if get_property_jsval(cx, instance_obj.handle(), c"process", process.handle_mut())
                    .is_err()
                {
                    return false;
                }
                if !process.is_object() || unsafe { !IsCallable(process.to_object()) } {
                    js::error::throw_type_error(
                        cx,
                        c"AudioWorkletProcessor: process is not callable",
                    );
                    return false;
                }
                // Each value is rooted above for the whole call.
                let args = [args_inputs.get(), args_outputs.get(), args_parameters.get()];
                unsafe {
                    Call(
                        cx,
                        instance.handle(),
                        process.handle(),
                        &value_array(&args),
                        rval.handle_mut(),
                    ) && !JS_IsExceptionPending(cx)
                }
            },
          )
        });
        if !ok {
            let details = report_pending_exception_with_info(cx);
            self.fail_with(id, details);
            return false;
        }

        // Phase 3: copy the outputs out.
        {
            let processors = self.processors.borrow();
            let Some(state) = processors.0.get(&id) else {
                return false;
            };
            for (n, block) in outputs.iter_mut().enumerate() {
                for (c, array) in state.output_arrays[n].iter().enumerate() {
                    read_f32(array.get(), block.data_chan_mut(c as u8));
                }
            }
        }
        unsafe { js::rust::ToBoolean(rval.handle()) }
    }

    fn fail(&self, id: u64) {
        self.fail_with(id, None);
    }

    fn fail_with(&self, id: u64, details: Option<(String, String, u32, u32)>) {
        {
            let processors = self.processors.borrow();
            let Some(state) = processors.get(&id) else {
                return;
            };
            state.dead.set(true);
        }
        self.processor_error_with(id, details);
    }

    /// Queue a task to fire `processorerror` at the node of processor `id`.
    /// <https://webaudio.github.io/web-audio-api/#dom-audioworkletnode-onprocessorerror>
    fn processor_error(&self, id: u64) {
        self.processor_error_with(id, None);
    }

    /// The same, with the exception's (message, filename, line, column) for the ErrorEvent.
    /// The context holds the node weakly (a strong reference from here would keep the node,
    /// and so its engine and this processor, alive forever): a node the page has dropped and
    /// the GC has collected gets no event, which nothing could observe.
    fn processor_error_with(&self, id: u64, details: Option<(String, String, u32, u32)>) {
        let context = self.context.clone();
        self.queue_main_thread_task(task!(audio_worklet_processor_error: move |cx| {
            if let Some(node) = context.root().audio_worklet_node(id) {
                node.fire_processor_error(cx, details);
            }
        }));
    }
}

/// A HandleValueArray over `values`. They must stay rooted (here: held by traced Heaps or
/// a Rooted) for as long as the array is used.
fn value_array(values: &[JSVal]) -> HandleValueArray {
    HandleValueArray {
        length_: values.len(),
        elements_: values.as_ptr(),
    }
}

/// Create the `inputs`/`outputs` arrays (empty channel lists) and the `parameters` object.
/// `state` must already be in the traced map.
#[expect(unsafe_code)]
fn build_processor_arrays(cx: &mut JSContext, state: &mut ProcessorState) -> Result<(), ()> {
    unsafe {
        rooted!(&in(cx) let parameters = JS_NewPlainObject(cx));
        if parameters.get().is_null() {
            return Err(());
        }
        state.parameters.set(ObjectValue(parameters.get()));
        for _ in 0..state.param_names.len() {
            rooted!(&in(cx) let mut a = null_mut::<JSObject>());
            TypedArray::<Float32, *mut JSObject>::create(
                cx,
                CreateWith::Length(FRAMES_PER_BLOCK_USIZE),
                a.handle_mut(),
            )?;
            rooted!(&in(cx) let mut b = null_mut::<JSObject>());
            TypedArray::<Float32, *mut JSObject>::create(cx, CreateWith::Length(1), b.handle_mut())?;
            state.param_full.push(Heap::boxed(a.get()));
            state.param_one.push(Heap::boxed(b.get()));
        }
    }
    let ins = vec![0; state.input_arrays.len()];
    let outs = vec![0; state.output_arrays.len()];
    reshape_arrays(cx, state, &ins, &outs)
}

/// (Re)build `inputs` and `outputs` for these channel counts.
#[expect(unsafe_code)]
fn reshape_arrays(
    cx: &mut JSContext,
    state: &mut ProcessorState,
    input_channels: &[usize],
    output_channels: &[usize],
) -> Result<(), ()> {
    fn build(
        cx: &mut JSContext,
        counts: &[usize],
        arrays: &mut Vec<Vec<Box<Heap<*mut JSObject>>>>,
        out: &Heap<JSVal>,
    ) -> Result<(), ()> {
        unsafe {
            rooted!(&in(cx) let outer = NewArrayObject(cx, &HandleValueArray::empty()));
            if outer.get().is_null() {
                return Err(());
            }
            arrays.clear();
            for (n, &channels) in counts.iter().enumerate() {
                rooted!(&in(cx) let inner = NewArrayObject(cx, &HandleValueArray::empty()));
                if inner.get().is_null() {
                    return Err(());
                }
                let mut list = Vec::with_capacity(channels);
                for c in 0..channels {
                    rooted!(&in(cx) let mut a = null_mut::<JSObject>());
                    TypedArray::<Float32, *mut JSObject>::create(
                        cx,
                        CreateWith::Length(FRAMES_PER_BLOCK_USIZE),
                        a.handle_mut(),
                    )?;
                    rooted!(&in(cx) let v = ObjectValue(a.get()));
                    if !JS_SetElement(cx, inner.handle(), c as u32, v.handle()) {
                        return Err(());
                    }
                    list.push(Heap::boxed(a.get()));
                }
                // inputs[n] and outputs[n] are FrozenArrays.
                if !js::rust::wrappers2::JS_FreezeObject(cx, inner.handle()) {
                    return Err(());
                }
                rooted!(&in(cx) let v = ObjectValue(inner.get()));
                if !JS_SetElement(cx, outer.handle(), n as u32, v.handle()) {
                    return Err(());
                }
                arrays.push(list);
            }
            if !js::rust::wrappers2::JS_FreezeObject(cx, outer.handle()) {
                return Err(());
            }
            out.set(ObjectValue(outer.get()));
            Ok(())
        }
    }
    build(cx, input_channels, &mut state.input_arrays, &state.inputs)?;
    build(cx, output_channels, &mut state.output_arrays, &state.outputs)
}

/// Whether serialized data holds platform objects that aren't exposed in AudioWorkletGlobalScope
/// (MessagePort, DOMException and the streams are; Blob, File, geometry, image and canvas types,
/// CryptoKey aren't).
fn has_objects_unexposed_in_audio_worklet(data: &StructuredSerializedData) -> bool {
    data.blobs.is_some() ||
        data.files.is_some() ||
        data.file_lists.is_some() ||
        data.points.is_some() ||
        data.rects.is_some() ||
        data.quads.is_some() ||
        data.matrices.is_some() ||
        data.image_bitmaps.is_some() ||
        data.transferred_image_bitmaps.is_some() ||
        data.offscreen_canvases.is_some() ||
        data.image_data.is_some() ||
        data.crypto_keys.is_some()
}

/// Whether a Float32Array we handed out has lost its buffer (been transferred).
#[expect(unsafe_code)]
fn is_detached(array: *mut JSObject) -> bool {
    match TypedArray::<Float32, *mut JSObject>::from(array) {
        #[allow(deprecated)]
        Ok(mut array) => unsafe { array.as_mut_slice() }.is_none_or(|s| s.is_empty()),
        Err(_) => true,
    }
}

/// Copy `data` into a Float32Array (no-op if it was detached or resized by the page).
#[expect(unsafe_code)]
fn write_f32(array: *mut JSObject, data: &[f32]) {
    if let Ok(mut array) = TypedArray::<Float32, *mut JSObject>::from(array) {
        #[allow(deprecated)]
        if let Some(slice) = unsafe { array.as_mut_slice() } {
            let n = slice.len().min(data.len());
            slice[..n].copy_from_slice(&data[..n]);
        }
    }
}

#[expect(unsafe_code)]
fn fill_f32(array: *mut JSObject, value: f32) {
    if let Ok(mut array) = TypedArray::<Float32, *mut JSObject>::from(array) {
        #[allow(deprecated)]
        if let Some(slice) = unsafe { array.as_mut_slice() } {
            slice.fill(value);
        }
    }
}

#[expect(unsafe_code)]
fn read_f32(array: *mut JSObject, out: &mut [f32]) {
    if let Ok(mut array) = TypedArray::<Float32, *mut JSObject>::from(array) {
        #[allow(deprecated)]
        if let Some(slice) = unsafe { array.as_mut_slice() } {
            let n = slice.len().min(out.len());
            out[..n].copy_from_slice(&slice[..n]);
        }
    }
}

impl AudioWorkletGlobalScopeMethods<crate::DomTypeHolder> for AudioWorkletGlobalScope {
    /// <https://webaudio.github.io/web-audio-api/#dom-audioworkletglobalscope-registerprocessor>
    #[expect(unsafe_code)]
    fn RegisterProcessor(
        &self,
        cx: &mut JSContext,
        name: DOMString,
        processor_ctor: RootedCallback<AudioWorkletProcessorConstructor>,
    ) -> Fallible<()> {
        let name = String::from(name);
        rooted!(&in(cx) let ctor_obj = processor_ctor.callback_holder().get());

        // Step 1. If name is an empty string, throw a NotSupportedError.
        if name.is_empty() {
            return Err(Error::NotSupported(None));
        }
        // Step 2. If name already exists, throw a NotSupportedError.
        if self.processor_definitions.borrow().contains_key(&name) {
            return Err(Error::NotSupported(None));
        }
        // Step 3. If IsConstructor(processorCtor) is false, throw a TypeError.
        if unsafe { !IsConstructor(ctor_obj.get()) } {
            return Err(Error::Type(c"registerProcessor: not a constructor".to_owned()));
        }
        // Step 4-5. prototype must be an object.
        rooted!(&in(cx) let mut prototype = UndefinedValue());
        get_property_jsval(cx, ctor_obj.handle(), c"prototype", prototype.handle_mut())?;
        if !prototype.is_object() {
            return Err(Error::Type(c"registerProcessor: prototype is not an object".to_owned()));
        }
        // Step 6. parameterDescriptors: a sequence<AudioParamDescriptor>, if present.
        let descriptors: Vec<AudioParamDescriptor> =
            get_property(cx, ctor_obj.handle(), c"parameterDescriptors", ())?.unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        let mut data = Vec::with_capacity(descriptors.len());
        for d in &descriptors {
            let param_name = String::from(d.name.clone());
            // Step 7.1-7.3: unique names, minValue <= defaultValue <= maxValue.
            if !seen.insert(param_name.clone()) {
                return Err(Error::NotSupported(None));
            }
            let (default_value, min_value, max_value) = (*d.defaultValue, *d.minValue, *d.maxValue);
            if !(min_value <= default_value && default_value <= max_value) {
                return Err(Error::InvalidState(None));
            }
            data.push(AudioParamDescriptorData {
                name: param_name,
                default_value,
                min_value,
                max_value,
                a_rate: d.automationRate == AutomationRate::A_rate,
            });
        }

        // Step 8. Append the key-value pair name → processorCtor.
        let definition = Box::new(ProcessorDefinition {
            constructor: Heap::default(),
            descriptors: data.clone(),
        });
        definition.constructor.set(ObjectValue(ctor_obj.get()));
        self.processor_definitions
            .borrow_mut()
            .insert(name.clone(), definition);

        // Step 9. Queue a media element task to append name → descriptors to the context's
        // node name to parameter descriptor map. Queued before addModule() resolves.
        let context = self.context.clone();
        self.queue_main_thread_task(task!(audio_worklet_register_processor: move |_cx| {
            context.root().register_audio_worklet_processor(name, data);
        }));
        Ok(())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioworkletglobalscope-currentframe>
    fn CurrentFrame(&self) -> u64 {
        self.current_frame.get()
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioworkletglobalscope-currenttime>
    fn CurrentTime(&self) -> crate::dom::bindings::num::Finite<f64> {
        crate::dom::bindings::num::Finite::wrap(self.current_time.get())
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioworkletglobalscope-samplerate>
    fn SampleRate(&self) -> crate::dom::bindings::num::Finite<f32> {
        crate::dom::bindings::num::Finite::wrap(self.sample_rate)
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-audioworkletglobalscope-port>
    fn Port(&self, cx: &mut JSContext) -> DomRoot<MessagePort> {
        self.port.or_init(|| {
            // Only if the AudioWorklet's end couldn't be transferred: an unentangled port.
            let port = MessagePort::new(cx, self.upcast());
            self.upcast::<GlobalScope>().track_message_port(&port, None);
            port
        })
    }
}

impl HasOrigin for AudioWorkletGlobalScope {
    fn origin(&self) -> MutableOrigin {
        self.upcast::<WorkletGlobalScope>().origin()
    }
}

// ---------------------------------------------------------------------------------------------
// The render-thread host
// ---------------------------------------------------------------------------------------------

#[derive(JSTraceable)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct HostRoots {
    global: Dom<AudioWorkletGlobalScope>,
}

/// What lives on the render thread: the JS runtime and the one global.
struct AudioWorkletHost {
    // Field order is drop order: the roots go before the runtime.
    roots: RootedTraceableBox<HostRoots>,
    closing: Arc<AtomicBool>,
    runtime: Runtime,
}

/// The render thread's host slot. Normally emptied by the render thread's exit hook. If it
/// still holds a host when thread-locals are destroyed (the thread ended some other way), the
/// host is leaked: dropping a SpiderMonkey runtime then would run after mozjs's own thread-locals
/// are gone and abort the process.
struct HostSlot(RefCell<Option<AudioWorkletHost>>);

impl Drop for HostSlot {
    fn drop(&mut self) {
        if let Some(host) = self.0.get_mut().take() {
            warn!("AudioWorklet host still alive at thread exit; leaking it.");
            std::mem::forget(host);
        }
    }
}

impl std::ops::Deref for HostSlot {
    type Target = RefCell<Option<AudioWorkletHost>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

thread_local! {
    /// Set when a processor ran in the current render quantum.
    static PROCESSED_THIS_QUANTUM: Cell<bool> = const { Cell::new(false) };
    static HOST: HostSlot = const { HostSlot(RefCell::new(None)) };
}

/// Run `f` with the render thread's global, if the host exists.
#[expect(unsafe_code)]
fn with_host<R>(f: impl FnOnce(&mut JSContext, &AudioWorkletGlobalScope) -> R) -> Option<R> {
    HOST.with(|host| {
        let host = host.borrow();
        let host = host.as_ref()?;
        if host.closing.load(Ordering::SeqCst) {
            return None;
        }
        let global = DomRoot::from_ref(&*host.roots.global);
        let mut cx = unsafe { host.runtime.cx() };
        Some(f(&mut cx, &global))
    })
}

/// Run a script task for the global, then a microtask checkpoint.
pub(crate) fn run_task_in_host(task: impl FnOnce(&mut JSContext, &AudioWorkletGlobalScope)) {
    with_host(|cx, global| {
        task(cx, global);
        global.upcast::<WorkletGlobalScope>().perform_a_microtask_checkpoint(cx);
    });
}

/// Everything needed to create the host, sent once to the render thread.
struct HostInit {
    pipeline_id: PipelineId,
    base_url: ServoUrl,
    inherited_secure_context: Option<bool>,
    init: WorkletGlobalScopeInit,
    sample_rate: f32,
    context: Trusted<BaseAudioContext>,
    sender: RenderTaskSender,
    worklet_id: WorkletId,
    global_port: Option<StructuredSerializedData>,
    cross_origin_isolated: bool,
}

#[expect(unsafe_code)]
fn ensure_host(init: HostInit) {
    let exists = HOST.with(|host| host.borrow().is_some());
    if exists {
        return;
    }
    debug!("Creating the AudioWorklet host on the render thread.");
    thread_state::initialize(ThreadState::SCRIPT | ThreadState::IN_WORKER);
    // Message ports and other ids are allocated on this thread.
    servo_base::id::PipelineNamespace::auto_install();
    let executor = WorkletExecutor::new_audio(init.worklet_id, init.sender.clone());
    let runtime = Runtime::new(Some(executor.event_loop_sender()));
    let mut cx = unsafe { runtime.cx() };
    let closing = Arc::new(AtomicBool::new(false));
    let global = AudioWorkletGlobalScope::new(
        &mut cx,
        init.pipeline_id,
        init.base_url,
        init.inherited_secure_context,
        executor,
        &init.init,
        closing.clone(),
        init.sample_rate,
        init.context,
        init.cross_origin_isolated,
    );
    // AudioWorkletGlobalScope.port: the transferred end of AudioWorklet.port, in place before
    // any module runs.
    if let Some(data) = init.global_port {
        rooted!(&in(&mut cx) let mut value = UndefinedValue());
        if let Ok(ports) = structuredclone::read(&mut cx, global.upcast(), data, value.handle_mut()) {
            if let Some(port) = ports.into_iter().next() {
                global.port.set(Some(&port));
            }
        }
    }
    let roots = RootedTraceableBox::new(HostRoots {
        global: Dom::from_ref(&*global),
    });
    drop(global);
    HOST.with(|host| {
        *host.borrow_mut() = Some(AudioWorkletHost {
            roots,
            closing,
            runtime,
        })
    });
    // Collect garbage while the sink is full, not inside process() (see `idle_gc`).
    unsafe {
        let cx = host_cx_for_params();
        // An eager nursery collection becomes due at this size (see IdleGc).
        js::jsapi::JS_SetGCParameter(
            cx,
            js::jsapi::JSGCParamKey::JSGC_NURSERY_EAGER_COLLECTION_THRESHOLD_KB,
            IDLE_NURSERY_THRESHOLD_KB,
        );
    }
    // <https://webaudio.github.io/web-audio-api/#rendering-loop>: perform a microtask checkpoint
    // after the render quantum (only if a processor ran, so idle contexts pay nothing).
    // Between quanta, currentFrame/currentTime are the context's (the next quantum's start).
    on_render_quantum_end(Box::new(|info| {
        let processed = PROCESSED_THIS_QUANTUM.with(|p| p.replace(false));
        with_host(|cx, global| {
            global.set_time(info);
            if processed {
                global.upcast::<WorkletGlobalScope>().perform_a_microtask_checkpoint(cx)
            }
        });
    }));
    let mut gc_policy = IdleGc::new();
    on_render_thread_idle(Box::new(move || gc_policy.run()));
    // Tear down in order when the context closes and the render loop ends.
    on_render_thread_exit(Box::new(|| {
        HOST.with(|host| {
            if let Some(host) = host.borrow_mut().take() {
                host.closing.store(true, Ordering::SeqCst);
                {
                    let global = &host.roots.global;
                    global.processors.borrow_mut().0.clear();
                    global.processor_definitions.borrow_mut().0.clear();
                }
                drop(host);
            }
        });
    }));
}

/// The raw JSContext of the render thread's runtime (only for setting GC parameters).
#[expect(unsafe_code)]
unsafe fn host_cx_for_params() -> *mut js::jsapi::JSContext {
    js::rust::Runtime::get().expect("worklet runtime").as_ptr()
}

/// The worklet runtime's GC policy. Servo keeps SpiderMonkey's incremental GC off (pre-barriers
/// aren't implemented correctly), so a collection is one pause. Left alone, SpiderMonkey starts
/// one when an allocation crosses its trigger, which happens inside `process()`, so the pause
/// lands in a render quantum. Instead, whenever the render thread is about to wait with the sink
/// full, collect if the heap has grown by half since the last collection (and is past a small
/// floor). The pause is then covered by the buffered audio, and SpiderMonkey's own triggers
/// (much higher) are rarely reached.
struct IdleGc {
    /// GC heap bytes after the last collection.
    after_last: u32,
    stats: bool,
    /// PARTII_AUDIOWORKLET_IDLE_GC=0 turns the policy off (for A/B measurements).
    enabled: bool,
}

/// Don't bother below this (a fresh worklet heap is around 1 MB).
const IDLE_GC_FLOOR_BYTES: u32 = 8 * 1024 * 1024;

/// Collect the nursery in idle time once it holds this much, so minor GCs (cheap, but not free:
/// their cost is the surviving objects) rarely land inside `process()`.
const IDLE_NURSERY_THRESHOLD_KB: u32 = 64;

impl IdleGc {
    fn new() -> IdleGc {
        IdleGc {
            after_last: 0,
            stats: std::env::var_os("PARTII_AUDIO_STATS").is_some_and(|v| v != "0"),
            enabled: std::env::var("PARTII_AUDIOWORKLET_IDLE_GC").map_or(true, |v| v != "0"),
        }
    }

    #[expect(unsafe_code)]
    fn run(&mut self) {
        if !self.enabled {
            return;
        }
        HOST.with(|host| {
            let Ok(host) = host.try_borrow() else {
                return;
            };
            let Some(host) = host.as_ref() else {
                return;
            };
            if host.closing.load(Ordering::SeqCst) {
                return;
            }
            // Minor GC first: empty the nursery if it holds more than the eager threshold.
            let started = std::time::Instant::now();
            unsafe {
                let rt = js::jsapi::JS_GetRuntime(host_cx_for_params());
                js::jsapi::JS::MaybeRunNurseryCollection(
                    rt,
                    js::jsapi::GCReason::EAGER_NURSERY_COLLECTION,
                );
            }
            servo_media::audio::render_stats::note_idle_work(started.elapsed());
            let bytes = unsafe {
                js::rust::wrappers2::JS_GetGCParameter(
                    host.runtime.cx_no_gc(),
                    js::jsapi::JSGCParamKey::JSGC_BYTES,
                )
            };
            let trigger = IDLE_GC_FLOOR_BYTES.max(self.after_last.saturating_mul(2));
            if bytes < trigger {
                return;
            }
            let started = std::time::Instant::now();
            let mut cx = unsafe { host.runtime.cx() };
            unsafe { js::rust::wrappers2::JS_GC(&mut cx, js::jsapi::GCReason::API) };
            self.after_last = unsafe {
                js::rust::wrappers2::JS_GetGCParameter(
                    host.runtime.cx_no_gc(),
                    js::jsapi::JSGCParamKey::JSGC_BYTES,
                )
            };
            let took = started.elapsed();
            servo_media::audio::render_stats::note_idle_work(took);
            if self.stats {
                eprintln!(
                    "[audioworklet-gc] idle GC {:.2}ms, {} -> {} KB",
                    took.as_secs_f64() * 1000.,
                    bytes / 1024,
                    self.after_last / 1024
                );
            }
        });
    }
}

// ---------------------------------------------------------------------------------------------
// The "thread pool": one global, on the render thread
// ---------------------------------------------------------------------------------------------

#[derive(JSTraceable)]
pub(crate) struct AudioWorkletThreadPool {
    #[no_trace]
    sender: RenderTaskSender,
    #[no_trace]
    init: WorkletGlobalScopeInit,
    sample_rate: f32,
    context: Trusted<BaseAudioContext>,
    /// AudioWorklet.port's partner, serialized for transfer; taken by the first addModule().
    #[no_trace]
    global_port: RefCell<Option<StructuredSerializedData>>,
    /// The window's cross-origin isolated capability, inherited by the worklet global.
    cross_origin_isolated: bool,
}

impl AudioWorkletThreadPool {
    pub(crate) fn new(
        context: &BaseAudioContext,
        init: WorkletGlobalScopeInit,
        global_port: Option<StructuredSerializedData>,
    ) -> AudioWorkletThreadPool {
        AudioWorkletThreadPool {
            global_port: RefCell::new(global_port),
            cross_origin_isolated: context.global().cross_origin_isolated_capability(),
            sender: context.audio_context_impl().lock().unwrap().render_task_sender(),
            init,
            sample_rate: *context.SampleRate(),
            context: Trusted::new(context),
        }
    }
}

impl WorkletThreadPool for AudioWorkletThreadPool {
    fn number_of_global_scopes(&self) -> isize {
        1
    }

    fn fetch_and_invoke_a_worklet_script(
        &self,
        pipeline_id: PipelineId,
        worklet_id: WorkletId,
        _global_type: WorkletGlobalScopeType,
        origin: ImmutableOrigin,
        base_url: ServoUrl,
        script_url: ServoUrl,
        policy_container: PolicyContainer,
        credentials: RequestCredentials,
        pending_tasks_struct: PendingTasksStruct,
        promise: &RootedPromise,
        inherited_secure_context: Option<bool>,
    ) {
        let host_init = HostInit {
            pipeline_id,
            base_url,
            inherited_secure_context,
            init: self.init.clone(),
            sample_rate: self.sample_rate,
            context: self.context.clone(),
            sender: self.sender.clone(),
            worklet_id,
            global_port: self.global_port.borrow_mut().take(),
            cross_origin_isolated: self.cross_origin_isolated,
        };
        let promise = TrustedPromise::from(promise);
        let script_thread_sender = self.init.to_script_thread_sender.clone();
        let task = Box::new(move || {
            ensure_host(host_init);
            run_task_in_host(move |cx, global| {
                fetch_and_invoke_a_worklet_script(
                    cx,
                    global.upcast(),
                    pipeline_id,
                    origin,
                    script_url,
                    policy_container,
                    credentials,
                    pending_tasks_struct,
                    promise,
                    script_thread_sender,
                    Destination::AudioWorklet,
                );
            });
        });
        if self.sender.send(task).is_err() {
            warn!("AudioWorklet addModule after the context closed.");
        }
    }

    fn exit_worklet(&self, _worklet_id: WorkletId) {
        // The global lives as long as the context's render thread; it goes when the context closes.
    }

    fn wake_threads(&self) {}

    fn perform_a_worklet_task(
        &self,
        _worklet_id: WorkletId,
        worklet_task: Box<dyn FnOnce(&mut JSContext, &WorkletGlobalScope) + Send>,
    ) {
        let _ = self.sender.send(Box::new(move || {
            run_task_in_host(|cx, global| worklet_task(cx, global.upcast()));
        }));
    }
}

// ---------------------------------------------------------------------------------------------
// The processor handle given to servo-media
// ---------------------------------------------------------------------------------------------

static NEXT_PROCESSOR_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct ProcessorHandle {
    id: u64,
    name: String,
    options: Option<StructuredSerializedData>,
    number_of_inputs: usize,
    number_of_outputs: usize,
}

impl ProcessorHandle {
    pub(crate) fn new(
        name: String,
        options: StructuredSerializedData,
        number_of_inputs: usize,
        number_of_outputs: usize,
    ) -> ProcessorHandle {
        ProcessorHandle {
            id: NEXT_PROCESSOR_ID.fetch_add(1, Ordering::Relaxed),
            name,
            options: Some(options),
            number_of_inputs,
            number_of_outputs,
        }
    }

    /// The processor's id: the key of its node in the context's weak map (for `processorerror`).
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
}

impl AudioWorkletProcessorHandle for ProcessorHandle {
    fn construct(&mut self, info: &BlockInfo) {
        let Some(options) = self.options.take() else {
            return;
        };
        let id = self.id;
        let (inputs, outputs) = (self.number_of_inputs, self.number_of_outputs);
        let name = std::mem::take(&mut self.name);
        run_task_in_host(|cx, global| {
            global.set_time(info);
            global.construct_processor(cx, id, &name, options, inputs, outputs);
        });
    }

    fn process(
        &mut self,
        inputs: &[Block],
        outputs: &mut [Block],
        params: &[&[f32]],
        info: &BlockInfo,
    ) -> bool {
        let id = self.id;
        // The microtask checkpoint comes once the whole quantum is rendered
        // (`on_render_quantum_end` in ensure_host), not after each processor.
        let result = with_host(|cx, global| global.process(cx, id, inputs, outputs, params, info));
        result.unwrap_or(false)
    }
}

impl Drop for ProcessorHandle {
    fn drop(&mut self) {
        let id = self.id;
        // On the render thread (graph teardown or node removal): forget the processor, if the
        // host is still there.
        let _ = HOST.try_with(|host| {
            if let Ok(host) = host.try_borrow() {
                if let Some(host) = host.as_ref() {
                    host.roots.global.processors.borrow_mut().remove(&id);
                }
            }
        });
    }
}

#[allow(dead_code)]
fn _assert_send() {
    fn is_send<T: Send>() {}
    is_send::<ProcessorHandle>();
}
