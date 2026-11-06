/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use std::collections::VecDeque;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use log::{info, warn};
use malloc_size_of::{MallocSizeOf, MallocSizeOfOps};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, Error, OptionalExtension, params};
use sea_query::{Condition, Expr, ExprTrait, IntoCondition, SqliteQueryBuilder};
use sea_query_rusqlite::RusqliteBinder;
use servo_base::threadpool::ThreadPool;
use storage_traits::indexeddb::{
    AsyncOperation, AsyncReadOnlyOperation, AsyncReadWriteOperation, AsyncSchemaOperation,
    BackendError, CreateObjectResult, IndexKeys, IndexedDBDirection, IndexedDBIndex,
    IndexedDBKeyRange, IndexedDBKeyType, IndexedDBRecord, IndexedDBTxnMode, KeyPath, PutItemResult,
};

use crate::indexeddb::IndexedDBDescription;
use crate::indexeddb::engines::{KvsEngine, KvsOperation, KvsTransaction};
use crate::shared::{DB_INIT_PRAGMAS, DB_PRAGMAS};

mod create;
mod database_model;
mod encoding;
mod object_data_model;
mod object_store_index_model;
mod object_store_model;

fn range_to_query(range: IndexedDBKeyRange) -> Condition {
    // Special case for optimization
    if let Some(singleton) = range.as_singleton() {
        let encoded = encoding::serialize(singleton);
        return Expr::column(object_data_model::Column::Key)
            .eq(encoded)
            .into_condition();
    }
    let mut parts = vec![];
    if let Some(upper) = range.upper.as_ref() {
        let upper_bytes = encoding::serialize(upper);
        let query = if range.upper_open {
            Expr::column(object_data_model::Column::Key).lt(upper_bytes)
        } else {
            Expr::column(object_data_model::Column::Key).lte(upper_bytes)
        };
        parts.push(query);
    }
    if let Some(lower) = range.lower.as_ref() {
        let lower_bytes = encoding::serialize(lower);
        let query = if range.lower_open {
            Expr::column(object_data_model::Column::Key).gt(lower_bytes)
        } else {
            Expr::column(object_data_model::Column::Key).gte(lower_bytes)
        };
        parts.push(query);
    }
    let mut condition = Condition::all();
    for part in parts {
        condition = condition.add(part);
    }
    condition
}

/// Partii: one record read from an object store or an index, with its keys still encoded.
struct RawRecord {
    key: Vec<u8>,
    primary_key: Vec<u8>,
    value: Vec<u8>,
}

/// Partii: the SQL condition ` AND column >= ? AND column < ?` for `range`; the encoded bounds go
/// to `params`.
fn range_condition(range: &IndexedDBKeyRange, column: &str, params: &mut Vec<SqlValue>) -> String {
    let mut condition = String::new();
    if let Some(lower) = &range.lower {
        let op = if range.lower_open { ">" } else { ">=" };
        condition.push_str(&format!(" AND {column} {op} ?"));
        params.push(SqlValue::Blob(encoding::serialize(lower)));
    }
    if let Some(upper) = &range.upper {
        let op = if range.upper_open { "<" } else { "<=" };
        condition.push_str(&format!(" AND {column} {op} ?"));
        params.push(SqlValue::Blob(encoding::serialize(upper)));
    }
    condition
}

/// Partii: the table holding the records of `index`.
fn index_table(index: &object_store_index_model::Model) -> &'static str {
    if index.unique_index {
        "unique_index_data"
    } else {
        "index_data"
    }
}

fn decode_key(bytes: &[u8]) -> Result<IndexedDBKeyType, Error> {
    encoding::deserialize(bytes).ok_or_else(|| engine_error("Undecodable IndexedDB key"))
}

fn decode_records(records: Vec<RawRecord>) -> Result<Vec<IndexedDBRecord>, Error> {
    records
        .into_iter()
        .map(|record| {
            Ok(IndexedDBRecord {
                key: decode_key(&record.key)?,
                primary_key: decode_key(&record.primary_key)?,
                value: record.value,
            })
        })
        .collect()
}

fn backend_error(error: Error) -> BackendError {
    BackendError::DbErr(format!("{error:?}"))
}

/// Partii: how many finished write transactions an engine remembers, so that a request batch that
/// was already handed to the thread pool when its transaction ended cannot open it again.
const CLOSED_TXN_MEMORY: usize = 64;

/// Partii: how long a statement waits for a lock held by another connection to the same file
/// (for example another process using the same profile) before failing with SQLITE_BUSY.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

fn engine_error(message: impl Into<String>) -> Error {
    Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISUSE),
        Some(message.into()),
    )
}

/// Partii: the one sqlite connection of an IndexedDB database.
///
/// Every request batch and every metadata query of the database runs on this connection, one at
/// a time. A readwrite or versionchange IndexedDB transaction maps onto one sqlite transaction on
/// it: the transaction's first batch (or the start of an upgrade) runs `BEGIN IMMEDIATE`, and the
/// manager ends it with [`KvsEngine::commit_transaction`] or [`KvsEngine::rollback_transaction`].
/// The manager never starts a second write transaction while one is open (see
/// `IndexedDBEnvironment::engine_write_txn`). Readonly transactions read through the same
/// connection; their scope never overlaps the open write transaction's scope (the IndexedDB
/// scheduling rules guarantee that), so they still only see committed data of their stores.
pub(crate) struct EngineConnection {
    connection: Connection,
    /// The IndexedDB transaction whose sqlite transaction is open on `connection`.
    open_txn: Option<u64>,
    /// Write transactions that recently committed or rolled back.
    closed_txns: VecDeque<u64>,
    /// Set when the engine is dropped; batches still queued on the pool then fail.
    engine_dropped: bool,
}

impl Deref for EngineConnection {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.connection
    }
}

impl EngineConnection {
    fn remember_closed(&mut self, txn: u64) {
        if self.closed_txns.contains(&txn) {
            return;
        }
        if self.closed_txns.len() == CLOSED_TXN_MEMORY {
            self.closed_txns.pop_front();
        }
        self.closed_txns.push_back(txn);
    }

    /// Make sure the sqlite transaction of `txn` is open.
    fn begin(&mut self, txn: u64) -> Result<(), Error> {
        if self.engine_dropped {
            return Err(engine_error("The database was closed"));
        }
        match self.open_txn {
            Some(open) if open == txn => Ok(()),
            Some(open) => Err(engine_error(format!(
                "Transaction {txn} cannot start while transaction {open} writes"
            ))),
            None => {
                if self.closed_txns.contains(&txn) {
                    return Err(engine_error(format!("Transaction {txn} already finished")));
                }
                self.connection.execute_batch("BEGIN IMMEDIATE")?;
                self.open_txn = Some(txn);
                Ok(())
            },
        }
    }

    fn commit(&mut self, txn: u64) -> Result<(), Error> {
        self.remember_closed(txn);
        if self.open_txn != Some(txn) {
            return Ok(());
        }
        self.open_txn = None;
        if let Err(error) = self.connection.execute_batch("COMMIT") {
            // A failed COMMIT can leave the transaction open: never keep it around.
            if !self.connection.is_autocommit() {
                let _ = self.connection.execute_batch("ROLLBACK");
            }
            return Err(error);
        }
        Ok(())
    }

    fn rollback(&mut self, txn: u64) {
        self.remember_closed(txn);
        if self.open_txn != Some(txn) {
            return;
        }
        self.open_txn = None;
        if !self.connection.is_autocommit() &&
            let Err(error) = self.connection.execute_batch("ROLLBACK")
        {
            warn!("Failed to roll back IndexedDB transaction {txn}: {error:?}");
        }
    }
}

fn lock_connection(shared: &Mutex<EngineConnection>) -> MutexGuard<'_, EngineConnection> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Partii: run `operation` so that it either fully happens or leaves nothing behind. `keep`
/// decides whether a successful result is kept (a put that hits a unique index is undone).
/// Inside an open transaction this is a savepoint; outside one it is its own transaction.
fn in_savepoint<T>(
    connection: &Connection,
    operation: impl FnOnce() -> Result<T, Error>,
    keep: impl FnOnce(&T) -> bool,
) -> Result<T, Error> {
    connection.execute_batch("SAVEPOINT idb_request")?;
    let result = operation();
    if matches!(&result, Ok(value) if keep(value)) {
        connection.execute_batch("RELEASE idb_request")?;
    } else {
        let _ = connection.execute_batch("ROLLBACK TO idb_request; RELEASE idb_request");
    }
    result
}

pub struct SqliteEngine {
    // Partii: shared with the request batches running on the thread pool.
    shared: Arc<Mutex<EngineConnection>>,
    read_pool: Arc<ThreadPool>,
    write_pool: Arc<ThreadPool>,
    created_db_path: bool,
}

impl Drop for SqliteEngine {
    fn drop(&mut self) {
        // Partii: closing or deleting the database, or shutting the manager down, must not leave
        // a write transaction open. Batches still queued on the pool fail instead of running.
        let mut shared = lock_connection(&self.shared);
        shared.engine_dropped = true;
        if let Some(txn) = shared.open_txn {
            shared.rollback(txn);
        }
    }
}

