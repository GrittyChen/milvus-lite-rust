//! 数据库：多 collection 的生命周期与引擎的关闭。M1 只有 `default` 一个数据库（0004「范围」）。

use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::{Condvar, Mutex, RwLock};

use crate::collection::{Collection, LoadState};
use crate::compute::Compute;
use crate::data::{InsertBatch, Pk};
use crate::data_dir::DataDir;
use crate::error::{EngineError, Result, py_repr};
use crate::filter::ParsedFilter;
use crate::index::IndexRequest;
use crate::schema::CollectionSchema;
use crate::storage::{fs_commit, recovery, runtime, schema_file::SchemaFile};

/// 每个 collection 一把读写锁：写入互斥、读取并发，仍是「单 collection 单写者」（不变量 7）。
pub type CollectionHandle = Arc<RwLock<Collection>>;

#[cfg(test)]
thread_local! {
    static AFTER_DELETE_QUERY: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[derive(Debug, Clone)]
enum OpenFailure {
    Storage(String),
    Unsupported {
        feature: &'static str,
        reason: String,
    },
    SchemaValidation(String),
    InvalidArgument(String),
}

impl OpenFailure {
    fn from_error(error: &EngineError) -> Option<Self> {
        match error {
            EngineError::Storage(reason) => Some(Self::Storage(reason.clone())),
            EngineError::Unsupported { feature, reason } => Some(Self::Unsupported {
                feature,
                reason: reason.clone(),
            }),
            EngineError::SchemaValidation(reason) => Some(Self::SchemaValidation(reason.clone())),
            EngineError::InvalidArgument(reason) => Some(Self::InvalidArgument(reason.clone())),
            _ => None,
        }
    }

    fn error(&self) -> EngineError {
        match self {
            Self::Storage(reason) => EngineError::Storage(reason.clone()),
            Self::Unsupported { feature, reason } => EngineError::Unsupported {
                feature,
                reason: reason.clone(),
            },
            Self::SchemaValidation(reason) => EngineError::SchemaValidation(reason.clone()),
            Self::InvalidArgument(reason) => EngineError::InvalidArgument(reason.clone()),
        }
    }
}

// 等待者也持强引用；只有最后一个使用者退出后才移除名字，不能在 drop 时换一把锁。
struct NameLease<'a> {
    registry: &'a Mutex<BTreeMap<String, Weak<Mutex<()>>>>,
    name: String,
    lock: Option<Arc<Mutex<()>>>,
}

impl NameLease<'_> {
    #[allow(clippy::expect_used)] // 名字锁守卫的借用先结束，析构才能独占租约并取走强引用。
    fn mutex(&self) -> &Arc<Mutex<()>> {
        self.lock.as_ref().expect("name lease is live")
    }
}

#[cfg(test)]
struct NameLeaseTestSync {
    sent: std::sync::mpsc::Sender<()>,
    ready: std::sync::mpsc::Receiver<()>,
    timeout: std::time::Duration,
}

#[cfg(test)]
impl NameLeaseTestSync {
    fn wait(&self) -> std::result::Result<(), std::sync::mpsc::RecvTimeoutError> {
        self.sent
            .send(())
            .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected)?;
        self.ready.recv_timeout(self.timeout)
    }
}

#[cfg(test)]
thread_local! {
    static NAME_LEASE_DROP_SYNC: std::cell::RefCell<Option<(NameLeaseTestSync, Option<std::sync::mpsc::RecvTimeoutError>)>> = const { std::cell::RefCell::new(None) };
}

impl Drop for NameLease<'_> {
    fn drop(&mut self) {
        let mut registry = self.registry.lock();
        // 减计数必须与回收判定同处表锁内，不能等 Drop 返回后才自动析构字段。
        drop(self.lock.take());
        if registry
            .get(&self.name)
            .is_some_and(|lock| lock.strong_count() == 0)
        {
            registry.remove(&self.name);
        }
        drop(registry);
        #[cfg(test)]
        if !std::thread::panicking() {
            // 同步故障留给析构之外的断言，不能在栈展开时等待同伴或引发二次 panic。
            NAME_LEASE_DROP_SYNC.with(|slot| {
                if let Some((sync, failure)) = slot.borrow_mut().as_mut()
                    && failure.is_none()
                    && let Err(error) = sync.wait()
                {
                    *failure = Some(error);
                }
            });
        }
    }
}

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

#[cfg(feature = "fault-injection")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryOperation {
    Create,
    Drop,
}
#[cfg(feature = "fault-injection")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryPoint {
    BeforeRename,
    AfterRename,
    CollectionsSync,
    StagingSync,
    Aliases,
}
#[cfg(feature = "fault-injection")]
#[derive(Clone, Copy, Debug)]
pub enum DirectoryFault {
    Error,
    Panic,
    Exit,
}

#[cfg(feature = "fault-injection")]
thread_local! {
    static DIRECTORY_FAULT: std::cell::Cell<Option<(DirectoryOperation, DirectoryPoint, DirectoryFault)>> = const { std::cell::Cell::new(None) };
    static OPEN_CLEANUP_PAUSE: std::cell::RefCell<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(feature = "fault-injection")]
pub fn inject_directory_fault_once(
    op: DirectoryOperation,
    point: DirectoryPoint,
    fault: DirectoryFault,
) {
    DIRECTORY_FAULT.with(|slot| slot.set(Some((op, point, fault))));
}

#[cfg(feature = "fault-injection")]
pub fn pause_open_cleanup_once(
    entered: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
) {
    OPEN_CLEANUP_PAUSE.with(|slot| *slot.borrow_mut() = Some((entered, resume)));
}

