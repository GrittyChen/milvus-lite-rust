//! 数据库：多 collection 的生命周期与引擎的关闭。M1 只有 `default` 一个数据库（0004「范围」）。

use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::{Condvar, Mutex, RwLock};

use crate::collection::{Collection, LoadState};
use crate::compute::Compute;
use crate::data::{InsertBatch, Pk};
use crate::data_dir::DataDir;
use crate::error::{EngineError, Result, py_repr};
use crate::filter::parse_pk_in;
use crate::index::IndexRequest;
use crate::schema::CollectionSchema;
use crate::storage::{fs_commit, recovery, runtime, schema_file::SchemaFile};

/// 每个 collection 一把读写锁：写入互斥、读取并发，仍是「单 collection 单写者」（不变量 7）。
pub type CollectionHandle = Arc<RwLock<Collection>>;

pub struct InsertOutcome {
    pub pks: Vec<Pk>,
    pub varchar: bool,
}

pub const DEFAULT_DATABASE: &str = "default";
static STAGING_NUMBER: AtomicU64 = AtomicU64::new(1);

#[cfg(feature = "fault-injection")]
thread_local! {
    static FAIL_NEXT_CLOSE_WORKER_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_NEXT_CREATE_NAME_VERIFICATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(feature = "fault-injection")]
pub fn inject_close_worker_spawn_failure_once() {
    FAIL_NEXT_CLOSE_WORKER_SPAWN.with(|flag| flag.set(true));
}

#[cfg(feature = "fault-injection")]
pub fn inject_create_name_verification_failure_once() {
    FAIL_NEXT_CREATE_NAME_VERIFICATION.with(|flag| flag.set(true));
}

#[derive(Debug)]
pub struct Db {
    /// 按名字排序，list 的顺序与 3.x 的 `sorted(names)`（码点序）一致。
    collections: RwLock<BTreeMap<String, CollectionHandle>>,
    gate: Mutex<Gate>,
    idle: Condvar,
    /// 服务的计算池（0005）。服务的运行状态与每个在途的 RPC 都持有 `Arc<Db>`，
    /// 所以它在最后一个在途任务结束之后才析构。
    compute: Compute,
    path: PathBuf,
    namespace: Mutex<()>,
    /// create/drop 与 collection 的写入/flush 互斥；目录提交点后失败时封锁后续写入。
    write_gate: RwLock<bool>,
}

/// 关闭闸门（0003「进程退出」第 2、3 步）：关闭后不再接受新任务，在途任务全部结束后才释放 LOCK，
/// 保证 LOCK 释放时没有任何线程还在读写引擎状态。
#[derive(Debug)]
struct Gate {
    closed: bool,
    in_flight: usize,
    cleanup_started: bool,
    /// 持有期间独占数据目录的 LOCK。
    data_dir: Option<DataDir>,
}

/// 一个在途任务。引擎的每次使用都要持有它；析构时若引擎已关闭且这是最后一个任务，就释放 LOCK。
#[must_use]
pub struct InFlight<'a> {
    db: &'a Db,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let mut gate = self.db.gate.lock();
        gate.in_flight -= 1;
        if gate.in_flight == 0 && gate.closed {
            self.db.idle.notify_all();
        }
    }
}

impl Db {
    pub fn new(data_dir: DataDir, compute: Compute) -> Self {
        let path = data_dir.path().to_path_buf();
        Self {
            collections: RwLock::new(BTreeMap::new()),
            gate: Mutex::new(Gate {
                closed: false,
                in_flight: 0,
                cleanup_started: false,
                data_dir: Some(data_dir),
            }),
            idle: Condvar::new(),
            compute,
            path,
            namespace: Mutex::new(()),
            write_gate: RwLock::new(false),
        }
    }

    pub fn compute(&self) -> &Compute {
        &self.compute
    }

