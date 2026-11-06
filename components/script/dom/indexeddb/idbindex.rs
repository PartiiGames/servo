/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use dom_struct::dom_struct;
use js::context::JSContext;
use js::conversions::ToJSValConvertible;
use js::gc::MutableHandleValue;
use js::jsapi::Heap;
use js::jsval::JSVal;
use js::rust::HandleValue;
use profile_traits::generic_callback::GenericCallback;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::IDBIndexBinding::IDBIndexMethods;
use script_bindings::codegen::GenericBindings::IDBTransactionBinding::IDBTransactionMode;
use script_bindings::error::{Error, ErrorResult, Fallible};
use script_bindings::reflector::{Reflector, reflect_dom_object};
use script_bindings::str::DOMString;
use serde::{Deserialize, Serialize};
use storage_traits::indexeddb::{AsyncOperation, AsyncReadOnlyOperation, BackendResult};

use crate::dom::bindings::codegen::Bindings::IDBCursorBinding::IDBCursorDirection;
use crate::dom::bindings::codegen::Bindings::IDBObjectStoreBinding::IDBGetAllOptions;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::globalscope::GlobalScope;
use crate::dom::idbobjectstore::KeyPath;
use crate::dom::indexeddb::idbcursor::{IDBCursor, IterationParam, ObjectStoreOrIndex};
use crate::dom::indexeddb::idbcursorwithvalue::IDBCursorWithValue;
use crate::dom::indexeddb::idbobjectstore::{GetAllKind, IDBObjectStore};
use crate::dom::indexeddb::idbrequest::{IDBRequest, IdbResult};
use crate::dom::indexeddb::key::{
    convert_value_to_key_range, get_all_arguments, get_all_options_arguments,
};

#[dom_struct]
pub(crate) struct IDBIndex {
    reflector_: Reflector,
    object_store: Dom<IDBObjectStore>,
    name: DomRefCell<DOMString>,
    multi_entry: bool,
    unique: bool,
    key_path: KeyPath,
    /// Partii: the keyPath attribute's value, converted once so that it is the same object
    /// each time (undefined until first read).
    #[ignore_malloc_size_of = "mozjs"]
    key_path_value: Heap<JSVal>,
    /// Partii: the index's name when the transaction started, if it existed then (None for an
    /// index created in this transaction). An aborted upgrade restores it.
    original_name: DomRefCell<Option<DOMString>>,
}

impl IDBIndex {
    pub fn new_inherited(
        object_store: &IDBObjectStore,
        name: DOMString,
        multi_entry: bool,
        unique: bool,
        key_path: KeyPath,
    ) -> IDBIndex {
        IDBIndex {
            reflector_: Reflector::new(),
            object_store: Dom::from_ref(object_store),
            name: DomRefCell::new(name),
            multi_entry,
            unique,
            key_path,
            key_path_value: Heap::default(),
            original_name: DomRefCell::new(None),
        }
    }

