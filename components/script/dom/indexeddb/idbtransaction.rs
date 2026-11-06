/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};

use dom_struct::dom_struct;
use js::context::JSContext;
use js::jsval::UndefinedValue;
use profile_traits::generic_callback::GenericCallback;
use profile_traits::generic_channel::channel;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericUnionTypes::StringOrStringSequence;
use script_bindings::reflector::reflect_dom_object;
use servo_base::generic_channel::{GenericSend, GenericSender, SendError, SendResult};
use servo_base::id::ScriptEventLoopId;
use storage_traits::indexeddb::{
    AsyncOperation, AsyncReadOnlyOperation, AsyncReadWriteOperation, BackendError, BackendResult,
    IndexKeys, IndexedDBDirection, IndexedDBIndex, IndexedDBRecord, IndexedDBThreadMsg,
    IndexedDBTxnMode, KeyPath, SyncOperation, TxnCompleteMsg,
};
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::DOMStringListBinding::DOMStringListMethods;
use crate::dom::bindings::codegen::Bindings::IDBDatabaseBinding::{
    IDBObjectStoreParameters, IDBTransactionDurability,
};
use crate::dom::bindings::codegen::Bindings::IDBObjectStoreBinding::IDBIndexParameters;
use crate::dom::bindings::codegen::Bindings::IDBTransactionBinding::{
    IDBTransactionMethods, IDBTransactionMode,
};
use crate::dom::bindings::error::{Error, Fallible, create_dom_exception};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::structuredclone;
use crate::dom::domexception::DOMException;
use crate::dom::domstringlist::DOMStringList;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::indexeddb::idbdatabase::IDBDatabase;
use crate::dom::indexeddb::idbobjectstore::{
    IDBObjectStore, IDBObjectStoreAbortState, KeyPath as ScriptKeyPath,
};
use crate::dom::indexeddb::idbrequest::{IDBRequest, notify_request_handled};
use crate::dom::indexeddb::key::{index_keys_for_value, map_backend_error_to_dom_error};
use crate::realms::enter_auto_realm;

/// Partii: indexes of one object store whose records script must compute and write: their name,
/// key path and multiEntry flag.
#[derive(Clone)]
pub(crate) struct PopulateIndexes {
    pub(crate) store_name: String,
    pub(crate) indexes: Vec<(String, KeyPath, bool)>,
    /// Abort the transaction with a ConstraintError when a unique index would hold a key twice
    /// (createIndex); otherwise leave such records out of the index.
    pub(crate) fail_on_clash: bool,
}

/// Partii: a message held back while an index is being populated.
enum HeldMessage {
    Message(IndexedDBThreadMsg),
    Populate(PopulateIndexes),
}

#[dom_struct]
pub struct IDBTransaction {
    eventtarget: EventTarget,
    object_store_names: Dom<DOMStringList>,
    mode: IDBTransactionMode,
    durability: IDBTransactionDurability,
    db: Dom<IDBDatabase>,
    error: MutNullableDom<DOMException>,

    store_handles: DomRefCell<HashMap<String, Dom<IDBObjectStore>>>,
    // https://www.w3.org/TR/IndexedDB-3/#transaction-request-list
    requests: DomRefCell<Vec<Dom<IDBRequest>>>,
    // https://www.w3.org/TR/IndexedDB-3/#transaction-active-flag
    active: Cell<bool>,
    // https://www.w3.org/TR/IndexedDB-3/#transaction-finish
    finished: Cell<bool>,
    abort_initiated: Cell<bool>,
    abort_requested: Cell<bool>,
    committing: Cell<bool>,
    commit_started: Cell<bool>,
    version_change_old_version: Cell<Option<u64>>,
    // https://w3c.github.io/IndexedDB/#abort-an-upgrade-transaction
    // Step 4. NOTE: This reverts the value of objectStoreNames returned by the IDBDatabase object.
    version_change_old_object_store_names: DomRefCell<Option<Vec<DOMString>>>,
    // https://w3c.github.io/IndexedDB/#transaction-concept
    // “A transaction optionally has a cleanup event loop which is an event loop.”
    #[no_trace]
    cleanup_event_loop: Cell<Option<ScriptEventLoopId>>,
    registered_in_global: Cell<bool>,
    // Tracks how many IDBRequest instances are still pending for this
    // transaction. The value is incremented when a request is added to the
    // transaction’s request list and decremented once the request has
    // finished.
    pending_request_count: Cell<usize>,
    next_request_id: Cell<u64>,
    // Smallest request_id that has not yet been marked handled (all < this are handled).
    next_unhandled_request_id: Cell<u64>,
    handled_pending: DomRefCell<HashSet<u64>>,
    /// Partii: set while an index is being populated (see `populate_indexes`).
    populating_index: Cell<bool>,
    /// Partii: the requests and schema changes placed while an index is being populated, and
    /// further indexes to populate. They go to the backend in order once it is done.
    #[no_trace]
    #[ignore_malloc_size_of = "messages to the storage thread"]
    held_messages: DomRefCell<VecDeque<HeldMessage>>,

    // An unique identifier, used to commit and revert this transaction
    // FIXME:(rasviitanen) Replace this with a channel
    serial_number: u64,
}