    /// 开始一个任务；引擎已关闭时报错（3.x 关闭后的调用同样报 `MilvusLite is closed`）。
    pub fn enter(&self) -> Result<InFlight<'_>> {
        let mut gate = self.gate.lock();
        if gate.closed {
            return Err(EngineError::Closed);
        }
        gate.in_flight += 1;
        Ok(InFlight { db: self })
    }

    /// 关闭引擎：拒绝新任务，等在途任务全部结束（任务有界，不设超时），然后释放 LOCK。可重复调用。
    pub fn close(&self) {
        let mut gate = self.gate.lock();
        gate.closed = true;
        if gate.cleanup_started {
            while gate.data_dir.is_some() {
                self.idle.wait(&mut gate);
            }
            return;
        }
        gate.cleanup_started = true;
        while gate.in_flight > 0 {
            self.idle.wait(&mut gate);
        }
        drop(gate);
        self.finish_storage(true);
        let mut gate = self.gate.lock();
        gate.data_dir = None;
        self.idle.notify_all();
    }

    /// 不阻塞的关闭：拒绝新任务；没有在途任务时立即释放 LOCK，否则由最后一个任务结束时释放。
    pub fn close_nowait(self: &Arc<Self>) {
        let mut gate = self.gate.lock();
        gate.closed = true;
        if gate.cleanup_started {
            return;
        }
        gate.cleanup_started = true;
        let db = Arc::clone(self);
        drop(gate);
        #[cfg(feature = "fault-injection")]
        let injected_failure = FAIL_NEXT_CLOSE_WORKER_SPAWN.with(|flag| flag.replace(false));
        #[cfg(not(feature = "fault-injection"))]
        let injected_failure = false;
        let spawned = if injected_failure {
            Err(std::io::Error::other("injected close worker spawn failure"))
        } else {
            std::thread::Builder::new()
                .name("milvus-lite-storage-close".to_owned())
                .spawn(move || {
                    let mut gate = db.gate.lock();
                    while gate.in_flight > 0 {
                        db.idle.wait(&mut gate);
                    }
                    drop(gate);
                    db.finish_storage(false);
                    let mut gate = db.gate.lock();
                    gate.data_dir = None;
                    db.idle.notify_all();
                })
        };
        if let Err(error) = spawned {
            eprintln!("cannot start storage close worker; synchronously syncing WAL: {error}");
            let mut gate = self.gate.lock();
            while gate.in_flight > 0 {
                self.idle.wait(&mut gate);
            }
            drop(gate);
            self.finish_storage(false);
            let mut gate = self.gate.lock();
            gate.data_dir = None;
            self.idle.notify_all();
        }
    }

    fn finish_storage(&self, flush: bool) {
        // 目录改名的持久化未确认时，任何 manifest 提交都可能固定错误的目录状态。
        let flush = flush && !*self.write_gate.read();
        let handles: Vec<_> = self.collections.read().values().cloned().collect();
        for handle in handles {
            if flush {
                match Self::flush_handle(&handle, true) {
                    Ok(()) => {
                        let extra_wal = handle
                            .read()
                            .persistent()
                            .ok_or_else(|| storage("persistent state missing"))
                            .and_then(|p| p.has_wal().map_err(storage));
                        match extra_wal {
                            Ok(true) => {
                                let frozen = handle
                                    .write()
                                    .persistent_mut()
                                    .ok_or_else(|| storage("persistent state missing"))
                                    .and_then(|p| p.freeze_active(true).map_err(storage));
                                if let Err(error) =
                                    frozen.and_then(|()| Self::flush_handle(&handle, false))
                                {
                                    eprintln!("collection close WAL cleanup failed: {error}");
                                }
                            }
                            Ok(false) => {}
                            Err(error) => eprintln!("collection close WAL scan failed: {error}"),
                        }
                    }
                    Err(error) => eprintln!("collection close flush failed: {error}"),
                }
            }
            let result = handle
                .write()
                .persistent_mut()
                .map(|p| p.sync_retained_wal());
            if let Some(Err(error)) = result {
                eprintln!("collection close WAL sync failed: {error}");
            }
        }
    }

    /// 检查顺序与 3.x 相同：数据库、名字合法性、schema 校验、是否重名。
    pub fn create_collection(
        &self,
        database: &str,
        name: &str,
        schema: CollectionSchema,
    ) -> Result<()> {
        require_database(database)?;
        validate_filesystem_name(name, "collection")?;
        let mut collection = Collection::new(name, schema)?;
        let mut blocked = self.write_gate.write();
        if *blocked {
            return Err(EngineError::Storage(
                "database writes are blocked after an unconfirmed directory change".to_owned(),
            ));
        }
        let _namespace = self.namespace.lock();
        let target = self.collection_path(name);
        if self.collections.read().contains_key(name) || fs::symlink_metadata(&target).is_ok() {
            return Err(EngineError::CollectionAlreadyExists(name.to_owned()));
        }
        let root = self.path.join("collections");
        let staging_root = self.path.join(".database-staging");
        for dir in [&root, &staging_root] {
            match fs::symlink_metadata(dir) {
                Ok(meta) if meta.file_type().is_dir() => {}
                Ok(_) => return Err(storage(format!("directory path {dir:?} is occupied"))),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(storage(error)),
            }
        }
        let mut aliases = self.read_aliases()?;
        let old_alias_count = aliases.len();
        aliases.retain(|_, target_name| {
            validate_filesystem_name(target_name, "collection").is_ok()
                && self.collection_path(target_name).exists()
        });
        if aliases.len() != old_alias_count {
            self.save_aliases(&aliases)?;
        }
        if aliases.contains_key(name) {
            return Err(EngineError::CollectionAlreadyExists(name.to_owned()));
        }
        let bytes = SchemaFile::from_core(name, collection.schema())
            .to_bytes()
            .map_err(storage)?;
        fs_commit::create_dir_synced(&root).map_err(storage)?;
        fs_commit::create_dir_synced(&staging_root).map_err(storage)?;
        let staging = staging_root.join(format!(
            "create-{}-{}",
            std::process::id(),
            STAGING_NUMBER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&staging).map_err(storage)?;
        if let Err(error) = fs_commit::sync_dir(&staging_root) {
            cleanup_failed_staging(&staging);
            return Err(storage(error));
        }
        if let Err(error) = fs_commit::write_new_schema(&staging.join("schema.json"), &bytes) {
            cleanup_failed_staging(&staging);
            return Err(storage(error));
        }
        if let Err(error) = fs::rename(&staging, &target) {
            cleanup_failed_staging(&staging);
            return Err(storage(error));
        }
        #[cfg(feature = "fault-injection")]
        let simulated_mismatch =
            FAIL_NEXT_CREATE_NAME_VERIFICATION.with(|flag| flag.replace(false));
        #[cfg(not(feature = "fault-injection"))]
        let simulated_mismatch = false;
        let exact_name = if simulated_mismatch {
            Ok(false)
        } else {
            self.exact_collection_name(name)
        };
        match exact_name {
            Ok(true) => {}
            verdict => {
                let reason = match verdict {
                    Ok(false) => {
                        format!("created collection name {name:?} was normalized by the filesystem")
                    }
                    Err(error) => error.to_string(),
                    Ok(true) => unreachable!(),
                };
                if let Err(error) = fs::rename(&target, &staging)
                    .and_then(|()| fs_commit::sync_dir(&root))
                    .and_then(|()| fs_commit::sync_dir(&staging_root))
                {
                    *blocked = true;
                    return Err(storage(format!(
                        "collection name verification failed ({reason}); rollback durability is unconfirmed: {error}"
                    )));
                }
                cleanup_failed_staging(&staging);
                return Err(storage(reason));
            }
        }
        collection.attach_persistence(
            runtime::PersistentCollection::new(target.clone()).map_err(EngineError::Storage)?,
        );
        self.collections
            .write()
            .insert(name.to_owned(), Arc::new(RwLock::new(collection)));
        if let Err(error) =
            fs_commit::sync_dir(&root).and_then(|()| fs_commit::sync_dir(&staging_root))
        {
            *blocked = true;
            return Err(EngineError::Storage(format!(
                "collection was created, but directory durability is unconfirmed: {error}"
            )));
        }
        Ok(())
    }

    /// drop 的提交点是把目录改名移入 staging；之后才清别名与 staging。
    pub fn drop_collection(&self, database: &str, name: &str) -> Result<()> {
        require_database(database)?;
        if name.is_empty() {
            return Ok(());
        }
        validate_filesystem_name(name, "collection")?;
        let mut blocked = self.write_gate.write();
        if *blocked {
            return Err(storage(
                "database writes are blocked after an unconfirmed directory change",
            ));
        }
        let _namespace = self.namespace.lock();
        let target = self.collection_path(name);
        let aliases = self.read_aliases()?;
        let exists = match fs::symlink_metadata(&target) {
            Ok(meta) => meta.file_type().is_dir() && target.join("schema.json").is_file(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(storage(error)),
        };
        if !exists || !self.exact_collection_name(name)? {
            self.remove_aliases_for(name, aliases)?;
            return Ok(());
        }
        let root = self.path.join("collections");
        let staging_root = self.path.join(".database-staging");
        fs_commit::create_dir_synced(&staging_root).map_err(storage)?;
        let handle = self.collections.read().get(name).cloned();
        let flush_lock = handle
            .as_ref()
            .and_then(|h| h.read().persistent().map(|p| p.flush_lock()));
        let _flush = flush_lock.as_ref().map(|lock| lock.lock());
        if let Some(handle) = &handle {
            let mut collection = handle.write();
            collection.mark_deleted(true);
            if let Some(persistent) = collection.persistent_mut()
                && let Err(error) = persistent.sync_retained_wal()
            {
                let rotate = persistent.freeze_active(true);
                collection.mark_deleted(false);
                rotate.map_err(storage)?;
                return Err(storage(error));
            }
        }
        let staging = staging_root.join(format!(
            "drop-{}-{}",
            std::process::id(),
            STAGING_NUMBER.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(error) = fs::rename(&target, &staging) {
            if let Some(handle) = &handle {
                let mut collection = handle.write();
                collection.mark_deleted(false);
                if let Some(persistent) = collection.persistent_mut() {
                    persistent.freeze_active(true).map_err(storage)?;
                }
            }
            return Err(storage(error));
        }
        self.collections.write().remove(name);
        if let Err(error) =
            fs_commit::sync_dir(&root).and_then(|()| fs_commit::sync_dir(&staging_root))
        {
            *blocked = true;
            return Err(storage(format!(
                "collection was removed, but directory durability is unconfirmed: {error}"
            )));
        }
        self.remove_aliases_for(name, aliases)?;
        if let Err(error) =
            fs::remove_dir_all(&staging).and_then(|()| fs_commit::sync_dir(&staging_root))
        {
            eprintln!("drop staging cleanup deferred for {staging:?}: {error}");
        }
        Ok(())
    }

    fn exact_collection_name(&self, name: &str) -> Result<bool> {
        let root = self.path.join("collections");
        match fs::symlink_metadata(&root) {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => return Err(storage("collections path is not a directory")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(storage(error)),
        }
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(storage(error)),
        };
        for entry in entries {
            if entry.map_err(storage)?.file_name() == name {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn read_aliases(&self) -> Result<BTreeMap<String, String>> {
        let path = self.path.join("aliases.json");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new());
            }
            Err(error) => return Err(storage(error)),
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(storage)?;
        let mut result = BTreeMap::new();
        if let serde_json::Value::Object(map) = value {
            for (alias, target) in map {
                if let Some(target) = target.as_str() {
                    result.insert(alias, target.to_owned());
                }
            }
        }
        Ok(result)
    }

    fn remove_aliases_for(&self, name: &str, mut aliases: BTreeMap<String, String>) -> Result<()> {
        let len = aliases.len();
        aliases.retain(|_, target| target != name);
        if aliases.len() == len {
            return Ok(());
        }
        self.save_aliases(&aliases)
    }

    fn save_aliases(&self, aliases: &BTreeMap<String, String>) -> Result<()> {
        let path = self.path.join("aliases.json");
        let tmp = self.path.join(format!(
            "aliases.json.tmp.{}.{}",
            std::process::id(),
            STAGING_NUMBER.fetch_add(1, Ordering::Relaxed)
        ));
        let bytes = serde_json::to_vec_pretty(&aliases).map_err(storage)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(storage)?;
        if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
            drop(file);
            if let Err(cleanup) = fs::remove_file(&tmp) {
                eprintln!("cannot clean failed aliases temp file {tmp:?}: {cleanup}");
            }
            return Err(storage(error));
        }
        drop(file);
        if let Err(error) = fs::rename(&tmp, &path) {
            if let Err(cleanup) = fs::remove_file(&tmp) {
                eprintln!("cannot clean failed aliases temp file {tmp:?}: {cleanup}");
            }
            return Err(storage(error));
        }
        fs_commit::sync_dir(&self.path).map_err(storage)
    }

    pub fn collection(&self, database: &str, name: &str) -> Result<CollectionHandle> {
        require_database(database)?;
        if let Some(handle) = self.collections.read().get(name) {
            return Ok(Arc::clone(handle));
        }
        let blocked = self.write_gate.read();
        if *blocked {
            if let Some(handle) = self.collections.read().get(name) {
                return Ok(Arc::clone(handle));
            }
            return Err(EngineError::Storage(
                "database writes are blocked after an unconfirmed directory change".to_owned(),
            ));
        }
        self.collection_inner(name)
    }

    /// 写入从数据库入口取得许可；自动 flush 的磁盘阶段不持 collection 状态锁。
    pub fn insert(&self, database: &str, name: &str, batch: InsertBatch) -> Result<Vec<Pk>> {
        self.insert_with_pk_type(database, name, batch)
            .map(|outcome| outcome.pks)
    }

    pub fn insert_with_pk_type(
        &self,
        database: &str,
        name: &str,
        batch: InsertBatch,
    ) -> Result<InsertOutcome> {
        self.mutate(database, name, |collection| {
            let varchar = collection.schema().fields.iter().any(|field| {
                field.is_primary && field.data_type == crate::schema::DataType::VarChar
            });
            let pks = collection.insert_from_db(batch)?;
            Ok(InsertOutcome { pks, varchar })
        })
    }

    pub fn delete(&self, database: &str, name: &str, pks: &[Pk]) -> Result<usize> {
        self.mutate(database, name, |collection| collection.delete_from_db(pks))
    }

    pub fn upsert_with_pk_type(
        &self,
        database: &str,
        name: &str,
        batch: InsertBatch,
    ) -> Result<InsertOutcome> {
        self.mutate(database, name, |collection| {
            let varchar = collection.schema().fields.iter().any(|field| {
                field.is_primary && field.data_type == crate::schema::DataType::VarChar
            });
            let pks = collection.upsert_from_db(batch)?;
            Ok(InsertOutcome { pks, varchar })
        })
    }

    pub fn delete_expr(
        &self,
        database: &str,
        name: &str,
        expr: &str,
        partition: &str,
    ) -> Result<(Vec<Pk>, bool)> {
        self.mutate(database, name, |collection| {
            let varchar = collection.schema().fields.iter().any(|field| {
                field.is_primary && field.data_type == crate::schema::DataType::VarChar
            });
            let pk = collection
                .schema()
                .fields
                .iter()
                .find(|field| field.is_primary)
                .ok_or_else(|| EngineError::schema("schema has no primary key field"))?;
            let literals = parse_pk_in(expr, &pk.name).ok_or_else(|| {
                EngineError::unsupported(
                    "filter expressions other than '<primary key> in [...]'",
                    "",
                )
            })?;
            if !literals.is_empty() && !matches!(partition, "" | "_default") {
                return Err(EngineError::unsupported(
                    "partitions other than '_default'",
                    "",
                ));
            }
            let pks = collection.delete_literals_from_db(&literals)?;
            Ok((pks, varchar))
        })
    }

    fn mutate<T>(
        &self,
        database: &str,
        name: &str,
        apply: impl FnOnce(&mut Collection) -> Result<T>,
    ) -> Result<T> {
        require_database(database)?;
        let blocked = self.write_gate.read();
        if *blocked {
            return Err(EngineError::Storage(
                "database writes are blocked after an unconfirmed directory change".to_owned(),
            ));
        }
        let handle = self.collection_inner(name)?;
        let mut collection = loop {
            let collection = handle.write();
            if collection.persistent().is_some_and(|p| p.needs_retry()) {
                drop(collection);
                Self::flush_handle(&handle, false)?;
                continue;
            }
            break collection;
        };
        let outcome = match apply(&mut collection) {
            Ok(outcome) => outcome,
            Err(error) => {
                let pending = collection
                    .persistent()
                    .is_some_and(|p| p.has_pending_generation());
                drop(collection);
                if pending && let Err(flush_error) = Self::flush_handle(&handle, false) {
                    eprintln!("WAL failure recovery flush of {name:?} failed: {flush_error}");
                }
                return Err(error);
            }
        };
        let auto_flush = collection
            .persistent()
            .is_some_and(|p| p.should_auto_flush());
        if auto_flush {
            collection
                .persistent_mut()
                .ok_or_else(|| storage("persistent state missing"))?
                .freeze_active(false)
                .map_err(storage)?;
        }
        drop(collection);
        if auto_flush {
            // 写入已经落 WAL 并在内存中可见；之后的提交故障由下一次写入先重试。
            if let Err(error) = Self::flush_handle(&handle, false) {
                eprintln!("automatic flush of {name:?} failed after accepted mutation: {error}");
            }
        }
        Ok(outcome)
    }

    pub fn flush_collection(&self, database: &str, name: &str) -> Result<()> {
        require_database(database)?;
        let blocked = self.write_gate.read();
        if *blocked {
            return Err(storage(
                "database writes are blocked after an unconfirmed directory change",
            ));
        }
        let handle = self.collection_inner(name)?;
        Self::flush_handle(&handle, true)
    }

    pub fn create_index(
        &self,
        database: &str,
        name: &str,
        field: &str,
        request: IndexRequest,
    ) -> Result<()> {
        require_database(database)?;
        let blocked = self.write_gate.read();
        if *blocked {
            return Err(storage(
                "database writes are blocked after an unconfirmed directory change",
            ));
        }
        let handle = self.collection_inner(name)?;
        Self::flush_handle(&handle, true)?;
        let lock = handle
            .read()
            .persistent()
            .ok_or_else(|| storage("persistent state missing"))?
            .flush_lock();
        let _flush = lock.lock();
        handle.write().create_index_from_db(field, request)
    }

    fn flush_handle(handle: &CollectionHandle, include_active: bool) -> Result<()> {
        let lock = handle
            .read()
            .persistent()
            .ok_or_else(|| storage("persistent state missing"))?
            .flush_lock();
        let _flush = lock.lock();
        {
            let mut collection = handle.write();
            if collection.is_deleted() {
                return Err(EngineError::CollectionNotFound(
                    collection.name().to_owned(),
                ));
            }
            let persistent = collection
                .persistent_mut()
                .ok_or_else(|| storage("persistent state missing"))?;
            if persistent.poisoned() {
                return Err(storage(
                    "collection state is unusable after a storage failure",
                ));
            }
            if !include_active
                && persistent.needs_retry()
                && let Some(reason) = persistent.throttled_failure()
            {
                return Err(storage(format!(
                    "flush retry is throttled after failure: {reason}"
                )));
            }
            if persistent.retry_manifest() {
                persistent.recommit().map_err(storage)?;
            }
            if include_active {
                persistent.freeze_active(false).map_err(storage)?;
            }
        }
        loop {
            let (work, manifest, dir, schema, sync_inherited_files) = {
                let collection = handle.read();
                if collection.is_deleted() {
                    return Err(EngineError::CollectionNotFound(
                        collection.name().to_owned(),
                    ));
                }
                let persistent = collection
                    .persistent()
                    .ok_or_else(|| storage("persistent state missing"))?;
                let Some(work) = persistent.front() else {
                    if include_active {
                        persistent.cleanup_committed_wal().map_err(storage)?;
                    }
                    return Ok(());
                };
                (
                    work,
                    persistent.manifest.clone(),
                    persistent.dir.clone(),
                    collection.schema().clone(),
                    !persistent.inherited_synced(),
                )
            };
            match runtime::flush_one(&dir, &manifest, &work, &schema, sync_inherited_files) {
                Ok(outcome) => {
                    let retry_manifest = matches!(
                        &outcome.post_commit_error,
                        Some(runtime::PostCommitError::Durability(_))
                    );
                    let mut collection = handle.write();
                    let persistent = collection
                        .persistent_mut()
                        .ok_or_else(|| storage("persistent state missing"))?;
                    persistent
                        .publish(
                            &work,
                            outcome.manifest,
                            &outcome.mapping,
                            outcome.data_rows,
                            retry_manifest,
                        )
                        .map_err(storage)?;
                    if let Some(runtime::PostCommitError::Durability(reason)) =
                        &outcome.post_commit_error
                    {
                        persistent.record_failure(reason.clone());
                    }
                    if let Some(error) = outcome.post_commit_error {
                        return Err(storage(match error {
                            runtime::PostCommitError::Durability(reason) => format!(
                                "manifest was replaced, but directory durability is unconfirmed: {reason}"
                            ),
                            runtime::PostCommitError::WalCleanup(reason) => {
                                format!("manifest committed, but WAL cleanup failed: {reason}")
                            }
                        }));
                    }
                }
                Err(error) => {
                    handle
                        .write()
                        .persistent_mut()
                        .ok_or_else(|| storage("persistent state missing"))?
                        .failed_before_replace(error.to_string());
                    return Err(storage(error));
                }
            }
        }
    }

    fn collection_path(&self, name: &str) -> PathBuf {
        self.path.join("collections").join(name)
    }

    fn collection_inner(&self, name: &str) -> Result<CollectionHandle> {
        validate_filesystem_name(name, "collection")?;
        if let Some(handle) = self.collections.read().get(name) {
            return Ok(Arc::clone(handle));
        }
        let _namespace = self.namespace.lock();
        if let Some(handle) = self.collections.read().get(name) {
            return Ok(Arc::clone(handle));
        }
        if !self.exact_collection_name(name)? {
            return Err(EngineError::CollectionNotFound(name.to_owned()));
        }
        let path = self.collection_path(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(EngineError::CollectionNotFound(name.to_owned()));
            }
            Err(error) => return Err(storage(error)),
        };
        if !metadata.file_type().is_dir() || !path.join("schema.json").is_file() {
            return Err(EngineError::CollectionNotFound(name.to_owned()));
        }
        // 不区分大小写的文件系统上只接受目录项的实际名字。
        let recovered = recovery::load_committed(&path).map_err(recovery_error)?;
        for warning in &recovered.wal_warnings {
            eprintln!("collection {name:?} recovered only a validated WAL prefix: {warning}");
        }
        let collection = Collection::from_recovered(name, recovered, path)?;
        let handle = Arc::new(RwLock::new(collection));
        self.collections
            .write()
            .insert(name.to_owned(), Arc::clone(&handle));
        Ok(handle)
    }

    /// 全部 collection 的名字与 load 状态。先在表读锁内取出句柄，放开表锁后再读各自的状态：
    /// 等待 collection 锁时若仍持有表锁，排队的写者会让所有 RPC 都停在表锁上。
    /// 取出之后被并发 drop 的 collection 仍会列出，不会报错。
    pub fn list_collections(&self, database: &str) -> Result<Vec<(String, LoadState)>> {
        require_database(database)?;
        let root = self.path.join("collections");
        match fs::symlink_metadata(&root) {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => return Err(storage("collections path is not a directory")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(storage(error)),
        }
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(storage(error)),
        };
        let mut result = Vec::new();
        for entry in entries {
            let entry = entry.map_err(storage)?;
            if !entry.file_type().map_err(storage)?.is_dir()
                || !entry.path().join("schema.json").is_file()
            {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let cached = self.collections.read().get(&name).cloned();
            let state = if let Some(handle) = cached {
                let collection = handle.read();
                if collection.is_deleted() || collection.persistent().is_some_and(|p| p.poisoned())
                {
                    LoadState::Released
                } else {
                    collection.load_state()
                }
            } else {
                unopened_load_state(&entry.path())
            };
            result.push((name, state));
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }
}

fn storage(error: impl std::fmt::Display) -> EngineError {
    EngineError::Storage(error.to_string())
}

fn unopened_load_state(dir: &std::path::Path) -> LoadState {
    use crate::storage::manifest::Manifest;

    let schema = dir.join("schema.json");
    if !fs::symlink_metadata(&schema).is_ok_and(|meta| meta.file_type().is_file()) {
        return LoadState::Released;
    }
    let schema_file = match SchemaFile::read(&schema) {
        Ok(file) => file,
        Err(_) => return LoadState::Released,
    };
    if schema_file.collection_name != dir.file_name().and_then(|name| name.to_str()).unwrap_or("")
        || schema_file.to_core().is_err()
    {
        return LoadState::Released;
    }
    let manifest_path = dir.join("manifest.json");
    match fs::symlink_metadata(&manifest_path) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => return LoadState::Released,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return if fs::symlink_metadata(dir.join("manifest.json.prev")).is_ok() {
                LoadState::Released
            } else {
                LoadState::Loaded
            };
        }
        Err(_) => return LoadState::Released,
    }
    let manifest = match Manifest::read(&manifest_path) {
        Ok(manifest) => manifest,
        Err(_) => return LoadState::Released,
    };
    if manifest.partitions.len() != 1 || !manifest.partitions.contains_key("_default") {
        return LoadState::Released;
    }
    if manifest.index_specs.is_empty() {
        LoadState::Loaded
    } else {
        LoadState::Released
    }
}

fn recovery_error(error: recovery::RecoveryError) -> EngineError {
    use crate::storage::manifest::ManifestError;
    use crate::storage::schema_file::SchemaFileError;

    match &error {
        recovery::RecoveryError::Schema(SchemaFileError::Unsupported(reason)) => {
            EngineError::unsupported("collection content", reason)
        }
        recovery::RecoveryError::Schema(SchemaFileError::Invalid(reason))
            if reason.starts_with("unknown field")
                || reason.starts_with("unknown dtype")
                || reason.starts_with("unknown function_type")
                || reason.starts_with("unsupported schema_format_version") =>
        {
            EngineError::unsupported("collection content", reason)
        }
        recovery::RecoveryError::Manifest(ManifestError::Invalid(reason))
            if reason.starts_with("unknown field")
                || reason.starts_with("unsupported manifest_format_version") =>
        {
            EngineError::unsupported("collection content", reason)
        }
        recovery::RecoveryError::Unsupported(reason) if reason == "non-default partitions" => {
            EngineError::unsupported("collection content", reason)
        }
        _ => storage(error),
    }
}

fn cleanup_failed_staging(path: &std::path::Path) {
    if let Err(error) = fs::remove_dir_all(path) {
        eprintln!("cannot clean failed create staging {path:?}: {error}");
    }
}

/// 3.x 的 `_require_database`：空名即 `default`；M1 没有其他数据库。
fn require_database(name: &str) -> Result<()> {
    if name.is_empty() || name == DEFAULT_DATABASE {
        return Ok(());
    }
    validate_filesystem_name(name, "database")?;
    Err(EngineError::DatabaseNotFound(name.to_owned()))
}

/// 3.x 的 `_validate_filesystem_name`：以 ValueError 抛出，因此是 InvalidArgument。
/// 名字按原样区分大小写（0004「决策」）。
fn validate_filesystem_name(name: &str, kind: &str) -> Result<()> {
    if name.is_empty() {
        return Err(EngineError::invalid(format!(
            "{kind} name must not be empty"
        )));
    }
    if name.contains(['/', '\\', '\0']) || name == "." || name == ".." {
        return Err(EngineError::invalid(format!(
            "invalid {kind} name: {}",
            py_repr(name)
        )));
    }
    if name.len() > 255 {
        return Err(EngineError::invalid(format!(
            "{kind} name too long ({} bytes, max 255)",
            name.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::InputValues;
    use crate::schema::{DataType, FieldSchema};

    fn basic_schema() -> CollectionSchema {
        let mut pk = FieldSchema::new("id", DataType::Int64);
        pk.is_primary = true;
        let mut vector = FieldSchema::new("v", DataType::FloatVector);
        vector.dim = Some(2);
        CollectionSchema {
            fields: vec![pk, vector],
            enable_dynamic_field: false,
            properties: Vec::new(),
        }
    }

    #[test]
    fn blocked_close_only_syncs_wal_and_leaves_manifest_untouched() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("blocked.db");
        let compute = || Compute::new(std::num::NonZeroUsize::MIN).unwrap();
        let db = Db::new(DataDir::open(&path).unwrap(), compute());
        db.create_collection("", "c", basic_schema()).unwrap();
        db.insert(
            "",
            "c",
            InsertBatch {
                num_rows: 1,
                columns: vec![
                    ("id".to_owned(), InputValues::Int(vec![1])),
                    (
                        "v".to_owned(),
                        InputValues::FloatVector {
                            dim: 2,
                            values: vec![1.0, 0.0],
                        },
                    ),
                ],
            },
        )
        .unwrap();
        *db.write_gate.write() = true;
        db.close();
        assert!(!path.join("collections/c/manifest.json").exists());
        assert!(
            path.join("collections/c/wal/wal_data_000001.arrow")
                .exists()
        );
        let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
        assert_eq!(reopened.collection("", "c").unwrap().read().count(), 1);
        reopened.close();
    }

    #[test]
    fn stale_handle_cannot_flush_after_drop_moved_its_directory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("stale.db");
        let compute = Compute::new(std::num::NonZeroUsize::MIN).unwrap();
        let db = Db::new(DataDir::open(&path).unwrap(), compute);
        db.create_collection("", "c", basic_schema()).unwrap();
        let stale = db.collection("", "c").unwrap();
        db.drop_collection("", "c").unwrap();
        assert!(matches!(
            Db::flush_handle(&stale, true),
            Err(EngineError::CollectionNotFound(_))
        ));
        assert!(!path.join("collections/c").exists());
        db.close();
    }
}