    pub fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        object_store: &IDBObjectStore,
        name: DOMString,
        multi_entry: bool,
        unique: bool,
        key_path: KeyPath,
    ) -> DomRoot<IDBIndex> {
        reflect_dom_object(
            cx,
            Box::new(IDBIndex::new_inherited(
                object_store,
                name,
                multi_entry,
                unique,
                key_path,
            )),
            global,
        )
    }

    pub(crate) fn object_store(&self) -> DomRoot<IDBObjectStore> {
        DomRoot::from_ref(&self.object_store)
    }

    /// Partii: the index's key path, for computing a record's index keys.
    pub(crate) fn key_path(&self) -> &KeyPath {
        &self.key_path
    }

    pub(crate) fn multi_entry(&self) -> bool {
        self.multi_entry
    }

    pub(crate) fn name(&self) -> DOMString {
        self.name.borrow().clone()
    }

    /// Partii: note that the index existed when the transaction started.
    pub(crate) fn mark_existing(&self) {
        *self.original_name.borrow_mut() = Some(self.name());
    }

    /// Partii: the index's name when the transaction started, if it existed then.
    pub(crate) fn original_name(&self) -> Option<DOMString> {
        self.original_name.borrow().clone()
    }

    /// Partii: <https://w3c.github.io/IndexedDB/#abort-an-upgrade-transaction> step 5.3: an index
    /// handle of an index that was not created in the transaction gets its index's name back.
    pub(crate) fn restore_name(&self, name: DOMString) {
        *self.name.borrow_mut() = name;
    }

    pub(crate) fn verify_not_deleted(&self) -> ErrorResult {
        let transaction = self.object_store.transaction();
        // Partii: this handle, not just an index of the same name, must still be in its object
        // store handle's index set.
        if !self.object_store.has_index_handle(self) ||
            !transaction
                .get_db()
                .object_store_exists(&self.object_store.get_name())
        {
            return Err(Error::InvalidState(Some(
                "Index or its object store has been deleted".to_owned(),
            )));
        }
        Ok(())
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbindex-opencursor>
    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbindex-openkeycursor>
    fn open_cursor(
        &self,
        cx: &mut JSContext,
        query: HandleValue,
        direction: IDBCursorDirection,
        key_only: bool,
    ) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this’s object store's transaction.
        let transaction = self.object_store.transaction();

        // Step 2. If index or its object store has been deleted, throw an "InvalidStateError" DOMException.
        let stored_name = self.name.borrow();
        if !self.object_store.has_index(&stored_name) ||
            !transaction
                .get_db()
                .object_store_exists(&self.object_store.get_name())
        {
            return Err(Error::InvalidState(Some(
                "Index or its object store has been deleted".to_owned(),
            )));
        }
        // Partii: release the name borrow before running script (key range conversion).
        let index_name = String::from(stored_name.clone());
        drop(stored_name);

        // Step 3. If transaction is not active, throw a "TransactionInactiveError" DOMException.
        if !transaction.is_active() || !transaction.is_usable() {
            return Err(Error::TransactionInactive(Some(
                "Transaction is not active".to_owned(),
            )));
        }

        // Step 4. Let range be the result of running the steps to convert a value to a key range with query.
        let range = convert_value_to_key_range(cx, query, Some(false))?;

        // Step 5. Let cursor be a new cursor with transaction, direction, source: Index, range.
        let cursor = if key_only {
            IDBCursor::new(
                cx,
                &self.global(),
                &transaction,
                direction,
                false,
                ObjectStoreOrIndex::Index(Dom::from_ref(self)),
                range.clone(),
                key_only,
            )
        } else {
            DomRoot::upcast(IDBCursorWithValue::new(
                cx,
                &self.global(),
                &transaction,
                direction,
                false,
                ObjectStoreOrIndex::Index(Dom::from_ref(self)),
                range.clone(),
                key_only,
            ))
        };

        // Step 6. Asynchronously execute a request with iteration_param.
        let iteration_param = IterationParam {
            cursor: Trusted::new(&cursor),
            key: None,
            primary_key: None,
            count: None,
        };

        IDBRequest::execute_async(
            cx,
            &self.object_store,
            |callback| {
                AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Iterate {
                    callback,
                    key_range: range,
                    index_name: Some(index_name),
                    key_only,
                })
            },
            None,
            Some(iteration_param),
        )
        .inspect(|request| {
            request.set_index_source(self);
            cursor.set_request(request)
        })
    }

    /// Partii: the checks every index read starts with. Step 1 to 4 of the IDBIndex get(),
    /// getKey(), count() and "create a request to retrieve multiple items" algorithms: the index
    /// and its object store exist, and the transaction is active.
    fn check_readable(&self) -> ErrorResult {
        self.verify_not_deleted()?;
        let transaction = self.object_store.transaction();
        if !transaction.is_active() || !transaction.is_usable() {
            return Err(Error::TransactionInactive(Some(
                "Transaction is not active".to_owned(),
            )));
        }
        Ok(())
    }

    /// Partii: a get(), getKey() or count() request on this index.
    fn read_request<T>(
        &self,
        cx: &mut JSContext,
        operation: impl FnOnce(
            GenericCallback<BackendResult<T>>,
            Option<String>,
        ) -> AsyncReadOnlyOperation,
    ) -> Fallible<DomRoot<IDBRequest>>
    where
        T: Into<IdbResult> + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
    {
        let index_name = Some(String::from(self.name()));
        let request = IDBRequest::execute_async(
            cx,
            &self.object_store,
            |callback| AsyncOperation::ReadOnly(operation(callback, index_name)),
            None,
            None,
        )?;
        request.set_index_source(self);
        Ok(request)
    }
}