impl SqliteEngine {
    fn connection(&self) -> MutexGuard<'_, EngineConnection> {
        lock_connection(&self.shared)
    }

    fn object_store_by_name(
        connection: &Connection,
        store_name: &str,
    ) -> Result<object_store_model::Model, Error> {
        connection.query_row(
            "SELECT * FROM object_store WHERE name = ?",
            params![store_name.to_string()],
            |row| object_store_model::Model::try_from(row),
        )
    }

    // TODO: intake dual pools
    pub fn new(
        path: PathBuf,
        created: bool,
        db_info: &IndexedDBDescription,
        pool: Arc<ThreadPool>,
    ) -> Result<Self, Error> {
        let db_path = path.join("indexeddb.sqlite");
        let connection = Self::init_db(&db_path, db_info)?;

        for stmt in DB_PRAGMAS {
            // TODO: Handle errors properly
            let _ = connection.execute(stmt, ());
        }
        connection.busy_timeout(BUSY_TIMEOUT)?;
        // Partii: bring databases written by older builds up to the current schema.
        create::upgrade_tables(&connection)?;

        Ok(Self {
            shared: Arc::new(Mutex::new(EngineConnection {
                connection,
                open_txn: None,
                closed_txns: VecDeque::new(),
                engine_dropped: false,
            })),
            read_pool: pool.clone(),
            write_pool: pool,
            created_db_path: created,
        })
    }

    /// Returns whether the physical db was created as part of `new`.
    pub(crate) fn created_db_path(&self) -> bool {
        self.created_db_path
    }

    fn init_db(path: &Path, db_info: &IndexedDBDescription) -> Result<Connection, Error> {
        let connection = Connection::open(path)?;
        if connection.table_exists(None, "database")? {
            // Database already exists, no need to initialize
            return Ok(connection);
        }
        info!("Initializing indexeddb database at {:?}", path);
        for stmt in DB_INIT_PRAGMAS {
            // FIXME(arihant2math): this fails occasionally
            let _ = connection.execute(stmt, ());
        }
        create::create_tables(&connection)?;
        // From https://w3c.github.io/IndexedDB/#database-version:
        // "When a database is first created, its version is 0 (zero)."
        connection.execute(
            "INSERT INTO database (name, origin, version) VALUES (?, ?, ?)",
            params![
                db_info.name.to_owned(),
                db_info.origin.ascii_serialization().into_owned(),
                i64::from_ne_bytes(0_u64.to_ne_bytes())
            ],
        )?;
        Ok(connection)
    }

    fn get_all(
        connection: &Connection,
        store: object_store_model::Model,
        key_range: IndexedDBKeyRange,
        count: Option<u32>,
    ) -> Result<Vec<object_data_model::Model>, Error> {
        let query = range_to_query(key_range);
        let mut sql_query = sea_query::Query::select();
        sql_query
            .from(object_data_model::Column::Table)
            .columns(vec![
                object_data_model::Column::ObjectStoreId,
                object_data_model::Column::Key,
                object_data_model::Column::Data,
            ])
            .and_where(query.and(Expr::col(object_data_model::Column::ObjectStoreId).is(store.id)));
        if let Some(count) = count {
            sql_query.limit(count as u64);
        }
        let (sql, values) = sql_query.build_rusqlite(SqliteQueryBuilder);
        let mut stmt = connection.prepare(&sql)?;
        let models = stmt
            .query_and_then(&*values.as_params(), |row| {
                object_data_model::Model::try_from(row)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(models)
    }

    fn get_all_keys(
        connection: &Connection,
        store: object_store_model::Model,
        key_range: IndexedDBKeyRange,
        count: Option<u32>,
    ) -> Result<Vec<Vec<u8>>, Error> {
        Self::get_all(connection, store, key_range, count)
            .map(|models| models.into_iter().map(|m| m.key).collect())
    }

    /// <https://w3c.github.io/IndexedDB/#store-a-record-into-an-object-store>
    ///
    /// Partii: the record and its index records are written as one database operation; a
    /// unique index clash undoes the record write and reports a "ConstraintError".
    fn put_item(
        connection: &Connection,
        store: object_store_model::Model,
        key: IndexedDBKeyType,
        value: Vec<u8>,
        should_overwrite: bool,
        key_generator_current_number: Option<i64>,
        index_keys: &IndexKeys,
    ) -> Result<PutItemResult, Error> {
        in_savepoint(
            connection,
            || {
                Self::put_item_in_savepoint(
                    connection,
                    store,
                    key,
                    value,
                    should_overwrite,
                    key_generator_current_number,
                    index_keys,
                )
            },
            |result| matches!(result, PutItemResult::Key(_)),
        )
    }

    fn put_item_in_savepoint(
        connection: &Connection,
        store: object_store_model::Model,
        key: IndexedDBKeyType,
        value: Vec<u8>,
        should_overwrite: bool,
        key_generator_current_number: Option<i64>,
        index_keys: &IndexKeys,
    ) -> Result<PutItemResult, Error> {
        let no_overwrite = !should_overwrite;
        let serialized_key: Vec<u8> = encoding::serialize(&key);
        let existing_item = connection
            .prepare("SELECT * FROM object_data WHERE key = ? AND object_store_id = ?")
            .and_then(|mut stmt| {
                stmt.query_row(params![serialized_key, store.id], |row| {
                    object_data_model::Model::try_from(row)
                })
                .optional()
            })?;
        if existing_item.is_some() {
            if no_overwrite {
                return Ok(PutItemResult::CannotOverwrite);
            }
            // Step 2. If the no-overwrite flag is false and a record exists in store with key
            // equal to key, remove the record (Partii: and its index records) from store.
            Self::delete_index_records(connection, store.id, &serialized_key)?;
            // Preserve `put()` semantics by replacing the stored value when the primary
            // key already exists.
            connection.execute(
                "UPDATE object_data SET data = ? WHERE object_store_id = ? AND key = ?",
                params![value, store.id, serialized_key],
            )?;
        } else {
            connection.execute(
                "INSERT INTO object_data (object_store_id, key, data) VALUES (?, ?, ?)",
                params![store.id, serialized_key, value],
            )?;
        }
        // Step 5. For each index which references store: store the record's index keys.
        if !Self::write_index_records(connection, store.id, &serialized_key, index_keys)? {
            return Ok(PutItemResult::CannotOverwrite);
        }
        if let Some(next_key_generator_current_number) = key_generator_current_number {
            connection.execute(
                "UPDATE object_store SET auto_increment = ? WHERE id = ?",
                params![next_key_generator_current_number, store.id],
            )?;
        }
        Ok(PutItemResult::Key(key))
    }

    /// Partii: the index named `name` of the store with id `store_id`.
    fn index_by_name(
        connection: &Connection,
        store_id: i32,
        name: &str,
    ) -> Result<Option<object_store_index_model::Model>, Error> {
        connection
            .query_row(
                "SELECT * FROM object_store_index WHERE object_store_id = ? AND name = ?",
                params![store_id, name],
                |row| object_store_index_model::Model::try_from(row),
            )
            .optional()
    }

    /// Partii: <https://w3c.github.io/IndexedDB/#store-a-record-into-an-object-store> step 5,
    /// for the record with (encoded) primary key `primary_key`. Returns false, having written
    /// some of the records, when a unique index already holds one of the keys for another
    /// record: the caller undoes the operation.
    fn write_index_records(
        connection: &Connection,
        store_id: i32,
        primary_key: &[u8],
        index_keys: &IndexKeys,
    ) -> Result<bool, Error> {
        for (index_name, keys) in index_keys {
            if keys.is_empty() {
                continue;
            }
            let Some(index) = Self::index_by_name(connection, store_id, index_name)? else {
                warn!("IndexedDB put names unknown index {index_name:?}");
                continue;
            };
            for key in keys {
                let value = encoding::serialize(key);
                if index.unique_index {
                    // Steps 5.3 and 5.4: a unique index may hold each key once.
                    let clash: bool = connection.query_row(
                        "SELECT EXISTS(SELECT 1 FROM unique_index_data \
                         WHERE index_id = ? AND value = ? AND object_data_key != ?)",
                        params![index.id, value, primary_key],
                        |row| row.get(0),
                    )?;
                    if clash {
                        return Ok(false);
                    }
                    connection.execute(
                        "INSERT OR REPLACE INTO unique_index_data \
                         (index_id, value, object_store_id, object_data_key) VALUES (?, ?, ?, ?)",
                        params![index.id, value, store_id, primary_key],
                    )?;
                } else {
                    // Steps 5.5 and 5.6.
                    connection.execute(
                        "INSERT OR IGNORE INTO index_data \
                         (index_id, value, object_data_key, object_store_id) VALUES (?, ?, ?, ?)",
                        params![index.id, value, primary_key, store_id],
                    )?;
                }
            }
        }
        Ok(true)
    }

    /// Partii: remove the index records of the record with (encoded) primary key `primary_key`.
    /// Partii: see `AsyncReadWriteOperation::SetIndexEntries`.
    fn set_index_entries(
        connection: &Connection,
        store: &object_store_model::Model,
        index_names: &[String],
        entries: &[(IndexedDBKeyType, IndexKeys)],
        fail_on_clash: bool,
    ) -> Result<bool, Error> {
        in_savepoint(
            connection,
            || {
                let mut indexes = Vec::with_capacity(index_names.len());
                for name in index_names {
                    // The index may have been deleted since script asked for it.
                    if let Some(index) = Self::index_by_name(connection, store.id, name)? {
                        connection.execute(
                            &format!("DELETE FROM {} WHERE index_id = ?", index_table(&index)),
                            params![index.id],
                        )?;
                        indexes.push(index);
                    }
                }
                for (primary_key, index_keys) in entries {
                    let primary_key = encoding::serialize(primary_key);
                    for (name, keys) in index_keys {
                        if !indexes.iter().any(|index| &index.name == name) {
                            continue;
                        }
                        let one_index = vec![(name.clone(), keys.clone())];
                        if !Self::write_index_records(
                            connection,
                            store.id,
                            &primary_key,
                            &one_index,
                        )? && fail_on_clash
                        {
                            return Ok(false);
                        }
                    }
                }
                for index in &indexes {
                    connection.execute(
                        "UPDATE object_store_index SET populated = 1 WHERE id = ?",
                        params![index.id],
                    )?;
                }
                Ok(true)
            },
            |populated| *populated,
        )
    }

    fn delete_index_records(
        connection: &Connection,
        store_id: i32,
        primary_key: &[u8],
    ) -> Result<(), Error> {
        connection.execute(
            "DELETE FROM index_data WHERE object_store_id = ? AND object_data_key = ?",
            params![store_id, primary_key],
        )?;
        connection.execute(
            "DELETE FROM unique_index_data WHERE object_store_id = ? AND object_data_key = ?",
            params![store_id, primary_key],
        )?;
        Ok(())
    }

    /// <https://w3c.github.io/IndexedDB/#delete-records-from-an-object-store>
    fn delete_item(
        connection: &Connection,
        store: object_store_model::Model,
        key_range: IndexedDBKeyRange,
    ) -> Result<(), Error> {
        in_savepoint(
            connection,
            || {
                // Partii: Step 3. Remove the records in range from each index of store.
                for key in Self::get_all_keys(connection, store.clone(), key_range.clone(), None)? {
                    Self::delete_index_records(connection, store.id, &key)?;
                }
                let query = range_to_query(key_range);
                let (sql, values) = sea_query::Query::delete()
                    .from_table(object_data_model::Column::Table)
                    .and_where(
                        query.and(Expr::col(object_data_model::Column::ObjectStoreId).is(store.id)),
                    )
                    .build_rusqlite(SqliteQueryBuilder);
                connection.prepare(&sql)?.execute(&*values.as_params())?;
                Ok(())
            },
            |_| true,
        )
    }

    /// <https://w3c.github.io/IndexedDB/#clear-an-object-store>
    fn clear(connection: &Connection, store: object_store_model::Model) -> Result<(), Error> {
        in_savepoint(
            connection,
            || {
                // Partii: Step 2. Remove all records from each index of store.
                connection.execute(
                    "DELETE FROM index_data WHERE object_store_id = ?",
                    params![store.id],
                )?;
                connection.execute(
                    "DELETE FROM unique_index_data WHERE object_store_id = ?",
                    params![store.id],
                )?;
                connection.execute(
                    "DELETE FROM object_data WHERE object_store_id = ?",
                    params![store.id],
                )?;
                Ok(())
            },
            |_| true,
        )
    }

    /// Partii: the records of the object store `store_id`, or of `index` in it, with keys in
    /// `range`, in `direction` order, at most `count` (0 or none means all) of them. An index
    /// record's key is its index key; its value is read from the object store record it
    /// references. `with_values` false leaves the values empty.
    ///
    /// <https://w3c.github.io/IndexedDB/#retrieve-multiple-items-from-an-object-store> and
    /// <https://w3c.github.io/IndexedDB/#retrieve-multiple-items-from-an-index>: "nextunique"
    /// and "prevunique" keep, for each index key, the record with the lowest primary key.
    #[allow(clippy::too_many_arguments)]
    fn read_records(
        connection: &Connection,
        store_id: i32,
        index: Option<&object_store_index_model::Model>,
        range: &IndexedDBKeyRange,
        direction: IndexedDBDirection,
        count: Option<u32>,
        with_values: bool,
    ) -> Result<Vec<RawRecord>, Error> {
        let count = count.filter(|count| *count > 0);
        // A store has one record per key, so its unique directions are plain ones.
        let unique = index.is_some() &&
            matches!(
                direction,
                IndexedDBDirection::NextUnique | IndexedDBDirection::PrevUnique
            );
        let descending = match direction {
            IndexedDBDirection::Next | IndexedDBDirection::NextUnique => false,
            IndexedDBDirection::Prev => true,
            // Read ascending to find each key's lowest primary key, then reverse.
            IndexedDBDirection::PrevUnique => !unique,
        };
        let order = if descending { "DESC" } else { "ASC" };
        let mut sql_params = vec![];
        let sql = match index {
            None => {
                sql_params.push(SqlValue::Integer(store_id.into()));
                let condition = range_condition(range, "key", &mut sql_params);
                let value = if with_values { "data" } else { "x''" };
                format!(
                    "SELECT key, key, {value} FROM object_data \
                     WHERE object_store_id = ?{condition} ORDER BY key {order}"
                )
            },
            Some(index) => {
                sql_params.push(SqlValue::Integer(index.id.into()));
                let condition = range_condition(range, "i.value", &mut sql_params);
                let table = index_table(index);
                let (value, join) = if with_values {
                    (
                        "d.data",
                        " JOIN object_data d \
                         ON d.object_store_id = i.object_store_id AND d.key = i.object_data_key",
                    )
                } else {
                    ("x''", "")
                };
                format!(
                    "SELECT i.value, i.object_data_key, {value} FROM {table} i{join} \
                     WHERE i.index_id = ?{condition} \
                     ORDER BY i.value {order}, i.object_data_key {order}"
                )
            },
        };
        let sql = match count {
            Some(count) if !unique => {
                sql_params.push(SqlValue::Integer(count.into()));
                format!("{sql} LIMIT ?")
            },
            _ => sql,
        };
        let mut statement = connection.prepare(&sql)?;
        let mut records = statement
            .query_map(rusqlite::params_from_iter(sql_params), |row| {
                Ok(RawRecord {
                    key: row.get(0)?,
                    primary_key: row.get(1)?,
                    value: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if unique {
            // `dedup_by` keeps the first record of each run of equal keys.
            records.dedup_by(|later, earlier| later.key == earlier.key);
            if direction == IndexedDBDirection::PrevUnique {
                records.reverse();
            }
            if let Some(count) = count {
                records.truncate(count as usize);
            }
        }
        Ok(records)
    }

    /// Partii: <https://w3c.github.io/IndexedDB/#count-the-records-in-a-range>, of an object
    /// store or an index.
    fn count_records(
        connection: &Connection,
        store_id: i32,
        index: Option<&object_store_index_model::Model>,
        range: &IndexedDBKeyRange,
    ) -> Result<u64, Error> {
        let mut sql_params = vec![];
        let sql = match index {
            None => {
                sql_params.push(SqlValue::Integer(store_id.into()));
                let condition = range_condition(range, "key", &mut sql_params);
                format!("SELECT COUNT(*) FROM object_data WHERE object_store_id = ?{condition}")
            },
            Some(index) => {
                sql_params.push(SqlValue::Integer(index.id.into()));
                let condition = range_condition(range, "value", &mut sql_params);
                let table = index_table(index);
                format!("SELECT COUNT(*) FROM {table} WHERE index_id = ?{condition}")
            },
        };
        connection
            .query_row(&sql, rusqlite::params_from_iter(sql_params), |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| count as u64)
    }

    /// Partii: run a read request against the object store or the index it names.
    fn execute_read(
        connection: &Connection,
        store: &object_store_model::Model,
        operation: AsyncReadOnlyOperation,
    ) {
        let index = match operation.index_name() {
            None => None,
            Some(name) => match Self::index_by_name(connection, store.id, name) {
                Ok(Some(index)) => Some(index),
                Ok(None) => {
                    operation.notify_error(BackendError::DbErr(format!("No index named {name}")));
                    return;
                },
                Err(error) => {
                    operation.notify_error(BackendError::DbErr(format!("{error:?}")));
                    return;
                },
            },
        };
        let index = index.as_ref();
        let first = |key_range: &IndexedDBKeyRange, with_values| {
            Self::read_records(
                connection,
                store.id,
                index,
                key_range,
                IndexedDBDirection::Next,
                Some(1),
                with_values,
            )
            .map(|records| records.into_iter().next())
        };
        let all = |key_range: &IndexedDBKeyRange, direction, count, with_values| {
            Self::read_records(
                connection,
                store.id,
                index,
                key_range,
                direction,
                count,
                with_values,
            )
        };
        let _ = match operation {
            AsyncReadOnlyOperation::GetKey {
                callback,
                key_range,
                ..
            } => callback.send(
                first(&key_range, false)
                    .and_then(|record| {
                        record
                            .map(|record| decode_key(&record.primary_key))
                            .transpose()
                    })
                    .map_err(backend_error),
            ),
            AsyncReadOnlyOperation::GetItem {
                callback,
                key_range,
                ..
            } => callback.send(
                first(&key_range, true)
                    .map(|record| record.map(|record| record.value))
                    .map_err(backend_error),
            ),
            AsyncReadOnlyOperation::GetAllKeys {
                callback,
                key_range,
                count,
                direction,
                ..
            } => callback.send(
                all(&key_range, direction, count, false)
                    .and_then(|records| {
                        records
                            .iter()
                            .map(|record| decode_key(&record.primary_key))
                            .collect()
                    })
                    .map_err(backend_error),
            ),
            AsyncReadOnlyOperation::GetAllItems {
                callback,
                key_range,
                count,
                direction,
                ..
            } => callback.send(
                all(&key_range, direction, count, true)
                    .map(|records| records.into_iter().map(|record| record.value).collect())
                    .map_err(backend_error),
            ),
            AsyncReadOnlyOperation::GetAllRecords {
                callback,
                key_range,
                count,
                direction,
                ..
            } => callback.send(
                all(&key_range, direction, count, true)
                    .and_then(decode_records)
                    .map_err(backend_error),
            ),
            AsyncReadOnlyOperation::Count {
                callback,
                key_range,
                ..
            } => callback.send(
                Self::count_records(connection, store.id, index, &key_range).map_err(backend_error),
            ),
            AsyncReadOnlyOperation::Iterate {
                callback,
                key_range,
                key_only,
                ..
            } => callback.send(
                all(&key_range, IndexedDBDirection::Next, None, !key_only)
                    .and_then(decode_records)
                    .map_err(backend_error),
            ),
        };
    }

    fn create_store(
        connection: &Connection,
        store_name: &str,
        key_path: Option<KeyPath>,
        auto_increment: bool,
    ) -> Result<CreateObjectResult, Error> {
        let mut stmt = connection.prepare("SELECT * FROM object_store WHERE name = ?")?;
        if stmt.exists(params![store_name.to_string()])? {
            // Store already exists
            return Ok(CreateObjectResult::AlreadyExists);
        }
        connection.execute(
            "INSERT INTO object_store (name, key_path, auto_increment) VALUES (?, ?, ?)",
            params![
                store_name.to_string(),
                key_path.map(|v| postcard::to_stdvec(&v).unwrap()),
                auto_increment as i32
            ],
        )?;
        Ok(CreateObjectResult::Created)
    }

    fn delete_store(connection: &Connection, store_name: &str) -> Result<(), Error> {
        // https://www.w3.org/TR/IndexedDB-3/#dom-idbdatabase-deleteobjectstore
        // Step 7. Destroy store.
        let object_store = Self::object_store_by_name(connection, store_name)?;

        connection.execute(
            "DELETE FROM index_data WHERE object_store_id = ?",
            params![object_store.id],
        )?;
        connection.execute(
            "DELETE FROM unique_index_data WHERE object_store_id = ?",
            params![object_store.id],
        )?;
        connection.execute(
            "DELETE FROM object_store_index WHERE object_store_id = ?",
            params![object_store.id],
        )?;
        connection.execute(
            "DELETE FROM object_data WHERE object_store_id = ?",
            params![object_store.id],
        )?;
        let result = connection.execute(
            "DELETE FROM object_store WHERE id = ?",
            params![object_store.id],
        )?;
        if result == 0 {
            Err(Error::QueryReturnedNoRows)
        } else if result > 1 {
            Err(Error::QueryReturnedMoreThanOneRow)
        } else {
            Ok(())
        }
    }

    fn create_index(
        connection: &Connection,
        store_name: &str,
        index_name: String,
        key_path: KeyPath,
        unique: bool,
        multi_entry: bool,
    ) -> Result<CreateObjectResult, Error> {
        let object_store = connection.query_row(
            "SELECT * FROM object_store WHERE name = ?",
            params![store_name.to_string()],
            |row| object_store_model::Model::try_from(row),
        )?;

        let index_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT * FROM object_store_index WHERE name = ? AND object_store_id = ?)",
            params![index_name, object_store.id],
            |row| row.get(0),
        )?;
        if index_exists {
            return Ok(CreateObjectResult::AlreadyExists);
        }

        connection.execute(
            // Partii: the transaction that creates the index also populates it.
            "INSERT INTO object_store_index (object_store_id, name, key_path, unique_index, multi_entry_index, populated)\
            VALUES (?, ?, ?, ?, ?, 1)",
            params![
                object_store.id,
                index_name,
                postcard::to_stdvec(&key_path).unwrap(),
                unique,
                multi_entry,
            ],
        )?;
        Ok(CreateObjectResult::Created)
    }

    /// Partii: <https://w3c.github.io/IndexedDB/#dom-idbobjectstore-name> step 9, "set store's
    /// name to name". Its records and indexes refer to the store by id, so they follow.
    fn rename_store(
        connection: &Connection,
        store_name: &str,
        new_name: &str,
    ) -> Result<(), Error> {
        let renamed = connection.execute(
            "UPDATE object_store SET name = ? WHERE name = ?",
            params![new_name, store_name],
        )?;
        if renamed == 0 {
            return Err(Error::QueryReturnedNoRows);
        }
        Ok(())
    }

    fn rename_index(
        connection: &Connection,
        store_name: &str,
        index_name: &str,
        new_name: &str,
    ) -> Result<(), Error> {
        let object_store = connection.query_row(
            "SELECT * FROM object_store WHERE name = ?",
            params![store_name],
            |row| object_store_model::Model::try_from(row),
        )?;

        // Rename the index if it exists
        let _ = connection.execute(
            "UPDATE object_store_index SET name = ? WHERE name = ? AND object_store_id = ?",
            params![new_name, index_name, object_store.id],
        )?;
        Ok(())
    }

    fn delete_index(
        connection: &Connection,
        store_name: &str,
        index_name: String,
    ) -> Result<(), Error> {
        let object_store = connection.query_row(
            "SELECT * FROM object_store WHERE name = ?",
            params![store_name.to_string()],
            |row| Ok(object_store_model::Model::try_from(row).unwrap()),
        )?;

        // Partii: https://w3c.github.io/IndexedDB/#dom-idbobjectstore-deleteindex
        // Step 8. Destroy index: its records go with it.
        if let Some(index) = Self::index_by_name(connection, object_store.id, &index_name)? {
            connection.execute(
                "DELETE FROM index_data WHERE index_id = ?",
                params![index.id],
            )?;
            connection.execute(
                "DELETE FROM unique_index_data WHERE index_id = ?",
                params![index.id],
            )?;
        }

        // Delete the index if it exists
        let _ = connection.execute(
            "DELETE FROM object_store_index WHERE name = ? AND object_store_id = ?",
            params![index_name, object_store.id],
        )?;
        Ok(())
    }

    /// Partii: run one request of a batch on the database connection.
    fn execute_request(connection: &Connection, request: KvsOperation) {
        if let AsyncOperation::Schema(AsyncSchemaOperation::CreateObjectStore {
            callback,
            key_path,
            auto_increment,
        }) = &request.operation
        {
            if let Err(error) = Self::create_store(
                connection,
                &request.store_name,
                key_path.clone(),
                *auto_increment,
            ) {
                let _ = callback.send(BackendError::DbErr(format!("{error:?}")));
            }
            return;
        }

        let object_store = connection
            .prepare("SELECT * FROM object_store WHERE name = ?")
            .and_then(|mut stmt| {
                stmt.query_row(params![request.store_name.to_string()], |row| {
                    object_store_model::Model::try_from(row)
                })
                .optional()
            });
        let object_store = match object_store {
            Ok(Some(store)) => store,
            Ok(None) => {
                request.operation.notify_error(BackendError::StoreNotFound);
                return;
            },
            Err(error) => {
                request
                    .operation
                    .notify_error(BackendError::DbErr(format!("{error:?}")));
                return;
            },
        };

        match request.operation {
            AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                callback,
                key,
                value,
                should_overwrite,
                key_generator_current_number,
                index_keys,
            }) => {
                let (key, key_generator_current_number) = match key {
                    Some(key) => (key, key_generator_current_number),
                    None => {
                        if object_store.auto_increment == 0 {
                            if let Err(error) = callback.send(Err(BackendError::DbErr(
                                "Missing key for PutItem request".to_string(),
                            ))) {
                                warn!("Failed to send PutItem missing key error: {error:?}");
                            }
                            return;
                        }
                        let Some(next_key_generator_current_number) =
                            object_store.auto_increment.checked_add(1)
                        else {
                            if let Err(error) = callback.send(Err(BackendError::DbErr(
                                "Key generator overflow".to_string(),
                            ))) {
                                warn!(
                                    "Failed to send PutItem key generator overflow error: {error:?}"
                                );
                            }
                            return;
                        };
                        (
                            IndexedDBKeyType::Number(object_store.auto_increment as f64),
                            Some(next_key_generator_current_number),
                        )
                    },
                };
                let _ = callback.send(
                    Self::put_item(
                        connection,
                        object_store,
                        key,
                        value,
                        should_overwrite,
                        key_generator_current_number,
                        &index_keys,
                    )
                    .map_err(|e| BackendError::DbErr(format!("{:?}", e))),
                );
            },
            AsyncOperation::ReadOnly(operation) => {
                Self::execute_read(connection, &object_store, operation);
            },
            AsyncOperation::ReadWrite(AsyncReadWriteOperation::RemoveItem {
                callback,
                key_range,
            }) => {
                let _ = callback.send(
                    Self::delete_item(connection, object_store, key_range)
                        .map_err(|e| BackendError::DbErr(format!("{:?}", e))),
                );
            },
            AsyncOperation::ReadWrite(AsyncReadWriteOperation::Clear(sender)) => {
                let _ = sender.send(
                    Self::clear(connection, object_store)
                        .map_err(|e| BackendError::DbErr(format!("{:?}", e))),
                );
            },
            AsyncOperation::ReadWrite(AsyncReadWriteOperation::SetIndexEntries {
                callback,
                index_names,
                entries,
                fail_on_clash,
            }) => {
                let _ = callback.send(
                    Self::set_index_entries(
                        connection,
                        &object_store,
                        &index_names,
                        &entries,
                        fail_on_clash,
                    )
                    .map_err(backend_error),
                );
            },
            AsyncOperation::Schema(AsyncSchemaOperation::CreateIndex {
                callback,
                index_name,
                key_path,
                unique,
                multi_entry,
            }) => {
                if let Err(error) = Self::create_index(
                    connection,
                    &request.store_name,
                    index_name,
                    key_path,
                    unique,
                    multi_entry,
                ) {
                    let _ = callback.send(BackendError::DbErr(format!("{error:?}")));
                }
            },
            AsyncOperation::Schema(AsyncSchemaOperation::CreateObjectStore { .. }) => {
                unreachable!("Should be handled above");
            },
            AsyncOperation::Schema(AsyncSchemaOperation::DeleteIndex {
                index_name,
                callback,
            }) => {
                if let Err(error) = Self::delete_index(connection, &request.store_name, index_name)
                {
                    let _ = callback.send(BackendError::DbErr(format!("{error:?}")));
                }
            },
            AsyncOperation::Schema(AsyncSchemaOperation::DeleteObjectStore { callback }) => {
                if let Err(error) = Self::delete_store(connection, &request.store_name) {
                    let _ = callback.send(BackendError::DbErr(format!("{error:?}")));
                }
            },
            AsyncOperation::Schema(AsyncSchemaOperation::RenameObjectStore {
                new_name,
                callback,
            }) => {
                if let Err(error) = Self::rename_store(connection, &request.store_name, &new_name) {
                    let _ = callback.send(BackendError::DbErr(format!("{error:?}")));
                }
            },
            AsyncOperation::Schema(AsyncSchemaOperation::RenameIndex {
                index_name,
                new_name,
                callback,
            }) => {
                if let Err(error) =
                    Self::rename_index(connection, &request.store_name, &index_name, &new_name)
                {
                    let _ = callback.send(BackendError::DbErr(format!("{error:?}")));
                }
            },
        }
    }
}

impl KvsEngine for SqliteEngine {
    type Error = Error;

    fn create_store(
        &self,
        store_name: &str,
        key_path: Option<KeyPath>,
        auto_increment: bool,
    ) -> Result<CreateObjectResult, Self::Error> {
        Self::create_store(&self.connection(), store_name, key_path, auto_increment)
    }
    fn create_index(
        &self,
        store_name: &str,
        index_name: String,
        key_path: KeyPath,
        unique: bool,
        multi_entry: bool,
    ) -> Result<CreateObjectResult, Self::Error> {
        Self::create_index(
            &self.connection(),
            store_name,
            index_name,
            key_path,
            unique,
            multi_entry,
        )
    }

    fn delete_store(&self, store_name: &str) -> Result<(), Self::Error> {
        Self::delete_store(&self.connection(), store_name)
    }

    fn close_store(&self, _store_name: &str) -> Result<(), Self::Error> {
        // TODO: do something
        Ok(())
    }

    fn process_transaction(
        &self,
        txn: u64,
        transaction: KvsTransaction,
        on_complete: Box<dyn FnOnce() + Send + 'static>,
    ) {
        let spawning_pool = if transaction.mode == IndexedDBTxnMode::Readonly {
            self.read_pool.clone()
        } else {
            self.write_pool.clone()
        };
        let shared = self.shared.clone();
        spawning_pool.spawn(move || {
            let mut connection = lock_connection(&shared);
            // Partii: readwrite and versionchange batches run inside the transaction's sqlite
            // transaction, which the first batch opens. Readonly batches only read.
            let prepared = if transaction.mode == IndexedDBTxnMode::Readonly {
                if connection.engine_dropped {
                    Err(engine_error("The database was closed"))
                } else {
                    Ok(())
                }
            } else {
                connection.begin(txn)
            };
            if let Err(error) = prepared {
                for request in transaction.requests {
                    request
                        .operation
                        .notify_error(BackendError::DbErr(format!("{error:?}")));
                }
                drop(connection);
                on_complete();
                return;
            }
            for request in transaction.requests {
                Self::execute_request(&connection, request);
            }
            drop(connection);
            on_complete();
        });
    }

    fn begin_transaction(&self, txn: u64) -> Result<(), Self::Error> {
        self.connection().begin(txn)
    }

    fn commit_transaction(&self, txn: u64) -> Result<(), Self::Error> {
        self.connection().commit(txn)
    }

    fn rollback_transaction(&self, txn: u64) {
        self.connection().rollback(txn)
    }

    fn key_generator_current_number(&self, store_name: &str) -> Option<i64> {
        self.connection()
            .prepare("SELECT * FROM object_store WHERE name = ?")
            .and_then(|mut stmt| {
                stmt.query_row(params![store_name.to_string()], |r| {
                    let object_store = object_store_model::Model::try_from(r).unwrap();
                    Ok(object_store.auto_increment)
                })
            })
            .optional()
            .unwrap()
            .and_then(|current_number| (current_number != 0).then_some(current_number))
    }

    fn set_key_generator_current_number(
        &self,
        store_name: &str,
        current_number: i64,
    ) -> Result<(), Self::Error> {
        // Ensure missing store is reported as QueryReturnedNoRows even when an
        // UPDATE might affect zero rows due to no-op assignment.
        let store_exists: bool = self.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM object_store WHERE name = ?)",
            params![store_name],
            |row| row.get(0),
        )?;
        if !store_exists {
            return Err(Error::QueryReturnedNoRows);
        }

        let rows_affected = self.connection().execute(
            "UPDATE object_store SET auto_increment = ? WHERE name = ?",
            params![current_number, store_name],
        )?;
        if rows_affected > 1 {
            return Err(Error::QueryReturnedMoreThanOneRow);
        }
        Ok(())
    }

    fn key_path(&self, store_name: &str) -> Option<KeyPath> {
        self.connection()
            .prepare("SELECT * FROM object_store WHERE name = ?")
            .and_then(|mut stmt| {
                stmt.query_row(params![store_name.to_string()], |r| {
                    let object_store = object_store_model::Model::try_from(r).unwrap();
                    Ok(object_store
                        .key_path
                        .map(|key_path| postcard::from_bytes(&key_path).unwrap()))
                })
            })
            .optional()
            .unwrap()
            // TODO: Wrong, same issues as has_key_generator
            .unwrap_or_default()
    }

    fn object_store_names(&self) -> Result<Vec<String>, Self::Error> {
        let connection = self.connection();
        let mut stmt = connection.prepare("SELECT name FROM object_store")?;
        stmt.query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()
    }

    fn indexes(&self, store_name: &str) -> Result<Vec<IndexedDBIndex>, Self::Error> {
        let connection = self.connection();
        let object_store = connection.query_row(
            "SELECT * FROM object_store WHERE name = ?",
            params![store_name.to_string()],
            |row| object_store_model::Model::try_from(row),
        )?;

        let mut stmt =
            connection.prepare("SELECT * FROM object_store_index WHERE object_store_id = ?")?;
        let indexes = stmt
            .query_map(params![object_store.id], |row| {
                let model = object_store_index_model::Model::try_from(row)?;
                Ok(IndexedDBIndex {
                    name: model.name,
                    key_path: postcard::from_bytes(&model.key_path).unwrap(),
                    unique: model.unique_index,
                    multi_entry: model.multi_entry_index,
                    populated: model.populated,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(indexes)
    }

    fn delete_index(&self, store_name: &str, index_name: String) -> Result<(), Self::Error> {
        Self::delete_index(&self.connection(), store_name, index_name)
    }

    fn version(&self) -> Result<u64, Self::Error> {
        let version: i64 =
            self.connection()
                .query_row("SELECT version FROM database LIMIT 1", [], |row| row.get(0))?;
        Ok(u64::from_ne_bytes(version.to_ne_bytes()))
    }

    fn set_version(&self, version: u64) -> Result<(), Self::Error> {
        let rows_affected = self.connection().execute(
            "UPDATE database SET version = ?",
            params![i64::from_ne_bytes(version.to_ne_bytes())],
        )?;
        if rows_affected == 0 {
            return Err(Error::QueryReturnedNoRows);
        }
        Ok(())
    }
}

fn get_db_status(connection: &Connection, op: i32) -> Result<i32, i32> {
    let mut p_curr = 0;
    let mut p_hiwater = 0;
    let res = unsafe {
        rusqlite::ffi::sqlite3_db_status(connection.handle(), op, &mut p_curr, &mut p_hiwater, 0)
    };
    if res != 0 { Err(res) } else { Ok(p_curr) }
}

impl MallocSizeOf for SqliteEngine {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        // 48 KB (3.3.1 at https://sqlite.org/malloc.html)
        const DEFAULT_LOOKASIDE_SIZE: usize = 48 * 1024;
        let base = self.created_db_path.size_of(ops) + DEFAULT_LOOKASIDE_SIZE;
        // Partii: don't wait for a running request batch just to report memory.
        let Ok(connection) = self.shared.try_lock() else {
            return base;
        };
        base + get_db_status(
            &connection,
            rusqlite::ffi::SQLITE_DBSTATUS_CACHE_USED_SHARED,
        )
        .unwrap_or_default() as usize +
            get_db_status(&connection, rusqlite::ffi::SQLITE_DBSTATUS_SCHEMA_USED)
                .unwrap_or_default() as usize +
            get_db_status(&connection, rusqlite::ffi::SQLITE_DBSTATUS_STMT_USED).unwrap_or_default()
                as usize
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Arc;

    use profile_traits::generic_callback::GenericCallback;
    use rusqlite::params;
    use serde::{Deserialize, Serialize};
    use servo_base::generic_channel::{self, GenericReceiver, GenericSender};
    use servo_base::id::{PIPELINE_NAMESPACE, PipelineNamespace, PipelineNamespaceId, WebViewId};
    use servo_base::threadpool::ThreadPool;
    use servo_url::ImmutableOrigin;
    use storage_traits::client_storage::{
        ClientStorageThreadHandle, StorageIdentifier, StorageProxyMap, StorageType,
    };
    use storage_traits::indexeddb::{
        AsyncOperation, AsyncReadOnlyOperation, AsyncReadWriteOperation, AsyncSchemaOperation,
        BackendResult, CreateObjectResult, IndexKeys, IndexedDBDirection, IndexedDBKeyRange,
        IndexedDBKeyType, IndexedDBTxnMode, KeyPath, PutItemResult,
    };
    use url::Host;

    use super::decode_records;
    use crate::ClientStorageThreadFactory;
    use crate::indexeddb::IndexedDBDescription;
    use crate::indexeddb::engines::sqlite::encoding;
    use crate::indexeddb::engines::{KvsEngine, KvsOperation, KvsTransaction, SqliteEngine};

    fn install_test_namespace() {
        PipelineNamespace::install(PipelineNamespaceId(1));
    }

    fn test_origin() -> ImmutableOrigin {
        ImmutableOrigin::Tuple(
            "test_origin".to_string(),
            Host::Domain("localhost".to_string()),
            80,
        )
    }

    fn get_pool() -> Arc<ThreadPool> {
        ThreadPool::global()
    }

    fn create_db(
        db_name: String,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        bool,
        StorageProxyMap,
        ClientStorageThreadHandle,
    ) {
        if PIPELINE_NAMESPACE.get().is_none() {
            install_test_namespace();
        }
        let tmp_dir = tempfile::tempdir().unwrap();
        let handle: ClientStorageThreadHandle =
            ClientStorageThreadFactory::new(Some(tmp_dir.path().to_path_buf()), true);

        let storage_proxy_map = handle
            .obtain_a_storage_bottle_map(
                StorageType::Local,
                Some(WebViewId::new(servo_base::id::TEST_PAINTER_ID)),
                StorageIdentifier::IndexedDB,
                test_origin(),
            )
            .recv()
            .unwrap()
            .unwrap();
        let (path, created) = handle
            .create_database(storage_proxy_map.bottle_id, db_name)
            .recv()
            .unwrap()
            .unwrap();
        (tmp_dir, path, created, storage_proxy_map, handle)
    }

    #[test]
    fn test_cycle() {
        let (_temp_dir, path, created, proxy_map, handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        // Test create
        let db = SqliteEngine::new(
            path.clone(),
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool.clone(),
        )
        .unwrap();
        drop(db);

        // Test open
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool.clone(),
        )
        .unwrap();
        let version = db.version().expect("Failed to get version");
        assert_eq!(version, 0);
        db.set_version(5).unwrap();
        let new_version = db.version().expect("Failed to get new version");
        assert_eq!(new_version, 5);
        drop(db);
        handle
            .delete_database(proxy_map.bottle_id, "test_db".to_string())
            .recv()
            .unwrap()
            .expect("Failed to delete database");
    }

    #[test]
    fn test_create_store() {
        let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool,
        )
        .unwrap();
        let store_name = "test_store";
        let result = db.create_store(store_name, None, true);
        assert!(result.is_ok());
        let create_result = result.unwrap();
        assert_eq!(create_result, CreateObjectResult::Created);
        // Try to create the same store again
        let result = db.create_store(store_name, None, false);
        assert!(result.is_ok());
        let create_result = result.unwrap();
        assert_eq!(create_result, CreateObjectResult::AlreadyExists);
        // Ensure store was not overwritten
        assert!(db.key_generator_current_number(store_name).is_some());
    }

    #[test]
    fn test_create_store_empty_name() {
        let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool,
        )
        .unwrap();
        let store_name = "";
        let result = db.create_store(store_name, None, true);
        assert!(result.is_ok());
        let create_result = result.unwrap();
        assert_eq!(create_result, CreateObjectResult::Created);
    }

    #[test]
    fn test_injection() {
        let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool,
        )
        .unwrap();
        // Create a normal store
        let store_name1 = "test_store";
        let result = db.create_store(store_name1, None, true);
        assert!(result.is_ok());
        let create_result = result.unwrap();
        assert_eq!(create_result, CreateObjectResult::Created);
        // Injection
        let store_name2 = "' OR 1=1 -- -";
        let result = db.create_store(store_name2, None, false);
        assert!(result.is_ok());
        let create_result = result.unwrap();
        assert_eq!(create_result, CreateObjectResult::Created);
    }

    #[test]
    fn test_key_path() {
        let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool,
        )
        .unwrap();
        let store_name = "test_store";
        let result = db.create_store(store_name, Some(KeyPath::String("test".to_string())), true);
        assert!(result.is_ok());
        assert_eq!(
            db.key_path(store_name),
            Some(KeyPath::String("test".to_string()))
        );
    }

    #[test]
    fn test_delete_store() {
        let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool,
        )
        .unwrap();
        db.create_store("test_store", None, false)
            .expect("Failed to create store");
        // Delete the store
        db.delete_store("test_store")
            .expect("Failed to delete store");
        // Try to delete the same store again
        let result = db.delete_store("test_store");
        assert!(result.is_err());
        // Try to delete a non-existing store
        let result = db.delete_store("test_store");
        // Should work as per spec
        assert!(result.is_err());
    }

    #[test]
    fn test_delete_store_removes_store_records() {
        let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool,
        )
        .unwrap();

        db.create_store("test_store", None, false)
            .expect("Failed to create store");
        let object_store = SqliteEngine::object_store_by_name(&db.connection(), "test_store")
            .expect("Failed to fetch store metadata");
        SqliteEngine::put_item(
            &db.connection(),
            object_store.clone(),
            IndexedDBKeyType::Number(1.0),
            vec![1, 2, 3],
            true,
            None,
            &vec![],
        )
        .expect("Failed to insert item");

        let row_count_before: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM object_data WHERE object_store_id = ?",
                rusqlite::params![object_store.id],
                |row| row.get(0),
            )
            .expect("Failed to count rows before delete");
        assert_eq!(row_count_before, 1);

        db.delete_store("test_store")
            .expect("Failed to delete store");

        let row_count_after: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM object_data WHERE object_store_id = ?",
                rusqlite::params![object_store.id],
                |row| row.get(0),
            )
            .expect("Failed to count rows after delete");
        assert_eq!(row_count_after, 0);
    }

    #[test]
    fn test_async_operations() {
        fn get_channel<T>() -> (GenericSender<T>, GenericReceiver<T>)
        where
            T: for<'de> Deserialize<'de> + Serialize,
        {
            generic_channel::channel().unwrap()
        }

        fn get_callback<T>(chan: GenericSender<T>) -> GenericCallback<T>
        where
            T: for<'de> Deserialize<'de> + Serialize + Send + Sync,
        {
            GenericCallback::new(move |r| {
                assert!(chan.send(r.unwrap()).is_ok());
            })
            .expect("Could not construct callback")
        }

        let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool,
        )
        .unwrap();
        let store_name = "test_store";
        db.create_store(store_name, None, false)
            .expect("Failed to create store");
        let put = get_channel();
        let put2 = get_channel();
        let put3 = get_channel();
        let put_dup = get_channel();
        let put_overwrite = get_channel();
        let get_item_some = get_channel();
        let get_item_none = get_channel();
        let get_all_items = get_channel();
        let count = get_channel();
        let remove = get_channel();
        let clear = get_channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        db.process_transaction(
            1,
            KvsTransaction {
                mode: IndexedDBTxnMode::Readwrite,
                requests: VecDeque::from(vec![
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                            callback: get_callback(put.0),
                            key: Some(IndexedDBKeyType::Number(1.0)),
                            value: vec![1, 2, 3],
                            should_overwrite: false,
                            key_generator_current_number: None,
                            index_keys: vec![],
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                            callback: get_callback(put2.0),
                            key: Some(IndexedDBKeyType::String("2.0".to_string())),
                            value: vec![4, 5, 6],
                            should_overwrite: false,
                            key_generator_current_number: None,
                            index_keys: vec![],
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                            callback: get_callback(put3.0),
                            key: Some(IndexedDBKeyType::Array(vec![
                                IndexedDBKeyType::String("3".to_string()),
                                IndexedDBKeyType::Number(0.0),
                            ])),
                            value: vec![7, 8, 9],
                            should_overwrite: false,
                            key_generator_current_number: None,
                            index_keys: vec![],
                        }),
                    },
                    // Try to put a duplicate key without overwrite
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                            callback: get_callback(put_dup.0),
                            key: Some(IndexedDBKeyType::Number(1.0)),
                            value: vec![10, 11, 12],
                            should_overwrite: false,
                            key_generator_current_number: None,
                            index_keys: vec![],
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                            callback: get_callback(put_overwrite.0),
                            key: Some(IndexedDBKeyType::Number(1.0)),
                            value: vec![13, 14, 15],
                            should_overwrite: true,
                            key_generator_current_number: None,
                            index_keys: vec![],
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        operation: AsyncOperation::ReadOnly(AsyncReadOnlyOperation::GetItem {
                            callback: get_callback(get_item_some.0),
                            key_range: IndexedDBKeyRange::only(IndexedDBKeyType::Number(1.0)),
                            index_name: None,
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        operation: AsyncOperation::ReadOnly(AsyncReadOnlyOperation::GetItem {
                            callback: get_callback(get_item_none.0),
                            key_range: IndexedDBKeyRange::only(IndexedDBKeyType::Number(5.0)),
                            index_name: None,
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        operation: AsyncOperation::ReadOnly(AsyncReadOnlyOperation::GetAllItems {
                            callback: get_callback(get_all_items.0),
                            key_range: IndexedDBKeyRange::lower_bound(
                                IndexedDBKeyType::Number(0.0),
                                false,
                            ),
                            count: None,
                            index_name: None,
                            direction: Default::default(),
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        operation: AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Count {
                            callback: get_callback(count.0),
                            key_range: IndexedDBKeyRange::only(IndexedDBKeyType::Number(1.0)),
                            index_name: None,
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::RemoveItem {
                            callback: get_callback(remove.0),
                            key_range: IndexedDBKeyRange::only(IndexedDBKeyType::Number(1.0)),
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::Clear(
                            get_callback(clear.0),
                        )),
                    },
                ]),
            },
            Box::new(move || {
                let _ = done_tx.send(());
            }),
        );
        let _ = done_rx.recv().unwrap();
        put.1.recv().unwrap().unwrap();
        put2.1.recv().unwrap().unwrap();
        put3.1.recv().unwrap().unwrap();
        let err = put_dup.1.recv().unwrap().unwrap();
        assert_eq!(err, PutItemResult::CannotOverwrite);
        let overwritten = put_overwrite.1.recv().unwrap().unwrap();
        assert_eq!(
            overwritten,
            PutItemResult::Key(IndexedDBKeyType::Number(1.0))
        );
        let get_result = get_item_some.1.recv().unwrap();
        let value = get_result.unwrap();
        assert_eq!(value, Some(vec![13, 14, 15]));
        let get_result = get_item_none.1.recv().unwrap();
        let value = get_result.unwrap();
        assert_eq!(value, None);
        let all_items = get_all_items.1.recv().unwrap().unwrap();
        assert_eq!(all_items.len(), 3);
        // Check that all three items are present
        assert!(all_items.contains(&vec![13, 14, 15]));
        assert!(all_items.contains(&vec![4, 5, 6]));
        assert!(all_items.contains(&vec![7, 8, 9]));
        let amount = count.1.recv().unwrap().unwrap();
        assert_eq!(amount, 1);
        remove.1.recv().unwrap().unwrap();
        clear.1.recv().unwrap().unwrap();
    }

    #[test]
    fn test_delete_item_range_respects_open_bounds() {
        fn remaining_keys_after_delete(
            lower: i32,
            upper: i32,
            lower_open: bool,
            upper_open: bool,
        ) -> Vec<i32> {
            let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
            let thread_pool = get_pool();
            let db = SqliteEngine::new(
                path,
                created,
                &IndexedDBDescription {
                    name: "test_db".to_string(),
                    origin: test_origin(),
                },
                thread_pool,
            )
            .unwrap();
            let store_name = "test_store";
            db.create_store(store_name, None, false)
                .expect("Failed to create store");
            let store = SqliteEngine::object_store_by_name(&db.connection(), store_name)
                .expect("Failed to get object store");

            for key in 1..=10 {
                SqliteEngine::put_item(
                    &db.connection(),
                    store.clone(),
                    IndexedDBKeyType::Number(key as f64),
                    vec![key as u8],
                    false,
                    None,
                    &vec![],
                )
                .expect("Failed to seed object store");
            }

            SqliteEngine::delete_item(
                &db.connection(),
                store.clone(),
                IndexedDBKeyRange::new(
                    Some(IndexedDBKeyType::Number(lower as f64)),
                    Some(IndexedDBKeyType::Number(upper as f64)),
                    lower_open,
                    upper_open,
                ),
            )
            .expect("Failed to delete key range");

            SqliteEngine::get_all_keys(&db.connection(), store, IndexedDBKeyRange::default(), None)
                .expect("Failed to read remaining keys")
                .into_iter()
                .map(|raw_key| match encoding::deserialize(&raw_key).unwrap() {
                    IndexedDBKeyType::Number(number) => number as i32,
                    other => panic!("Expected numeric key, got {other:?}"),
                })
                .collect()
        }

        assert_eq!(
            remaining_keys_after_delete(3, 8, false, false),
            vec![1, 2, 9, 10]
        );
        assert_eq!(
            remaining_keys_after_delete(3, 8, true, false),
            vec![1, 2, 3, 9, 10]
        );
        assert_eq!(
            remaining_keys_after_delete(3, 8, false, true),
            vec![1, 2, 8, 9, 10]
        );
        assert_eq!(
            remaining_keys_after_delete(3, 8, true, true),
            vec![1, 2, 3, 8, 9, 10]
        );
    }

    /// Partii: a fresh engine for one test.
    fn open_engine() -> (
        tempfile::TempDir,
        StorageProxyMap,
        ClientStorageThreadHandle,
        SqliteEngine,
    ) {
        let (temp_dir, path, created, proxy_map, handle) = create_db("test_db".to_string());
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            get_pool(),
        )
        .unwrap();
        (temp_dir, proxy_map, handle, db)
    }

    /// Partii: run one batch of `txn` and wait for it to finish.
    fn run_batch(db: &SqliteEngine, txn: u64, mode: IndexedDBTxnMode, requests: Vec<KvsOperation>) {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        db.process_transaction(
            txn,
            KvsTransaction {
                mode,
                requests: VecDeque::from(requests),
            },
            Box::new(move || {
                let _ = done_tx.send(());
            }),
        );
        done_rx.recv().unwrap();
    }

    fn put_request(
        store_name: &str,
        key: f64,
        value: Vec<u8>,
        sender: GenericSender<BackendResult<PutItemResult>>,
    ) -> KvsOperation {
        KvsOperation {
            store_name: store_name.to_owned(),
            operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                callback: GenericCallback::new(move |result| {
                    let _ = sender.send(result.unwrap());
                })
                .unwrap(),
                key: Some(IndexedDBKeyType::Number(key)),
                value,
                should_overwrite: true,
                key_generator_current_number: None,
                index_keys: vec![],
            }),
        }
    }

    fn record_count(db: &SqliteEngine) -> i64 {
        db.connection()
            .query_row("SELECT COUNT(*) FROM object_data", [], |row| row.get(0))
            .unwrap()
    }

    /// Partii: an aborted readwrite transaction leaves nothing behind, a committed one persists,
    /// and a batch that arrives after its transaction ended cannot reopen it.
    #[test]
    fn test_readwrite_transaction_rollback_and_commit() {
        let (_temp_dir, _proxy_map, _handle, db) = open_engine();
        db.create_store("store", None, false).unwrap();

        let (sender, receiver) = generic_channel::channel().unwrap();
        run_batch(
            &db,
            1,
            IndexedDBTxnMode::Readwrite,
            vec![put_request("store", 1.0, vec![1], sender.clone())],
        );
        assert!(receiver.recv().unwrap().is_ok());
        assert_eq!(record_count(&db), 1, "the transaction sees its own write");
        db.rollback_transaction(1);
        assert_eq!(record_count(&db), 0, "rollback removes the write");

        // A late batch of the rolled back transaction fails instead of writing.
        run_batch(
            &db,
            1,
            IndexedDBTxnMode::Readwrite,
            vec![put_request("store", 2.0, vec![2], sender.clone())],
        );
        assert!(receiver.recv().unwrap().is_err());
        assert_eq!(record_count(&db), 0);

        run_batch(
            &db,
            2,
            IndexedDBTxnMode::Readwrite,
            vec![put_request("store", 3.0, vec![3], sender.clone())],
        );
        assert!(receiver.recv().unwrap().is_ok());
        // Another write transaction cannot start while transaction 2 is open.
        assert!(db.begin_transaction(3).is_err());
        db.commit_transaction(2).unwrap();
        assert_eq!(record_count(&db), 1);
        assert!(db.begin_transaction(3).is_ok());
        db.rollback_transaction(3);
        assert_eq!(record_count(&db), 1, "committed data survives");
    }

    fn rename_store_request(store_name: &str, new_name: &str) -> KvsOperation {
        KvsOperation {
            store_name: store_name.to_owned(),
            operation: AsyncOperation::Schema(AsyncSchemaOperation::RenameObjectStore {
                callback: GenericCallback::new(|_| {}).unwrap(),
                new_name: new_name.to_owned(),
            }),
        }
    }

    /// Partii: a store renamed in an upgrade transaction keeps its records; the rename is undone
    /// when the transaction aborts and kept when it commits.
    #[test]
    fn test_rename_store_rolls_back_and_commits() {
        let (_temp_dir, _proxy_map, _handle, db) = open_engine();
        db.create_store("store", None, false).unwrap();
        let (sender, receiver) = generic_channel::channel().unwrap();
        run_batch(
            &db,
            1,
            IndexedDBTxnMode::Readwrite,
            vec![put_request("store", 1.0, vec![1], sender.clone())],
        );
        assert!(receiver.recv().unwrap().is_ok());
        db.commit_transaction(1).unwrap();

        run_batch(
            &db,
            2,
            IndexedDBTxnMode::Versionchange,
            vec![rename_store_request("store", "renamed")],
        );
        assert_eq!(db.object_store_names().unwrap(), vec!["renamed".to_owned()]);
        db.rollback_transaction(2);
        assert_eq!(db.object_store_names().unwrap(), vec!["store".to_owned()]);

        run_batch(
            &db,
            3,
            IndexedDBTxnMode::Versionchange,
            vec![rename_store_request("store", "renamed")],
        );
        db.commit_transaction(3).unwrap();
        assert_eq!(db.object_store_names().unwrap(), vec!["renamed".to_owned()]);
        let store = SqliteEngine::object_store_by_name(&db.connection(), "renamed").unwrap();
        let count: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM object_data WHERE object_store_id = ?",
                params![store.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "the records follow the store");
    }

    fn schema_request(store_name: &str, operation: AsyncSchemaOperation) -> KvsOperation {
        KvsOperation {
            store_name: store_name.to_owned(),
            operation: AsyncOperation::Schema(operation),
        }
    }

    fn index_names(db: &SqliteEngine, store_name: &str) -> Vec<String> {
        let mut names: Vec<String> = db
            .indexes(store_name)
            .unwrap()
            .into_iter()
            .map(|index| index.name)
            .collect();
        names.sort();
        names
    }

    /// Partii: index and store metadata changed in an upgrade transaction (createIndex,
    /// deleteIndex, deleteObjectStore) comes back when the transaction aborts.
    #[test]
    fn test_aborted_upgrade_rolls_back_index_metadata() {
        let (_temp_dir, _proxy_map, _handle, db) = open_engine();
        db.create_store("store", None, false).unwrap();
        db.create_store("other", None, false).unwrap();
        db.create_index(
            "store",
            "kept".to_owned(),
            KeyPath::String("a".into()),
            false,
            false,
        )
        .unwrap();
        let callback = || GenericCallback::new(|_| {}).unwrap();

        run_batch(
            &db,
            1,
            IndexedDBTxnMode::Versionchange,
            vec![
                schema_request(
                    "store",
                    AsyncSchemaOperation::CreateIndex {
                        callback: callback(),
                        index_name: "added".to_owned(),
                        key_path: KeyPath::String("b".into()),
                        unique: false,
                        multi_entry: false,
                    },
                ),
                schema_request(
                    "store",
                    AsyncSchemaOperation::DeleteIndex {
                        callback: callback(),
                        index_name: "kept".to_owned(),
                    },
                ),
                schema_request(
                    "other",
                    AsyncSchemaOperation::DeleteObjectStore {
                        callback: callback(),
                    },
                ),
            ],
        );
        assert_eq!(index_names(&db, "store"), vec!["added".to_owned()]);
        assert_eq!(db.object_store_names().unwrap(), vec!["store".to_owned()]);
        db.rollback_transaction(1);
        assert_eq!(index_names(&db, "store"), vec!["kept".to_owned()]);
        let mut stores = db.object_store_names().unwrap();
        stores.sort();
        assert_eq!(stores, vec!["other".to_owned(), "store".to_owned()]);
    }

    fn index_row_count(db: &SqliteEngine) -> i64 {
        db.connection()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM index_data) + (SELECT COUNT(*) FROM unique_index_data)",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn put_with_index_keys(
        db: &SqliteEngine,
        key: f64,
        index_keys: IndexKeys,
        should_overwrite: bool,
    ) -> PutItemResult {
        let store = SqliteEngine::object_store_by_name(&db.connection(), "store").unwrap();
        SqliteEngine::put_item(
            &db.connection(),
            store,
            IndexedDBKeyType::Number(key),
            vec![key as u8],
            should_overwrite,
            None,
            &index_keys,
        )
        .unwrap()
    }

    fn string_key(value: &str) -> IndexedDBKeyType {
        IndexedDBKeyType::String(value.to_owned())
    }

    /// Partii: a put writes the record's index records, an overwrite replaces them and a delete
    /// removes them; a unique index clash fails the put with no trace of it.
    #[test]
    fn test_index_records_and_unique_constraint() {
        let (_temp_dir, _proxy_map, _handle, db) = open_engine();
        db.create_store("store", None, false).unwrap();
        db.create_index(
            "store",
            "by_name".to_owned(),
            KeyPath::String("name".to_owned()),
            true,
            false,
        )
        .unwrap();

        let one = vec![("by_name".to_owned(), vec![string_key("a")])];
        assert_eq!(
            put_with_index_keys(&db, 1.0, one.clone(), true),
            PutItemResult::Key(IndexedDBKeyType::Number(1.0))
        );
        assert_eq!(index_row_count(&db), 1);
        // Overwriting the same record with the same index key is not a clash.
        assert!(matches!(
            put_with_index_keys(&db, 1.0, one.clone(), true),
            PutItemResult::Key(_)
        ));
        assert_eq!(index_row_count(&db), 1);

        // Another record with the same unique key fails with ConstraintError and leaves nothing.
        assert_eq!(
            put_with_index_keys(&db, 2.0, one.clone(), true),
            PutItemResult::CannotOverwrite
        );
        assert_eq!(record_count(&db), 1);
        assert_eq!(index_row_count(&db), 1);

        // Overwriting record 1 with another key frees "a".
        let other = vec![("by_name".to_owned(), vec![string_key("b")])];
        assert!(matches!(
            put_with_index_keys(&db, 1.0, other, true),
            PutItemResult::Key(_)
        ));
        assert!(matches!(
            put_with_index_keys(&db, 2.0, one, true),
            PutItemResult::Key(_)
        ));
        assert_eq!(index_row_count(&db), 2);

        let store = SqliteEngine::object_store_by_name(&db.connection(), "store").unwrap();
        SqliteEngine::delete_item(
            &db.connection(),
            store.clone(),
            IndexedDBKeyRange::only(IndexedDBKeyType::Number(1.0)),
        )
        .unwrap();
        assert_eq!(index_row_count(&db), 1);
        SqliteEngine::clear(&db.connection(), store).unwrap();
        assert_eq!(index_row_count(&db), 0);
    }

    /// Partii: a multiEntry index gets one record per subkey; deleting the index removes them.
    #[test]
    fn test_multi_entry_index_records() {
        let (_temp_dir, _proxy_map, _handle, db) = open_engine();
        db.create_store("store", None, false).unwrap();
        db.create_index(
            "store",
            "tags".to_owned(),
            KeyPath::String("tags".to_owned()),
            false,
            true,
        )
        .unwrap();
        let tags = vec![(
            "tags".to_owned(),
            vec![string_key("x"), string_key("y"), string_key("z")],
        )];
        assert!(matches!(
            put_with_index_keys(&db, 1.0, tags.clone(), true),
            PutItemResult::Key(_)
        ));
        assert!(matches!(
            put_with_index_keys(&db, 2.0, tags, true),
            PutItemResult::Key(_)
        ));
        assert_eq!(index_row_count(&db), 6);
        db.delete_index("store", "tags".to_owned()).unwrap();
        assert_eq!(index_row_count(&db), 0);
    }

    /// Partii: an aborted readwrite transaction leaves no record and no index record.
    #[test]
    fn test_aborted_transaction_leaves_no_index_records() {
        let (_temp_dir, _proxy_map, _handle, db) = open_engine();
        db.create_store("store", None, false).unwrap();
        db.create_index(
            "store",
            "by_name".to_owned(),
            KeyPath::String("name".to_owned()),
            false,
            false,
        )
        .unwrap();
        let (sender, receiver) = generic_channel::channel().unwrap();
        let mut put = put_request("store", 1.0, vec![1], sender);
        if let AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem { index_keys, .. }) =
            &mut put.operation
        {
            *index_keys = vec![("by_name".to_owned(), vec![string_key("a")])];
        }
        run_batch(&db, 7, IndexedDBTxnMode::Readwrite, vec![put]);
        assert!(receiver.recv().unwrap().is_ok());
        assert_eq!(index_row_count(&db), 1);
        db.rollback_transaction(7);
        assert_eq!(record_count(&db), 0);
        assert_eq!(index_row_count(&db), 0);
    }

    /// Partii: reads through an index come in (index key, primary key) order, honour the range
    /// and every direction, and return primary keys.
    #[test]
    fn test_index_get_all_order() {
        let (_temp_dir, _proxy_map, _handle, db) = open_engine();
        db.create_store("store", None, false).unwrap();
        db.create_index(
            "store",
            "by_name".to_owned(),
            KeyPath::String("name".to_owned()),
            false,
            false,
        )
        .unwrap();
        for (key, name) in [(1.0, "b"), (2.0, "a"), (3.0, "b"), (4.0, "a"), (5.0, "c")] {
            let index_keys = vec![("by_name".to_owned(), vec![string_key(name)])];
            assert!(matches!(
                put_with_index_keys(&db, key, index_keys, true),
                PutItemResult::Key(_)
            ));
        }
        let connection = db.connection();
        let store = SqliteEngine::object_store_by_name(&connection, "store").unwrap();
        let index = SqliteEngine::index_by_name(&connection, store.id, "by_name")
            .unwrap()
            .unwrap();
        let read = |range: &IndexedDBKeyRange, direction, count| {
            decode_records(
                SqliteEngine::read_records(
                    &connection,
                    store.id,
                    Some(&index),
                    range,
                    direction,
                    count,
                    true,
                )
                .unwrap(),
            )
            .unwrap()
            .into_iter()
            .map(|record| {
                let IndexedDBKeyType::String(name) = record.key else {
                    panic!("index keys are strings");
                };
                let IndexedDBKeyType::Number(primary_key) = record.primary_key else {
                    panic!("primary keys are numbers");
                };
                assert_eq!(record.value, vec![primary_key as u8]);
                (name, primary_key)
            })
            .collect::<Vec<_>>()
        };
        let all = IndexedDBKeyRange::default();
        let pairs = |list: &[(&str, f64)]| {
            list.iter()
                .map(|(name, key)| (name.to_string(), *key))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            read(&all, IndexedDBDirection::Next, None),
            pairs(&[("a", 2.0), ("a", 4.0), ("b", 1.0), ("b", 3.0), ("c", 5.0)])
        );
        assert_eq!(
            read(&all, IndexedDBDirection::Prev, None),
            pairs(&[("c", 5.0), ("b", 3.0), ("b", 1.0), ("a", 4.0), ("a", 2.0)])
        );
        assert_eq!(
            read(&all, IndexedDBDirection::NextUnique, None),
            pairs(&[("a", 2.0), ("b", 1.0), ("c", 5.0)])
        );
        assert_eq!(
            read(&all, IndexedDBDirection::PrevUnique, Some(2)),
            pairs(&[("c", 5.0), ("b", 1.0)])
        );
        assert_eq!(
            read(&all, IndexedDBDirection::Next, Some(3)),
            pairs(&[("a", 2.0), ("a", 4.0), ("b", 1.0)])
        );
        let only_b = IndexedDBKeyRange::only(string_key("b"));
        assert_eq!(
            read(&only_b, IndexedDBDirection::Next, Some(0)),
            pairs(&[("b", 1.0), ("b", 3.0)])
        );
        assert_eq!(
            SqliteEngine::count_records(&connection, store.id, Some(&index), &only_b).unwrap(),
            2
        );
        assert_eq!(
            SqliteEngine::count_records(&connection, store.id, None, &all).unwrap(),
            5
        );
    }

    fn index_populated(db: &SqliteEngine, store: &str, index: &str) -> bool {
        db.indexes(store)
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.name == index)
            .unwrap()
            .populated
    }

    /// Partii: a database written before index records existed gets a `populated` column on
    /// open; its indexes over stores with records start unpopulated, and SetIndexEntries fills
    /// them. A unique clash with `fail_on_clash` writes nothing; without it the record is left
    /// out.
    #[test]
    fn test_populate_index_of_old_database() {
        let (_temp_dir, _proxy_map, _handle, db) = open_engine();
        db.create_store("store", None, false).unwrap();
        db.create_store("empty", None, false).unwrap();
        for (store, index, unique) in [
            ("store", "by_name", false),
            ("store", "by_tag", true),
            ("empty", "by_name", false),
        ] {
            db.create_index(
                store,
                index.to_owned(),
                KeyPath::String(index.to_owned()),
                unique,
                false,
            )
            .unwrap();
        }
        assert!(index_populated(&db, "store", "by_name"));
        // Records written by a build that did not write index records.
        for key in [1.0, 2.0] {
            assert!(matches!(
                put_with_index_keys(&db, key, vec![], true),
                PutItemResult::Key(_)
            ));
        }
        assert_eq!(index_row_count(&db), 0);

        // Open it as an old database: no populated column yet.
        {
            let connection = db.connection();
            connection
                .execute_batch("ALTER TABLE object_store_index DROP COLUMN populated")
                .unwrap();
            super::create::upgrade_tables(&connection).unwrap();
            // Upgrading again is a no-op.
            super::create::upgrade_tables(&connection).unwrap();
        }
        assert!(!index_populated(&db, "store", "by_name"));
        assert!(!index_populated(&db, "store", "by_tag"));
        assert!(index_populated(&db, "empty", "by_name"));

        let store = SqliteEngine::object_store_by_name(&db.connection(), "store").unwrap();
        let entries = |tag_of_two: &str| {
            vec![
                (
                    IndexedDBKeyType::Number(1.0),
                    vec![
                        ("by_name".to_owned(), vec![string_key("a")]),
                        ("by_tag".to_owned(), vec![string_key("x")]),
                    ],
                ),
                (
                    IndexedDBKeyType::Number(2.0),
                    vec![
                        ("by_name".to_owned(), vec![string_key("b")]),
                        ("by_tag".to_owned(), vec![string_key(tag_of_two)]),
                    ],
                ),
            ]
        };
        let names = vec!["by_name".to_owned(), "by_tag".to_owned()];

        // createIndex: a clash fails and leaves nothing behind.
        assert!(
            !SqliteEngine::set_index_entries(&db.connection(), &store, &names, &entries("x"), true)
                .unwrap()
        );
        assert_eq!(index_row_count(&db), 0);
        assert!(!index_populated(&db, "store", "by_name"));

        // Lazy population skips the clashing record in the unique index only.
        assert!(
            SqliteEngine::set_index_entries(&db.connection(), &store, &names, &entries("x"), false)
                .unwrap()
        );
        assert_eq!(index_row_count(&db), 3);
        assert!(index_populated(&db, "store", "by_name"));
        assert!(index_populated(&db, "store", "by_tag"));

        // Populating again replaces the index records instead of adding to them.
        assert!(
            SqliteEngine::set_index_entries(&db.connection(), &store, &names, &entries("y"), true)
                .unwrap()
        );
        assert_eq!(index_row_count(&db), 4);
    }
}