impl IDBTransaction {
    fn new_inherited(
        connection: &IDBDatabase,
        mode: IDBTransactionMode,
        durability: IDBTransactionDurability,
        scope: &DOMStringList,
        serial_number: u64,
    ) -> IDBTransaction {
        IDBTransaction {
            eventtarget: EventTarget::new_inherited(),
            object_store_names: Dom::from_ref(scope),
            mode,
            durability,
            db: Dom::from_ref(connection),
            error: Default::default(),

            store_handles: Default::default(),
            requests: Default::default(),
            active: Cell::new(true),
            finished: Cell::new(false),
            abort_initiated: Cell::new(false),
            abort_requested: Cell::new(false),
            committing: Cell::new(false),
            commit_started: Cell::new(false),
            version_change_old_version: Cell::new(None),
            version_change_old_object_store_names: DomRefCell::new(
                (mode == IDBTransactionMode::Versionchange)
                    .then(|| connection.object_store_names_snapshot()),
            ),
            cleanup_event_loop: Cell::new(None),
            registered_in_global: Cell::new(false),
            pending_request_count: Cell::new(0),
            next_request_id: Cell::new(0),
            next_unhandled_request_id: Cell::new(0),
            handled_pending: Default::default(),
            populating_index: Cell::new(false),
            held_messages: Default::default(),
            serial_number,
        }
    }