impl IDBIndexMethods<crate::DomTypeHolder> for IDBIndex {
    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-name>
    fn Name(&self) -> DOMString {
        self.name.borrow().clone()
    }

    /// <https://www.w3.org/TR/IndexedDB/#ref-for-dom-idbindex-name%E2%91%A2>
    fn SetName(&self, name: DOMString) -> ErrorResult {
        // Step 1: Let name be the given value.
        // Step 2: Let transaction be this’s transaction.
        let transaction = self.object_store.transaction();

        // Step 3: Let index be this’s index.
        // We do not have an explicit object representing the underlying index.

        // Step 4: If transaction is not an upgrade transaction, throw an "InvalidStateError" DOMException.
        if transaction.get_mode() != IDBTransactionMode::Versionchange {
            return Err(Error::InvalidState(Some(
                "Transaction is not an upgrade transaction".to_owned(),
            )));
        }

        // Step 5: If transaction’s state is not active, then throw a "TransactionInactiveError" DOMException.
        if !transaction.is_active() {
            return Err(Error::TransactionInactive(Some(
                "Transaction is not active while updating index name".to_owned(),
            )));
        }

        // Step 6: If index or index’s object store has been deleted, throw an "InvalidStateError" DOMException.
        if !self.object_store.has_index_handle(self) ||
            !transaction
                .get_db()
                .object_store_exists(&self.object_store.get_name())
        {
            return Err(Error::InvalidState(Some(
                "Index or its object store has been deleted".to_owned(),
            )));
        }

        let mut stored_name = self.name.borrow_mut();
        // Step 7: If index’s name is equal to name, terminate these steps.
        if *stored_name == name {
            return Ok(());
        }

        // Step 8: If an index named name already exists in index’s object store, throw a "ConstraintError" DOMException.
        if self.object_store.has_index(&name) {
            return Err(Error::Constraint(Some(
                "An index with the given name already exists".to_owned(),
            )));
        }

        // Step 9: Set index’s name to name.
        self.object_store.rename_index(&stored_name, &name);

        // Step 10: Set this’s name to name.
        *stored_name = name;
        Ok(())
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-objectstore>
    fn ObjectStore(&self) -> DomRoot<IDBObjectStore> {
        self.object_store.as_rooted()
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-multientry>
    fn MultiEntry(&self) -> bool {
        self.multi_entry
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-unique>
    fn Unique(&self) -> bool {
        self.unique
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-keypath>
    fn KeyPath(&self, cx: &mut JSContext, mut retval: MutableHandleValue) {
        // Partii: return the same object each time.
        if !self.key_path_value.get().is_undefined() {
            retval.set(self.key_path_value.get());
            return;
        }
        match &self.key_path {
            KeyPath::String(string) => {
                string.to_jsval(cx, retval.reborrow());
            },
            KeyPath::StringSequence(sequence) => {
                sequence.to_jsval(cx, retval.reborrow());
            },
        }
        self.key_path_value.set(retval.get());
    }

    /// Partii: <https://w3c.github.io/IndexedDB/#dom-idbindex-get>
    fn Get(&self, cx: &mut JSContext, query: HandleValue) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1-4. The index exists and the transaction is active.
        self.check_readable()?;
        // Step 5. Let range be the result of converting a value to a key range with query and
        // true. Rethrow any exceptions.
        let key_range = convert_value_to_key_range(cx, query, Some(true))?;
        // Step 6. Let operation be an algorithm to run retrieve a referenced value from an index
        // with the current Realm record, index, and range.
        // Step 7. Return the result (an IDBRequest) of running asynchronously execute a request
        // with this and operation.
        self.read_request(cx, |callback, index_name| AsyncReadOnlyOperation::GetItem {
            callback,
            key_range,
            index_name,
        })
    }

    /// Partii: <https://w3c.github.io/IndexedDB/#dom-idbindex-getkey>
    fn GetKey(&self, cx: &mut JSContext, query: HandleValue) -> Fallible<DomRoot<IDBRequest>> {
        self.check_readable()?;
        let key_range = convert_value_to_key_range(cx, query, Some(true))?;
        // Retrieve a value from an index: the primary key of the first record in range.
        self.read_request(cx, |callback, index_name| AsyncReadOnlyOperation::GetKey {
            callback,
            key_range,
            index_name,
        })
    }

    /// Partii: <https://w3c.github.io/IndexedDB/#dom-idbindex-getall>
    fn GetAll(
        &self,
        cx: &mut JSContext,
        query_or_options: HandleValue,
        count: Option<u32>,
    ) -> Fallible<DomRoot<IDBRequest>> {
        self.check_readable()?;
        let arguments = get_all_arguments(cx, query_or_options, count)?;
        self.object_store
            .get_all_request(cx, Some(self), GetAllKind::Value, arguments)
    }

    /// Partii: <https://w3c.github.io/IndexedDB/#dom-idbindex-getallkeys>
    fn GetAllKeys(
        &self,
        cx: &mut JSContext,
        query_or_options: HandleValue,
        count: Option<u32>,
    ) -> Fallible<DomRoot<IDBRequest>> {
        self.check_readable()?;
        let arguments = get_all_arguments(cx, query_or_options, count)?;
        self.object_store
            .get_all_request(cx, Some(self), GetAllKind::Key, arguments)
    }

    /// Partii: <https://w3c.github.io/IndexedDB/#dom-idbindex-getallrecords>
    fn GetAllRecords(
        &self,
        cx: &mut JSContext,
        options: &IDBGetAllOptions,
    ) -> Fallible<DomRoot<IDBRequest>> {
        self.check_readable()?;
        let arguments = get_all_options_arguments(cx, options)?;
        self.object_store
            .get_all_request(cx, Some(self), GetAllKind::Record, arguments)
    }

    /// Partii: <https://w3c.github.io/IndexedDB/#dom-idbindex-count>
    fn Count(&self, cx: &mut JSContext, query: HandleValue) -> Fallible<DomRoot<IDBRequest>> {
        self.check_readable()?;
        // Step 5. Let range be the result of converting a value to a key range with query.
        let key_range = convert_value_to_key_range(cx, query, None)?;
        self.read_request(cx, |callback, index_name| AsyncReadOnlyOperation::Count {
            callback,
            key_range,
            index_name,
        })
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbindex-opencursor>
    fn OpenCursor(
        &self,
        cx: &mut JSContext,
        query: HandleValue,
        direction: IDBCursorDirection,
    ) -> Fallible<DomRoot<IDBRequest>> {
        self.open_cursor(cx, query, direction, false)
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbindex-openkeycursor>
    fn OpenKeyCursor(
        &self,
        cx: &mut JSContext,
        query: HandleValue,
        direction: IDBCursorDirection,
    ) -> Fallible<DomRoot<IDBRequest>> {
        self.open_cursor(cx, query, direction, true)
    }
}