#[cfg(feature = "fault-injection")]
fn directory_fault(op: DirectoryOperation, point: DirectoryPoint) -> std::io::Result<()> {
    let fault = DIRECTORY_FAULT.with(|slot| match slot.get() {
        Some((wanted_op, wanted_point, fault)) if op == wanted_op && point == wanted_point => {
            slot.set(None);
            Some(fault)
        }
        _ => None,
    });
    match fault {
        Some(DirectoryFault::Error) => Err(std::io::Error::other(
            "injected directory lifecycle failure",
        )),
        Some(DirectoryFault::Panic) => panic!("injected directory lifecycle panic"),
        Some(DirectoryFault::Exit) => std::process::exit(88),
        None => Ok(()),
    }
}

// 改名后的同步 panic 与 I/O 错误必须走同一封锁协议，不能越过内存收尾。
fn sync_directory_change(
    root: &std::path::Path,
    staging: &std::path::Path,
    creating: bool,
) -> std::io::Result<()> {
    std::panic::catch_unwind(|| {
        #[cfg(feature = "fault-injection")]
        {
            let op = if creating {
                DirectoryOperation::Create
            } else {
                DirectoryOperation::Drop
            };
            directory_fault(op, DirectoryPoint::AfterRename)?;
            directory_fault(op, DirectoryPoint::CollectionsSync)?;
        }
        #[cfg(not(feature = "fault-injection"))]
        let _creating = creating;
        fs_commit::sync_dir(root)?;
        #[cfg(feature = "fault-injection")]
        directory_fault(
            if creating {
                DirectoryOperation::Create
            } else {
                DirectoryOperation::Drop
            },
            DirectoryPoint::StagingSync,
        )?;
        fs_commit::sync_dir(staging)
    })
    .unwrap_or_else(|_| Err(std::io::Error::other("directory sync panicked")))
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
    name_locks: Mutex<BTreeMap<String, Weak<Mutex<()>>>>,
    open_failures: RwLock<BTreeMap<String, OpenFailure>>,
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
            name_locks: Mutex::new(BTreeMap::new()),
            open_failures: RwLock::new(BTreeMap::new()),
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
        let lease = self.name_lease(name);
        let _name = lease.mutex().lock();
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
            self.save_aliases(&aliases, true)?;
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
        #[cfg(feature = "fault-injection")]
        if let Err(error) =
            directory_fault(DirectoryOperation::Create, DirectoryPoint::BeforeRename)
        {
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
        *blocked = true;
        self.open_failures.write().remove(name);
        collection.attach_persistence(
            runtime::PersistentCollection::new(target.clone()).map_err(EngineError::Storage)?,
        );
        self.collections
            .write()
            .insert(name.to_owned(), Arc::new(RwLock::new(collection)));
        if let Err(error) = sync_directory_change(&root, &staging_root, true) {
            return Err(EngineError::Storage(format!(
                "collection was created, but directory durability is unconfirmed: {error}"
            )));
        }
        *blocked = false;
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
        let lease = self.name_lease(name);
        let _name = lease.mutex().lock();
        let target = self.collection_path(name);
        let aliases = self.read_aliases()?;
        let exists = match fs::symlink_metadata(&target) {
            Ok(meta) => meta.file_type().is_dir() && target.join("schema.json").is_file(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(storage(error)),
        };
        if !exists || !self.exact_collection_name(name)? {
            let handle = self.collections.read().get(name).cloned();
            let flush_lock = handle
                .as_ref()
                .and_then(|h| h.read().persistent().map(|p| p.flush_lock()));
            let _flush = flush_lock.as_ref().map(|lock| lock.lock());
            if let Some(handle) = &handle {
                let mut collection = handle.write();
                if let Some(persistent) = collection.persistent_mut() {
                    // 只有整个原目录不在时才退役原 File；schema 缺失等残留仍用通常同步协议。
                    match fs::symlink_metadata(&target) {
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            persistent.sync_displaced_wal().map_err(storage)?
                        }
                        Err(error) => return Err(storage(error)),
                        Ok(_) => persistent.sync_retained_wal().map_err(storage)?,
                    }
                }
                collection.mark_deleted(true);
                // collection 表是叶子锁：所有读者先释放表锁再取句柄锁；此处保持 deleted 与撤表同一临界区。
                self.collections.write().remove(name);
            }
            self.open_failures.write().remove(name);
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
                collection.mark_deleted(false);
                return Err(storage(error));
            }
        }
        let staging = staging_root.join(format!(
            "drop-{}-{}",
            std::process::id(),
            STAGING_NUMBER.fetch_add(1, Ordering::Relaxed)
        ));
        let rename = || {
            #[cfg(feature = "fault-injection")]
            directory_fault(DirectoryOperation::Drop, DirectoryPoint::BeforeRename)?;
            fs::rename(&target, &staging)
        };
        if let Err(error) = rename() {
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
        self.open_failures.write().remove(name);
        *blocked = true;
        if let Err(error) = sync_directory_change(&root, &staging_root, false) {
            return Err(storage(format!(
                "collection was removed, but directory durability is unconfirmed: {error}"
            )));
        }
        *blocked = false;
        self.remove_aliases_for(name, aliases).map_err(|error| {
            storage(format!(
                "collection was removed, but aliases cleanup is pending: {error}"
            ))
        })?;
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
        self.save_aliases(&aliases, false)
    }

    fn save_aliases(&self, aliases: &BTreeMap<String, String>, creating: bool) -> Result<()> {
        #[cfg(feature = "fault-injection")]
        directory_fault(
            if creating {
                DirectoryOperation::Create
            } else {
                DirectoryOperation::Drop
            },
            DirectoryPoint::Aliases,
        )
        .map_err(storage)?;
        #[cfg(not(feature = "fault-injection"))]
        let _creating = creating;
        let path = self.path.join("aliases.json");
        let tmp = self.path.join(format!(
            "aliases.json.tmp.{}.{}",
            std::process::id(),
            STAGING_NUMBER.fetch_add(1, Ordering::Relaxed)
        ));
        // 上一次 drop 可能已经改名成功却未能写别名；任意后续别名提交都须清掉悬空目标。
        let mut live_aliases = BTreeMap::new();
        for (alias, target) in aliases {
            if validate_filesystem_name(target, "collection").is_err() {
                continue;
            }
            match fs::metadata(self.collection_path(target)) {
                Ok(_) => {
                    live_aliases.insert(alias, target);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(storage(error)),
            }
        }
        let bytes = serde_json::to_vec_pretty(&live_aliases).map_err(storage)?;
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
        let cached = self.collections.read().get(name).cloned();
        if let Some(handle) = cached {
            return checked_read_handle(handle);
        }
        let blocked = self.write_gate.read();
        if *blocked {
            let cached = self.collections.read().get(name).cloned();
            if let Some(handle) = cached {
                return checked_read_handle(handle);
            }
            return Err(EngineError::Storage(
                "database writes are blocked after an unconfirmed directory change".to_owned(),
            ));
        }
        checked_read_handle(self.collection_inner(name)?)
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
        require_database(database)?;
        let blocked = self.write_gate.read();
        if *blocked {
            return Err(storage(
                "database writes are blocked after an unconfirmed directory change",
            ));
        }
        let handle = self.collection_inner(name)?;
        let snapshot = loop {
            let collection = handle.read();
            if collection.is_deleted() {
                return Err(EngineError::CollectionNotFound(name.to_owned()));
            }
            if collection.persistent().is_some_and(|p| p.poisoned()) {
                return Err(storage(
                    "collection state is unusable after a storage failure",
                ));
            }
            if collection.persistent().is_some_and(|p| p.needs_retry()) {
                drop(collection);
                Self::flush_handle(&handle, false)?;
                continue;
            }
            break collection.read_snapshot();
        };
        let pk = snapshot
            .schema()
            .fields
            .iter()
            .find(|field| field.is_primary)
            .ok_or_else(|| EngineError::schema("schema has no primary key field"))?;
        let varchar = pk.data_type == crate::schema::DataType::VarChar;
        let parsed = ParsedFilter::parse(expr)?;
        let literals = parsed.primary_keys(&pk.name);
        if let Some(literals) = literals {
            return self.mutate_handle(&handle, name, |collection| {
                if !literals.is_empty() && !matches!(partition, "" | "_default") {
                    return Err(EngineError::unsupported(
                        "partitions other than '_default'",
                        "",
                    ));
                }
                Ok((collection.delete_literals_from_db(&literals)?, varchar))
            });
        }
        let pks = snapshot.query_for_delete(&parsed)?;
        #[cfg(test)]
        AFTER_DELETE_QUERY.with(|hook| {
            if let Some(pause) = hook.borrow_mut().take() {
                pause();
            }
        });
        if !pks.is_empty() && !matches!(partition, "" | "_default") {
            return Err(EngineError::unsupported(
                "partitions other than '_default'",
                "",
            ));
        }
        // 与 3.x 的 query → delete 相同：之后写入的同主键新版本也会被删除；不升级共享许可。
        self.mutate_handle(&handle, name, |collection| {
            collection.delete_from_db(&pks)?;
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
        self.mutate_handle(&handle, name, apply)
    }

    fn mutate_handle<T>(
        &self,
        handle: &CollectionHandle,
        name: &str,
        apply: impl FnOnce(&mut Collection) -> Result<T>,
    ) -> Result<T> {
        let mut collection = loop {
            let collection = handle.write();
            if collection.persistent().is_some_and(|p| p.poisoned()) {
                return Err(storage(
                    "collection state is unusable after a storage failure",
                ));
            }
            if collection.persistent().is_some_and(|p| p.needs_retry()) {
                drop(collection);
                Self::flush_handle(handle, false)?;
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
                if pending && let Err(flush_error) = Self::flush_handle(handle, false) {
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
            if let Err(error) = Self::flush_handle(handle, false) {
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
                    collection.publish_generation(
                        &work,
                        outcome.manifest,
                        &outcome.mapping,
                        outcome.data,
                        outcome.norms,
                        retry_manifest,
                    )?;
                    let persistent = collection
                        .persistent_mut()
                        .ok_or_else(|| storage("persistent state missing"))?;
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

    fn name_lease(&self, name: &str) -> NameLease<'_> {
        let mut registry = self.name_locks.lock();
        let lock = match registry.get(name).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(Mutex::new(()));
                registry.insert(name.to_owned(), Arc::downgrade(&lock));
                lock
            }
        };
        NameLease {
            registry: &self.name_locks,
            name: name.to_owned(),
            lock: Some(lock),
        }
    }

    fn remember_open_failure(&self, name: &str, error: EngineError) -> EngineError {
        if let Some(failure) = OpenFailure::from_error(&error) {
            self.open_failures.write().insert(name.to_owned(), failure);
        }
        error
    }

    fn opening_error(&self, name: &str, error: recovery::RecoveryError) -> EngineError {
        let cache = cacheable_recovery_error(&error);
        let error = recovery_error(error);
        if cache {
            self.remember_open_failure(name, error)
        } else {
            error
        }
    }

    fn collection_inner(&self, name: &str) -> Result<CollectionHandle> {
        validate_filesystem_name(name, "collection")?;
        if let Some(handle) = self.collections.read().get(name) {
            return Ok(Arc::clone(handle));
        }
        let lease = self.name_lease(name);
        let _name = lease.mutex().lock();
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
        if let Some(failure) = self.open_failures.read().get(name) {
            return Err(failure.error());
        }
        // 不区分大小写的文件系统上只接受目录项的实际名字。
        let recovered =
            recovery::load_committed(&path).map_err(|error| self.opening_error(name, error))?;
        for warning in &recovered.wal_warnings {
            eprintln!("collection {name:?} recovered only a validated WAL prefix: {warning}");
        }
        let manifest = recovered.manifest.clone();
        let pinned_files = recovered.pinned_files.clone();
        let collection = Collection::from_recovered(name, recovered, path.clone())
            .map_err(|error| self.remember_open_failure(name, error))?;
        #[cfg(feature = "fault-injection")]
        OPEN_CLEANUP_PAUSE.with(|slot| -> Result<()> {
            if let Some((entered, resume)) = slot.borrow_mut().take() {
                entered.send(()).map_err(storage)?;
                resume.recv().map_err(storage)?;
            }
            Ok(())
        })?;
        for warning in recovery::cleanup_unreferenced(&path, &manifest, &pinned_files)
            .map_err(|error| self.opening_error(name, error))?
        {
            eprintln!("collection {name:?} orphan cleanup deferred: {warning}");
        }
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
                let failed = self.open_failures.read().contains_key(&name);
                if failed {
                    LoadState::Released
                } else {
                    unopened_load_state(&entry.path())
                }
            };
            result.push((name, state));
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }
}

fn checked_read_handle(handle: CollectionHandle) -> Result<CollectionHandle> {
    let poisoned = handle.read().persistent().is_some_and(|p| p.poisoned());
    if poisoned {
        Err(storage(
            "collection state is unusable after a storage failure",
        ))
    } else {
        Ok(handle)
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

fn cacheable_recovery_error(error: &recovery::RecoveryError) -> bool {
    use std::error::Error;
    // 扫描失败及 WAL 真 I/O 故障可随环境恢复；引用文件缺失属于确定的目录损坏。
    if matches!(
        error,
        recovery::RecoveryError::Cleanup { .. } | recovery::RecoveryError::Wal(_)
    ) {
        return false;
    }
    let mut cause: &(dyn Error + 'static) = error;
    loop {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return io.kind() == std::io::ErrorKind::NotFound;
        }
        match cause.source() {
            Some(source) => cause = source,
            None => return true,
        }
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

    fn test_db(path: &std::path::Path) -> Db {
        Db::new(
            DataDir::open(path).unwrap(),
            Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
        )
    }

    #[test]
    fn filter_delete_uses_one_handle_and_deletes_the_snapshot_keys_after_concurrent_overwrite() {
        use std::sync::mpsc;
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        let db = Arc::new(test_db(root.path()));
        db.create_collection("", "c", basic_schema()).unwrap();
        db.create_index(
            "",
            "c",
            "v",
            IndexRequest {
                index_type: "FLAT".into(),
                metric_type: Some("L2".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let batch = |ids: Vec<i64>| InsertBatch {
            num_rows: ids.len(),
            columns: vec![
                ("id".into(), InputValues::Int(ids.clone())),
                (
                    "v".into(),
                    InputValues::FloatVector {
                        dim: 2,
                        values: [1.0, 0.0].repeat(ids.len()),
                    },
                ),
            ],
        };
        db.insert("", "c", batch(vec![1])).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let reader = Arc::clone(&db);
        let worker = std::thread::spawn(move || {
            AFTER_DELETE_QUERY.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    ready_tx.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                }))
            });
            reader.delete_expr("", "c", "id>=0", "").unwrap()
        });
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        db.upsert_with_pk_type("", "c", batch(vec![1, 2])).unwrap();
        db.collection("", "c").unwrap().write().release();
        resume_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap().0, [Pk::Int64(1)]);
        let handle = db.collection("", "c").unwrap();
        assert_eq!(handle.read().load_state(), LoadState::Released);
        handle.write().load();
        assert_eq!(
            handle
                .read()
                .get(&[Pk::Int64(1), Pk::Int64(2)], &[])
                .unwrap()
                .rows
                .pks,
            [Pk::Int64(2)]
        );
        db.close();
    }

    fn seed_lazy_collections(path: &std::path::Path) {
        let db = test_db(path);
        for name in ["c", "other"] {
            db.create_collection("", name, basic_schema()).unwrap();
            db.insert(
                "",
                name,
                InsertBatch {
                    num_rows: 1,
                    columns: vec![
                        ("id".into(), InputValues::Int(vec![1])),
                        (
                            "v".into(),
                            InputValues::FloatVector {
                                dim: 2,
                                values: vec![1.0, 0.0],
                            },
                        ),
                    ],
                },
            )
            .unwrap();
        }
        db.close();
    }

    #[test]
    fn name_lock_identity_survives_waiters_and_reclaims_unused_names() {
        let root = tempfile::tempdir().unwrap();
        let db = test_db(root.path());
        let first = db.name_lease("c");
        let second = db.name_lease("c");
        assert!(Arc::ptr_eq(first.mutex(), second.mutex()));
        drop(first);
        let third = db.name_lease("c");
        assert!(Arc::ptr_eq(second.mutex(), third.mutex()));
        let other = db.name_lease("other");
        assert!(!Arc::ptr_eq(other.mutex(), third.mutex()));
        drop((second, third, other));
        assert!(db.name_locks.lock().is_empty());
        for n in 0..100 {
            assert!(matches!(
                db.collection("", &format!("missing-{n}")),
                Err(EngineError::CollectionNotFound(_))
            ));
        }
        assert!(db.name_locks.lock().is_empty());
        assert!(db.open_failures.read().is_empty());
        db.close();
    }

    #[test]
    fn concurrent_name_lease_release_reclaims_the_last_weak_entry() {
        let root = tempfile::tempdir().unwrap();
        let db = Arc::new(test_db(root.path()));
        let (acquired_a, acquired_b) = name_lease_sync_pair(std::time::Duration::from_secs(5));
        let (releasing_a, releasing_b) = name_lease_sync_pair(std::time::Duration::from_secs(5));
        let mut threads = Vec::new();
        for (acquired, releasing) in [(acquired_a, releasing_a), (acquired_b, releasing_b)] {
            let db = Arc::clone(&db);
            threads.push(std::thread::spawn(move || {
                NAME_LEASE_DROP_SYNC.with(|slot| *slot.borrow_mut() = Some((releasing, None)));
                for _ in 0..20_000 {
                    let lease = db.name_lease("same-name");
                    {
                        let _name = lease.mutex().lock();
                    }
                    acquired.wait().unwrap();
                    // 钉住两份引用，再强制两个析构体均返回到字段自动析构前的窗口。
                    drop(lease);
                    let failure = NAME_LEASE_DROP_SYNC
                        .with(|slot| slot.borrow_mut().as_mut().unwrap().1.take());
                    assert!(
                        failure.is_none(),
                        "name lease drop synchronization failed: {failure:?}"
                    );
                }
                NAME_LEASE_DROP_SYNC.with(|slot| *slot.borrow_mut() = None);
            }));
        }
        // 先收齐两个结果，避免第一个 panic 使另一线程留在后台继续持有数据库。
        let results: Vec<_> = threads.into_iter().map(|thread| thread.join()).collect();
        for result in results {
            result.unwrap();
        }
        assert!(db.name_locks.lock().is_empty());
        db.close();
    }

    fn name_lease_sync_pair(
        timeout: std::time::Duration,
    ) -> (NameLeaseTestSync, NameLeaseTestSync) {
        let (sent_a, ready_b) = std::sync::mpsc::channel();
        let (sent_b, ready_a) = std::sync::mpsc::channel();
        (
            NameLeaseTestSync {
                sent: sent_a,
                ready: ready_a,
                timeout,
            },
            NameLeaseTestSync {
                sent: sent_b,
                ready: ready_b,
                timeout,
            },
        )
    }

    #[test]
    fn name_lease_drop_sync_reports_timeout_and_disconnect_outside_destructor() {
        use std::sync::mpsc::RecvTimeoutError;
        for disconnect in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let db = test_db(root.path());
            let (sync, peer) = name_lease_sync_pair(std::time::Duration::from_millis(20));
            let peer = if disconnect {
                drop(peer);
                None
            } else {
                Some(peer)
            };
            NAME_LEASE_DROP_SYNC.with(|slot| *slot.borrow_mut() = Some((sync, None)));
            drop(db.name_lease("c"));
            let failure = NAME_LEASE_DROP_SYNC.with(|slot| slot.borrow_mut().take().unwrap().1);
            assert_eq!(
                failure,
                Some(if disconnect {
                    RecvTimeoutError::Disconnected
                } else {
                    RecvTimeoutError::Timeout
                })
            );
            assert!(db.name_locks.lock().is_empty());
            drop(peer);
            db.close();
        }
    }

    #[test]
    fn name_lease_unwind_skips_peer_wait_and_preserves_original_panic() {
        let root = tempfile::tempdir().unwrap();
        let db = test_db(root.path());
        let (sync, peer) = name_lease_sync_pair(std::time::Duration::from_millis(20));
        NAME_LEASE_DROP_SYNC.with(|slot| *slot.borrow_mut() = Some((sync, None)));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _lease = db.name_lease("c");
            panic!("original name lease test panic");
        }));
        assert_eq!(
            result.unwrap_err().downcast_ref::<&str>(),
            Some(&"original name lease test panic")
        );
        assert!(matches!(
            peer.ready.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        let failure = NAME_LEASE_DROP_SYNC.with(|slot| slot.borrow_mut().take().unwrap().1);
        assert!(failure.is_none());
        assert!(db.name_locks.lock().is_empty());
        db.close();
    }

    #[test]
    fn deterministic_open_failures_keep_original_errors_until_drop_or_service_rebuild() {
        for kind in ["schema", "manifest", "snapshot", "missing-data", "parquet"] {
            let root = tempfile::tempdir().unwrap();
            seed_lazy_collections(root.path());
            let dir = root.path().join("collections/c");
            let manifest: serde_json::Value =
                serde_json::from_slice(&fs::read(dir.join("manifest.json")).unwrap()).unwrap();
            let data = dir.join("partitions/_default").join(
                manifest["partitions"]["_default"]["data_files"][0]
                    .as_str()
                    .unwrap(),
            );
            let damaged = match kind {
                "schema" => dir.join("schema.json"),
                "manifest" => dir.join("manifest.json"),
                "snapshot" => {
                    let path = dir.join("snapshots/metadata/bad.json");
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(&path, b"{").unwrap();
                    path
                }
                _ => data,
            };
            let original = fs::read(&damaged).unwrap();
            if kind == "missing-data" {
                fs::remove_file(&damaged).unwrap();
            } else if kind == "schema" {
                let mut json: serde_json::Value = serde_json::from_slice(&original).unwrap();
                json["future_key"] = true.into();
                fs::write(&damaged, serde_json::to_vec(&json).unwrap()).unwrap();
            } else {
                fs::write(&damaged, b"{").unwrap();
            }
            let orphan = dir.join("partitions/_default/data/orphan");
            fs::write(&orphan, b"keep").unwrap();
            let db = test_db(root.path());
            let first = db.collection("", "c").unwrap_err();
            assert!(db.open_failures.read().contains_key("c"), "{kind}: {first}");
            if kind == "snapshot" {
                fs::remove_file(&damaged).unwrap();
            } else {
                fs::write(&damaged, &original).unwrap();
            }
            let repeated = db.collection("", "c").unwrap_err();
            assert_eq!(
                std::mem::discriminant(&first),
                std::mem::discriminant(&repeated)
            );
            assert_eq!(first.to_string(), repeated.to_string());
            assert!(orphan.exists());
            assert_eq!(
                db.list_collections("").unwrap(),
                vec![
                    ("c".into(), LoadState::Released),
                    ("other".into(), LoadState::Loaded)
                ]
            );
            assert_eq!(db.collection("", "other").unwrap().read().count(), 1);
            if kind == "manifest" {
                db.close();
                let db = test_db(root.path());
                assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
                assert!(!orphan.exists());
                db.close();
            } else {
                db.drop_collection("", "c").unwrap();
                assert!(!db.open_failures.read().contains_key("c"));
                db.create_collection("", "c", basic_schema()).unwrap();
                assert_eq!(db.collection("", "c").unwrap().read().count(), 0);
                db.close();
            }
        }
    }

    #[test]
    fn recovery_failure_cache_distinguishes_io_from_directory_damage() {
        use crate::storage::{
            manifest::ManifestError, parquet_file::ParquetFileError, schema_file::SchemaFileError,
            snapshot::SnapshotError, wal::WalError,
        };
        use recovery::RecoveryError;
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::Other,
            std::io::ErrorKind::NotFound,
        ] {
            let io = || std::io::Error::from(kind);
            let path = || PathBuf::from("referenced");
            let failures = [
                RecoveryError::Io {
                    path: path(),
                    source: io(),
                },
                RecoveryError::Schema(SchemaFileError::Io {
                    path: path(),
                    source: io(),
                }),
                RecoveryError::Manifest(ManifestError::Io {
                    path: path(),
                    source: io(),
                }),
                RecoveryError::Snapshot(SnapshotError::Io {
                    path: path(),
                    source: io(),
                }),
                RecoveryError::Parquet(ParquetFileError::Io {
                    path: path(),
                    source: io(),
                }),
                RecoveryError::Parquet(ParquetFileError::Format {
                    path: path(),
                    source: parquet::errors::ParquetError::from(io()),
                }),
            ];
            for failure in failures {
                assert_eq!(
                    cacheable_recovery_error(&failure),
                    kind == std::io::ErrorKind::NotFound,
                    "{failure}"
                );
            }
            assert!(!cacheable_recovery_error(&RecoveryError::Cleanup {
                path: path(),
                source: io()
            }));
            assert!(!cacheable_recovery_error(&RecoveryError::Wal(
                WalError::Io {
                    path: path(),
                    source: io()
                }
            )));
        }
        use crate::storage::{
            arrow_data::ArrowDataError,
            memtable::MemTableError,
            visibility::{SourceId, VisibilityError},
        };
        let deterministic = [
            RecoveryError::Arrow(ArrowDataError::ColumnCount),
            RecoveryError::Visibility(VisibilityError::UnknownSource(SourceId(1))),
            RecoveryError::MemTable(MemTableError::PositionExhausted),
            RecoveryError::Unsupported("future content".into()),
            RecoveryError::EmptyReferencedFile {
                path: PathBuf::from("empty.parquet"),
                kind: "data",
            },
            RecoveryError::MemTable(MemTableError::Arrow(ArrowDataError::TypeMismatch {
                name: "id".into(),
            })),
        ];
        for failure in deterministic {
            assert!(cacheable_recovery_error(&failure), "{failure}");
        }
        let wrapped_io =
            RecoveryError::Arrow(ArrowDataError::Arrow(arrow_schema::ArrowError::IoError(
                "wrapped I/O".into(),
                std::io::ErrorKind::PermissionDenied.into(),
            )));
        assert!(!cacheable_recovery_error(&wrapped_io));
        let nested_io = RecoveryError::MemTable(MemTableError::Arrow(ArrowDataError::Arrow(
            arrow_schema::ArrowError::IoError(
                "nested I/O".into(),
                std::io::ErrorKind::Other.into(),
            ),
        )));
        assert!(!cacheable_recovery_error(&nested_io));
    }

    #[test]
    fn cached_failure_does_not_hide_absent_or_moved_collection_directory() {
        for missing in ["moved", "deleted", "no-schema"] {
            let root = tempfile::tempdir().unwrap();
            seed_lazy_collections(root.path());
            let dir = root.path().join("collections/c");
            let manifest = dir.join("manifest.json");
            let healthy = fs::read(&manifest).unwrap();
            fs::write(&manifest, b"{").unwrap();
            let db = test_db(root.path());
            let original = db.collection("", "c").unwrap_err().to_string();
            let schema = fs::read(dir.join("schema.json")).unwrap();
            match missing {
                "moved" => fs::rename(&dir, root.path().join("saved")).unwrap(),
                "deleted" => fs::remove_dir_all(&dir).unwrap(),
                _ => fs::remove_file(dir.join("schema.json")).unwrap(),
            }
            assert!(matches!(
                db.collection("", "c"),
                Err(EngineError::CollectionNotFound(_))
            ));
            assert!(db.open_failures.read().contains_key("c"));
            assert_eq!(
                db.list_collections("").unwrap(),
                vec![("other".into(), LoadState::Loaded)]
            );
            if missing == "moved" {
                fs::rename(root.path().join("saved"), &dir).unwrap();
                fs::write(&manifest, healthy).unwrap();
                assert_eq!(db.collection("", "c").unwrap_err().to_string(), original);
            } else if missing == "no-schema" {
                fs::write(dir.join("schema.json"), schema).unwrap();
                assert_eq!(db.collection("", "c").unwrap_err().to_string(), original);
            }
            assert_eq!(db.collection("", "other").unwrap().read().count(), 1);
            db.drop_collection("", "c").unwrap();
            assert!(!db.open_failures.read().contains_key("c"));
            db.create_collection("", "c", basic_schema()).unwrap();
            assert_eq!(db.collection("", "c").unwrap().read().count(), 0);
            db.close();
        }
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn lazy_open_is_shared_by_same_name_and_parallel_across_names() {
        use std::sync::mpsc;
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        seed_lazy_collections(root.path());
        let db = Arc::new(test_db(root.path()));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let opener = Arc::clone(&db);
        let opening = std::thread::spawn(move || {
            let _flight = opener.enter().unwrap();
            pause_open_cleanup_once(entered_tx, resume_rx);
            opener.collection("", "c").unwrap()
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let sibling = Arc::clone(&db);
        let (other_tx, other_rx) = mpsc::channel();
        let independent = std::thread::spawn(move || {
            let _flight = sibling.enter().unwrap();
            other_tx
                .send(sibling.collection("", "other").unwrap())
                .unwrap();
        });
        assert_eq!(
            other_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .read()
                .count(),
            1
        );
        independent.join().unwrap();
        let sibling = Arc::clone(&db);
        let (same_tx, same_rx) = mpsc::channel();
        let waiting = std::thread::spawn(move || {
            let _flight = sibling.enter().unwrap();
            same_tx.send(sibling.collection("", "c").unwrap()).unwrap();
        });
        assert!(matches!(
            same_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        resume_tx.send(()).unwrap();
        let first = opening.join().unwrap();
        let same = same_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        waiting.join().unwrap();
        assert!(Arc::ptr_eq(&first, &same));
        assert!(db.name_locks.lock().is_empty());
        // 若重复恢复并清理，同名第二次访问会误删这份新放入的孤儿。
        let orphan = root
            .path()
            .join("collections/c/partitions/_default/data/late-orphan");
        fs::write(&orphan, b"keep").unwrap();
        assert!(Arc::ptr_eq(&first, &db.collection("", "c").unwrap()));
        assert!(orphan.exists());
        db.close();
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn interrupted_cleanup_is_not_cached_and_name_lock_is_released() {
        use std::sync::mpsc;
        let root = tempfile::tempdir().unwrap();
        seed_lazy_collections(root.path());
        let db = test_db(root.path());
        let (entered_tx, _entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        drop(resume_tx);
        pause_open_cleanup_once(entered_tx, resume_rx);
        assert!(db.collection("", "c").is_err());
        assert!(db.open_failures.read().is_empty());
        assert!(db.name_locks.lock().is_empty());
        assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
        db.close();
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn lazy_open_close_waits_for_registration_before_releasing_lock() {
        use std::sync::mpsc;
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        seed_lazy_collections(root.path());
        let db = Arc::new(test_db(root.path()));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let opener = Arc::clone(&db);
        let opening = std::thread::spawn(move || {
            let _flight = opener.enter().unwrap();
            pause_open_cleanup_once(entered_tx, resume_rx);
            opener.collection("", "c").unwrap()
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let closer = Arc::clone(&db);
        let (closed_tx, closed_rx) = mpsc::channel();
        let closing = std::thread::spawn(move || {
            closer.close();
            closed_tx.send(()).unwrap();
        });
        let waiting = matches!(
            closed_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        let locked = matches!(
            DataDir::open(root.path()),
            Err(crate::data_dir::DataDirError::Locked { .. })
        );
        resume_tx.send(()).unwrap();
        opening.join().unwrap();
        closed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        closing.join().unwrap();
        assert!(waiting);
        assert!(locked);
        assert!(matches!(db.enter(), Err(EngineError::Closed)));
        assert_eq!(
            test_db(root.path())
                .collection("", "c")
                .unwrap()
                .read()
                .count(),
            1
        );
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn lazy_open_and_same_name_drop_recreate_do_not_share_old_handle() {
        use std::sync::mpsc;
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        seed_lazy_collections(root.path());
        let db = Arc::new(test_db(root.path()));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let opener = Arc::clone(&db);
        let opening = std::thread::spawn(move || {
            let _flight = opener.enter().unwrap();
            pause_open_cleanup_once(entered_tx, resume_rx);
            opener.collection("", "c").unwrap()
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let changer = Arc::clone(&db);
        let (done_tx, done_rx) = mpsc::channel();
        let changing = std::thread::spawn(move || {
            let _flight = changer.enter().unwrap();
            changer.drop_collection("", "c").unwrap();
            changer.create_collection("", "c", basic_schema()).unwrap();
            done_tx.send(changer.collection("", "c").unwrap()).unwrap();
        });
        let waiting = matches!(
            done_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        resume_tx.send(()).unwrap();
        let old = opening.join().unwrap();
        let new = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        changing.join().unwrap();
        assert!(waiting);
        assert!(old.read().is_deleted());
        assert!(!Arc::ptr_eq(&old, &new));
        assert_eq!(new.read().count(), 0);
        assert!(matches!(
            Db::flush_handle(&old, true),
            Err(EngineError::CollectionNotFound(_))
        ));
        assert_eq!(db.collection("", "other").unwrap().read().count(), 1);
        db.close();
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn failure_cache_changes_at_directory_commit_even_when_sync_fails() {
        for creating in [true, false] {
            for point in [
                DirectoryPoint::BeforeRename,
                DirectoryPoint::CollectionsSync,
            ] {
                let root = tempfile::tempdir().unwrap();
                seed_lazy_collections(root.path());
                let manifest = root.path().join("collections/c/manifest.json");
                let healthy_manifest = fs::read(&manifest).unwrap();
                fs::write(&manifest, b"{").unwrap();
                let db = test_db(root.path());
                let original = db.collection("", "c").unwrap_err().to_string();
                if creating {
                    // 外部移走目录后直接 create，专门检查成功提交是否清除旧判定。
                    fs::rename(root.path().join("collections/c"), root.path().join("saved"))
                        .unwrap();
                }
                inject_directory_fault_once(
                    if creating {
                        DirectoryOperation::Create
                    } else {
                        DirectoryOperation::Drop
                    },
                    point,
                    DirectoryFault::Error,
                );
                let error = if creating {
                    db.create_collection("", "c", basic_schema())
                } else {
                    db.drop_collection("", "c")
                }
                .unwrap_err();
                if point == DirectoryPoint::BeforeRename {
                    assert!(
                        error
                            .to_string()
                            .contains("injected directory lifecycle failure")
                    );
                    // 再次读取损坏文件会重新缓存，掩盖提交点之前提前清除判定的错误。
                    assert!(db.open_failures.read().contains_key("c"));
                    if !creating {
                        fs::write(&manifest, &healthy_manifest).unwrap();
                    }
                    if creating {
                        assert!(matches!(
                            db.collection("", "c"),
                            Err(EngineError::CollectionNotFound(_))
                        ));
                        db.create_collection("", "c", basic_schema()).unwrap();
                    } else {
                        assert_eq!(db.collection("", "c").unwrap_err().to_string(), original);
                        db.drop_collection("", "c").unwrap();
                    }
                } else {
                    assert!(error.to_string().contains("durability is unconfirmed"));
                    assert!(*db.write_gate.read());
                    if creating {
                        assert_eq!(db.collection("", "c").unwrap().read().count(), 0);
                    } else {
                        assert!(!db.collections.read().contains_key("c"));
                    }
                }
                assert!(!db.open_failures.read().contains_key("c"));
                assert!(db.name_locks.lock().is_empty());
                db.close();
            }
        }
        let root = tempfile::tempdir().unwrap();
        seed_lazy_collections(root.path());
        fs::write(root.path().join("collections/c/manifest.json"), b"{").unwrap();
        let db = test_db(root.path());
        db.collection("", "c").unwrap_err();
        fs::remove_dir_all(root.path().join("collections/c")).unwrap();
        db.drop_collection("", "c").unwrap();
        assert!(!db.open_failures.read().contains_key("c"));
        db.create_collection("", "c", basic_schema()).unwrap();
        assert_eq!(db.collection("", "c").unwrap().read().count(), 0);
        db.close();
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
    #[cfg(feature = "fault-injection")]
    #[test]
    fn lazy_cleanup_holds_shared_write_permission_before_name_lock() {
        use std::sync::mpsc;
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        let db = Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
        );
        db.create_collection("", "c", basic_schema()).unwrap();
        db.close();
        let db = Arc::new(Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
        ));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let opener = Arc::clone(&db);
        let opening = std::thread::spawn(move || {
            pause_open_cleanup_once(entered_tx, resume_rx);
            opener.collection("", "c").unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(db.write_gate.try_write().is_none());
        assert!(db.namespace.try_lock().is_some());
        let lease = db.name_lease("c");
        assert!(lease.mutex().try_lock().is_none());
        resume_tx.send(()).unwrap();
        opening.join().unwrap();
        db.close();
    }
    #[cfg(feature = "fault-injection")]
    #[test]
    fn directory_changes_wait_for_an_unrelated_writer_before_taking_namespace() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        for creating in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let db = Arc::new(Db::new(
                DataDir::open(root.path()).unwrap(),
                Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
            ));
            db.create_collection("", "writer", basic_schema()).unwrap();
            db.create_collection("", "victim", basic_schema()).unwrap();
            let handle = db.collection("", "writer").unwrap();
            let state = handle.write();
            let writer = Arc::clone(&db);
            let writing = std::thread::spawn(move || {
                writer.insert(
                    "",
                    "writer",
                    InsertBatch {
                        num_rows: 1,
                        columns: vec![
                            ("id".into(), InputValues::Int(vec![1])),
                            (
                                "v".into(),
                                InputValues::FloatVector {
                                    dim: 2,
                                    values: vec![1.0, 0.0],
                                },
                            ),
                        ],
                    },
                )
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while db.write_gate.try_write().is_some() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
            let changer = Arc::clone(&db);
            let (started_tx, started_rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel();
            let changing = std::thread::spawn(move || {
                inject_directory_fault_once(
                    if creating {
                        DirectoryOperation::Create
                    } else {
                        DirectoryOperation::Drop
                    },
                    DirectoryPoint::CollectionsSync,
                    DirectoryFault::Error,
                );
                started_tx.send(()).unwrap();
                let result = if creating {
                    changer.create_collection("", "new", basic_schema())
                } else {
                    changer.drop_collection("", "victim")
                };
                done_tx.send(result).unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(matches!(
                done_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            assert!(db.namespace.try_lock().is_some());
            drop(state);
            writing.join().unwrap().unwrap();
            assert!(
                done_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("durability is unconfirmed")
            );
            changing.join().unwrap();
            assert_eq!(handle.read().count(), 1);
            assert!(
                db.flush_collection("", "writer")
                    .unwrap_err()
                    .to_string()
                    .contains("blocked")
            );
            db.close();
        }
    }
}