    /// Does a blocking call to create a backend transaction and get its id.
    pub fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        connection: &IDBDatabase,
        mode: IDBTransactionMode,
        durability: IDBTransactionDurability,
        scope: &DOMStringList,
    ) -> DomRoot<IDBTransaction> {
        let serial_number = IDBTransaction::create_transaction(global, connection, mode, scope);
        IDBTransaction::new_with_serial(
            cx,
            global,
            connection,
            mode,
            durability,
            scope,
            serial_number,
        )
    }

    pub(crate) fn new_with_serial(
        cx: &mut JSContext,
        global: &GlobalScope,
        connection: &IDBDatabase,
        mode: IDBTransactionMode,
        durability: IDBTransactionDurability,
        scope: &DOMStringList,
        serial_number: u64,
    ) -> DomRoot<IDBTransaction> {
        reflect_dom_object(
            cx,
            Box::new(IDBTransaction::new_inherited(
                connection,
                mode,
                durability,
                scope,
                serial_number,
            )),
            global,
        )
    }

    fn create_transaction(
        global: &GlobalScope,
        connection: &IDBDatabase,
        mode: IDBTransactionMode,
        scope: &DOMStringList,
    ) -> u64 {
        let backend_mode = match mode {
            IDBTransactionMode::Readonly => IndexedDBTxnMode::Readonly,
            IDBTransactionMode::Readwrite => IndexedDBTxnMode::Readwrite,
            IDBTransactionMode::Versionchange => IndexedDBTxnMode::Versionchange,
        };
        let scope: Vec<String> = (0..scope.Length())
            .filter_map(|i| scope.Item(i))
            .map(String::from)
            .collect();
        let (sender, receiver) = channel(global.time_profiler_chan().clone()).unwrap();

        global
            .storage_threads()
            .send(IndexedDBThreadMsg::Sync(SyncOperation::CreateTransaction {
                sender,
                origin: global.origin().immutable().clone(),
                db_name: String::from(connection.get_name()),
                mode: backend_mode,
                scope,
                connection: connection.id(),
            }))
            .expect("Failed to send IndexedDBThreadMsg::Sync");

        receiver.recv().unwrap().expect("CreateTransaction failed")
    }

    /// <https://w3c.github.io/IndexedDB/#transaction-lifecycle>
    pub fn set_active_flag(&self, status: bool) {
        // inactive
        // A transaction is in this state after control returns to the event loop after its creation,
        //  and when events are not being dispatched.
        // No requests can be made against the transaction when it is in this state.
        self.active.set(status);
    }

    pub fn is_active(&self) -> bool {
        self.active.get()
    }

    /// <https://w3c.github.io/IndexedDB/#transaction-lifetime>
    pub(crate) fn is_usable(&self) -> bool {
        // A transaction can be aborted at any time before it is finished,
        //  even if the transaction isn’t currently active or hasn’t yet started.
        // An explicit call to abort() will initiate an abort.
        // An abort will also be initiated following a failed request that is not handled by script.
        !self.finished.get() && !self.abort_initiated.get() && !self.committing.get()
    }

    pub(crate) fn is_inactive(&self) -> bool {
        !self.active.get() &&
            !self.finished.get() &&
            !self.abort_initiated.get() &&
            !self.committing.get()
    }

    pub(crate) fn is_committing(&self) -> bool {
        self.committing.get()
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.finished.get()
    }

    pub(crate) fn set_cleanup_event_loop(&self) {
        // https://w3c.github.io/IndexedDB/#transaction-concept
        // A transaction optionally has a cleanup event loop which is an event loop.
        self.cleanup_event_loop.set(ScriptEventLoopId::installed());
    }

    pub(crate) fn clear_cleanup_event_loop(&self) {
        // https://w3c.github.io/IndexedDB/#cleanup-indexed-database-transactions
        // Clear transaction’s cleanup event loop.
        self.cleanup_event_loop.set(None);
    }

    pub(crate) fn cleanup_event_loop_matches_current(&self) -> bool {
        match ScriptEventLoopId::installed() {
            Some(current) => self.cleanup_event_loop.get() == Some(current),
            None => false,
        }
    }

    pub(crate) fn set_registered_in_global(&self) {
        self.registered_in_global.set(true);
    }

    pub(crate) fn clear_registered_in_global(&self) {
        self.registered_in_global.set(false);
    }

    pub(crate) fn set_versionchange_old_version(&self, version: u64) {
        self.version_change_old_version.set(Some(version));
    }

    pub(crate) fn register_object_store_handle(&self, name: &DOMString, store: &IDBObjectStore) {
        self.store_handles
            .borrow_mut()
            .insert(name.to_string(), Dom::from_ref(store));
    }

    pub(crate) fn rename_object_store_handle_cache(
        &self,
        old_name: &DOMString,
        new_name: &DOMString,
        store: &IDBObjectStore,
    ) {
        let mut store_handles = self.store_handles.borrow_mut();
        store_handles.remove(&old_name.to_string());
        store_handles.insert(new_name.to_string(), Dom::from_ref(store));
    }

    /// <https://w3c.github.io/IndexedDB/#abort-an-upgrade-transaction>
    fn restore_associated_object_store_handles_after_abort(&self, cx: &mut JSContext) {
        // Step 5. For each object store handle handle associated with transaction,
        // including those for object stores that were created or deleted during
        // transaction:
        let stores = self
            .store_handles
            .borrow()
            .values()
            .map(|store| DomRoot::from_ref(&**store))
            .collect::<Vec<_>>();
        for store in stores {
            store.restore_metadata_after_abort(cx);
        }
    }

    fn attempt_commit(&self) -> bool {
        if self.commit_started.get() {
            return true;
        }
        let this = Trusted::new(self);
        let global = self.global();
        let task_source = global
            .task_manager()
            .dom_manipulation_task_source()
            .to_sendable();

        // TODO: Reuse a shared transaction callback path (similar to IDBFactory
        // connection callbacks) instead of creating one per transaction operation.
        let callback = GenericCallback::new(move |message: Result<TxnCompleteMsg, SendError>| {
            let this = this.clone();
            let task_source = task_source.clone();
            task_source.queue(task!(handle_commit_result: move |cx| {
                let this = this.root();
                let message = message.expect("Could not unwrap message");
                match message.result {
                    Ok(()) => {
                        this.finalize_commit();
                    }
                    Err(_err) => {
                         // TODO: Map backend commit/rollback failure to an appropriate DOMException
                        this.initiate_abort(cx, Error::Operation(None));

                        this.finalize_abort();
                    }
                }
                // TODO: https://w3c.github.io/IndexedDB/#commit-a-transaction
                // Backend commit/rollback is not yet atomic.
            }));
        })
        .expect("Could not create callback");

        let commit_operation = SyncOperation::Commit(
            callback,
            global.origin().immutable().clone(),
            String::from(self.db.get_name()),
            self.serial_number,
        );

        // https://w3c.github.io/IndexedDB/#transaction-lifecycle
        // When committing, the transaction state is set to committing.
        let send_result = self
            .get_idb_thread()
            .send(IndexedDBThreadMsg::Sync(commit_operation));
        if send_result.is_err() {
            return false;
        }

        self.committing.set(true);
        self.commit_started.set(true);
        true
    }

    pub(crate) fn maybe_commit(&self, cx: &mut JSContext) {
        // https://w3c.github.io/IndexedDB/#transaction-lifetime
        // Step 5: transaction when all requests
        //  placed against the transaction have completed and their returned results handled,
        //  no new requests have been placed against the transaction, and the transaction has
        //  not been aborted.
        let finished = self.finished.get();
        let abort_initiated = self.abort_initiated.get();
        let commit_started = self.commit_started.get();
        let active = self.active.get();
        let pending_request_count = self.pending_request_count.get();
        let next_unhandled_request_id = self.next_unhandled_request_id.get();
        let issued_count = self.issued_count();
        if finished || abort_initiated || commit_started {
            return;
        }
        if active || pending_request_count != 0 {
            return;
        }
        if next_unhandled_request_id != issued_count {
            return;
        }
        if !self.attempt_commit() {
            // We failed to initiate the commit algorithm (backend task could not be queued),
            // so the transaction cannot progress to a successful "complete".
            // Choose the most appropriate DOMException mapping for Servo here.
            self.initiate_abort(cx, Error::InvalidState(None));
            self.finalize_abort();
        }
    }

    fn force_commit(&self) {
        // https://w3c.github.io/IndexedDB/#transaction-lifetime
        // An explicit call to commit() will initiate a commit without waiting for request results
        //  to be handled by script.
        //
        // This differs from automatic commit:
        // The implementation must attempt to commit an inactive transaction when all requests
        // placed against the transaction have completed and their returned results handled,
        // no new requests have been placed against the transaction, and the transaction has not been aborted
        if self.finished.get() || self.abort_initiated.get() || self.commit_started.get() {
            return;
        }
        if self.active.get() || self.pending_request_count.get() != 0 {
            return;
        }
        self.attempt_commit();
    }

    pub fn get_mode(&self) -> IDBTransactionMode {
        self.mode
    }

    pub fn get_db_name(&self) -> DOMString {
        self.db.get_name()
    }

    pub(crate) fn get_db(&self) -> &IDBDatabase {
        &self.db
    }

    pub fn get_serial_number(&self) -> u64 {
        self.serial_number
    }

    pub(crate) fn issued_count(&self) -> u64 {
        self.next_request_id.get()
    }

    /// request_id is only required to be unique within this transaction.
    /// The backend keys “handled” state by (txn, request_id).
    pub(crate) fn allocate_request_id(&self) -> u64 {
        let id = self.next_request_id.get();
        self.next_request_id.set(id + 1);
        id
    }

    pub(crate) fn mark_request_handled(&self, request_id: u64) {
        let current = self.next_unhandled_request_id.get();
        if request_id == current {
            let mut next = current + 1;
            {
                let mut pending = self.handled_pending.borrow_mut();
                while pending.remove(&next) {
                    next += 1;
                }
            }
            self.next_unhandled_request_id.set(next);
        } else if request_id > current {
            self.handled_pending.borrow_mut().insert(request_id);
        }
    }

    pub fn add_request(&self, request: &IDBRequest) {
        // Partii: a cursor's request is added again each time the cursor iterates; keep only
        // its latest place in the list, so an abort fires its error event in request order.
        let mut requests = self.requests.borrow_mut();
        if request.set_listed() {
            requests.retain(|existing| !std::ptr::eq(&**existing, request));
        }
        requests.push(Dom::from_ref(request));
        drop(requests);
        // Increase the number of outstanding requests so that we can detect when
        // the transaction is allowed to finish.
        self.pending_request_count
            .set(self.pending_request_count.get() + 1);
    }

    pub fn request_finished(&self) {
        // https://w3c.github.io/IndexedDB/#transaction-lifecycle
        // finished
        // Once a transaction has committed or aborted, it enters this state.
        // No requests can be made against the transaction when it is in this state.
        if self.pending_request_count.get() == 0 {
            return;
        }
        let remaining = self.pending_request_count.get() - 1;
        self.pending_request_count.set(remaining);
    }

    /// Partii: send a request or schema change of this transaction to the backend. While an
    /// index is being populated it waits, so that it runs after the index records are written.
    pub(crate) fn send_to_backend(&self, message: IndexedDBThreadMsg) -> SendResult {
        if self.populating_index.get() {
            self.held_messages
                .borrow_mut()
                .push_back(HeldMessage::Message(message));
            return Ok(());
        }
        self.get_idb_thread().send(message)
    }

    fn backend_mode(&self) -> IndexedDBTxnMode {
        match self.mode {
            IDBTransactionMode::Readonly => IndexedDBTxnMode::Readonly,
            IDBTransactionMode::Readwrite => IndexedDBTxnMode::Readwrite,
            IDBTransactionMode::Versionchange => IndexedDBTxnMode::Versionchange,
        }
    }

    /// Partii: write the records of indexes that have none yet: a new index (createIndex step
    /// 11, "populate the index with the records of the store"), or one of a database written
    /// before index records existed. Keys come from key paths evaluated on the stored values,
    /// which only script can do, so:
    /// 1. read all records of the store, in this transaction's request order;
    /// 2. hold back every request placed after this point (`send_to_backend`);
    /// 3. in a task, deserialize the values and compute their index keys;
    /// 4. send the index records (SetIndexEntries), then the held requests.
    ///
    /// Two internal request ids keep the transaction from committing meanwhile.
    pub(crate) fn populate_indexes(&self, job: PopulateIndexes) {
        if self.finished.get() || self.abort_initiated.get() {
            return;
        }
        if self.populating_index.get() {
            self.held_messages
                .borrow_mut()
                .push_back(HeldMessage::Populate(job));
            return;
        }
        self.populating_index.set(true);
        let scan_id = self.allocate_request_id();
        let write_id = self.allocate_request_id();
        self.pending_request_count
            .set(self.pending_request_count.get() + 1);

        let global = self.global();
        let this = Trusted::new(self);
        let task_source = global
            .task_manager()
            .database_access_task_source()
            .to_sendable();
        let store_name = job.store_name.clone();
        let callback = GenericCallback::new(
            move |message: Result<BackendResult<Vec<IndexedDBRecord>>, SendError>| {
                let this = this.clone();
                let job = job.clone();
                task_source.queue(task!(populate_index_records: move |cx| {
                    let this = this.root();
                    let records = match message {
                        Ok(Ok(records)) => Some(records),
                        _ => None,
                    };
                    this.write_index_records(cx, job, records, scan_id, write_id);
                }));
            },
        )
        .expect("Could not create callback");
        let operation = AsyncOperation::ReadOnly(AsyncReadOnlyOperation::GetAllRecords {
            callback,
            key_range: Default::default(),
            count: None,
            index_name: None,
            direction: IndexedDBDirection::Next,
        });
        let _ = self.get_idb_thread().send(IndexedDBThreadMsg::Async(
            global.origin().immutable().clone(),
            String::from(self.db.get_name()),
            store_name,
            self.serial_number,
            scan_id,
            self.backend_mode(),
            operation,
        ));
    }

    /// Partii: step 3 and 4 of `populate_indexes`.
    fn write_index_records(
        &self,
        cx: &mut JSContext,
        job: PopulateIndexes,
        records: Option<Vec<IndexedDBRecord>>,
        scan_id: u64,
        write_id: u64,
    ) {
        if self.finished.get() || self.abort_initiated.get() {
            self.finish_populating_indexes(cx, false, scan_id, write_id);
            return;
        }
        let Some(records) = records else {
            self.initiate_abort(cx, Error::Operation(None));
            self.request_backend_abort();
            self.finish_populating_indexes(cx, false, scan_id, write_id);
            return;
        };
        let global = self.global();
        let mut realm = enter_auto_realm(cx, self);
        let cx: &mut JSContext = &mut realm;
        let indexes: Vec<(String, ScriptKeyPath, bool)> = job
            .indexes
            .iter()
            .map(|(name, key_path, multi_entry)| {
                (name.clone(), key_path.clone().into(), *multi_entry)
            })
            .collect();
        let mut entries = Vec::with_capacity(records.len());
        for record in records {
            rooted!(&in(cx) let mut value = UndefinedValue());
            let read = postcard::from_bytes(&record.value)
                .map_err(|_| Error::Data(None))
                .and_then(|data| structuredclone::read(cx, &global, data, value.handle_mut()));
            if read.is_err() {
                warn!("Could not read an IndexedDB record to populate an index");
                continue;
            }
            let index_keys: IndexKeys = indexes
                .iter()
                .filter_map(|(name, key_path, multi_entry)| {
                    let keys = index_keys_for_value(cx, value.handle(), key_path, *multi_entry);
                    (!keys.is_empty()).then(|| (name.clone(), keys))
                })
                .collect();
            if !index_keys.is_empty() {
                entries.push((record.key, index_keys));
            }
        }

        let this = Trusted::new(self);
        let task_source = global
            .task_manager()
            .database_access_task_source()
            .to_sendable();
        let callback =
            GenericCallback::new(move |message: Result<BackendResult<bool>, SendError>| {
                let this = this.clone();
                task_source.queue(task!(populated_index: move |cx| {
                    let this = this.root();
                    match message {
                        Ok(Ok(true)) => {},
                        Ok(Ok(false)) => {
                            // A unique index would hold a key twice: createIndex aborts the
                            // upgrade transaction with a ConstraintError.
                            this.initiate_abort(cx, Error::Constraint(None));
                            this.request_backend_abort();
                        },
                        _ => {
                            this.initiate_abort(cx, Error::Operation(None));
                            this.request_backend_abort();
                        },
                    }
                    let flush = !this.abort_initiated.get();
                    this.finish_populating_indexes(cx, flush, scan_id, write_id);
                }));
            })
            .expect("Could not create callback");
        let operation = AsyncOperation::ReadWrite(AsyncReadWriteOperation::SetIndexEntries {
            callback,
            index_names: job.indexes.into_iter().map(|(name, _, _)| name).collect(),
            entries,
            fail_on_clash: job.fail_on_clash,
        });
        let _ = self.get_idb_thread().send(IndexedDBThreadMsg::Async(
            global.origin().immutable().clone(),
            String::from(self.db.get_name()),
            job.store_name,
            self.serial_number,
            write_id,
            self.backend_mode(),
            operation,
        ));
    }

    /// Partii: end `populate_indexes`: send the held messages (or drop them when the transaction
    /// aborted), then let the transaction commit once everything is handled.
    fn finish_populating_indexes(
        &self,
        cx: &mut JSContext,
        flush: bool,
        scan_id: u64,
        write_id: u64,
    ) {
        self.populating_index.set(false);
        self.request_finished();
        if flush && !self.finished.get() {
            loop {
                let next = self.held_messages.borrow_mut().pop_front();
                match next {
                    None => break,
                    Some(HeldMessage::Message(message)) => {
                        let _ = self.get_idb_thread().send(message);
                    },
                    Some(HeldMessage::Populate(job)) => {
                        self.populate_indexes(job);
                        break;
                    },
                }
            }
        } else {
            // The backend dropped this transaction's queued requests too.
            self.held_messages.borrow_mut().clear();
        }
        notify_request_handled(cx, self, scan_id);
        notify_request_handled(cx, self, write_id);
    }

    pub(crate) fn initiate_abort(&self, cx: &mut JSContext, error: Error) {
        // https://w3c.github.io/IndexedDB/#transaction-lifetime
        // Step 4: An abort will also be initiated following a failed request that is not handled by script.
        // A transaction can be aborted at any time before it is finished,
        // even if the transaction isn’t currently active or hasn’t yet started.
        if self.finished.get() || self.abort_initiated.get() {
            return;
        }
        if self.mode == IDBTransactionMode::Versionchange {
            // https://w3c.github.io/IndexedDB/#abort-an-upgrade-transaction
            // Step 4. Set connection’s object store set to the set of object stores in database if database previously existed,
            // or the empty set if database was newly created.
            if let Some(names) = self
                .version_change_old_object_store_names
                .borrow()
                .as_ref()
                .cloned()
            {
                self.db.restore_object_store_names(names);
            }
            self.restore_associated_object_store_handles_after_abort(cx);
        }
        self.abort_initiated.set(true);
        // https://w3c.github.io/IndexedDB/#transaction-concept
        // A transaction has a error which is set if the transaction is aborted.
        // NOTE: Implementors need to keep in mind that the value "null" is considered an error, as it is set from abort()
        if self.error.get().is_none() &&
            let Ok(exception) = create_dom_exception(cx, &self.global(), error)
        {
            self.error.set(Some(&exception));
        }
        self.abort_pending_requests();
    }

    /// Partii: <https://w3c.github.io/IndexedDB/#abort-a-transaction> step 6: every request that
    /// is not done yet gets an AbortError, in request order, before the transaction's abort
    /// event. This runs on the script side from the request list, because the backend drops the
    /// queued requests of an aborted transaction without answering them; answers that do still
    /// arrive are ignored (see `IDBTransaction::is_aborted_or_finished`).
    fn abort_pending_requests(&self) {
        let pending: Vec<Trusted<IDBRequest>> = self
            .requests
            .borrow()
            .iter()
            .filter(|request| !request.is_done())
            .map(|request| Trusted::new(&**request))
            .collect();
        let global = self.global();
        let task_manager = global.task_manager();
        let task_source = task_manager.database_access_task_source();
        for request in pending {
            task_source.queue(task!(abort_pending_request: move |cx| {
                request.root().abort_with_abort_error(cx);
            }));
        }
    }

    /// Partii: whether the transaction was aborted or finished, after which the backend's
    /// answers to its requests are ignored.
    pub(crate) fn is_aborted_or_finished(&self) -> bool {
        self.abort_initiated.get() || self.finished.get()
    }

    pub(crate) fn request_backend_abort(&self) {
        if self.abort_requested.get() {
            return;
        }
        self.abort_requested.set(true);
        let this = Trusted::new(self);
        let global = self.global();
        let task_source = global
            .task_manager()
            .dom_manipulation_task_source()
            .to_sendable();
        let callback = GenericCallback::new(move |message: Result<TxnCompleteMsg, SendError>| {
            let this = this.clone();
            let task_source = task_source.clone();
            task_source.queue(task!(handle_abort_result: move || {
                let this = this.root();
                let _ = message.expect("Could not unwrap message");
                this.finalize_abort();
            }));
        })
        .expect("Could not create callback");
        let operation = SyncOperation::Abort(
            callback,
            global.origin().immutable().clone(),
            String::from(self.db.get_name()),
            self.serial_number,
        );
        let _ = self
            .get_idb_thread()
            .send(IndexedDBThreadMsg::Sync(operation));
    }

    fn notify_backend_transaction_finished(&self) {
        let global = self.global();
        let _ = self.get_idb_thread().send(IndexedDBThreadMsg::Sync(
            SyncOperation::TransactionFinished {
                origin: global.origin().immutable().clone(),
                db_name: String::from(self.db.get_name()),
                txn: self.serial_number,
            },
        ));
    }

    pub(crate) fn finalize_abort(&self) {
        if self.finished.get() {
            return;
        }
        self.committing.set(false);
        self.commit_started.set(false);
        let this = Trusted::new(self);
        // Partii: a database task, queued after the error events of the pending requests
        // (`abort_pending_requests`).
        self.global()
            .task_manager()
            .database_access_task_source()
            .queue(task!(send_abort_notification: move |cx| {
                let this = this.root();
                // Partii: finalize_abort can be reached twice (a failed commit and the backend's
                // abort answer); fire abort only once.
                if this.finished.get() {
                    return;
                }
                this.active.set(false);
                if this.mode == IDBTransactionMode::Versionchange {
                    if let Some(old_version) = this.version_change_old_version.get() {
                        // IndexedDB §5.8 "Aborting an upgrade transaction":
                        // set connection's version to database's version (or 0 if newly created).
                        // Spec note: this reverts the value of `IDBDatabase.version`.
                        // https://w3c.github.io/IndexedDB/#abort-an-upgrade-transaction
                        this.db.set_version(old_version);
                    }
                    this.db.clear_upgrade_transaction(&this);
                }
                let global = this.global();
                // https://w3c.github.io/IndexedDB/#abort-a-transaction step 7.2: fire an event
                // named abort at transaction with its bubbles attribute initialized to true.
                // Partii: it bubbles to the connection.
                let event = Event::new(
                    cx,
                    &global,
                    Atom::from("abort"),
                    EventBubbles::Bubbles,
                    EventCancelable::NotCancelable,
                );
                event.fire(cx, this.upcast());
                if this.mode == IDBTransactionMode::Versionchange {
                    this.global()
                        .ensure_indexeddb_factory(cx)
                        .clear_open_request_transaction_for_txn(&this);
                    let origin = this.global().origin().immutable().clone();
                    let db_name = String::from(this.db.get_name());
                    let txn = this.serial_number;
                    let _ = this.get_idb_thread().send(IndexedDBThreadMsg::Sync(
                        SyncOperation::UpgradeTransactionFinished {
                            origin,
                            db_name,
                            txn,
                            committed: false,
                        },
                    ));
                }
                // https://w3c.github.io/IndexedDB/#transaction-lifetime
                // Step 6: When a transaction is committed or aborted, its state is set to finished.
                this.finished.set(true);
                this.version_change_old_version.set(None);
                this.version_change_old_object_store_names.borrow_mut().take();
                this.notify_backend_transaction_finished();
                if this.registered_in_global.get() {
                    this.global().ensure_indexeddb_factory(cx).unregister_indexeddb_transaction(&this);
                }
            }));
    }

    pub(crate) fn finalize_commit(&self) {
        if self.finished.get() {
            return;
        }
        self.dispatch_complete();
    }

    fn dispatch_complete(&self) {
        let global = self.global();
        let this = Trusted::new(self);
        global.task_manager().database_access_task_source().queue(
            task!(send_complete_notification: move |cx| {
                let this = this.root();
                this.committing.set(false);
                this.commit_started.set(false);
                this.version_change_old_version.set(None);
                this.version_change_old_object_store_names
                    .borrow_mut()
                    .take();
                if this.mode == IDBTransactionMode::Versionchange {
                    // https://w3c.github.io/IndexedDB/#commit-transaction
                    // Step 5.1: If transaction is an upgrade transaction, then set transaction’s connection’s
                    // associated database’s upgrade transaction to null.
                    this.db.clear_upgrade_transaction(&this);
                }
                // https://w3c.github.io/IndexedDB/#commit-transaction
                // Step 5.2: Set transaction’s state to finished.
                this.finished.set(true);
                let global = this.global();
                let event = Event::new(
                    cx,
                    &global,
                    Atom::from("complete"),
                    EventBubbles::DoesNotBubble,
                    EventCancelable::NotCancelable,
                );
                // https://w3c.github.io/IndexedDB/#commit-transaction
                // Step 5.3: Fire an event named complete at transaction.
                event.fire(cx, this.upcast());
                if this.mode == IDBTransactionMode::Versionchange {
                    // https://w3c.github.io/IndexedDB/#commit-transaction
                    //  Step 5.1: If transaction is an upgrade transaction, then let request be the request
                    // associated with transaction and set request’s transaction to null.
                    this.global()
                        .ensure_indexeddb_factory(cx)
                        .clear_open_request_transaction_for_txn(&this);
                    let origin = this.global().origin().immutable().clone();
                    let db_name = String::from(this.db.get_name());
                    let txn = this.serial_number;
                    let _ = this.get_idb_thread().send(IndexedDBThreadMsg::Sync(
                        SyncOperation::UpgradeTransactionFinished {
                            origin,
                            db_name,
                            txn,
                            committed: true,
                        },
                    ));
                }
                this.notify_backend_transaction_finished();
                if this.registered_in_global.get() {
                    this.global().ensure_indexeddb_factory(cx).unregister_indexeddb_transaction(&this);
                }
            }),
        );
    }

    fn get_idb_thread(&self) -> GenericSender<IndexedDBThreadMsg> {
        self.global().storage_threads().sender()
    }

    fn object_store_parameters(
        &self,
        object_store_name: &DOMString,
    ) -> Option<(IDBObjectStoreParameters, Vec<IndexedDBIndex>, Option<i64>)> {
        let global = self.global();
        let idb_sender = global.storage_threads().sender();
        let (sender, receiver) =
            channel(global.time_profiler_chan().clone()).expect("failed to create channel");

        let origin = global.origin().immutable().clone();
        let db_name = String::from(self.db.get_name());
        let object_store_name = object_store_name.to_string();

        let operation = SyncOperation::GetObjectStore(sender, origin, db_name, object_store_name);

        let _ = idb_sender.send(IndexedDBThreadMsg::Sync(operation));

        // First unwrap for ipc
        // Second unwrap will never happen unless this db gets manually deleted somehow
        let object_store = receiver.recv().ok()?.ok()?;

        // First unwrap for ipc
        // Second unwrap will never happen unless this db gets manually deleted somehow
        let key_path = object_store.key_path.map(|key_path| match key_path {
            KeyPath::String(string) => StringOrStringSequence::String(string.into()),
            KeyPath::Sequence(seq) => {
                StringOrStringSequence::StringSequence(seq.into_iter().map(Into::into).collect())
            },
        });
        Some((
            IDBObjectStoreParameters {
                autoIncrement: object_store.has_key_generator,
                keyPath: key_path,
            },
            object_store.indexes,
            object_store.key_generator_current_number,
        ))
    }

    pub(crate) fn create_abort_callback(&self) -> GenericCallback<BackendError> {
        let trusted_transaction = Trusted::new(self);
        let task_source = self
            .global()
            .task_manager()
            .storage_task_source()
            .to_sendable();
        GenericCallback::new(move |error: Result<BackendError, SendError>| {
            let Ok(error) = error else {
                return;
            };
            let trusted_transaction = trusted_transaction.clone();
            task_source.queue(task!(delete_failed: move |cx| {
                let transaction = trusted_transaction.root();
                transaction.initiate_abort(cx, map_backend_error_to_dom_error(error));
                transaction.request_backend_abort();
            }));
        })
        .expect("Could not create GenericCallback")
    }
}

