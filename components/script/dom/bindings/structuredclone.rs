/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! This module implements structured cloning, as defined by [HTML](https://html.spec.whatwg.org/multipage/#safe-passing-of-structured-data).

#![cfg_attr(crown, allow(crown::jscontext_first_arg))]

use std::ffi::CStr;
use std::os::raw;
use std::ptr::{self, NonNull};

use js::context::{JSContext, NoGC};
use js::conversions::ToJSValConvertible;
use js::gc::RootedVec;
use js::glue::{
    CopyJSStructuredCloneData, GetLengthOfJSStructuredCloneData, WriteBytesToJSStructuredCloneData,
};
use js::jsapi::{
    CloneDataPolicy, HandleObject as RawHandleObject, Heap, JS_ReadUint32Pair,
    JS_STRUCTURED_CLONE_VERSION, JS_WriteUint32PairUnchecked, JSContext as RawJSContext, JSObject,
    JSStructuredCloneCallbacks, JSStructuredCloneReader, JSStructuredCloneWriter,
    MutableHandleObject as RawMutableHandleObject, StructuredCloneScope, TransferableOwnership,
};
use js::jsval::UndefinedValue;
use js::realm::CurrentRealm;
use js::rust::wrappers2::{JS_IsExceptionPending, JS_ReadStructuredClone, JS_WriteStructuredClone};
use js::rust::{
    CustomAutoRooterGuard, HandleValue, JSAutoStructuredCloneBufferWrapper, MutableHandleValue,
};
use rustc_hash::FxHashMap;
use script_bindings::conversions::IDLInterface;
use servo_base::id::{
    BlobId, CryptoKeyId, DomExceptionId, DomMatrixId, DomPointId, DomQuadId, DomRectId, FileId,
    FileListId, ImageBitmapId, ImageDataId, Index, MessagePortId, NamespaceIndex,
    OffscreenCanvasId, PipelineNamespaceId, QuotaExceededErrorId,
};
use servo_constellation_traits::{
    AgentClusterKey, BlobImpl, DomException, DomMatrix, DomPoint, DomQuad, DomRect,
    InProcessCloneData, MessagePortImpl, Serializable as SerializableInterface,
    SerializableCryptoKey, SerializableFile, SerializableFileList, SerializableImageBitmap,
    SerializableImageData, SerializableQuotaExceededError, StructuredSerializedData,
    TransferableOffscreenCanvas, Transferrable as TransferrableInterface, TransformStreamData,
};
use strum::IntoEnumIterator;

use crate::dom::bindings::conversions::root_from_object;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::serializable::{Serializable, StorageKey};
use crate::dom::bindings::transferable::Transferable;
use crate::dom::bindings::wasm_module::WasmModuleRef;
use crate::dom::blob::Blob;
#[cfg(feature = "webcrypto")]
use crate::dom::cryptokey::CryptoKey;
use crate::dom::dompoint::DOMPoint;
use crate::dom::dompointreadonly::DOMPointReadOnly;
use crate::dom::file::File;
use crate::dom::filelist::FileList;
use crate::dom::globalscope::GlobalScope;
use crate::dom::imagebitmap::ImageBitmap;
use crate::dom::imagedata::ImageData;
use crate::dom::messageport::MessagePort;
use crate::dom::offscreencanvas::OffscreenCanvas;
use crate::dom::stream::readablestream::ReadableStream;
use crate::dom::stream::writablestream::WritableStream;
use crate::dom::types::{
    DOMException, DOMMatrix, DOMMatrixReadOnly, DOMQuad, DOMRect, DOMRectReadOnly,
    QuotaExceededError, TransformStream,
};
use crate::realms::enter_auto_realm;

// TODO: Should we add Min and Max const to https://github.com/servo/rust-mozjs/blob/master/src/consts.rs?
// TODO: Determine for sure which value Min and Max should have.
// NOTE: Current values found at https://dxr.mozilla.org/mozilla-central/
// rev/ff04d410e74b69acfab17ef7e73e7397602d5a68/js/public/StructuredClone.h#323
#[repr(u32)]
pub(super) enum StructuredCloneTags {
    /// To support additional types, add new tags with values incremented from the last one before Max.
    Min = 0xFFFF8000,
    DomBlob = 0xFFFF8001,
    DomFile = 0xFFFF8002,
    DomFileList = 0xFFFF8003,
    MessagePort = 0xFFFF8004,
    Principals = 0xFFFF8005,
    DomPointReadOnly = 0xFFFF8006,
    DomPoint = 0xFFFF8007,
    ReadableStream = 0xFFFF8008,
    DomException = 0xFFFF8009,
    WritableStream = 0xFFFF800A,
    TransformStream = 0xFFFF800B,
    ImageBitmap = 0xFFFF800C,
    OffscreenCanvas = 0xFFFF800D,
    QuotaExceededError = 0xFFFF800E,
    DomRect = 0xFFFF800F,
    DomRectReadOnly = 0xFFFF8010,
    DomQuad = 0xFFFF8011,
    DomMatrix = 0xFFFF8012,
    DomMatrixReadOnly = 0xFFFF8013,
    ImageData = 0xFFFF8014,
    #[cfg(feature = "webcrypto")]
    CryptoKey = 0xFFFF8015,
    /// A `WebAssembly.Module`, in-process clones only (see `wasm_module.rs`).
    WasmModule = 0xFFFF8016,
    Max = 0xFFFFFFFF,
}

impl From<SerializableInterface> for StructuredCloneTags {
    fn from(v: SerializableInterface) -> Self {
        match v {
            SerializableInterface::File => StructuredCloneTags::DomFile,
            SerializableInterface::FileList => StructuredCloneTags::DomFileList,
            SerializableInterface::Blob => StructuredCloneTags::DomBlob,
            SerializableInterface::DomPoint => StructuredCloneTags::DomPoint,
            SerializableInterface::DomPointReadOnly => StructuredCloneTags::DomPointReadOnly,
            SerializableInterface::DomRect => StructuredCloneTags::DomRect,
            SerializableInterface::DomRectReadOnly => StructuredCloneTags::DomRectReadOnly,
            SerializableInterface::DomQuad => StructuredCloneTags::DomQuad,
            SerializableInterface::DomMatrix => StructuredCloneTags::DomMatrix,
            SerializableInterface::DomMatrixReadOnly => StructuredCloneTags::DomMatrixReadOnly,
            SerializableInterface::DomException => StructuredCloneTags::DomException,
            SerializableInterface::ImageBitmap => StructuredCloneTags::ImageBitmap,
            SerializableInterface::QuotaExceededError => StructuredCloneTags::QuotaExceededError,
            SerializableInterface::ImageData => StructuredCloneTags::ImageData,
            #[cfg(feature = "webcrypto")]
            SerializableInterface::CryptoKey => StructuredCloneTags::CryptoKey,
        }
    }
}

impl From<TransferrableInterface> for StructuredCloneTags {
    fn from(v: TransferrableInterface) -> Self {
        match v {
            TransferrableInterface::ImageBitmap => StructuredCloneTags::ImageBitmap,
            TransferrableInterface::MessagePort => StructuredCloneTags::MessagePort,
            TransferrableInterface::OffscreenCanvas => StructuredCloneTags::OffscreenCanvas,
            TransferrableInterface::ReadableStream => StructuredCloneTags::ReadableStream,
            TransferrableInterface::WritableStream => StructuredCloneTags::WritableStream,
            TransferrableInterface::TransformStream => StructuredCloneTags::TransformStream,
        }
    }
}

fn reader_for_type(
    val: SerializableInterface,
) -> unsafe fn(
    cx: &mut JSContext,
    &GlobalScope,
    *mut JSStructuredCloneReader,
    &mut StructuredDataReader<'_>,
) -> *mut JSObject {
    match val {
        SerializableInterface::File => read_object::<File>,
        SerializableInterface::FileList => read_object::<FileList>,
        SerializableInterface::Blob => read_object::<Blob>,
        SerializableInterface::DomPoint => read_object::<DOMPoint>,
        SerializableInterface::DomPointReadOnly => read_object::<DOMPointReadOnly>,
        SerializableInterface::DomRect => read_object::<DOMRect>,
        SerializableInterface::DomRectReadOnly => read_object::<DOMRectReadOnly>,
        SerializableInterface::DomQuad => read_object::<DOMQuad>,
        SerializableInterface::DomMatrix => read_object::<DOMMatrix>,
        SerializableInterface::DomMatrixReadOnly => read_object::<DOMMatrixReadOnly>,
        SerializableInterface::DomException => read_object::<DOMException>,
        SerializableInterface::ImageBitmap => read_object::<ImageBitmap>,
        SerializableInterface::QuotaExceededError => read_object::<QuotaExceededError>,
        SerializableInterface::ImageData => read_object::<ImageData>,
        #[cfg(feature = "webcrypto")]
        SerializableInterface::CryptoKey => read_object::<CryptoKey>,
    }
}

unsafe fn read_object<T: Serializable>(
    cx: &mut JSContext,
    owner: &GlobalScope,
    r: *mut JSStructuredCloneReader,
    sc_reader: &mut StructuredDataReader<'_>,
) -> *mut JSObject {
    let mut name_space: u32 = 0;
    let mut index: u32 = 0;
    unsafe {
        assert!(JS_ReadUint32Pair(
            r,
            &mut name_space as *mut u32,
            &mut index as *mut u32
        ));
    }
    let storage_key = StorageKey { index, name_space };

    // 1. Re-build the key for the storage location
    // of the serialized object.
    let id: NamespaceIndex<T::Index> = storage_key.into();

    // 2. Get the transferred object from its storage, using the key.
    let objects = T::serialized_storage(StructuredData::Reader(sc_reader));
    let objects_map = objects
        .as_mut()
        .expect("The SC holder does not have any relevant objects");
    let serialized = objects_map
        .remove(&id)
        .expect("No object to be deserialized found.");
    if objects_map.is_empty() {
        *objects = None;
    }

    if let Ok(obj) = T::deserialize(cx, owner, serialized) {
        let reflector = obj.reflector().get_jsobject().get();
        sc_reader.roots.push(Heap::boxed(reflector));
        return reflector;
    }
    warn!("Reading structured data failed in {:?}.", owner.get_url());
    ptr::null_mut()
}

unsafe fn write_object<T: Serializable>(
    no_gc: &NoGC,
    interface: SerializableInterface,
    owner: &GlobalScope,
    object: &T,
    w: *mut JSStructuredCloneWriter,
    sc_writer: &mut StructuredDataWriter,
) -> bool {
    if let Ok((new_id, serialized)) = object.serialize(no_gc) {
        let objects = T::serialized_storage(StructuredData::Writer(sc_writer))
            .get_or_insert(FxHashMap::default());
        objects.insert(new_id, serialized);
        let storage_key = StorageKey::new(new_id);

        unsafe {
            assert!(JS_WriteUint32PairUnchecked(
                w,
                StructuredCloneTags::from(interface) as u32,
                0
            ));
            assert!(JS_WriteUint32PairUnchecked(
                w,
                storage_key.name_space,
                storage_key.index
            ));
        }
        return true;
    }
    warn!("Writing structured data failed in {:?}.", owner.get_url());
    false
}

unsafe extern "C" fn read_callback(
    cx: *mut RawJSContext,
    r: *mut JSStructuredCloneReader,
    _policy: *const CloneDataPolicy,
    tag: u32,
    _data: u32,
    closure: *mut raw::c_void,
) -> *mut JSObject {
    assert!(
        tag < StructuredCloneTags::Max as u32,
        "tag should be lower than StructuredCloneTags::Max"
    );
    assert!(
        tag > StructuredCloneTags::Min as u32,
        "tag should be higher than StructuredCloneTags::Min"
    );

    // SAFETY: it is safe to construct a JSContext from engine hook.
    let mut cx = unsafe { JSContext::from_ptr(NonNull::new(cx).unwrap()) };
    let cx = &mut cx;

    let sc_reader = unsafe { &mut *(closure as *mut StructuredDataReader<'_>) };

    if tag == StructuredCloneTags::WasmModule as u32 {
        return match sc_reader.wasm_modules.get(_data as usize) {
            Some(module) => module.create_object(unsafe { cx.raw_cx() }),
            None => ptr::null_mut(),
        };
    }

    let mut realm = CurrentRealm::assert(cx);
    let global = GlobalScope::from_current_realm(&mut realm);

    for serializable in SerializableInterface::iter() {
        if tag == StructuredCloneTags::from(serializable) as u32 {
            let reader = reader_for_type(serializable);
            return unsafe { reader(cx, &global, r, sc_reader) };
        }
    }

    ptr::null_mut()
}

enum OperationError {
    InterfaceDoesNotMatch,
    Exception(Error),
}

unsafe fn try_serialize<T: Serializable + IDLInterface>(
    cx: &mut JSContext,
    val: SerializableInterface,
    object: RawHandleObject,
    global: &GlobalScope,
    w: *mut JSStructuredCloneWriter,
    writer: &mut StructuredDataWriter,
) -> Result<bool, OperationError> {
    let object = unsafe { root_from_object::<T>(cx, *object) };
    if let Ok(obj) = object {
        return unsafe { Ok(write_object(cx.no_gc(), val, global, &*obj, w, writer)) };
    }
    Err(OperationError::InterfaceDoesNotMatch)
}

type SerializeOperation = unsafe fn(
    &mut JSContext,
    SerializableInterface,
    RawHandleObject,
    &GlobalScope,
    *mut JSStructuredCloneWriter,
    &mut StructuredDataWriter,
) -> Result<bool, OperationError>;

fn serialize_for_type(val: SerializableInterface) -> SerializeOperation {
    match val {
        SerializableInterface::File => try_serialize::<File>,
        SerializableInterface::FileList => try_serialize::<FileList>,
        SerializableInterface::Blob => try_serialize::<Blob>,
        SerializableInterface::DomPoint => try_serialize::<DOMPoint>,
        SerializableInterface::DomPointReadOnly => try_serialize::<DOMPointReadOnly>,
        SerializableInterface::DomRect => try_serialize::<DOMRect>,
        SerializableInterface::DomRectReadOnly => try_serialize::<DOMRectReadOnly>,
        SerializableInterface::DomQuad => try_serialize::<DOMQuad>,
        SerializableInterface::DomMatrix => try_serialize::<DOMMatrix>,
        SerializableInterface::DomMatrixReadOnly => try_serialize::<DOMMatrixReadOnly>,
        SerializableInterface::DomException => try_serialize::<DOMException>,
        SerializableInterface::ImageBitmap => try_serialize::<ImageBitmap>,
        SerializableInterface::QuotaExceededError => try_serialize::<QuotaExceededError>,
        SerializableInterface::ImageData => try_serialize::<ImageData>,
        #[cfg(feature = "webcrypto")]
        SerializableInterface::CryptoKey => try_serialize::<CryptoKey>,
    }
}

unsafe extern "C" fn write_callback(
    cx: *mut RawJSContext,
    w: *mut JSStructuredCloneWriter,
    obj: RawHandleObject,
    _same_process_scope_required: *mut bool,
    closure: *mut raw::c_void,
) -> bool {
    // SAFETY: it is safe to construct a JSContext from engine hook.
    let mut cx = unsafe { JSContext::from_ptr(NonNull::new(cx).unwrap()) };
    let cx = &mut cx;

    let sc_writer = unsafe { &mut *(closure as *mut StructuredDataWriter) };

    // A WebAssembly.Module: by reference, within this process only.
    if let Some(module) = WasmModuleRef::from_object(obj) {
        if !sc_writer.in_process {
            sc_writer.error = Some(Error::DataClone(Some(
                "A WebAssembly.Module can only be sent within its agent cluster".to_owned(),
            )));
            return false;
        }
        let index = sc_writer.wasm_modules.len() as u32;
        sc_writer.wasm_modules.push(module);
        return unsafe {
            JS_WriteUint32PairUnchecked(w, StructuredCloneTags::WasmModule as u32, index)
        };
    }

    let mut realm = CurrentRealm::assert(cx);
    let global = GlobalScope::from_current_realm(&mut realm);

    for serializable in SerializableInterface::iter() {
        let serializer = serialize_for_type(serializable);
        if let Ok(result) = unsafe { serializer(cx, serializable, obj, &global, w, sc_writer) } {
            return result;
        }
    }
    false
}

fn receiver_for_type(
    val: TransferrableInterface,
) -> fn(
    &mut JSContext,
    &GlobalScope,
    &mut StructuredDataReader<'_>,
    u64,
    RawMutableHandleObject,
) -> Result<(), ()> {
    match val {
        TransferrableInterface::ImageBitmap => receive_object::<ImageBitmap>,
        TransferrableInterface::MessagePort => receive_object::<MessagePort>,
        TransferrableInterface::OffscreenCanvas => receive_object::<OffscreenCanvas>,
        TransferrableInterface::ReadableStream => receive_object::<ReadableStream>,
        TransferrableInterface::WritableStream => receive_object::<WritableStream>,
        TransferrableInterface::TransformStream => receive_object::<TransformStream>,
    }
}

fn receive_object<T: Transferable>(
    cx: &mut JSContext,
    owner: &GlobalScope,
    sc_reader: &mut StructuredDataReader<'_>,
    extra_data: u64,
    return_object: RawMutableHandleObject,
) -> Result<(), ()> {
    // 1. Re-build the key for the storage location
    // of the transferred object.
    let big: [u8; 8] = extra_data.to_ne_bytes();
    let (name_space, index) = big.split_at(4);

    let namespace_id = PipelineNamespaceId(u32::from_ne_bytes(
        name_space
            .try_into()
            .expect("name_space to be a slice of four."),
    ));
    let id: NamespaceIndex<T::Index> = NamespaceIndex {
        namespace_id,
        index: Index::new(u32::from_ne_bytes(
            index.try_into().expect("index to be a slice of four."),
        ))
        .expect("Index to be non-zero"),
    };

    // 2. Get the transferred object from its storage, using the key.
    let storage = T::serialized_storage(StructuredData::Reader(sc_reader));
    let serialized = if let Some(objects) = storage.as_mut() {
        let object = objects.remove(&id).expect("Transferred port to be stored");
        if objects.is_empty() {
            *storage = None;
        }
        object
    } else {
        panic!(
            "An interface was transfer-received, yet the SC holder does not have any serialized objects"
        );
    };

    let Ok(received) = T::transfer_receive(cx, owner, id, serialized) else {
        return Err(());
    };
    return_object.set(received.reflector().rootable().get());
    sc_reader.roots.push(Heap::boxed(return_object.get()));
    Ok(())
}

unsafe extern "C" fn read_transfer_callback(
    cx: *mut RawJSContext,
    _r: *mut JSStructuredCloneReader,
    _policy: *const CloneDataPolicy,
    tag: u32,
    _content: *mut raw::c_void,
    extra_data: u64,
    closure: *mut raw::c_void,
    return_object: RawMutableHandleObject,
) -> bool {
    let sc_reader = unsafe { &mut *(closure as *mut StructuredDataReader<'_>) };
    let mut cx = unsafe {
        // This is safe because we are in SM hook
        JSContext::from_ptr(
            NonNull::new(cx).expect("JSContext pointer should not be null in SM hook"),
        )
    };
    let mut realm = CurrentRealm::assert(&mut cx);
    let owner = GlobalScope::from_current_realm(&mut realm);

    for transferrable in TransferrableInterface::iter() {
        if tag == StructuredCloneTags::from(transferrable) as u32 {
            let transfer_receiver = receiver_for_type(transferrable);
            if transfer_receiver(&mut realm, &owner, sc_reader, extra_data, return_object).is_ok() {
                return true;
            }
        }
    }
    false
}

unsafe fn try_transfer<T: Transferable + IDLInterface>(
    interface: TransferrableInterface,
    obj: RawHandleObject,
    cx: &mut JSContext,
    sc_writer: &mut StructuredDataWriter,
    tag: *mut u32,
    ownership: *mut TransferableOwnership,
    extra_data: *mut u64,
) -> Result<(), OperationError> {
    let object = unsafe { root_from_object::<T>(cx, *obj) };
    let Ok(object) = object else {
        return Err(OperationError::InterfaceDoesNotMatch);
    };

    unsafe { *tag = StructuredCloneTags::from(interface) as u32 };
    unsafe { *ownership = TransferableOwnership::SCTAG_TMO_CUSTOM };

    let (id, object) = object.transfer(cx).map_err(OperationError::Exception)?;

    // 2. Store the transferred object at a given key.
    let objects = T::serialized_storage(StructuredData::Writer(sc_writer))
        .get_or_insert(FxHashMap::default());
    objects.insert(id, object);

    let index = id.index.0.get();

    let mut big: [u8; 8] = [0; 8];
    let name_space = id.namespace_id.0.to_ne_bytes();
    let index = index.to_ne_bytes();

    let (left, right) = big.split_at_mut(4);
    left.copy_from_slice(&name_space);
    right.copy_from_slice(&index);

    // 3. Return a u64 representation of the key where the object is stored.
    unsafe { *extra_data = u64::from_ne_bytes(big) };
    Ok(())
}

type TransferOperation = unsafe fn(
    TransferrableInterface,
    RawHandleObject,
    &mut JSContext,
    &mut StructuredDataWriter,
    *mut u32,
    *mut TransferableOwnership,
    *mut u64,
) -> Result<(), OperationError>;

fn transfer_for_type(val: TransferrableInterface) -> TransferOperation {
    match val {
        TransferrableInterface::ImageBitmap => try_transfer::<ImageBitmap>,
        TransferrableInterface::MessagePort => try_transfer::<MessagePort>,
        TransferrableInterface::OffscreenCanvas => try_transfer::<OffscreenCanvas>,
        TransferrableInterface::ReadableStream => try_transfer::<ReadableStream>,
        TransferrableInterface::WritableStream => try_transfer::<WritableStream>,
        TransferrableInterface::TransformStream => try_transfer::<TransformStream>,
    }
}

/// <https://html.spec.whatwg.org/multipage/#structuredserializewithtransfer>
unsafe extern "C" fn write_transfer_callback(
    cx: *mut RawJSContext,
    obj: RawHandleObject,
    closure: *mut raw::c_void,
    tag: *mut u32,
    ownership: *mut TransferableOwnership,
    _content: *mut *mut raw::c_void,
    extra_data: *mut u64,
) -> bool {
    let sc_writer = unsafe { &mut *(closure as *mut StructuredDataWriter) };
    let mut cx = unsafe {
        // This is safe because we are in SM hook
        JSContext::from_ptr(
            NonNull::new(cx).expect("JSContext pointer should not be null in SM hook"),
        )
    };
    for transferable in TransferrableInterface::iter() {
        let try_transfer = transfer_for_type(transferable);

        let transfer_result = unsafe {
            try_transfer(
                transferable,
                obj,
                &mut cx,
                sc_writer,
                tag,
                ownership,
                extra_data,
            )
        };
        match transfer_result {
            Err(error) => match error {
                OperationError::InterfaceDoesNotMatch => {},
                OperationError::Exception(error) => {
                    sc_writer.error = Some(error);
                    return false;
                },
            },
            Ok(..) => return true,
        }
    }

    false
}

unsafe extern "C" fn free_transfer_callback(
    _tag: u32,
    _ownership: TransferableOwnership,
    _content: *mut raw::c_void,
    _extra_data: u64,
    _closure: *mut raw::c_void,
) {
}

unsafe fn can_transfer_for_type(
    cx: &mut JSContext,
    transferable: TransferrableInterface,
    obj: RawHandleObject,
) -> Result<bool, ()> {
    unsafe fn can_transfer<T: Transferable + IDLInterface>(
        cx: &mut JSContext,
        obj: RawHandleObject,
    ) -> Result<bool, ()> {
        unsafe { root_from_object::<T>(cx, *obj).map(|o| Transferable::can_transfer(&*o)) }
    }

    unsafe {
        match transferable {
            TransferrableInterface::ImageBitmap => can_transfer::<ImageBitmap>(cx, obj),
            TransferrableInterface::MessagePort => can_transfer::<MessagePort>(cx, obj),
            TransferrableInterface::OffscreenCanvas => can_transfer::<OffscreenCanvas>(cx, obj),
            TransferrableInterface::ReadableStream => can_transfer::<ReadableStream>(cx, obj),
            TransferrableInterface::WritableStream => can_transfer::<WritableStream>(cx, obj),
            TransferrableInterface::TransformStream => can_transfer::<TransformStream>(cx, obj),
        }
    }
}

unsafe extern "C" fn can_transfer_callback(
    cx: *mut RawJSContext,
    obj: RawHandleObject,
    _same_process_scope_required: *mut bool,
    _closure: *mut raw::c_void,
) -> bool {
    // SAFETY: it is safe to construct a JSContext from engine hook.
    let mut cx = unsafe { JSContext::from_ptr(NonNull::new(cx).unwrap()) };
    let cx = &mut cx;

    for transferable in TransferrableInterface::iter() {
        let can_transfer = unsafe { can_transfer_for_type(cx, transferable, obj) };
        if let Ok(can_transfer) = can_transfer {
            return can_transfer;
        }
    }
    false
}

unsafe extern "C" fn report_error_callback(
    _cx: *mut RawJSContext,
    _errorid: u32,
    closure: *mut raw::c_void,
    error_message: *const ::std::os::raw::c_char,
) {
    let msg_result = unsafe { CStr::from_ptr(error_message).to_str().map(str::to_string) };

    if let Ok(msg) = msg_result {
        let error = unsafe { &mut *(closure as *mut Option<Error>) };

        if error.is_none() {
            *error = Some(Error::DataClone(Some(msg)));
        }
    }
}

/// SpiderMonkey calls this after it has written or read a `SharedArrayBuffer` (or shared
/// `WebAssembly.Memory`). It only gets that far when the clone's `CloneDataPolicy` allowed shared
/// memory, which `write_impl` grants only to cross-origin isolated globals and `read` only to a
/// reader in the writer's agent cluster; the policy is the gate, this just records that shared
/// memory went in.
unsafe extern "C" fn sab_cloned_callback(
    _cx: *mut RawJSContext,
    receiving: bool,
    closure: *mut ::std::os::raw::c_void,
) -> bool {
    if !receiving {
        let writer = unsafe { &mut *(closure as *mut StructuredDataWriter) };
        writer.wrote_shared_memory = true;
    }
    true
}

/// A SpiderMonkey clone buffer that may hold pointers (written with the `SameProcess` scope, or
/// with `UnknownDestination` when it took shared memory), carried whole (never as bytes) inside
/// `StructuredSerializedData::in_process` to its one reader, with the agent cluster it was
/// written in: shared memory in it deserializes only there.
struct InProcessCloneBuffer(
    JSAutoStructuredCloneBufferWrapper,
    Vec<WasmModuleRef>,
    AgentClusterKey,
);

// SAFETY: the buffer is filled on the writing thread and then owned by exactly one message, which
// one thread at a time reads or drops. Dropping it unread only releases `SharedArrayRawBuffer`
// references (atomic refcounts) and frees owned transferred buffer contents (malloc'd memory);
// neither needs the writing thread's JSContext. Servo's `freeTransfer` callback is a no-op.
unsafe impl Send for InProcessCloneBuffer {}

static STRUCTURED_CLONE_CALLBACKS: JSStructuredCloneCallbacks = JSStructuredCloneCallbacks {
    read: Some(read_callback),
    write: Some(write_callback),
    reportError: Some(report_error_callback),
    readTransfer: Some(read_transfer_callback),
    writeTransfer: Some(write_transfer_callback),
    freeTransfer: Some(free_transfer_callback),
    canTransfer: Some(can_transfer_callback),
    sabCloned: Some(sab_cloned_callback),
};

pub(crate) enum StructuredData<'a, 'b> {
    Reader(&'a mut StructuredDataReader<'b>),
    Writer(&'a mut StructuredDataWriter),
}

/// Reader and writer structs for results from, and inputs to, structured-data read/write operations.
/// <https://html.spec.whatwg.org/multipage/#safe-passing-of-structured-data>
#[repr(C)]
pub(crate) struct StructuredDataReader<'a> {
    /// A error record.
    error: Option<Error>,
    /// Rooted copies of every deserialized object to ensure they are not garbage collected.
    roots: RootedVec<'a, Box<Heap<*mut JSObject>>>,
    /// A map of port implementations,
    /// used as part of the "transfer-receiving" steps of ports,
    /// to produce the DOM ports stored in `message_ports` above.
    pub(crate) port_impls: Option<FxHashMap<MessagePortId, MessagePortImpl>>,
    /// A map of transform stream implementations,
    pub(crate) transform_streams_port_impls: Option<FxHashMap<MessagePortId, TransformStreamData>>,
    /// A map of blob implementations,
    /// used as part of the "deserialize" steps of blobs,
    /// to produce the DOM blobs stored in `blobs` above.
    pub(crate) blob_impls: Option<FxHashMap<BlobId, BlobImpl>>,
    /// A map of serialized files.
    pub(crate) files: Option<FxHashMap<FileId, SerializableFile>>,
    /// A map of serialized file lists.
    pub(crate) file_lists: Option<FxHashMap<FileListId, SerializableFileList>>,
    /// A map of serialized points.
    pub(crate) points: Option<FxHashMap<DomPointId, DomPoint>>,
    /// A map of serialized rects.
    pub(crate) rects: Option<FxHashMap<DomRectId, DomRect>>,
    /// A map of serialized quads.
    pub(crate) quads: Option<FxHashMap<DomQuadId, DomQuad>>,
    /// A map of serialized matrices.
    pub(crate) matrices: Option<FxHashMap<DomMatrixId, DomMatrix>>,
    /// A map of serialized exceptions.
    pub(crate) exceptions: Option<FxHashMap<DomExceptionId, DomException>>,
    /// A map of serialized quota exceeded errors.
    pub(crate) quota_exceeded_errors:
        Option<FxHashMap<QuotaExceededErrorId, SerializableQuotaExceededError>>,
    // A map of serialized image bitmaps.
    pub(crate) image_bitmaps: Option<FxHashMap<ImageBitmapId, SerializableImageBitmap>>,
    /// A map of transferred image bitmaps.
    pub(crate) transferred_image_bitmaps: Option<FxHashMap<ImageBitmapId, SerializableImageBitmap>>,
    /// A map of transferred offscreen canvases.
    pub(crate) offscreen_canvases:
        Option<FxHashMap<OffscreenCanvasId, TransferableOffscreenCanvas>>,
    // A map of serialized image data.
    pub(crate) image_data: Option<FxHashMap<ImageDataId, SerializableImageData>>,
    // A map of serialized crypto keys.
    pub(crate) crypto_keys: Option<FxHashMap<CryptoKeyId, SerializableCryptoKey>>,
    /// The `WebAssembly.Module`s of an in-process clone, by index.
    pub(crate) wasm_modules: Vec<WasmModuleRef>,
}

/// A data holder for transferred and serialized objects.
#[derive(Default)]
#[repr(C)]
pub(crate) struct StructuredDataWriter {
    /// Error record.
    pub(crate) error: Option<Error>,
    /// Whether a `SharedArrayBuffer` (or shared wasm memory) was written.
    pub(crate) wrote_shared_memory: bool,
    /// Transferred ports.
    pub(crate) ports: Option<FxHashMap<MessagePortId, MessagePortImpl>>,
    /// Transferred transform streams.
    pub(crate) transform_streams_port: Option<FxHashMap<MessagePortId, TransformStreamData>>,
    /// Serialized points.
    pub(crate) points: Option<FxHashMap<DomPointId, DomPoint>>,
    /// Serialized rects.
    pub(crate) rects: Option<FxHashMap<DomRectId, DomRect>>,
    /// Serialized quads.
    pub(crate) quads: Option<FxHashMap<DomQuadId, DomQuad>>,
    /// Serialized matrices.
    pub(crate) matrices: Option<FxHashMap<DomMatrixId, DomMatrix>>,
    /// Serialized exceptions.
    pub(crate) exceptions: Option<FxHashMap<DomExceptionId, DomException>>,
    /// Serialized quota exceeded errors.
    pub(crate) quota_exceeded_errors:
        Option<FxHashMap<QuotaExceededErrorId, SerializableQuotaExceededError>>,
    /// Serialized blobs.
    pub(crate) blobs: Option<FxHashMap<BlobId, BlobImpl>>,
    /// Serialized files.
    pub(crate) files: Option<FxHashMap<FileId, SerializableFile>>,
    /// Serialized file lists.
    pub(crate) file_lists: Option<FxHashMap<FileListId, SerializableFileList>>,
    /// Serialized image bitmaps.
    pub(crate) image_bitmaps: Option<FxHashMap<ImageBitmapId, SerializableImageBitmap>>,
    /// Transferred image bitmaps.
    pub(crate) transferred_image_bitmaps: Option<FxHashMap<ImageBitmapId, SerializableImageBitmap>>,
    /// Transferred offscreen canvases.
    pub(crate) offscreen_canvases:
        Option<FxHashMap<OffscreenCanvasId, TransferableOffscreenCanvas>>,
    // A map of serialized image data.
    pub(crate) image_data: Option<FxHashMap<ImageDataId, SerializableImageData>>,
    // A map of serialized crypto keys.
    pub(crate) crypto_keys: Option<FxHashMap<CryptoKeyId, SerializableCryptoKey>>,
    /// Whether this clone travels in-process (`write_for_same_agent_cluster` with a cross-origin
    /// isolated global), which is the only way a `WebAssembly.Module` can be cloned.
    pub(crate) in_process: bool,
    /// The `WebAssembly.Module`s written, by index (tag `WasmModule`, data = index).
    pub(crate) wasm_modules: Vec<WasmModuleRef>,
}

/// Where a structured clone may be read, which decides how it is written.
enum CloneTarget {
    /// Anywhere, now or later (storage, another process): bytes, never shared memory.
    Bytes,
    /// A reader in this process (SpiderMonkey's `SameProcess` scope): shared memory and
    /// transferred buffers by pointer, carried whole. Shared memory deserializes only in the
    /// writer's agent cluster.
    InProcess(AgentClusterKey),
    /// A reader that may be in another process: bytes, unless shared memory was written
    /// (SpiderMonkey's `UnknownDestination` scope), which then travels like `InProcess`.
    Unknown(AgentClusterKey),
}

/// Writes a structured clone. Returns a `DataClone` error if that fails.
///
/// The result may be stored or sent to another process, so it never contains shared memory: a
/// `SharedArrayBuffer` in `message` is a `DataCloneError`.
pub(crate) fn write(
    cx: &mut JSContext,
    message: HandleValue,
    transfer: Option<CustomAutoRooterGuard<Vec<*mut JSObject>>>,
) -> Fallible<StructuredSerializedData> {
    write_impl(cx, message, transfer, CloneTarget::Bytes)
}

/// Writes a structured clone for a reader in this process: a dedicated worker and its owner, a
/// window of this event loop, a message port, or `structuredClone()`. If `global` is cross-origin
/// isolated the clone may hold `SharedArrayBuffer`s
/// (<https://html.spec.whatwg.org/multipage/#structuredserializeinternal> step 13: shared memory
/// needs the cross-origin isolated capability), which deserialize only for a reader in `global`'s
/// agent cluster (<https://html.spec.whatwg.org/multipage/#structureddeserialize>); any other
/// reader fails with a `DataCloneError` (a `messageerror` event).
pub(crate) fn write_for_same_agent_cluster(
    cx: &mut JSContext,
    global: &GlobalScope,
    message: HandleValue,
    transfer: Option<CustomAutoRooterGuard<Vec<*mut JSObject>>>,
) -> Fallible<StructuredSerializedData> {
    let target = if global.cross_origin_isolated_capability() {
        CloneTarget::InProcess(global.agent_cluster())
    } else {
        CloneTarget::Bytes
    };
    write_impl(cx, message, transfer, target)
}

/// Writes a structured clone for a message whose reader may be in another event loop or process
/// (a cross-origin window, a service worker or client, a `BroadcastChannel`). Like
/// `write_for_same_agent_cluster`, a cross-origin isolated `global` may serialize a
/// `SharedArrayBuffer`; the receiver then gets a `messageerror` unless it is in `global`'s agent
/// cluster in this process. Without shared memory the clone is plain bytes.
pub(crate) fn write_for_messaging(
    cx: &mut JSContext,
    global: &GlobalScope,
    message: HandleValue,
    transfer: Option<CustomAutoRooterGuard<Vec<*mut JSObject>>>,
) -> Fallible<StructuredSerializedData> {
    let target = if global.cross_origin_isolated_capability() {
        CloneTarget::Unknown(global.agent_cluster())
    } else {
        CloneTarget::Bytes
    };
    write_impl(cx, message, transfer, target)
}

fn write_impl(
    cx: &mut JSContext,
    message: HandleValue,
    transfer: Option<CustomAutoRooterGuard<Vec<*mut JSObject>>>,
    target: CloneTarget,
) -> Fallible<StructuredSerializedData> {
    // Shared memory can only be passed by pointer, so such a clone uses the SameProcess scope (or
    // UnknownDestination, which becomes SameProcess once shared memory is written) and travels as
    // the whole SpiderMonkey buffer, never as bytes.
    let (scope, allow_shared_memory, in_process_only) = match target {
        CloneTarget::Bytes => (StructuredCloneScope::DifferentProcess, false, false),
        CloneTarget::InProcess(_) => (StructuredCloneScope::SameProcess, true, true),
        CloneTarget::Unknown(_) => (StructuredCloneScope::UnknownDestination, true, false),
    };
    unsafe {
        rooted!(&in(cx) let mut val = UndefinedValue());
        if let Some(transfer) = transfer {
            transfer.to_jsval(cx, val.handle_mut());
        }
        let mut sc_writer = StructuredDataWriter {
            in_process: in_process_only,
            ..Default::default()
        };
        let sc_writer_ptr = &mut sc_writer as *mut _;

        let scbuf = JSAutoStructuredCloneBufferWrapper::new(scope, &STRUCTURED_CLONE_CALLBACKS);
        let scdata = &mut ((*scbuf.as_raw_ptr()).data_);
        let policy = CloneDataPolicy {
            allowIntraClusterClonableSharedObjects_: allow_shared_memory,
            allowSharedMemoryObjects_: allow_shared_memory,
        };
        let result = JS_WriteStructuredClone(
            cx,
            message,
            scdata,
            scope,
            &policy,
            &STRUCTURED_CLONE_CALLBACKS,
            sc_writer_ptr as *mut raw::c_void,
            val.handle(),
        );
        if !result {
            let error = if JS_IsExceptionPending(cx) {
                Error::JSFailed
            } else {
                sc_writer.error.unwrap_or(Error::DataClone(None))
            };

            return Err(error);
        }

        let agent_cluster = match target {
            CloneTarget::InProcess(key) => Some(key),
            CloneTarget::Unknown(key) if sc_writer.wrote_shared_memory => Some(key),
            _ => None,
        };
        let (data, in_process) = if let Some(agent_cluster) = agent_cluster {
            (
                Vec::new(),
                Some(InProcessCloneData(Box::new(InProcessCloneBuffer(
                    scbuf,
                    std::mem::take(&mut sc_writer.wasm_modules),
                    agent_cluster,
                )))),
            )
        } else {
            let nbytes = GetLengthOfJSStructuredCloneData(scdata);
            let mut data = Vec::with_capacity(nbytes);
            CopyJSStructuredCloneData(scdata, data.as_mut_ptr());
            data.set_len(nbytes);
            (data, None)
        };

        let data = StructuredSerializedData {
            serialized: data,
            in_process,
            ports: sc_writer.ports.take(),
            transform_streams: sc_writer.transform_streams_port.take(),
            points: sc_writer.points.take(),
            rects: sc_writer.rects.take(),
            quads: sc_writer.quads.take(),
            matrices: sc_writer.matrices.take(),
            exceptions: sc_writer.exceptions.take(),
            quota_exceeded_errors: sc_writer.quota_exceeded_errors.take(),
            blobs: sc_writer.blobs.take(),
            files: sc_writer.files.take(),
            file_lists: sc_writer.file_lists.take(),
            image_bitmaps: sc_writer.image_bitmaps.take(),
            transferred_image_bitmaps: sc_writer.transferred_image_bitmaps.take(),
            offscreen_canvases: sc_writer.offscreen_canvases.take(),
            image_data: sc_writer.image_data.take(),
            crypto_keys: sc_writer.crypto_keys.take(),
        };

        Ok(data)
    }
}

/// Read structured serialized data, possibly containing transferred objects.
/// Returns a vec of rooted transfer-received ports, or an error.
pub(crate) fn read(
    cx: &mut JSContext,
    global: &GlobalScope,
    mut data: StructuredSerializedData,
    rval: MutableHandleValue,
) -> Fallible<Vec<DomRoot<MessagePort>>> {
    let mut realm = enter_auto_realm(cx, global);
    let cx = &mut realm.current_realm();

    rooted_vec!(let mut roots);
    let mut sc_reader = StructuredDataReader {
        error: None,
        roots,
        port_impls: data.ports.take(),
        transform_streams_port_impls: data.transform_streams.take(),
        blob_impls: data.blobs.take(),
        files: data.files.take(),
        file_lists: data.file_lists.take(),
        points: data.points.take(),
        rects: data.rects.take(),
        quads: data.quads.take(),
        matrices: data.matrices.take(),
        exceptions: data.exceptions.take(),
        quota_exceeded_errors: data.quota_exceeded_errors.take(),
        image_bitmaps: data.image_bitmaps.take(),
        transferred_image_bitmaps: data.transferred_image_bitmaps.take(),
        offscreen_canvases: data.offscreen_canvases.take(),
        image_data: data.image_data.take(),
        crypto_keys: data.crypto_keys.take(),
        wasm_modules: Vec::new(),
    };
    // An in-process clone (see `write_for_same_agent_cluster`) is read straight from its buffer;
    // everything else from bytes. Shared memory is accepted only by a cross-origin isolated reader
    // in the agent cluster it was written in
    // (<https://html.spec.whatwg.org/multipage/#structureddeserialize>: "If targetRealm's
    // corresponding agent cluster is not serialized.[[AgentCluster]], then throw a
    // DataCloneError"); SpiderMonkey also refuses it unless the reader's realm has shared memory.
    let in_process = data
        .in_process
        .take()
        .and_then(|payload| payload.0.downcast::<InProcessCloneBuffer>().ok());
    let allow_shared_memory = in_process.as_ref().is_some_and(|buffer| {
        global.cross_origin_isolated_capability() && global.is_in_agent_cluster(&buffer.2)
    });
    let in_process = in_process.map(|buffer| {
        let InProcessCloneBuffer(scbuf, wasm_modules, _) = *buffer;
        sc_reader.wasm_modules = wasm_modules;
        scbuf
    });
    let sc_reader_ptr = &mut sc_reader as *mut _;
    unsafe {
        let (scope, scbuf) = match in_process {
            Some(scbuf) => (StructuredCloneScope::SameProcess, scbuf),
            None => {
                let scbuf = JSAutoStructuredCloneBufferWrapper::new(
                    StructuredCloneScope::DifferentProcess,
                    &STRUCTURED_CLONE_CALLBACKS,
                );
                WriteBytesToJSStructuredCloneData(
                    data.serialized.as_mut_ptr() as *const u8,
                    data.serialized.len(),
                    &mut ((*scbuf.as_raw_ptr()).data_),
                );
                (StructuredCloneScope::DifferentProcess, scbuf)
            },
        };
        let scdata = &mut ((*scbuf.as_raw_ptr()).data_);

        let result = JS_ReadStructuredClone(
            cx,
            scdata,
            JS_STRUCTURED_CLONE_VERSION,
            scope,
            rval,
            &CloneDataPolicy {
                allowIntraClusterClonableSharedObjects_: allow_shared_memory,
                allowSharedMemoryObjects_: allow_shared_memory,
            },
            &STRUCTURED_CLONE_CALLBACKS,
            sc_reader_ptr as *mut raw::c_void,
        );
        if !result {
            let error = if JS_IsExceptionPending(cx) {
                Error::JSFailed
            } else {
                sc_reader.error.unwrap_or(Error::DataClone(None))
            };

            return Err(error);
        }

        let mut message_ports = vec![];
        for reflector in sc_reader.roots.iter() {
            let Ok(message_port) = root_from_object::<MessagePort>(cx, reflector.get()) else {
                continue;
            };
            message_ports.push(message_port);
        }
        // Any transfer-received port-impls should have been taken out.
        assert!(sc_reader.port_impls.is_none());
        Ok(message_ports)
    }
}