impl IDBTransactionMethods<crate::DomTypeHolder> for IDBTransaction {
    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbtransaction-db>
    fn Db(&self) -> DomRoot<IDBDatabase> {
        DomRoot::from_ref(&*self.db)
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbtransaction-objectstore>
    fn ObjectStore(
        &self,
        cx: &mut JSContext,
        name: DOMString,
    ) -> Fallible<DomRoot<IDBObjectStore>> {
        // Step 1: If transaction has finished, throw an "InvalidStateError" DOMException.
        if self.finished.get() || self.abort_initiated.get() {
            return Err(Error::InvalidState(None));
        }

        // Step 2: Check that the object store exists in this transaction's scope.
        // For versionchange transactions, the scope tracks object store changes
        // performed during the upgrade.
        let in_scope = if self.mode == IDBTransactionMode::Versionchange {
            self.db.object_store_exists(&name)
        } else {
            self.object_store_names.Contains(name.clone())
        };
        if !in_scope {
            return Err(Error::NotFound(None));
        }

        // Step 3: Each call to this method on the same
        // IDBTransaction instance with the same name
        // returns the same IDBObjectStore instance.
        if let Some(store) = self.store_handles.borrow().get(&*name.str()) {
            return Ok(DomRoot::from_ref(store));
        }

        let parameters = self.object_store_parameters(&name);
        let store = IDBObjectStore::new(
            cx,
            &self.global(),
            self.db.get_name(),
            name.clone(),
            parameters.as_ref().map(|(params, _, _)| params),
            IDBObjectStoreAbortState {
                newly_created_during_transaction: false,
                rollback_indexes_on_abort: if self.mode == IDBTransactionMode::Versionchange {
                    parameters
                        .as_ref()
                        .map(|(_, indexes, _)| indexes.clone())
                        .unwrap_or_default()
                } else {
                    Vec::new()
                },
                key_generator_current_number: parameters
                    .as_ref()
                    .and_then(|(_, _, key_generator_current_number)| *key_generator_current_number),
            },
            self,
        );
        if let Some(indexes) = parameters.map(|(_, indexes, _)| indexes) {
            // Partii: indexes written by an older build have no records yet; a transaction that
            // can write populates them before its own requests run. Read-only transactions see
            // only the records written since.
            let unpopulated: Vec<(String, KeyPath, bool)> = indexes
                .iter()
                .filter(|index| !index.populated)
                .map(|index| {
                    (
                        index.name.clone(),
                        index.key_path.clone(),
                        index.multi_entry,
                    )
                })
                .collect();
            if !unpopulated.is_empty() && self.mode != IDBTransactionMode::Readonly {
                self.populate_indexes(PopulateIndexes {
                    store_name: name.to_string(),
                    indexes: unpopulated,
                    fail_on_clash: false,
                });
            }
            for index in indexes {
                store.add_index(
                    cx,
                    index.name.into(),
                    &IDBIndexParameters {
                        multiEntry: index.multi_entry,
                        unique: index.unique,
                    },
                    index.key_path.into(),
                );
            }
        }
        self.register_object_store_handle(&name, &store);
        Ok(store)
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbtransaction-commit>
    fn Commit(&self) -> Fallible<()> {
        // Step 1. If this’s state is not active, then throw an "InvalidStateError" DOMException.
        if !self.active.get() {
            return Err(Error::InvalidState(None));
        }

        // Step 2. Run commit a transaction with this.
        self.set_active_flag(false);
        self.committing.set(true);
        self.force_commit();

        Ok(())
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbtransaction-abort>
    fn Abort(&self, cx: &mut JSContext) -> Fallible<()> {
        // Step 1. If this's state is committing or finished, then throw an "InvalidStateError"
        // DOMException.
        // Partii: an aborted transaction is finished from the moment it aborts.
        if self.finished.get() || self.committing.get() || self.abort_initiated.get() {
            return Err(Error::InvalidState(None));
        }
        self.active.set(false);
        self.initiate_abort(cx, Error::Abort(None));
        self.request_backend_abort();

        Ok(())
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbtransaction-objectstorenames>
    fn ObjectStoreNames(&self, cx: &mut JSContext) -> DomRoot<DOMStringList> {
        if self.mode == IDBTransactionMode::Versionchange {
            self.db.object_stores(cx)
        } else {
            self.object_store_names.as_rooted()
        }
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbtransaction-mode>
    fn Mode(&self) -> IDBTransactionMode {
        self.mode
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbtransaction-durability>
    fn Durability(&self) -> IDBTransactionDurability {
        self.durability
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbtransaction-error>
    fn GetError(&self) -> Option<DomRoot<DOMException>> {
        self.error.get()
    }

    // https://www.w3.org/TR/IndexedDB-3/#dom-idbtransaction-onabort
    event_handler!(abort, GetOnabort, SetOnabort);

    // https://www.w3.org/TR/IndexedDB-3/#dom-idbtransaction-oncomplete
    event_handler!(complete, GetOncomplete, SetOncomplete);

    // https://www.w3.org/TR/IndexedDB-3/#dom-idbtransaction-onerror
    event_handler!(error, GetOnerror, SetOnerror);
}
