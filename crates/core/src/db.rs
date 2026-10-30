//! 数据库：多 collection 的生命周期与引擎的关闭。M1 只有 `default` 一个数据库（0004「范围」）。

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use crate::lock_order::{Mutex as OrderedMutex, RwLock};
use parking_lot::{Condvar, Mutex};
type NameMutex = OrderedMutex<(), 3>;

use crate::alias::{self, Aliases};
use crate::collection::{Collection, LoadState};
use crate::compute::Compute;
use crate::data::{InsertBatch, Pk};
use crate::data_dir::DataDir;
use crate::error::{EngineError, Result, py_repr};
use crate::filter::ParsedFilter;
use crate::index::IndexRequest;
use crate::schema::CollectionSchema;
use crate::storage::fs_commit::{self, guarded_io};
use crate::storage::rename_intent::{self, RenameIntent};
use crate::storage::{recovery, runtime, schema_file::SchemaFile};

#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionPausePoint {
    Checked,
    Flushed,
    BeforeReplace,
    Published,
}
#[cfg(feature = "fault-injection")]
type PartitionPause = (
    PartitionPausePoint,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

/// 每个 collection 一把读写锁：写入互斥、读取并发，仍是「单 collection 单写者」（不变量 7）。
pub type CollectionHandle = Arc<RwLock<Collection, 5>>;

#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatePanicPoint {
    Mutation,
    Freeze,
    Publish,
    Index,
    Recommit,
}
#[cfg(feature = "fault-injection")]
thread_local! {
    static STATE_PANIC: std::cell::Cell<Option<StatePanicPoint>> = const { std::cell::Cell::new(None) };
    static CLOSE_PANIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(feature = "fault-injection")]
pub fn inject_close_finish_panic_once() {
    CLOSE_PANIC.with(|slot| slot.set(true));
}
#[cfg(feature = "fault-injection")]
pub fn inject_state_panic_once(point: StatePanicPoint) {
    STATE_PANIC.with(|slot| slot.set(Some(point)));
}
#[cfg(feature = "fault-injection")]
pub(crate) fn state_panic(point: StatePanicPoint) {
    if STATE_PANIC.with(|slot| {
        if slot.get() == Some(point) {
            slot.set(None);
            true
        } else {
            false
        }
    }) {
        panic!("injected collection state panic: {point:?}");
    }
}

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

/// 数据库作用域内的 collection 身份。`database` 已在入口归一化（空串即 `default`）；
/// 按（数据库，名字）排序，同一数据库内的顺序与 3.x 的 `sorted(names)`（码点序）一致。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CollectionKey {
    pub(crate) database: String,
    pub(crate) name: String,
}

/// 一个数据库的共享状态。A 中只有 `default`（0023 A 实施设计「模块与数据结构」）。
#[derive(Debug)]
struct DatabaseState {
    /// 数据库根目录，含 `collections/` 与 `aliases.json`；默认数据库即数据根目录。
    root: PathBuf,
    /// `aliases.json` 的缓存；叶子锁，覆盖完整的读改写（0023 A「别名」）。
    aliases: OrderedMutex<Option<Aliases>, 6>,
    /// 改名未完成（0023 A「改名」）：`Some` 为最近一次收尾失败的原因。此时 create、drop、改名与别名写入
    /// 先按磁盘上的意图重试收尾，仍失败则拒绝；数据读写照常。只在独占许可下修改。
    rename_pending: OrderedMutex<Option<String>, 6>,
}

impl DatabaseState {
    fn collections_dir(&self) -> PathBuf {
        self.root.join("collections")
    }

    fn collection_path(&self, name: &str) -> PathBuf {
        self.collections_dir().join(name)
    }
}

// 等待者也持强引用；只有最后一个使用者退出后才移除名字，不能在 drop 时换一把锁。
struct NameLease<'a> {
    registry: &'a OrderedMutex<BTreeMap<CollectionKey, Weak<NameMutex>>, 6>,
    key: CollectionKey,
    lock: Option<Arc<NameMutex>>,
}

impl<'a> NameLease<'a> {
    #[allow(clippy::expect_used)] // 名字锁守卫的借用先结束，析构才能独占租约并取走强引用。
    fn mutex(&self) -> &Arc<NameMutex> {
        self.lock.as_ref().expect("name lease is live")
    }

    /// 同时持有两个名字锁（改名的新旧名字）：按（数据库，名字）的字节序先后取得，同层只允许这一种嵌套。
    fn lock_pair<'g>(
        a: &'g NameLease<'a>,
        b: &'g NameLease<'a>,
    ) -> [crate::lock_order::MutexGuard<'g, ()>; 2] {
        let (first, second) = if a.key < b.key { (a, b) } else { (b, a) };
        debug_assert!(first.key < second.key, "a name lock pair needs two names");
        let held = first.mutex().lock();
        let next = second.mutex().lock_after(&held);
        [held, next]
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
            .get(&self.key)
            .is_some_and(|lock| lock.strong_count() == 0)
        {
            registry.remove(&self.key);
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
    /// 别名 RPC 的 `aliases.json` 写入，只用 `Aliases` 与 `AliasesAfterRename` 两点。
    Alias,
    /// AlterCollection 的 `schema.json` 替换，只用 `BeforeRename` 与 `AfterRename`（目录同步之前）两点。
    Alter,
    /// 改名：`BeforeRename` 为目录改名之前，`CollectionsSync` 为退役旧名后（及重开收尾时）同步 `collections/`，
    /// `Aliases` / `AliasesAfterRename` 为别名改指，其余为改名专用的点。
    Rename,
}
#[cfg(feature = "fault-injection")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryPoint {
    BeforeRename,
    AfterRename,
    CollectionsSync,
    StagingSync,
    Aliases,
    AliasesAfterRename,
    /// 改名意图写入的替换之前 / 之后。
    Intent,
    IntentAfterRename,
    /// 改名后的精确名字核对：`Error` 按核对不通过处理。
    NameVerification,
    /// 核对不通过后改回旧名之前。
    Rollback,
    /// 新目录 `schema.json` 的替换之前 / 之后。
    Schema,
    SchemaAfterRename,
    /// 删除改名意图之前 / 之后（同步数据库根目录之前）。
    IntentRemove,
    IntentRemoveAfter,
}

/// 改名过程中可暂停的位置，供测试在锁内观察其他请求。
#[cfg(feature = "fault-injection")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenamePausePoint {
    /// 旧名已退役、`collections/` 已同步，元数据尚未改写。
    Retired,
    /// `schema.json` 已改写，别名尚未改指。
    SchemaReplaced,
}
#[cfg(feature = "fault-injection")]
type RenamePauseSlot = (
    RenamePausePoint,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);
#[cfg(feature = "fault-injection")]
#[derive(Clone, Copy, Debug)]
pub enum DirectoryFault {
    Error,
    Panic,
    Exit,
}

#[cfg(feature = "fault-injection")]
thread_local! {
    static DIRECTORY_FAULT: std::cell::RefCell<Vec<(DirectoryOperation, DirectoryPoint, DirectoryFault)>> = const { std::cell::RefCell::new(Vec::new()) };
    static OPEN_CLEANUP_PAUSE: std::cell::RefCell<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>> = const { std::cell::RefCell::new(None) };
    static RENAME_PAUSE: std::cell::RefCell<Option<RenamePauseSlot>> = const { std::cell::RefCell::new(None) };
}

#[cfg(feature = "fault-injection")]
pub fn pause_rename_once(
    point: RenamePausePoint,
    entered: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
) {
    RENAME_PAUSE.with(|slot| *slot.borrow_mut() = Some((point, entered, resume)));
}

#[cfg(feature = "fault-injection")]
pub fn inject_directory_fault_once(
    op: DirectoryOperation,
    point: DirectoryPoint,
    fault: DirectoryFault,
) {
    DIRECTORY_FAULT.with(|slot| slot.borrow_mut().push((op, point, fault)));
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
    // 可同时登记多个不同的点，各自只触发一次（例如改名失败之后的意图删除也失败）。
    let fault = DIRECTORY_FAULT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let index = slot
            .iter()
            .position(|(wanted_op, wanted_point, _)| op == *wanted_op && point == *wanted_point)?;
        Some(slot.remove(index).2)
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

/// 改名各步骤的故障点；非故障注入构建中不做任何事。
#[derive(Clone, Copy, Debug)]
enum RenameStep {
    Intent,
    IntentAfterRename,
    Directory,
    NameVerification,
    Rollback,
    CollectionsSync,
    Schema,
    SchemaAfterRename,
    Aliases,
    AliasesAfterRename,
    IntentRemove,
    IntentRemoveAfter,
}

fn rename_fault(step: RenameStep) -> std::io::Result<()> {
    #[cfg(feature = "fault-injection")]
    {
        let point = match step {
            RenameStep::Intent => DirectoryPoint::Intent,
            RenameStep::IntentAfterRename => DirectoryPoint::IntentAfterRename,
            RenameStep::Directory => DirectoryPoint::BeforeRename,
            RenameStep::NameVerification => DirectoryPoint::NameVerification,
            RenameStep::Rollback => DirectoryPoint::Rollback,
            RenameStep::CollectionsSync => DirectoryPoint::CollectionsSync,
            RenameStep::Schema => DirectoryPoint::Schema,
            RenameStep::SchemaAfterRename => DirectoryPoint::SchemaAfterRename,
            RenameStep::Aliases => DirectoryPoint::Aliases,
            RenameStep::AliasesAfterRename => DirectoryPoint::AliasesAfterRename,
            RenameStep::IntentRemove => DirectoryPoint::IntentRemove,
            RenameStep::IntentRemoveAfter => DirectoryPoint::IntentRemoveAfter,
        };
        directory_fault(DirectoryOperation::Rename, point)
    }
    #[cfg(not(feature = "fault-injection"))]
    {
        let _step = step;
        Ok(())
    }
}

/// 文件替换类步骤的两个故障点。
fn rename_replace_fault(
    before: RenameStep,
    after: RenameStep,
) -> impl Fn(fs_commit::ReplacePoint) -> std::io::Result<()> {
    move |point| {
        rename_fault(match point {
            fs_commit::ReplacePoint::BeforeRename => before,
            fs_commit::ReplacePoint::AfterRename => after,
        })
    }
}

#[derive(Clone, Copy)]
enum RenamePause {
    Retired,
    SchemaReplaced,
}

fn rename_pause(at: RenamePause) -> Result<()> {
    #[cfg(feature = "fault-injection")]
    {
        let point = match at {
            RenamePause::Retired => RenamePausePoint::Retired,
            RenamePause::SchemaReplaced => RenamePausePoint::SchemaReplaced,
        };
        let pause = RENAME_PAUSE.with(|slot| {
            let mut slot = slot.borrow_mut();
            match slot.take() {
                Some((wanted, entered, resume)) if wanted == point => Some((entered, resume)),
                other => {
                    *slot = other;
                    None
                }
            }
        });
        if let Some((entered, resume)) = pause {
            entered.send(()).map_err(storage)?;
            resume.recv().map_err(storage)?;
        }
    }
    #[cfg(not(feature = "fault-injection"))]
    let _at = at;
    Ok(())
}

/// 改名收尾（P3、P4）失败的两类结果。
enum RenameFailure {
    /// 文件替换之前失败或无法判定：改名未完成，可重试。
    Pending(String),
    /// 文件已替换、所在目录同步未确认。
    Unconfirmed(String),
}

impl RenameFailure {
    fn reason(self) -> String {
        match self {
            Self::Pending(reason) | Self::Unconfirmed(reason) => reason,
        }
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
    collections: RwLock<BTreeMap<CollectionKey, CollectionHandle>, 6>,
    gate: Mutex<Gate>,
    idle: Condvar,
    /// 服务的计算池（0005）。服务的运行状态与每个在途的 RPC 都持有 `Arc<Db>`，
    /// 所以它在最后一个在途任务结束之后才析构。
    compute: Compute,
    path: PathBuf,
    namespace: OrderedMutex<(), 2>,
    databases: BTreeMap<String, Arc<DatabaseState>>,
    name_locks: OrderedMutex<BTreeMap<CollectionKey, Weak<NameMutex>>, 6>,
    open_failures: RwLock<BTreeMap<CollectionKey, OpenFailure>, 6>,
    /// create/drop 与 collection 的写入/flush 互斥；目录提交点后失败时封锁后续写入。
    write_gate: RwLock<bool, 1>,
    maintenance: Arc<crate::maintenance::Maintenance>,
    namespace_waiters: AtomicUsize,
    shutdown_failed: AtomicBool,
    #[cfg(feature = "fault-injection")]
    partition_pause: Mutex<Option<PartitionPause>>,
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

/// 一个在途任务。析构递减计数并唤醒收尾等待者；LOCK 仍由最终 close 完成后释放。
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

struct NamespaceIntent<'a>(&'a AtomicUsize);
impl<'a> NamespaceIntent<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter)
    }
}
impl Drop for NamespaceIntent<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
impl Drop for Db {
    fn drop(&mut self) {
        self.maintenance.stop(false);
        if self.gate.get_mut().data_dir.is_some() {
            self.finish_storage(false);
        }
        let data_dir = self.gate.get_mut().data_dir.take();
        self.release_data_dir(data_dir);
    }
}

impl Db {
    pub fn new(data_dir: DataDir, compute: Compute) -> Result<Arc<Self>> {
        let path = data_dir.path().to_path_buf();
        let db = Arc::new(Self {
            collections: RwLock::new(BTreeMap::new()),
            gate: Mutex::new(Gate {
                closed: false,
                in_flight: 0,
                cleanup_started: false,
                data_dir: Some(data_dir),
            }),
            idle: Condvar::new(),
            compute,
            path: path.clone(),
            namespace: OrderedMutex::new(()),
            databases: BTreeMap::from([(
                DEFAULT_DATABASE.to_owned(),
                Arc::new(DatabaseState {
                    root: path,
                    aliases: OrderedMutex::new(None),
                    rename_pending: OrderedMutex::new(None),
                }),
            )]),
            name_locks: OrderedMutex::new(BTreeMap::new()),
            open_failures: RwLock::new(BTreeMap::new()),
            write_gate: RwLock::new(false),
            maintenance: crate::maintenance::Maintenance::new(),
            namespace_waiters: AtomicUsize::new(0),
            shutdown_failed: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            partition_pause: Mutex::new(None),
        });
        // 0023 A「改名 / 启动处理」：LOCK 与 staging 清理之后、开始服务之前；失败不让启动失败。
        for state in db.databases.values() {
            if let Err(failure) = db.settle_rename(state, false) {
                let reason = failure.reason();
                eprintln!(
                    "collection rename in {:?} is unfinished: {reason}",
                    state.root
                );
                *state.rename_pending.lock() = Some(reason);
            }
        }
        db.maintenance.start(Arc::downgrade(&db))?;
        Ok(db)
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

    /// 宽限结束后只封住公开入场；在途任务、维护和 LOCK 仍由最终 close 收尾。
    pub fn begin_shutdown(&self) {
        self.gate.lock().closed = true;
    }

    pub fn shutdown_failed(&self) -> bool {
        self.shutdown_failed.load(Ordering::Acquire)
    }

    fn report_shutdown_failure(&self, reason: impl std::fmt::Display) {
        self.shutdown_failed.store(true, Ordering::Release);
        eprintln!("{reason}");
    }

    fn release_data_dir(&self, data_dir: Option<DataDir>) {
        if let Some(data_dir) = data_dir
            && let Err(error) = data_dir.unlock()
        {
            self.report_shutdown_failure(format!(
                "cannot unlock data directory {:?}: {error}",
                data_dir.path()
            ));
        }
    }

    pub(crate) fn record_maintenance_shutdown_failure(&self) {
        if self.gate.lock().cleanup_started {
            self.shutdown_failed.store(true, Ordering::Release);
        }
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
        if !self.maintenance.stop(true) {
            self.report_shutdown_failure("maintenance shutdown failed");
        }
        self.finish_storage(false);
        let mut gate = self.gate.lock();
        self.release_data_dir(gate.data_dir.take());
        self.idle.notify_all();
    }

    /// 拒绝新任务并由收尾线程等待在途/维护结束、同步保留 WAL 后释放 LOCK。
    pub fn close_nowait(self: &Arc<Self>) {
        self.maintenance.abort.store(true, Ordering::Release);
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
                    db.maintenance.stop(false);
                    db.finish_storage(false);
                    let mut gate = db.gate.lock();
                    db.release_data_dir(gate.data_dir.take());
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
            self.maintenance.stop(false);
            self.finish_storage(false);
            let mut gate = self.gate.lock();
            self.release_data_dir(gate.data_dir.take());
            self.idle.notify_all();
        }
    }

    fn finish_storage(&self, flush: bool) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(feature = "fault-injection")]
            if CLOSE_PANIC.with(|slot| slot.replace(false)) {
                panic!("injected storage close panic");
            }
            self.finish_storage_inner(flush);
        }));
        if let Err(payload) = result {
            self.report_shutdown_failure(format!(
                "storage close panicked: {}",
                crate::error::panic_description(payload.as_ref())
            ));
            // 关闭已经不可逆；停止 manifest 提交，只同步原 WAL，再让关闭闸门完成并释放 LOCK。
            let handles: Vec<_> = self.collections.read().values().cloned().collect();
            for handle in handles {
                let result = state_change(&mut handle.write(), |collection| {
                    if let Some(persistent) = collection.persistent_mut() {
                        persistent.poison();
                        persistent.sync_retained_wal().map_err(storage)?;
                    }
                    Ok(())
                });
                if let Err(error) = result {
                    self.report_shutdown_failure(format!(
                        "cannot sync WAL after close panic: {error}"
                    ));
                }
            }
        }
    }

    fn finish_storage_inner(&self, flush: bool) {
        // 目录改名的持久化未确认时，任何 manifest 提交都可能固定错误的目录状态。
        let flush = if flush && *self.write_gate.read() {
            self.report_shutdown_failure(
                "storage close flush skipped: database directory durability is unconfirmed",
            );
            false
        } else {
            flush
        };
        let handles: Vec<_> = self.collections.read().values().cloned().collect();
        for handle in handles {
            if flush {
                match self.flush_handle(&handle, true) {
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
                                    frozen.and_then(|()| self.flush_handle(&handle, false))
                                {
                                    self.report_shutdown_failure(format!(
                                        "collection close WAL cleanup failed: {error}"
                                    ));
                                }
                            }
                            Ok(false) => {}
                            Err(error) => self.report_shutdown_failure(format!(
                                "collection close WAL scan failed: {error}"
                            )),
                        }
                    }
                    Err(error) => self
                        .report_shutdown_failure(format!("collection close flush failed: {error}")),
                }
            }
            let result = handle
                .write()
                .persistent_mut()
                .map(|p| p.sync_retained_wal());
            if let Some(Err(error)) = result {
                self.report_shutdown_failure(format!("collection close WAL sync failed: {error}"));
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
        let key = self.key(database, name)?;
        self.create_collection_inner(&key, schema)?;
        let handle = self.collections.read().get(&key).cloned();
        if let Some(handle) = handle {
            self.schedule_maintenance(&handle);
        }
        Ok(())
    }

    fn create_collection_inner(&self, key: &CollectionKey, schema: CollectionSchema) -> Result<()> {
        let name = key.name.as_str();
        validate_filesystem_name(name, "collection")?;
        let mut collection = Collection::new(name, schema)?;
        let _intent = NamespaceIntent::new(&self.namespace_waiters);
        let mut blocked = self.write_gate.write();
        if *blocked {
            return Err(EngineError::Storage(
                "database writes are blocked after an unconfirmed directory change".to_owned(),
            ));
        }
        self.settle_pending_rename(self.database(&key.database)?, &mut blocked)?;
        let _namespace = self.namespace.lock();
        let lease = self.name_lease(key);
        let _name = lease.mutex().lock();
        let database = self.database(&key.database)?;
        let target = database.collection_path(name);
        if self.collections.read().contains_key(key) || fs::symlink_metadata(&target).is_ok() {
            return Err(EngineError::CollectionAlreadyExists(name.to_owned()));
        }
        let root = database.collections_dir();
        let staging_root = self.path.join(".database-staging");
        for dir in [&root, &staging_root] {
            match fs::symlink_metadata(dir) {
                Ok(meta) if meta.file_type().is_dir() => {}
                Ok(_) => return Err(storage(format!("directory path {dir:?} is occupied"))),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(storage(error)),
            }
        }
        {
            let mut cache = database.aliases.lock();
            let aliases = alias::load(&mut cache, &database.root)?;
            let mut live = aliases.clone();
            retain_live_aliases(database, &mut live).map_err(storage)?;
            if live.len() != aliases.len() {
                save_lifecycle_aliases(database, aliases, live, true)?;
            }
            if aliases.contains_key(name) {
                return Err(EngineError::CollectionAlreadyExists(name.to_owned()));
            }
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
        let mut initial_manifest = crate::storage::manifest::Manifest::default();
        crate::partition_key::register(collection.schema(), &mut initial_manifest);
        if collection.schema().partition_key().is_some() {
            let result = initial_manifest
                .to_bytes()
                .map_err(storage)
                .and_then(|bytes| {
                    fs_commit::write_new_schema(&staging.join("manifest.json"), &bytes)
                        .map_err(storage)
                });
            if let Err(error) = result {
                cleanup_failed_staging(&staging);
                return Err(error);
            }
        }
        let rename = guarded_io(|| {
            #[cfg(feature = "fault-injection")]
            directory_fault(DirectoryOperation::Create, DirectoryPoint::BeforeRename)?;
            fs::rename(&staging, &target)
        });
        if let Err(error) = rename {
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
            self.exact_collection_name(key)
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
        self.open_failures.write().remove(key);
        collection.attach_persistence(
            runtime::PersistentCollection::with_manifest(target.clone(), initial_manifest)
                .map_err(EngineError::Storage)?,
        );
        self.collections
            .write()
            .insert(key.clone(), Arc::new(RwLock::new(collection)));
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
        let key = self.key(database, name)?;
        if name.is_empty() {
            return Ok(());
        }
        validate_filesystem_name(name, "collection")?;
        let _intent = NamespaceIntent::new(&self.namespace_waiters);
        let mut blocked = self.write_gate.write();
        if *blocked {
            return Err(storage(
                "database writes are blocked after an unconfirmed directory change",
            ));
        }
        self.settle_pending_rename(self.database(&key.database)?, &mut blocked)?;
        let _namespace = self.namespace.lock();
        // 经别名删除目标集合及指向它的全部别名（3.x 相同）。独占许可下解析，别名不会在此之后改指。
        let key = self.resolve(key)?;
        let name = key.name.as_str();
        validate_filesystem_name(name, "collection")?;
        let lease = self.name_lease(&key);
        let _name = lease.mutex().lock();
        let database = self.database(&key.database)?;
        let target = database.collection_path(name);
        let exists = match fs::symlink_metadata(&target) {
            Ok(meta) => meta.file_type().is_dir() && target.join("schema.json").is_file(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(storage(error)),
        };
        if !exists || !self.exact_collection_name(&key)? {
            let handle = self.collections.read().get(&key).cloned();
            let flush_lock = handle
                .as_ref()
                .and_then(|h| h.read().persistent().map(|p| p.flush_lock()));
            let _flush = flush_lock.as_ref().map(|lock| lock.lock());
            if let Some(handle) = &handle {
                let mut collection = handle.write();
                state_change(&mut collection, |collection| {
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
                    Ok(())
                })?;
                // collection 表是叶子锁：所有读者先释放表锁再取句柄锁；此处保持 deleted 与撤表同一临界区。
                self.collections.write().remove(&key);
            }
            self.open_failures.write().remove(&key);
            remove_aliases_for(database, name)?;
            return Ok(());
        }
        let root = database.collections_dir();
        let staging_root = self.path.join(".database-staging");
        fs_commit::create_dir_synced(&staging_root).map_err(storage)?;
        let handle = self.collections.read().get(&key).cloned();
        let flush_lock = handle
            .as_ref()
            .and_then(|h| h.read().persistent().map(|p| p.flush_lock()));
        let _flush = flush_lock.as_ref().map(|lock| lock.lock());
        if let Some(handle) = &handle {
            let mut collection = handle.write();
            state_change(&mut collection, |collection| {
                collection.mark_deleted(true);
                if let Some(persistent) = collection.persistent_mut()
                    && let Err(error) = persistent.sync_retained_wal()
                {
                    collection.mark_deleted(false);
                    return Err(storage(error));
                }
                Ok(())
            })?;
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
        if let Err(error) = guarded_io(rename) {
            if let Some(handle) = &handle {
                let mut collection = handle.write();
                state_change(&mut collection, |collection| {
                    collection.mark_deleted(false);
                    if let Some(persistent) = collection.persistent_mut() {
                        persistent.freeze_active(true).map_err(storage)?;
                    }
                    Ok(())
                })?;
            }
            return Err(storage(error));
        }
        self.collections.write().remove(&key);
        self.open_failures.write().remove(&key);
        *blocked = true;
        if let Err(error) = sync_directory_change(&root, &staging_root, false) {
            return Err(storage(format!(
                "collection was removed, but directory durability is unconfirmed: {error}"
            )));
        }
        *blocked = false;
        remove_aliases_for(database, name).map_err(|error| {
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

    /// 3.x 的 `_require_database`：空名即 `default`。其他数据库尚未实现：3.x 认为存在的
    /// （`databases/<db>` 是目录）按 0006 第 15 项返回 UNIMPLEMENTED，而不是谎称不存在；
    /// 名字须精确出现在目录列表中，只差大小写的名字仍报不存在（0023 名字与文件系统）。
    /// 3.x 的 `os.path.isdir` 把任何 I/O 错误当作不存在，这里的列目录与 metadata 同样如此。
    fn require_database(&self, name: &str) -> Result<()> {
        if name.is_empty() || name == DEFAULT_DATABASE {
            return Ok(());
        }
        validate_filesystem_name(name, "database")?;
        let root = self.path.join("databases");
        let exact = fs::read_dir(&root).is_ok_and(|mut entries| {
            entries.any(|entry| entry.is_ok_and(|entry| entry.file_name() == name))
        });
        if exact && fs::metadata(root.join(name)).is_ok_and(|meta| meta.is_dir()) {
            return Err(EngineError::unsupported(
                "database",
                format!(
                    "database {} exists, but non-default databases are not supported yet",
                    py_repr(name)
                ),
            ));
        }
        Err(EngineError::DatabaseNotFound(name.to_owned()))
    }

    /// 归一化数据库名并确认它可用；集合名在各入口按原有顺序另行校验。
    fn key(&self, database: &str, name: &str) -> Result<CollectionKey> {
        self.require_database(database)?;
        Ok(CollectionKey {
            database: if database.is_empty() {
                DEFAULT_DATABASE.to_owned()
            } else {
                database.to_owned()
            },
            name: name.to_owned(),
        })
    }

    fn database(&self, name: &str) -> Result<&DatabaseState> {
        self.databases
            .get(name)
            .map(Arc::as_ref)
            .ok_or_else(|| EngineError::DatabaseNotFound(name.to_owned()))
    }

    /// 0023 必须保留的行为 1：每个集合入口先解析别名，命中即换成目标名。只解析一层，别名指向的总是真实集合名。
    /// 写入口在许可之后调用，解析结果在许可期间不会被别名写入（独占许可）改变。
    fn resolve(&self, key: CollectionKey) -> Result<CollectionKey> {
        let database = self.database(&key.database)?;
        let mut cache = database.aliases.lock();
        let target = alias::load(&mut cache, &database.root)?
            .get(&key.name)
            .cloned();
        Ok(match target {
            Some(name) => CollectionKey { name, ..key },
            None => key,
        })
    }

    fn exact_collection_name(&self, key: &CollectionKey) -> Result<bool> {
        self.exact_name_in(self.database(&key.database)?, &key.name)
    }

    fn exact_name_in(&self, database: &DatabaseState, name: &str) -> Result<bool> {
        let root = database.collections_dir();
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

    /// 读请求：别名叶子锁内取一次解析结果，再按目标名查找或懒打开。
    pub fn collection(&self, database: &str, name: &str) -> Result<CollectionHandle> {
        let key = self.resolve(self.key(database, name)?)?;
        let cached = self.collections.read().get(&key).cloned();
        if let Some(handle) = cached {
            return checked_read_handle(handle);
        }
        let blocked = self.write_gate.read();
        if *blocked {
            let cached = self.collections.read().get(&key).cloned();
            if let Some(handle) = cached {
                return checked_read_handle(handle);
            }
            return Err(EngineError::Storage(
                "database writes are blocked after an unconfirmed directory change".to_owned(),
            ));
        }
        checked_read_handle(self.collection_inner(&key)?)
    }

    /// 写入从数据库入口取得许可；自动 flush 的磁盘阶段不持 collection 状态锁。
    pub fn insert(&self, database: &str, name: &str, batch: InsertBatch) -> Result<Vec<Pk>> {
        self.insert_with_pk_type(database, name, batch)
            .map(|outcome| outcome.pks)
    }

    #[cfg(feature = "fault-injection")]
    pub fn pause_partition_once(
        &self,
        point: PartitionPausePoint,
        entered: std::sync::mpsc::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) {
        *self.partition_pause.lock() = Some((point, entered, resume));
    }

    #[cfg(feature = "fault-injection")]
    fn partition_pause(&self, point: PartitionPausePoint) -> Result<()> {
        let pause = {
            let mut slot = self.partition_pause.lock();
            if slot.as_ref().is_some_and(|(wanted, _, _)| *wanted == point) {
                slot.take()
            } else {
                None
            }
        };
        if let Some((_, entered, resume)) = pause {
            entered.send(()).map_err(storage)?;
            resume
                .recv_timeout(std::time::Duration::from_secs(30))
                .map_err(storage)?;
        }
        Ok(())
    }

    pub fn create_partition(&self, database: &str, name: &str, partition: &str) -> Result<()> {
        let key = self.key(database, name)?;
        let _intent = NamespaceIntent::new(&self.namespace_waiters);
        let blocked = self.write_gate.write();
        if *blocked {
            return Err(storage(
                "database writes are blocked after an unconfirmed directory change",
            ));
        }
        let _namespace = self.namespace.lock();
        let key = self.resolve(key)?;
        let handle = checked_read_handle(self.collection_inner(&key)?)?;
        {
            let collection = handle.read();
            if let Some(key) = collection.schema().partition_key() {
                return Err(EngineError::schema(format!(
                    "cannot create manual partitions when partition key is set (partition_key field: {})",
                    crate::error::py_repr(&key.name)
                )));
            }
            let p = collection
                .persistent()
                .ok_or_else(|| storage("persistent state missing"))?;
            if p.manifest.partitions.contains_key(partition) {
                return Err(EngineError::PartitionAlreadyExists(partition.to_owned()));
            }
        }
        if !crate::storage::partition_paths::valid_name(partition) {
            return Err(EngineError::schema(format!(
                "invalid partition name: {}",
                crate::error::py_repr(partition)
            )));
        }
        #[cfg(feature = "fault-injection")]
        self.partition_pause(PartitionPausePoint::Checked)?;
        if handle.read().persistent().is_some_and(|p| p.needs_retry()) {
            self.flush_handle(&handle, false)?;
        }
        #[cfg(feature = "fault-injection")]
        self.partition_pause(PartitionPausePoint::Flushed)?;
        let lock = handle
            .read()
            .persistent()
            .ok_or_else(|| storage("persistent state missing"))?
            .flush_lock();
        let _flush = lock.lock();
        self.check_partition_index_intent(&handle)?;
        self.cleanup_partition_files(&handle)?;
        let (dir, candidate, registry, pins, inherited) = {
            let mut collection = handle.write();
            state_change(&mut collection, |collection| {
                collection.enable_partition_visibility()
            })?;
            let p = collection
                .persistent()
                .ok_or_else(|| storage("persistent state missing"))?;
            let mut candidate = p.manifest.clone();
            candidate
                .partitions
                .insert(partition.to_owned(), Default::default());
            candidate.partition_order.push(partition.to_owned());
            candidate.version = candidate
                .version
                .checked_add(1)
                .ok_or_else(|| storage("manifest version space exhausted"))?;
            let mut registry = p.partitions.clone();
            registry.add(partition).map_err(storage)?;
            (
                p.dir.clone(),
                candidate,
                registry,
                p.pinned_files.clone(),
                p.inherited_synced(),
            )
        };
        let original = handle
            .read()
            .persistent()
            .ok_or_else(|| storage("persistent state missing"))?
            .manifest
            .clone();
        crate::storage::partition_paths::prepare(&dir, &original, &pins, partition)
            .map_err(storage)?;
        if !inherited {
            crate::storage::flush::sync_inherited(&dir, &original).map_err(storage)?;
            handle
                .write()
                .persistent_mut()
                .ok_or_else(|| storage("persistent state missing"))?
                .mark_inherited_synced();
        }
        let mut prepared = fs_commit::prepare_manifest(&dir, &candidate).map_err(storage)?;
        #[cfg(feature = "fault-injection")]
        self.partition_pause(PartitionPausePoint::BeforeReplace)?;
        handle
            .read()
            .persistent()
            .ok_or_else(|| storage("persistent state missing"))?
            .validate_partition_commit()
            .map_err(storage)?;
        // 独占许可及 flush 锁覆盖提交；磁盘 rename/fsync 不挡住读取旧快照。
        let committed = prepared.replace();
        let mut collection = handle.write();
        let result = state_change(&mut collection, |collection| {
            collection
                .persistent_mut()
                .ok_or_else(|| storage("persistent state missing"))?
                .publish_partition_manifest(candidate, registry, committed)
                .map_err(storage)
        });
        drop(collection);
        #[cfg(feature = "fault-injection")]
        self.partition_pause(PartitionPausePoint::Published)?;
        result
    }

    pub fn drop_partition(&self, database: &str, name: &str, partition: &str) -> Result<()> {
        let key = self.key(database, name)?;
        let _intent = NamespaceIntent::new(&self.namespace_waiters);
        let blocked = self.write_gate.write();
        if *blocked {
            return Err(storage(
                "database writes are blocked after an unconfirmed directory change",
            ));
        }
        let _namespace = self.namespace.lock();
        let key = self.resolve(key)?;
        let handle = checked_read_handle(self.collection_inner(&key)?)?;
        if let Some(key) = handle.read().schema().partition_key() {
            return Err(EngineError::schema(format!(
                "cannot drop partitions when partition key is set (partition_key field: {})",
                crate::error::py_repr(&key.name)
            )));
        }
        if !handle
            .read()
            .persistent()
            .is_some_and(|p| p.manifest.partitions.contains_key(partition))
        {
            return Err(EngineError::PartitionNotFound(partition.into()));
        }
        #[cfg(feature = "fault-injection")]
        self.partition_pause(PartitionPausePoint::Checked)?;
        // 参照在拒绝默认分区删除之前也会 Flush 待写数据。
        self.flush_handle(&handle, true)?;
        if partition == "_default" {
            return Err(EngineError::schema(
                "cannot remove default partition '_default'",
            ));
        }
        let recovered_wal = {
            let mut collection = handle.write();
            let retained = collection
                .persistent()
                .ok_or_else(|| storage("persistent state missing"))?
                .retained_old_wal();
            if retained {
                // 恢复可滤掉全部已提交行；移除分区前仍须发布空恢复代并排空旧 WAL。
                state_change(&mut collection, |collection| {
                    collection
                        .persistent_mut()
                        .ok_or_else(|| storage("persistent state missing"))?
                        .freeze_active(true)
                        .map_err(storage)
                })?;
            }
            retained
        };
        if recovered_wal {
            self.flush_handle(&handle, false)?;
        }
        #[cfg(feature = "fault-injection")]
        self.partition_pause(PartitionPausePoint::Flushed)?;
        let lock = handle
            .read()
            .persistent()
            .ok_or_else(|| storage("persistent state missing"))?
            .flush_lock();
        let _flush = lock.lock();
        self.check_partition_index_intent(&handle)?;
        let (dir, old, pins, mut registry, id, inherited) = {
            let c = handle.read();
            let p = c
                .persistent()
                .ok_or_else(|| storage("persistent state missing"))?;
            (
                p.dir.clone(),
                p.manifest.clone(),
                p.pinned_files.clone(),
                p.partitions.clone(),
                p.partitions
                    .id(partition)
                    .ok_or_else(|| storage("partition identifier missing"))?,
                p.inherited_synced(),
            )
        };
        crate::storage::partition_paths::validate(&dir, &old, &pins, None).map_err(storage)?;
        if !inherited {
            crate::storage::flush::sync_inherited(&dir, &old).map_err(storage)?;
        }
        let candidate = crate::storage::partition_drop::prepare(&dir, &old, &pins, partition)
            .map_err(storage)?;
        registry.remove(partition).map_err(storage)?;
        let mut prepared = fs_commit::prepare_manifest(&dir, &candidate).map_err(storage)?;
        #[cfg(feature = "fault-injection")]
        self.partition_pause(PartitionPausePoint::BeforeReplace)?;
        handle
            .read()
            .persistent()
            .ok_or_else(|| storage("persistent state missing"))?
            .validate_partition_commit()
            .map_err(storage)?;
        // 独占许可及 flush 锁覆盖提交；磁盘 rename/fsync 不挡住读取旧快照。
        let committed = prepared.replace();
        let mut c = handle.write();
        let publication = state_change(&mut c, |c| {
            let p = c
                .persistent_mut()
                .ok_or_else(|| storage("persistent state missing"))?;
            let result = p
                .publish_partition_manifest(candidate, registry, committed)
                .map_err(storage);
            // replace 后的 fsync 错误已发布新 manifest，必须同步内存而非保留旧分区。
            if !p.manifest.partitions.contains_key(partition)
                && let Err(error) = c.publish_partition_drop(partition, id)
            {
                c.persistent_mut()
                    .ok_or_else(|| storage("persistent state missing"))?
                    .poison();
                return Err(error);
            }
            result
        });
        drop(c);
        #[cfg(feature = "fault-injection")]
        self.partition_pause(PartitionPausePoint::Published)?;

        publication?;
        self.cleanup_partition_files(&handle)?;
        Ok(())
    }

    fn cleanup_partition_files(&self, handle: &CollectionHandle) -> Result<()> {
        let state = {
            let c = handle.read();
            let p = c
                .persistent()
                .ok_or_else(|| storage("persistent state missing"))?;
            if !p.partition_cleanup || p.retry_manifest() || p.poisoned() {
                return Ok(());
            }
            (p.dir.clone(), p.manifest.clone(), p.pinned_files.clone())
        };
        let warnings =
            recovery::cleanup_unreferenced(&state.0, &state.1, &state.2).map_err(recovery_error)?;
        let complete = warnings.is_empty();
        for warning in warnings {
            eprintln!("{warning}");
        }
        handle
            .write()
            .persistent_mut()
            .ok_or_else(|| storage("persistent state missing"))?
            .partition_cleanup = !complete;
        Ok(())
    }

    // 调用者持有命名空间许可及 flush 锁；索引删除意图必须先完成或封锁后续文件清理。
    fn check_partition_index_intent(&self, handle: &CollectionHandle) -> Result<()> {
        let (dir, mut intent) = {
            let c = handle.read();
            let p = c
                .persistent()
                .ok_or_else(|| storage("persistent state missing"))?;
            (p.dir.clone(), p.index_intent.clone())
        };
        let result = intent.check(&dir).map_err(EngineError::invalid);
        handle
            .write()
            .persistent_mut()
            .ok_or_else(|| storage("persistent state missing"))?
            .index_intent = intent;
        result
    }

    pub fn has_partition(&self, database: &str, name: &str, partition: &str) -> Result<bool> {
        let handle = self.collection(database, name)?;
        let collection = handle.read();
        Ok(collection
            .persistent()
            .map_or(partition == "_default", |p| {
                p.manifest.partitions.contains_key(partition)
            }))
    }

    pub fn list_partitions(&self, database: &str, name: &str) -> Result<(Vec<String>, LoadState)> {
        let handle = self.collection(database, name)?;
        let collection = handle.read();
        let names = collection.persistent().map_or_else(
            || vec!["_default".into()],
            |p| p.manifest.partitions.keys().cloned().collect(),
        );
        Ok((names, collection.load_state()))
    }

    pub fn insert_with_pk_type(
        &self,
        database: &str,
        name: &str,
        batch: InsertBatch,
    ) -> Result<InsertOutcome> {
        self.insert_partition_with_pk_type(database, name, batch, "_default")
    }

    pub fn insert_partition_with_pk_type(
        &self,
        database: &str,
        name: &str,
        batch: InsertBatch,
        partition: &str,
    ) -> Result<InsertOutcome> {
        self.mutate(database, name, |collection| {
            let varchar = collection.schema().fields.iter().any(|field| {
                field.is_primary && field.data_type == crate::schema::DataType::VarChar
            });
            let pks = if matches!(partition, "" | "_default") {
                collection.insert_from_db(batch)?
            } else {
                collection.insert_partition_from_db(batch, partition)?
            };
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
        self.upsert_partition_with_pk_type(database, name, batch, "_default")
    }

    pub fn upsert_partition_with_pk_type(
        &self,
        database: &str,
        name: &str,
        batch: InsertBatch,
        partition: &str,
    ) -> Result<InsertOutcome> {
        self.mutate(database, name, |collection| {
            let varchar = collection.schema().fields.iter().any(|field| {
                field.is_primary && field.data_type == crate::schema::DataType::VarChar
            });
            let pks = collection.upsert_partition_from_db(batch, partition)?;
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
        let key = self.key(database, name)?;
        let blocked = self.write_gate.read();
        if *blocked {
            return Err(storage(
                "database writes are blocked after an unconfirmed directory change",
            ));
        }
        let key = self.resolve(key)?;
        let name = key.name.as_str();
        let handle = self.collection_inner(&key)?;
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
                self.flush_handle(&handle, false)?;
                continue;
            }
            break if partition.is_empty() {
                collection.read_snapshot()
            } else {
                collection.read_named_snapshot(&[partition.into()])?
            };
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
                Ok((
                    collection.delete_partition_literals_from_db(&literals, partition)?,
                    varchar,
                ))
            });
        }
        let pks = snapshot.query_for_delete(&parsed)?;
        #[cfg(test)]
        AFTER_DELETE_QUERY.with(|hook| {
            if let Some(pause) = hook.borrow_mut().take() {
                pause();
            }
        });
        // 与 3.x 的 query → delete 相同：之后写入的同主键新版本也会被删除；不升级共享许可。
        self.mutate_handle(&handle, name, |collection| {
            collection.delete_partition_from_db(&pks, partition)?;
            Ok((pks, varchar))
        })
    }

    fn mutate<T>(
        &self,
        database: &str,
        name: &str,
        apply: impl FnOnce(&mut Collection) -> Result<T>,
    ) -> Result<T> {
        let key = self.key(database, name)?;
        let blocked = self.write_gate.read();
        if *blocked {
            return Err(EngineError::Storage(
                "database writes are blocked after an unconfirmed directory change".to_owned(),
            ));
        }
        let key = self.resolve(key)?;
        let handle = self.collection_inner(&key)?;
        self.mutate_handle(&handle, &key.name, apply)
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
                self.flush_handle(handle, false)?;
                continue;
            }
            break collection;
        };
        let outcome = match state_change(&mut collection, |collection| {
            let result = apply(collection)?;
            #[cfg(feature = "fault-injection")]
            state_panic(StatePanicPoint::Mutation);
            Ok(result)
        }) {
            Ok(outcome) => outcome,
            Err(error) => {
                let pending = collection
                    .persistent()
                    .is_some_and(|p| p.has_pending_generation());
                drop(collection);
                if pending && let Err(flush_error) = self.flush_handle(handle, false) {
                    eprintln!("WAL failure recovery flush of {name:?} failed: {flush_error}");
                }
                return Err(error);
            }
        };
        let auto_flush = collection
            .persistent()
            .is_some_and(|p| p.should_auto_flush());
        if auto_flush
            && let Err(error) = state_change(&mut collection, |collection| {
                collection
                    .persistent_mut()
                    .ok_or_else(|| storage("persistent state missing"))?
                    .freeze_active(false)
                    .map_err(storage)?;
                Ok(())
            })
        {
            if let Some(persistent) = collection.persistent_mut() {
                persistent.poison();
            }
            // 本次追加与内存应用已完成；后续冻结失败不能撤销已经生效的写入。
            eprintln!("automatic freeze of {name:?} failed after accepted mutation: {error}");
            return Ok(outcome);
        }
        drop(collection);
        if auto_flush {
            // 写入已经落 WAL 并在内存中可见；之后的提交故障由下一次写入先重试。
            if let Err(error) = self.flush_handle(handle, false) {
                eprintln!("automatic flush of {name:?} failed after accepted mutation: {error}");
            }
        }
        Ok(outcome)
    }

    pub fn flush_collection(&self, database: &str, name: &str) -> Result<()> {
        let key = self.key(database, name)?;
        let blocked = self.write_gate.read();
        if *blocked {
            return Err(storage(
                "database writes are blocked after an unconfirmed directory change",
            ));
        }
        let key = self.resolve(key)?;
        let handle = self.collection_inner(&key)?;
        self.flush_handle(&handle, true)
    }

    pub fn create_index(
        &self,
        database: &str,
        name: &str,
        field: &str,
        request: IndexRequest,
    ) -> Result<()> {
        self.change_index(database, name, |c| {
            c.prepare_create_index(field, request)
                .map(crate::index::IndexChange::Create)
        })
    }

    pub fn drop_index(
        &self,
        database: &str,
        name: &str,
        field: &str,
        index_name: &str,
    ) -> Result<()> {
        self.change_index(database, name, |c| {
            c.prepare_drop_index(field, index_name)
                .map(crate::index::IndexChange::Drop)
        })
    }

    fn change_index(
        &self,
        database: &str,
        name: &str,
        make_change: impl FnOnce(&Collection) -> Result<crate::index::IndexChange>,
    ) -> Result<()> {
        #[cfg(feature = "fault-injection")]
        use crate::storage::index_intent;
        use crate::storage::{flush, index_files, index_platform};
        let key = self.key(database, name)?;
        let blocked = self.write_gate.read();
        if *blocked {
            return Err(storage(
                "database writes are blocked after an unconfirmed directory change",
            ));
        }
        let key = self.resolve(key)?;
        let handle = self.collection_inner(&key)?;
        self.flush_handle(&handle, true)?;
        let lock = handle
            .read()
            .persistent()
            .ok_or_else(|| storage("persistent state missing"))?
            .flush_lock();
        let _flush = lock.lock();
        let (change, dir, candidate, inherited, mut intent) = {
            let collection = handle.read();
            let change = make_change(&collection)?;
            let p = collection
                .persistent()
                .ok_or_else(|| storage("persistent state missing"))?;
            (
                change.clone(),
                p.dir.clone(),
                p.index_manifest(&change).map_err(storage)?,
                p.inherited_synced(),
                p.index_intent.clone(),
            )
        };
        let mut prepared = None;
        let mut cleanup_needed = false;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            index_files::check_root(&dir).map_err(EngineError::invalid)?;
            intent.check(&dir).map_err(EngineError::invalid)?;
            let original = handle
                .read()
                .persistent()
                .ok_or_else(|| storage("persistent state missing"))?
                .manifest
                .clone();
            let files =
                index_files::preflight(&dir, &original, &change).map_err(EngineError::invalid)?;
            if !inherited {
                #[cfg(feature = "fault-injection")]
                index_intent::fault(index_intent::IndexPoint::InheritedSync).map_err(storage)?;
                flush::sync_inherited(&dir, &original).map_err(storage)?;
                handle
                    .write()
                    .persistent_mut()
                    .ok_or_else(|| storage("persistent state missing"))?
                    .mark_inherited_synced();
            }
            prepared = Some(fs_commit::prepare_manifest(&dir, &candidate).map_err(storage)?);
            let tmp = prepared
                .as_ref()
                .ok_or_else(|| storage("manifest preparation missing"))?
                .tmp_path();
            if !files.is_empty() {
                // 此预检保护后续不可回滚的侧车删除；无删除时沿用实际 manifest 替换的错误协议。
                index_platform::check_replace(&dir, &tmp, &dir.join("manifest.json"))
                    .map_err(EngineError::invalid)?;
                cleanup_needed = true;
                intent
                    .persist(
                        &dir,
                        change.operation(),
                        change
                            .specs()
                            .iter()
                            .map(|s| s.field_name.clone())
                            .collect(),
                        &files,
                    )
                    .map_err(storage)?;
                intent.delete(&dir, &files).map_err(storage)?;
            }
            #[cfg(feature = "fault-injection")]
            index_intent::before_commit().map_err(storage)?;
            state_change(&mut handle.write(), |c| {
                c.validate_index_change(&change)?;
                c.persistent_mut()
                    .ok_or_else(|| storage("persistent state missing"))?
                    .commit_index_manifest(
                        candidate.clone(),
                        prepared
                            .as_mut()
                            .ok_or_else(|| storage("manifest preparation missing"))?,
                    )
                    .map_err(storage)?;
                c.publish_index_change(change.clone());
                #[cfg(feature = "fault-injection")]
                state_panic(StatePanicPoint::Index);
                Ok(())
            })
        }));
        let mut result = match result {
            Ok(result) => result,
            Err(payload) => Err(storage(format!(
                "index operation panicked: {}",
                crate::error::panic_description(payload.as_ref())
            ))),
        };
        if let Some(prepared) = prepared.as_mut()
            && let Err(error) = prepared.cleanup()
        {
            result = match result {
                Err(original) => Err(index_cleanup_error(
                    original,
                    format!("manifest tmp cleanup failed: {error}"),
                )),
                Ok(()) => {
                    eprintln!("manifest tmp cleanup failed at {dir:?}: {error}");
                    Ok(())
                }
            };
        }
        if cleanup_needed && let Err(error) = intent.cleanup(&dir) {
            result = match result {
                Err(original) => Err(index_cleanup_error(original, error)),
                Ok(()) => {
                    eprintln!("{error}");
                    Ok(())
                }
            };
        }
        handle
            .write()
            .persistent_mut()
            .ok_or_else(|| storage("persistent state missing"))?
            .index_intent = intent;
        result
    }

    fn flush_handle(&self, handle: &CollectionHandle, include_active: bool) -> Result<()> {
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
                state_change(&mut collection, |collection| {
                    collection
                        .persistent_mut()
                        .ok_or_else(|| storage("persistent state missing"))?
                        .recommit()
                        .map_err(storage)?;
                    Ok(())
                })?;
            }
            if include_active {
                state_change(&mut collection, |collection| {
                    collection
                        .persistent_mut()
                        .ok_or_else(|| storage("persistent state missing"))?
                        .freeze_active(false)
                        .map_err(storage)?;
                    Ok(())
                })?;
            }
        }
        self.cleanup_partition_files(handle)?;
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
                    state_change(&mut collection, |collection| {
                        if let Some(partitioned) = outcome.partitioned {
                            collection.publish_partitioned_generation(
                                &work,
                                outcome.manifest,
                                partitioned,
                                retry_manifest,
                            )?;
                        } else {
                            collection.publish_generation(
                                &work,
                                outcome.manifest,
                                &outcome.mapping,
                                outcome.data,
                                outcome.norms,
                                retry_manifest,
                            )?;
                        }
                        #[cfg(feature = "fault-injection")]
                        state_panic(StatePanicPoint::Publish);
                        Ok(())
                    })?;
                    let persistent = collection
                        .persistent_mut()
                        .ok_or_else(|| storage("persistent state missing"))?;
                    if let Some(runtime::PostCommitError::Durability(reason)) =
                        &outcome.post_commit_error
                    {
                        persistent.record_failure(reason.clone());
                    }
                    drop(collection);
                    if work.table.size() > 0 {
                        self.schedule_maintenance(handle);
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

    fn schedule_maintenance(&self, handle: &CollectionHandle) {
        if let Err(error) = self.maintenance.enqueue(handle)
            && !matches!(error, EngineError::Closed)
        {
            eprintln!("cannot schedule maintenance: {error}");
        }
    }

    fn maintenance_cancelled(&self) -> bool {
        self.maintenance.abort.load(Ordering::Acquire)
            || self.namespace_waiters.load(Ordering::Acquire) != 0
    }

    pub(crate) fn compact_handle(&self, handle: &CollectionHandle) -> Result<usize> {
        let phase = std::cell::Cell::new(fs_commit::CommitPhase::BeforeReplace);
        let cleanup = std::cell::Cell::new(false);
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.compact_transaction(handle, &phase, &cleanup)
        }))
        .unwrap_or_else(|payload| {
            Err(storage(format!(
                "compaction panicked ({:?}, cleanup={}): {}",
                phase.get(),
                cleanup.get(),
                crate::error::panic_description(payload.as_ref())
            )))
        })
    }

    fn compact_transaction(
        &self,
        handle: &CollectionHandle,
        phase: &std::cell::Cell<fs_commit::CommitPhase>,
        cleanup: &std::cell::Cell<bool>,
    ) -> Result<usize> {
        use crate::storage::{compaction, flush};
        if self.maintenance_cancelled() {
            return Err(EngineError::Closed);
        }
        let _task = {
            let mut gate = self.gate.lock();
            if gate.data_dir.is_none() || self.maintenance.abort.load(Ordering::Acquire) {
                return Err(EngineError::Closed);
            }
            gate.in_flight += 1;
            InFlight { db: self }
        };
        let blocked = self.write_gate.read();
        if *blocked {
            return Err(storage("database directory durability is unconfirmed"));
        }
        if self.maintenance_cancelled() {
            return Err(EngineError::Closed);
        }
        let lock = handle
            .read()
            .persistent()
            .ok_or_else(|| storage("persistent state missing"))?
            .flush_lock();
        let view = {
            let _flush = lock.lock();
            if !self.prepare_maintenance_boundary(handle, false)? {
                return Ok(0);
            }
            handle.read().compaction_view()?
        };
        let Some(view) = view else {
            return Ok(0);
        };
        let inputs = compaction::select(&view.dir, view.inputs.clone())?;
        if inputs.is_empty() {
            return Ok(0);
        }
        compaction::reject_external_sequence_ties(
            &view.inputs,
            &inputs,
            view.schema
                .primary_position()
                .ok_or_else(|| storage("compaction schema has no primary key"))?,
        )?;
        {
            let _flush = lock.lock();
            if !self.prepare_maintenance_boundary(handle, true)? {
                return Ok(0);
            }
        }
        #[cfg(feature = "fault-injection")]
        self.compaction_pause(crate::CompactionPausePoint::BeforePrepare)?;
        #[cfg(feature = "fault-injection")]
        let fault = self.maintenance.fault.lock().take();
        #[cfg(feature = "fault-injection")]
        match fault {
            Some(crate::CompactionFault::PreparePanic) => {
                panic!("injected compaction preparation panic")
            }
            Some(crate::CompactionFault::BeforeReplacePanic) => {
                fs_commit::inject_manifest_panic_once(fs_commit::CommitPhase::BeforeReplace)
            }
            Some(crate::CompactionFault::AfterReplacePanic) => {
                fs_commit::inject_manifest_panic_once(fs_commit::CommitPhase::AfterReplace)
            }
            Some(crate::CompactionFault::DirectorySyncIo) => {
                fs_commit::inject_manifest_dir_sync_failure_once()
            }
            Some(crate::CompactionFault::ExitBeforeReplace) => {
                fs_commit::inject_manifest_process_exit_once(fs_commit::CommitPhase::BeforeReplace)
            }
            Some(crate::CompactionFault::ExitAfterReplace) => {
                fs_commit::inject_manifest_process_exit_once(fs_commit::CommitPhase::AfterReplace)
            }
            _ => {}
        }
        let deletes = handle.read().compaction_deletes(&inputs)?;
        if view.sync_inherited {
            flush::sync_inherited(&view.dir, &view.manifest).map_err(storage)?;
        }
        let mut prepared = compaction::prepare(&view.dir, &view.schema, inputs, &deletes, || {
            self.maintenance_cancelled()
        })?;
        #[cfg(feature = "fault-injection")]
        self.compaction_pause(crate::CompactionPausePoint::BeforeCommit)?;
        if self.maintenance_cancelled() {
            return Err(EngineError::Closed);
        }
        let _flush = lock.lock();
        if !self.prepare_maintenance_boundary(handle, true)? {
            return Err(storage("compaction deferred until foreground flush retry"));
        }
        let manifest = {
            let collection = handle.read();
            if collection.is_deleted() {
                return Err(EngineError::Closed);
            }
            let p = collection
                .persistent()
                .ok_or_else(|| storage("persistent state missing"))?;
            if !p.can_compact().map_err(storage)? {
                return Err(storage("compaction state changed before commit"));
            }
            collection.validate_compaction(&prepared)?;
            p.compaction_sources(prepared.outputs.len())
                .map_err(storage)?;
            p.manifest.clone()
        };
        if self.maintenance_cancelled() {
            return Err(EngineError::Closed);
        }
        let submission = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            compaction::promote(&view.dir, &mut prepared, &view.pins)?;
            let (candidate, threshold, obsolete) =
                compaction::candidate(&manifest, &prepared, &view.pins)?;
            let durability_error = match fs_commit::commit_manifest(&view.dir, &candidate) {
                Ok(_) => {
                    phase.set(fs_commit::CommitPhase::AfterReplace);
                    None
                }
                Err(error) if error.phase == fs_commit::CommitPhase::AfterReplace => {
                    phase.set(fs_commit::CommitPhase::AfterReplace);
                    Some(error.to_string())
                }
                Err(error) => return Err(storage(error)),
            };
            Ok((candidate, threshold, obsolete, durability_error))
        }))
        .unwrap_or_else(|payload| {
            Err(storage(format!(
                "compaction submission panicked ({:?}): {}",
                phase.get(),
                crate::error::panic_description(payload.as_ref())
            )))
        });
        if submission.is_err() && phase.get() == fs_commit::CommitPhase::BeforeReplace {
            // 保留到下一次持 flush 锁的清理；最终名可能被前台 flush 复用，不能在 Drop 中删除。
            handle
                .write()
                .persistent_mut()
                .ok_or_else(|| storage("persistent state missing"))?
                .compaction_cleanup
                .extend(prepared.outputs.iter().filter_map(|output| {
                    output
                        .relative
                        .as_ref()
                        .map(|name| PathBuf::from(&prepared.partition).join(name))
                }));
        }
        let (candidate, threshold, obsolete, durability_error) = submission?;
        #[cfg(feature = "fault-injection")]
        if let Err(error) = self.compaction_pause(crate::CompactionPausePoint::AfterReplace) {
            if let Some(p) = handle.write().persistent_mut() {
                p.poison();
            }
            return Err(storage(format!(
                "compaction pause failed after replace: {error}"
            )));
        }
        let requested = self.maintenance.diagnostics.then(std::time::Instant::now);
        let mut collection = handle.write();
        let acquired = requested.map(|_| std::time::Instant::now());
        let publication = state_change(&mut collection, |c| {
            #[cfg(feature = "fault-injection")]
            if matches!(fault, Some(crate::CompactionFault::PublishPanic)) {
                panic!("injected compaction publication panic");
            }
            c.publish_compaction(
                &mut prepared,
                candidate,
                threshold,
                durability_error.clone(),
            )
        });
        if let Err(error) = publication {
            if let Some(p) = collection.persistent_mut() {
                p.poison();
            }
            return Err(error);
        }
        collection
            .persistent_mut()
            .ok_or_else(|| storage("persistent state missing"))?
            .compaction_cleanup
            .extend(obsolete);
        let held = acquired.map(|start| start.elapsed());
        drop(collection);
        if let (Some(requested), Some(acquired), Some(held)) = (requested, acquired, held) {
            eprintln!(
                "compaction publication: input_rows={} wait_seconds={:.6} lock_seconds={:.6}",
                prepared.input_rows,
                acquired.duration_since(requested).as_secs_f64(),
                held.as_secs_f64(),
            );
        }
        if let Some(reason) = durability_error {
            return Err(storage(format!(
                "compaction manifest replaced but durability unconfirmed: {reason}"
            )));
        }
        cleanup.set(true);
        #[cfg(feature = "fault-injection")]
        if matches!(fault, Some(crate::CompactionFault::CleanupPanic)) {
            panic!("injected compaction cleanup panic");
        }
        self.cleanup_compaction_files(handle)?;
        Ok(prepared.input_rows)
    }

    fn prepare_maintenance_boundary(
        &self,
        handle: &CollectionHandle,
        cleanup_wal: bool,
    ) -> Result<bool> {
        let (dir, limit) = {
            let collection = handle.read();
            if collection.is_deleted() {
                return Err(EngineError::Closed);
            }
            let p = collection
                .persistent()
                .ok_or_else(|| storage("persistent state missing"))?;
            if p.poisoned() {
                return Err(storage(
                    "collection state is unusable after a storage failure",
                ));
            }
            // 后台维护不能绕过前台重试节流，也不能抢先消耗前台故障注入/重新提交边界。
            if p.needs_retry() {
                return Ok(false);
            }
            let p = collection
                .persistent()
                .ok_or_else(|| storage("persistent state missing"))?;
            (p.dir.clone(), p.committed_wal_limit())
        };
        if cleanup_wal && let Some(limit) = limit {
            crate::storage::flush::cleanup_wal(&dir.join("wal"), limit).map_err(storage)?;
        }
        self.cleanup_compaction_files(handle)?;
        self.cleanup_partition_files(handle)?;
        Ok(true)
    }

    fn cleanup_compaction_files(&self, handle: &CollectionHandle) -> Result<()> {
        let (dir, pins, obsolete) = {
            let c = handle.read();
            let p = c
                .persistent()
                .ok_or_else(|| storage("persistent state missing"))?;
            if p.retry_manifest() || p.poisoned() {
                return Ok(());
            }
            let obsolete: Vec<_> = p
                .compaction_cleanup
                .iter()
                .filter(|path| {
                    let mut parts = path.components();
                    let Some(std::path::Component::Normal(name)) = parts.next() else {
                        return true;
                    };
                    let relative = parts.as_path().to_str();
                    !name
                        .to_str()
                        .and_then(|name| p.manifest.partitions.get(name))
                        .is_some_and(|files| {
                            files
                                .data_files
                                .iter()
                                .chain(&files.delta_files)
                                .any(|f| Some(f.as_str()) == relative)
                        })
                })
                .cloned()
                .collect();
            (p.dir.clone(), p.pinned_files.clone(), obsolete)
        };
        if obsolete.is_empty() {
            handle
                .write()
                .persistent_mut()
                .ok_or_else(|| storage("persistent state missing"))?
                .compaction_cleanup
                .clear();
            return Ok(());
        }
        let finished = crate::storage::compaction::cleanup(&dir, &obsolete, &pins, || {
            self.maintenance_cancelled() || handle.read().persistent().is_some_and(|p| p.poisoned())
        })?;
        if finished {
            handle
                .write()
                .persistent_mut()
                .ok_or_else(|| storage("persistent state missing"))?
                .compaction_cleanup
                .clear();
        }
        Ok(())
    }

    #[cfg(feature = "fault-injection")]
    pub fn wait_for_maintenance(
        &self,
        database: &str,
        name: &str,
        timeout: std::time::Duration,
    ) -> Result<crate::MaintenanceReport> {
        let key = self.key(database, name)?;
        validate_filesystem_name(name, "collection")?;
        let cached = self.collections.read().get(&key).cloned();
        let handle = match cached {
            Some(handle) => handle,
            None => self.collection(database, name)?,
        };
        let status = handle
            .read()
            .persistent()
            .ok_or_else(|| storage("persistent state missing"))?
            .maintenance_status
            .clone();
        status.wait(timeout)
    }

    #[cfg(feature = "fault-injection")]
    pub fn pause_compaction_before_commit_once(
        &self,
        entered: std::sync::mpsc::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) {
        self.pause_compaction_once(crate::CompactionPausePoint::BeforeCommit, entered, resume);
    }

    #[cfg(feature = "fault-injection")]
    pub fn pause_compaction_once(
        &self,
        point: crate::CompactionPausePoint,
        entered: std::sync::mpsc::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) {
        *self.maintenance.pause.lock() = Some((point, entered, resume));
    }

    #[cfg(feature = "fault-injection")]
    fn compaction_pause(&self, point: crate::CompactionPausePoint) -> Result<()> {
        let pause = {
            let mut slot = self.maintenance.pause.lock();
            if slot.as_ref().is_some_and(|(wanted, _, _)| *wanted == point) {
                slot.take()
            } else {
                None
            }
        };
        if let Some((_, entered, resume)) = pause {
            entered.send(()).map_err(storage)?;
            resume
                .recv_timeout(std::time::Duration::from_secs(30))
                .map_err(storage)?;
        }
        Ok(())
    }

    #[cfg(feature = "fault-injection")]
    pub fn inject_compaction_fault_once(&self, fault: crate::CompactionFault) {
        *self.maintenance.fault.lock() = Some(fault);
    }

    #[cfg(feature = "fault-injection")]
    pub fn pending_namespace_changes(&self) -> usize {
        self.namespace_waiters.load(Ordering::Acquire)
    }

    #[cfg(feature = "fault-injection")]
    pub fn trace_next_maintenance(&self, trace: std::sync::mpsc::Sender<Vec<fs_commit::FsEvent>>) {
        *self.maintenance.trace.lock() = Some(trace);
    }

    fn name_lease(&self, key: &CollectionKey) -> NameLease<'_> {
        let mut registry = self.name_locks.lock();
        let lock = match registry.get(key).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(NameMutex::new(()));
                registry.insert(key.clone(), Arc::downgrade(&lock));
                lock
            }
        };
        NameLease {
            registry: &self.name_locks,
            key: key.clone(),
            lock: Some(lock),
        }
    }

    fn remember_open_failure(&self, key: &CollectionKey, error: EngineError) -> EngineError {
        if let Some(failure) = OpenFailure::from_error(&error) {
            self.open_failures.write().insert(key.clone(), failure);
        }
        error
    }

    fn opening_error(&self, key: &CollectionKey, error: recovery::RecoveryError) -> EngineError {
        let cache = cacheable_recovery_error(&error);
        let error = recovery_error(error);
        if cache {
            self.remember_open_failure(key, error)
        } else {
            error
        }
    }

    fn collection_inner(&self, key: &CollectionKey) -> Result<CollectionHandle> {
        let name = key.name.as_str();
        validate_filesystem_name(name, "collection")?;
        if let Some(handle) = self.collections.read().get(key) {
            return Ok(Arc::clone(handle));
        }
        let lease = self.name_lease(key);
        let _name = lease.mutex().lock();
        if let Some(handle) = self.collections.read().get(key) {
            return Ok(Arc::clone(handle));
        }
        if !self.exact_collection_name(key)? {
            return Err(EngineError::CollectionNotFound(name.to_owned()));
        }
        let path = self.database(&key.database)?.collection_path(name);
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
        if let Some(failure) = self.open_failures.read().get(key) {
            return Err(failure.error());
        }
        // 不区分大小写的文件系统上只接受目录项的实际名字。
        let recovered =
            recovery::load_committed(&path).map_err(|error| self.opening_error(key, error))?;
        for warning in &recovered.wal_warnings {
            eprintln!("collection {name:?} recovered only a validated WAL prefix: {warning}");
        }
        let manifest = recovered.manifest.clone();
        let pinned_files = recovered.pinned_files.clone();
        let collection = Collection::from_recovered(name, recovered, path.clone())
            .map_err(|error| self.remember_open_failure(key, error))?;
        #[cfg(feature = "fault-injection")]
        OPEN_CLEANUP_PAUSE.with(|slot| -> Result<()> {
            if let Some((entered, resume)) = slot.borrow_mut().take() {
                entered.send(()).map_err(storage)?;
                resume.recv().map_err(storage)?;
            }
            Ok(())
        })?;
        for warning in recovery::cleanup_unreferenced(&path, &manifest, &pinned_files)
            .map_err(|error| self.opening_error(key, error))?
        {
            eprintln!("collection {name:?} orphan cleanup deferred: {warning}");
        }
        let handle = Arc::new(RwLock::new(collection));
        self.collections
            .write()
            .insert(key.clone(), Arc::clone(&handle));
        drop(_name);
        drop(lease);
        self.schedule_maintenance(&handle);
        Ok(handle)
    }

    /// 全部 collection 的名字与 load 状态。先在表读锁内取出句柄，放开表锁后再读各自的状态：
    /// 等待 collection 锁时若仍持有表锁，排队的写者会让所有 RPC 都停在表锁上。
    /// 取出之后被并发 drop 的 collection 仍会列出，不会报错。
    pub fn list_collections(&self, database: &str) -> Result<Vec<(String, LoadState)>> {
        let scope = self.key(database, "")?;
        let root = self.database(&scope.database)?.collections_dir();
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
            let key = CollectionKey {
                name: entry.file_name().to_string_lossy().into_owned(),
                ..scope.clone()
            };
            let cached = self.collections.read().get(&key).cloned();
            let state = if let Some(handle) = cached {
                let collection = handle.read();
                if collection.is_deleted() || collection.persistent().is_some_and(|p| p.poisoned())
                {
                    LoadState::Released
                } else {
                    collection.load_state()
                }
            } else {
                let failed = self.open_failures.read().contains_key(&key);
                if failed {
                    LoadState::Released
                } else {
                    unopened_load_state(&entry.path())
                }
            };
            result.push((key.name, state));
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }

    /// 3.x `alter_collection_properties`：先删 `delete_keys` 再设 `properties`（新键追加、已有键原位替换），
    /// 值为字符串。持独占许可（0023 A「对合同方向的细化」）；集合须能按本实现打开，范围外 schema 返回
    /// Unsupported。请求中的 `timezone` 由 schema 校验拒绝，因而先判集合存在、零写入（取舍 1）。
    pub fn alter_collection(
        &self,
        database: &str,
        name: &str,
        properties: &[(String, String)],
        delete_keys: &[String],
    ) -> Result<()> {
        let key = self.key(database, name)?;
        let _intent = NamespaceIntent::new(&self.namespace_waiters);
        let mut blocked = self.write_gate.write();
        if *blocked {
            return Err(storage(
                "database writes are blocked after an unconfirmed directory change",
            ));
        }
        let key = self.resolve(key)?;
        let handle = checked_read_handle(self.collection_inner(&key)?)?;
        let lock = handle
            .read()
            .persistent()
            .ok_or_else(|| storage("persistent state missing"))?
            .flush_lock();
        let _flush = lock.lock();
        let (dir, mut schema) = {
            let collection = handle.read();
            let p = collection
                .persistent()
                .ok_or_else(|| storage("persistent state missing"))?;
            (p.dir.clone(), collection.schema().clone())
        };
        schema
            .properties
            .retain(|(key, _)| !delete_keys.contains(key));
        for (key, value) in properties {
            match schema.properties.iter_mut().find(|(k, _)| k == key) {
                Some(entry) => entry.1.clone_from(value),
                None => schema.properties.push((key.clone(), value.clone())),
            }
        }
        schema.validate()?;
        let current = fs::read(dir.join("schema.json")).map_err(storage)?;
        let bytes = SchemaFile::with_properties(&current, &key.name, &schema.properties)
            .map_err(storage)?;
        // 已是规范字节时不写；只差键序（0.3.0rc3 写出的文件）或名字时照常重写。
        if bytes != current {
            let fault = |point| {
                #[cfg(feature = "fault-injection")]
                directory_fault(
                    DirectoryOperation::Alter,
                    match point {
                        fs_commit::ReplacePoint::BeforeRename => DirectoryPoint::BeforeRename,
                        fs_commit::ReplacePoint::AfterRename => DirectoryPoint::AfterRename,
                    },
                )?;
                #[cfg(not(feature = "fault-injection"))]
                let _point: fs_commit::ReplacePoint = point;
                Ok(())
            };
            match fs_commit::replace_file(&dir, "schema.json", &bytes, fault) {
                Ok(()) => {}
                Err(fs_commit::ReplaceError::Unchanged(error)) => return Err(storage(error)),
                Err(fs_commit::ReplaceError::Unconfirmed(error)) => {
                    handle.write().set_properties(schema.properties);
                    *blocked = true;
                    return Err(storage(format!(
                        "collection properties were changed, but schema.json durability is unconfirmed: {error}"
                    )));
                }
            }
        }
        handle.write().set_properties(schema.properties);
        Ok(())
    }

    /// 0023 A「改名」：P0 检查 → P1a 同步旧句柄的 WAL → P1b 意图与目录改名 → P2 退役旧名 → P3 改写
    /// `schema.json` 的名字与别名 → P4 删除意图。判定顺序同 3.x：解析旧名（可以是别名）→ 旧名存在 → 新名合法 →
    /// 新名冲突；`new_db` 按取舍 4 在旧名判定之前拒绝。持独占许可，因而与 create、drop 及别名写入不交错。
    pub fn rename_collection(
        &self,
        database: &str,
        old: &str,
        new: &str,
        new_db: &str,
    ) -> Result<()> {
        let key = self.key(database, old)?;
        let _intent = NamespaceIntent::new(&self.namespace_waiters);
        let mut blocked = self.write_gate.write();
        if *blocked {
            return Err(storage(
                "database writes are blocked after an unconfirmed directory change",
            ));
        }
        let state = self.database(&key.database)?;
        self.settle_pending_rename(state, &mut blocked)?;
        if !new_db.is_empty() && new_db != key.database {
            return Err(EngineError::unsupported(
                "cross-database rename",
                format!(
                    "renaming a collection into database {} is not supported",
                    py_repr(new_db)
                ),
            ));
        }
        let _namespace = self.namespace.lock();
        let old_key = self.resolve(key)?;
        // 与 3.x 的 `has_collection` 相同，非法名字视为不存在。
        if !self.alias_target_exists(state, &old_key.name)? {
            return Err(EngineError::CollectionNotFound(old_key.name));
        }
        validate_filesystem_name(new, "collection")?;
        let old_dir = state.collection_path(&old_key.name);
        let new_dir = state.collection_path(new);
        // 新名路径存在即「已存在」：含没有 `schema.json` 的残留目录与只差大小写的名字（0023 有意的差异），不覆盖任何目录。
        let alias_taken = {
            let mut cache = state.aliases.lock();
            alias::load(&mut cache, &state.root)?.contains_key(new)
        };
        if alias_taken || path_exists(&new_dir)? {
            return Err(EngineError::CollectionAlreadyExists(new.to_owned()));
        }
        let new_key = CollectionKey {
            database: old_key.database.clone(),
            name: new.to_owned(),
        };
        let leases = [self.name_lease(&old_key), self.name_lease(&new_key)];
        let _names = NameLease::lock_pair(&leases[0], &leases[1]);
        let current = fs::read(old_dir.join("schema.json")).map_err(storage)?;
        SchemaFile::with_name(&current, new).map_err(storage)?;

        // P1a：同 drop，标记已移除后同步全部 WAL；之后的维护在提交边界看到标记即放弃。
        let handle = self.collections.read().get(&old_key).cloned();
        let flush_lock = handle
            .as_ref()
            .and_then(|h| h.read().persistent().map(|p| p.flush_lock()));
        let _flush = flush_lock.as_ref().map(|lock| lock.lock());
        if let Some(handle) = &handle {
            state_change(&mut handle.write(), |collection| {
                collection.mark_deleted(true);
                if let Some(persistent) = collection.persistent_mut()
                    && let Err(error) = persistent.sync_retained_wal()
                {
                    collection.mark_deleted(false);
                    return Err(storage(error));
                }
                Ok(())
            })?;
        }

        // P1b
        let intent = RenameIntent {
            old: old_key.name.clone(),
            new: new.to_owned(),
        };
        let written = rename_intent::write(
            &state.root,
            &intent,
            rename_replace_fault(RenameStep::Intent, RenameStep::IntentAfterRename),
        );
        if let Err(
            fs_commit::ReplaceError::Unchanged(error) | fs_commit::ReplaceError::Unconfirmed(error),
        ) = written
        {
            return Err(self.abort_rename(
                state,
                handle.as_ref(),
                format!("cannot write rename intent: {error}"),
            ));
        }
        let moved = guarded_io(|| {
            rename_fault(RenameStep::Directory)?;
            fs::rename(&old_dir, &new_dir)
        });
        if let Err(error) = moved {
            return Err(self.abort_rename(state, handle.as_ref(), error.to_string()));
        }
        let mismatch = match guarded_io(|| rename_fault(RenameStep::NameVerification)) {
            Err(_) => Some(format!(
                "renamed collection name {new:?} was normalized by the filesystem"
            )),
            Ok(()) => match self.exact_name_in(state, new) {
                Ok(true) => None,
                Ok(false) => Some(format!(
                    "renamed collection name {new:?} was normalized by the filesystem"
                )),
                Err(error) => Some(error.to_string()),
            },
        };
        let collections = state.collections_dir();
        if let Some(reason) = mismatch {
            let rollback = guarded_io(|| {
                rename_fault(RenameStep::Rollback)?;
                fs::rename(&new_dir, &old_dir)?;
                fs_commit::sync_dir(&collections)
            });
            if let Err(error) = rollback {
                *blocked = true;
                return Err(storage(format!(
                    "collection name verification failed ({reason}); rollback durability is unconfirmed: {error}"
                )));
            }
            return Err(self.abort_rename(state, handle.as_ref(), reason));
        }

        // P2：核对通过后立即退役旧名，新名在第一次访问时按目录重新打开（有索引定义即 released，同 3.x）。
        {
            let mut table = self.collections.write();
            table.remove(&old_key);
            table.remove(&new_key);
        }
        {
            let mut failures = self.open_failures.write();
            failures.remove(&old_key);
            failures.remove(&new_key);
        }
        let synced = guarded_io(|| {
            rename_fault(RenameStep::CollectionsSync)?;
            fs_commit::sync_dir(&collections)
        });
        if let Err(error) = synced {
            *blocked = true;
            return Err(storage(format!(
                "collection was renamed, but directory durability is unconfirmed: {error}"
            )));
        }
        rename_pause(RenamePause::Retired)?;

        // P3、P4
        match self.finish_rename(state, &intent) {
            Ok(()) => Ok(()),
            Err(RenameFailure::Pending(reason)) => {
                *state.rename_pending.lock() = Some(reason.clone());
                Err(storage(format!(
                    "collection was renamed, but metadata maintenance is pending: {reason}"
                )))
            }
            Err(RenameFailure::Unconfirmed(reason)) => {
                *blocked = true;
                Err(storage(format!(
                    "collection was renamed, but metadata durability is unconfirmed: {reason}"
                )))
            }
        }
    }

    /// P1b 失败：删除意图（不存在也同步根目录），撤销已移除标记并让旧句柄进入编号更大的新代、恢复可写
    /// （WAL 写者已在 P1a 关闭，同 drop 目录改名失败）。只有删除意图失败时进入改名未完成状态。
    fn abort_rename(
        &self,
        state: &DatabaseState,
        handle: Option<&CollectionHandle>,
        reason: String,
    ) -> EngineError {
        let mut reason = reason;
        if let Err(error) = rename_intent::remove(
            &state.root,
            rename_replace_fault(RenameStep::IntentRemove, RenameStep::IntentRemoveAfter),
        ) {
            *state.rename_pending.lock() = Some(format!("cannot remove rename intent: {error}"));
            reason = format!("{reason}; rename intent cleanup is pending: {error}");
        }
        if let Some(handle) = handle {
            let reopened = state_change(&mut handle.write(), |collection| {
                collection.mark_deleted(false);
                if let Some(persistent) = collection.persistent_mut() {
                    persistent.freeze_active(true).map_err(storage)?;
                }
                Ok(())
            });
            if let Err(error) = reopened {
                reason = format!("{reason}; {error}");
            }
        }
        storage(reason)
    }

    /// P3、P4：把新目录 `schema.json` 的 `collection_name` 改为新名、把指向旧名的别名改指新名并写出
    /// `aliases.json`（3.x 改名总会写出它，没有别名时为 `{}`）、删除意图。每一步先比较规范字节，已完成则只
    /// 同步所在目录，确认上一次未确认的替换；因此当前实例的重试与重开时的收尾都可重入。
    fn finish_rename(
        &self,
        state: &DatabaseState,
        intent: &RenameIntent,
    ) -> std::result::Result<(), RenameFailure> {
        let pending = |error: &dyn std::fmt::Display| RenameFailure::Pending(error.to_string());
        let replaced = |result: std::result::Result<(), fs_commit::ReplaceError>| match result {
            Ok(()) => Ok(()),
            Err(fs_commit::ReplaceError::Unchanged(error)) => Err(pending(&error)),
            Err(fs_commit::ReplaceError::Unconfirmed(error)) => {
                Err(RenameFailure::Unconfirmed(error.to_string()))
            }
        };
        let dir = state.collection_path(&intent.new);
        let current = fs::read(dir.join("schema.json")).map_err(|e| pending(&e))?;
        let bytes = SchemaFile::with_name(&current, &intent.new).map_err(|e| pending(&e))?;
        if bytes == current {
            fs_commit::sync_dir(&dir).map_err(|e| pending(&e))?;
        } else {
            replaced(fs_commit::replace_file(
                &dir,
                "schema.json",
                &bytes,
                rename_replace_fault(RenameStep::Schema, RenameStep::SchemaAfterRename),
            ))?;
        }
        rename_pause(RenamePause::SchemaReplaced).map_err(|e| pending(&e))?;
        {
            let mut cache = state.aliases.lock();
            let aliases = alias::load(&mut cache, &state.root).map_err(|e| pending(&e))?;
            let mut next = aliases.clone();
            for target in next.values_mut() {
                if *target == intent.old {
                    intent.new.clone_into(target);
                }
            }
            let on_disk = match fs::read(state.root.join("aliases.json")) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(pending(&error)),
            };
            if on_disk.as_deref() != Some(alias::to_bytes(&next).as_slice()) {
                let result = alias::write(
                    &state.root,
                    &next,
                    rename_replace_fault(RenameStep::Aliases, RenameStep::AliasesAfterRename),
                );
                if !matches!(result, Err(fs_commit::ReplaceError::Unchanged(_))) {
                    *aliases = next;
                }
                replaced(result)?;
            }
        }
        rename_intent::remove(
            &state.root,
            rename_replace_fault(RenameStep::IntentRemove, RenameStep::IntentRemoveAfter),
        )
        .map_err(|e| pending(&e))
    }

    /// 重开时与「改名未完成」状态下按磁盘上的意图收尾（0023 A「改名」的重开判定）：先同步 `collections/`，
    /// 再按精确名字且带 `schema.json` 判定新旧名字。`retry` 时意图已不存在也走一次删除（同步数据库根目录），
    /// 确认上一次未确认的删除。
    fn settle_rename(
        &self,
        state: &DatabaseState,
        retry: bool,
    ) -> std::result::Result<(), RenameFailure> {
        let pending = |error: &dyn std::fmt::Display| RenameFailure::Pending(error.to_string());
        let manual = |reason: &dyn std::fmt::Display| {
            RenameFailure::Pending(format!(
                "{reason}; stop the service, check the collections directory and aliases.json, then remove {:?} and reopen",
                state.root.join(rename_intent::INTENT_FILE)
            ))
        };
        let intent = match rename_intent::read(&state.root) {
            Ok(Some(intent)) => intent,
            Ok(None) if retry => {
                return rename_intent::remove(
                    &state.root,
                    rename_replace_fault(RenameStep::IntentRemove, RenameStep::IntentRemoveAfter),
                )
                .map_err(|e| pending(&e));
            }
            Ok(None) => return Ok(()),
            Err(error @ rename_intent::ReadError::Io(_)) => return Err(pending(&error)),
            Err(error @ rename_intent::ReadError::Invalid(_)) => return Err(manual(&error)),
        };
        let collections = state.collections_dir();
        let present = match fs::symlink_metadata(&collections) {
            Ok(meta) if meta.file_type().is_dir() => true,
            Ok(_) => return Err(pending(&"collections path is not a directory")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(pending(&error)),
        };
        if present {
            guarded_io(|| {
                rename_fault(RenameStep::CollectionsSync)?;
                fs_commit::sync_dir(&collections)
            })
            .map_err(|e| pending(&e))?;
        }
        let exists = |name: &str| {
            self.alias_target_exists(state, name)
                .map_err(|e| pending(&e))
        };
        match (exists(&intent.old)?, exists(&intent.new)?) {
            (false, true) => return self.finish_rename(state, &intent),
            (false, false) => {
                let old_path =
                    path_exists(&state.collection_path(&intent.old)).map_err(|e| pending(&e))?;
                let new_path =
                    path_exists(&state.collection_path(&intent.new)).map_err(|e| pending(&e))?;
                if old_path || new_path {
                    return Err(manual(&format!(
                        "neither {} nor {} exists with that exact name, but a matching path does",
                        py_repr(&intent.old),
                        py_repr(&intent.new)
                    )));
                }
            }
            // 目录改名之前退出，或之后又被其他写入者改动：不改别名与 schema。
            (true, _) => {}
        }
        rename_intent::remove(
            &state.root,
            rename_replace_fault(RenameStep::IntentRemove, RenameStep::IntentRemoveAfter),
        )
        .map_err(|e| pending(&e))
    }

    /// 受限操作（create、drop、改名、别名写入）在独占许可下调用：改名未完成时先重试收尾，仍失败则拒绝。
    fn settle_pending_rename(&self, state: &DatabaseState, blocked: &mut bool) -> Result<()> {
        if state.rename_pending.lock().is_none() {
            return Ok(());
        }
        match self.settle_rename(state, true) {
            Ok(()) => {
                *state.rename_pending.lock() = None;
                Ok(())
            }
            Err(failure) => {
                if matches!(failure, RenameFailure::Unconfirmed(_)) {
                    *blocked = true;
                }
                let reason = failure.reason();
                *state.rename_pending.lock() = Some(reason.clone());
                Err(storage(format!(
                    "a collection rename is unfinished: {reason}"
                )))
            }
        }
    }

    /// 3.x 的判定顺序：解析目标（可以是别名）→ 别名名字 → 目标存在 → 与集合冲突 → 别名已存在。
    pub fn create_alias(&self, database: &str, collection: &str, alias: &str) -> Result<()> {
        self.change_aliases(database, |state, aliases| {
            let target = aliases.get(collection).map_or(collection, String::as_str);
            validate_filesystem_name(alias, "collection")?;
            if !self.alias_target_exists(state, target)? {
                return Err(EngineError::CollectionNotFound(target.to_owned()));
            }
            // 与 3.x 相同按路径判定：只差大小写的已有集合构成冲突，没有 schema.json 的残留目录不构成冲突。
            if state
                .collection_path(alias)
                .join("schema.json")
                .try_exists()
                .map_err(storage)?
            {
                return Err(EngineError::AliasConflictsWithCollection(alias.to_owned()));
            }
            if aliases.contains_key(alias) {
                return Err(EngineError::AliasAlreadyExists(alias.to_owned()));
            }
            let mut next = aliases.clone();
            next.insert(alias.to_owned(), target.to_owned());
            Ok(Some(next))
        })
    }

    /// 3.x 先判别名存在、再判目标存在，不校验别名名字；目标可以是别名。
    pub fn alter_alias(&self, database: &str, collection: &str, alias: &str) -> Result<()> {
        self.change_aliases(database, |state, aliases| {
            let target = aliases.get(collection).map_or(collection, String::as_str);
            if !aliases.contains_key(alias) {
                return Err(EngineError::AliasNotFound(alias.to_owned()));
            }
            if !self.alias_target_exists(state, target)? {
                return Err(EngineError::CollectionNotFound(target.to_owned()));
            }
            let mut next = aliases.clone();
            next.insert(alias.to_owned(), target.to_owned());
            Ok(Some(next))
        })
    }

    /// 不校验名字；不存在时成功且不写文件（3.x 相同）。
    pub fn drop_alias(&self, database: &str, alias: &str) -> Result<()> {
        self.change_aliases(database, |_, aliases| {
            if !aliases.contains_key(alias) {
                return Ok(None);
            }
            let mut next = aliases.clone();
            next.remove(alias);
            Ok(Some(next))
        })
    }

    /// 3.x 的 ShowCollections 对筛选后的每个集合调用 `get_collection`，因而 `aliases.json` 无法解析时失败；
    /// 列表为空时不读取别名。
    pub fn check_aliases(&self, database: &str) -> Result<()> {
        let scope = self.key(database, "")?;
        let state = self.database(&scope.database)?;
        let mut cache = state.aliases.lock();
        alias::load(&mut cache, &state.root).map(|_| ())
    }

    /// 返回别名指向的集合名；悬空别名照常返回。
    pub fn describe_alias(&self, database: &str, alias: &str) -> Result<String> {
        let scope = self.key(database, "")?;
        let state = self.database(&scope.database)?;
        let mut cache = state.aliases.lock();
        alias::load(&mut cache, &state.root)?
            .get(alias)
            .cloned()
            .ok_or_else(|| EngineError::AliasNotFound(alias.to_owned()))
    }

    /// 全部别名，或指向 `collection`（先解析）的别名，按码点排序；同时返回解析后的集合名，
    /// `collection` 为空时为空串。集合不存在时返回空列表（3.x 相同）。
    pub fn list_aliases(&self, database: &str, collection: &str) -> Result<(String, Vec<String>)> {
        let scope = self.key(database, "")?;
        let state = self.database(&scope.database)?;
        let mut cache = state.aliases.lock();
        let aliases = alias::load(&mut cache, &state.root)?;
        if collection.is_empty() {
            return Ok((String::new(), aliases.keys().cloned().collect()));
        }
        let target = aliases.get(collection).map_or(collection, String::as_str);
        let names = aliases
            .iter()
            .filter(|(_, to)| to.as_str() == target)
            .map(|(alias, _)| alias.clone())
            .collect();
        Ok((target.to_owned(), names))
    }

    /// 别名写入持数据库写入闸门的独占许可，读改写全程持别名叶子锁（0023 A「锁」）。文件替换之后
    /// 目录同步失败时缓存仍随磁盘更新，并按 0007 封锁本服务实例的写入。
    fn change_aliases(
        &self,
        database: &str,
        change: impl FnOnce(&DatabaseState, &Aliases) -> Result<Option<Aliases>>,
    ) -> Result<()> {
        let scope = self.key(database, "")?;
        let _intent = NamespaceIntent::new(&self.namespace_waiters);
        let mut blocked = self.write_gate.write();
        if *blocked {
            return Err(storage(
                "database writes are blocked after an unconfirmed directory change",
            ));
        }
        let state = self.database(&scope.database)?;
        self.settle_pending_rename(state, &mut blocked)?;
        let mut cache = state.aliases.lock();
        let aliases = alias::load(&mut cache, &state.root)?;
        let Some(next) = change(state, aliases)? else {
            return Ok(());
        };
        let fault = |point| {
            #[cfg(feature = "fault-injection")]
            directory_fault(DirectoryOperation::Alias, alias_fault_point(point))?;
            #[cfg(not(feature = "fault-injection"))]
            let _point: fs_commit::ReplacePoint = point;
            Ok(())
        };
        match alias::write(&state.root, &next, fault) {
            Ok(()) => {
                *aliases = next;
                Ok(())
            }
            Err(fs_commit::ReplaceError::Unchanged(error)) => Err(storage(error)),
            Err(fs_commit::ReplaceError::Unconfirmed(error)) => {
                *aliases = next;
                *blocked = true;
                Err(storage(format!(
                    "aliases were changed, but aliases.json durability is unconfirmed: {error}"
                )))
            }
        }
    }

    /// 别名目标按精确名字且带 `schema.json` 判定（0023 名字与文件系统）；非法名字视为不存在。
    fn alias_target_exists(&self, state: &DatabaseState, name: &str) -> Result<bool> {
        if validate_filesystem_name(name, "collection").is_err() {
            return Ok(false);
        }
        let path = state.collection_path(name);
        let shaped = match fs::symlink_metadata(&path) {
            Ok(meta) => meta.file_type().is_dir() && path.join("schema.json").is_file(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(storage(error)),
        };
        Ok(shaped && self.exact_name_in(state, name)?)
    }
}

/// create / drop 沿用 0007 的悬空别名剔除：目标路径不存在即悬空，按路径而非精确名字判定，
/// 大小写不敏感文件系统上指向只差大小写名字的 3.x 别名不受影响。别名 RPC 不剔除。
fn retain_live_aliases(database: &DatabaseState, aliases: &mut Aliases) -> std::io::Result<()> {
    let mut dangling = Vec::new();
    for (alias, target) in aliases.iter() {
        if validate_filesystem_name(target, "collection").is_err() {
            dangling.push(alias.clone());
            continue;
        }
        match fs::metadata(database.collection_path(target)) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                dangling.push(alias.clone());
            }
            Err(error) => return Err(error),
        }
    }
    for alias in dangling {
        aliases.remove(&alias);
    }
    Ok(())
}

/// drop 提交后删除指向 `name` 的别名；上一次 drop 可能已改名却未能写别名，任何一次写入都同时剔除悬空目标。
fn remove_aliases_for(database: &DatabaseState, name: &str) -> Result<()> {
    let mut cache = database.aliases.lock();
    let aliases = alias::load(&mut cache, &database.root)?;
    if !aliases.values().any(|target| target == name) {
        return Ok(());
    }
    let mut next = aliases.clone();
    next.retain(|_, target| target != name);
    retain_live_aliases(database, &mut next).map_err(storage)?;
    save_lifecycle_aliases(database, aliases, next, false)
}

/// create / drop 的别名写入失败只报错、不封锁（0007）：剔除的都是悬空别名，替换未落盘时至多重新出现。
/// 文件已替换时缓存随之更新，与磁盘一致。
fn save_lifecycle_aliases(
    database: &DatabaseState,
    cache: &mut Aliases,
    next: Aliases,
    creating: bool,
) -> Result<()> {
    database.aliases.assert_held();
    let fault = |point| {
        #[cfg(feature = "fault-injection")]
        directory_fault(
            if creating {
                DirectoryOperation::Create
            } else {
                DirectoryOperation::Drop
            },
            alias_fault_point(point),
        )?;
        #[cfg(not(feature = "fault-injection"))]
        let _point: fs_commit::ReplacePoint = point;
        Ok(())
    };
    #[cfg(not(feature = "fault-injection"))]
    let _creating = creating;
    match alias::write(&database.root, &next, fault) {
        Ok(()) => {
            *cache = next;
            Ok(())
        }
        Err(fs_commit::ReplaceError::Unchanged(error)) => Err(storage(error)),
        Err(fs_commit::ReplaceError::Unconfirmed(error)) => {
            *cache = next;
            Err(storage(error))
        }
    }
}

#[cfg(feature = "fault-injection")]
fn alias_fault_point(point: fs_commit::ReplacePoint) -> DirectoryPoint {
    match point {
        fs_commit::ReplacePoint::BeforeRename => DirectoryPoint::Aliases,
        fs_commit::ReplacePoint::AfterRename => DirectoryPoint::AliasesAfterRename,
    }
}

/// 路径（任意类型，含悬空符号链接）是否存在；NotFound 以外的错误上报。
fn path_exists(path: &std::path::Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(storage(error)),
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

// parking_lot 不会中毒；必须在状态锁放开之前封锁可能已被部分更新的 collection。
fn state_change<T>(
    collection: &mut Collection,
    apply: impl FnOnce(&mut Collection) -> Result<T>,
) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| apply(collection))) {
        Ok(result) => result,
        Err(payload) => {
            if let Some(persistent) = collection.persistent_mut() {
                persistent.poison();
            }
            let reason = crate::error::panic_description(payload.as_ref());
            Err(storage(format!("collection state panicked: {reason}")))
        }
    }
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
    if schema_file.to_core().is_err() {
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

/// 3.x 的 `_validate_filesystem_name`：以 ValueError 抛出，因此是 InvalidArgument。
/// 名字按原样区分大小写（0004「决策」）。
pub(crate) fn validate_filesystem_name(name: &str, kind: &str) -> Result<()> {
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

// 清理失败不改变原业务错误的类别，尤其不能把并发 load 的 code 6 改为存储 code 1。
fn index_cleanup_error(original: EngineError, cleanup: String) -> EngineError {
    match original {
        EngineError::SchemaValidation(reason) => {
            EngineError::SchemaValidation(format!("{reason}; {cleanup}"))
        }
        EngineError::InvalidArgument(reason) => {
            EngineError::InvalidArgument(format!("{reason}; {cleanup}"))
        }
        EngineError::IndexNotFound(reason) => {
            EngineError::IndexNotFound(format!("{reason}; {cleanup}"))
        }
        EngineError::Storage(reason) => EngineError::Storage(format!("{reason}; {cleanup}")),
        other => {
            eprintln!("index cleanup failed after {other}: {cleanup}");
            other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::InputValues;
    use crate::schema::{DataType, FieldSchema};

    #[test]
    fn named_partition_writes_flush_and_reopen_preserve_both_reinsert_delete_branches() {
        for committed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let db = test_db(root.path());
            db.create_collection("", "c", basic_schema()).unwrap();
            for name in ["p1", "p2"] {
                db.create_partition("", "c", name).unwrap();
            }
            let batch = |value| InsertBatch {
                num_rows: 1,
                columns: vec![
                    ("id".into(), InputValues::Int(vec![1])),
                    (
                        "v".into(),
                        InputValues::FloatVector {
                            dim: 2,
                            values: vec![value, 0.],
                        },
                    ),
                ],
            };
            let insert = |part: &str, value| {
                db.insert_partition_with_pk_type("", "c", batch(value), part)
                    .unwrap()
            };
            insert("p1", 1.);
            insert("p2", 2.);
            let handle = db.collection("", "c").unwrap();
            let count = |part: &str| {
                let c = handle.read();
                let ids = c.resolve_partitions(&[part.into()]).unwrap();
                c.read_partition_snapshot(ids.as_deref())
                    .unwrap()
                    .count_filtered("")
                    .unwrap()
            };
            assert_eq!(count("p1"), 0);
            db.flush_collection("", "c").unwrap();
            insert("p1", 3.);
            db.flush_collection("", "c").unwrap();
            assert_eq!(count("p1"), 1);
            assert_eq!(count("p2"), 1);
            assert_eq!(handle.read().count(), 1);
            db.delete("", "c", &[Pk::Int64(1)]).unwrap();
            if committed {
                db.flush_collection("", "c").unwrap();
            }
            insert("p2", 4.);
            assert_eq!(count("p1"), usize::from(!committed));
            db.close();
            drop(handle);
            drop(db);
            let reopened = test_db(root.path());
            let handle = reopened.collection("", "c").unwrap();
            let c = handle.read();
            let ids = c.resolve_partitions(&["p1".into()]).unwrap();
            assert_eq!(
                c.read_partition_snapshot(ids.as_deref())
                    .unwrap()
                    .count_filtered("")
                    .unwrap(),
                usize::from(!committed)
            );
            assert_eq!(
                c.get(&[Pk::Int64(1)], &["v".into()]).unwrap().rows.fields[0].1,
                crate::ColumnData::FloatVector {
                    dim: 2,
                    values: vec![4., 0.]
                }
            );
            drop(c);
            reopened.close();
        }
    }

    #[test]
    fn partition_namespace_waits_for_an_existing_shared_write_permit() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        for drop_partition in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let db = test_db(root.path());
            db.create_collection("", "c", basic_schema()).unwrap();
            db.create_partition("", "c", "p").unwrap();
            let (entered_tx, entered_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let writer_db = Arc::clone(&db);
            let writer = std::thread::spawn(move || {
                let _task = writer_db.enter().unwrap();
                writer_db
                    .mutate("", "c", |_| {
                        entered_tx.send(()).unwrap();
                        resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                        Ok(())
                    })
                    .unwrap();
            });
            entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let worker_db = Arc::clone(&db);
            let (done_tx, done_rx) = mpsc::channel();
            let namespace = std::thread::spawn(move || {
                let _task = worker_db.enter().unwrap();
                let result = if drop_partition {
                    worker_db.drop_partition("", "c", "p")
                } else {
                    worker_db.create_partition("", "c", "q")
                };
                done_tx.send(result).unwrap();
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            while db.namespace_waiters.load(Ordering::Acquire) == 0 {
                assert!(
                    Instant::now() < deadline,
                    "namespace operation did not enter"
                );
                std::thread::yield_now();
            }
            // 已观测到命名空间请求，旧写者仍持共享许可，不能在其退出前完成变更。
            assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
            resume_tx.send(()).unwrap();
            writer.join().unwrap();
            done_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            namespace.join().unwrap();
            db.close();
        }
    }

    #[test]
    fn first_partition_activation_keeps_preexisting_frozen_default_generations_correct() {
        let put = |db: &Db, part: &str, id, value| {
            db.insert_partition_with_pk_type(
                "",
                "c",
                InsertBatch {
                    num_rows: 1,
                    columns: vec![
                        ("id".into(), InputValues::Int(vec![id])),
                        (
                            "v".into(),
                            InputValues::FloatVector {
                                dim: 2,
                                values: vec![value, 0.],
                            },
                        ),
                    ],
                },
                part,
            )
            .unwrap();
        };
        let count = |db: &Db, part: &str| {
            db.collection("", "c")
                .unwrap()
                .read()
                .count_partition(part)
                .unwrap()
        };
        for frozen_delete in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let db = test_db(root.path());
            db.create_collection("", "c", basic_schema()).unwrap();
            put(&db, "_default", 1, 1.);
            let handle = db.collection("", "c").unwrap();
            handle
                .write()
                .persistent_mut()
                .unwrap()
                .freeze_active(false)
                .unwrap();
            if frozen_delete {
                db.delete("", "c", &[Pk::Int64(1)]).unwrap();
                handle
                    .write()
                    .persistent_mut()
                    .unwrap()
                    .freeze_active(false)
                    .unwrap();
            }
            let old = handle.read().read_snapshot();
            db.create_partition("", "c", "p").unwrap();
            put(&db, "p", 1, 2.);
            db.flush_collection("", "c").unwrap();
            assert_eq!(count(&db, "_default"), usize::from(!frozen_delete));
            assert_eq!(count(&db, "p"), 1);
            assert_eq!(old.count_filtered("").unwrap(), usize::from(!frozen_delete));
            db.close();
            std::mem::drop(handle);
            std::mem::drop(db);
            let db = test_db(root.path());
            assert_eq!(count(&db, "_default"), usize::from(!frozen_delete));
            assert_eq!(count(&db, "p"), 1);
            db.close();
        }
    }

    #[test]
    fn partition_drop_unknown_has_no_flush_and_default_flushes_before_rejection() {
        let root = tempfile::tempdir().unwrap();
        let db = test_db(root.path());
        db.create_collection("", "c", basic_schema()).unwrap();
        db.insert(
            "",
            "c",
            InsertBatch {
                num_rows: 1,
                columns: vec![
                    ("id".into(), InputValues::Int(vec![1])),
                    (
                        "v".into(),
                        InputValues::FloatVector {
                            dim: 2,
                            values: vec![1., 0.],
                        },
                    ),
                ],
            },
        )
        .unwrap();
        let collection = root.path().join("collections/c");
        let wal_bytes = || {
            fs::read_dir(collection.join("wal"))
                .unwrap()
                .map(|entry| {
                    let path = entry.unwrap().path();
                    (
                        path.file_name().unwrap().to_os_string(),
                        fs::read(path).unwrap(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        let manifest_bytes = || match fs::read(collection.join("manifest.json")) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("cannot inspect manifest: {error}"),
        };
        let before = manifest_bytes();
        let wal_before = wal_bytes();
        assert!(matches!(
            db.drop_partition("", "c", "missing"),
            Err(EngineError::PartitionNotFound(_))
        ));
        assert_eq!(manifest_bytes(), before);
        assert_eq!(wal_bytes(), wal_before);
        assert!(
            matches!(db.drop_partition("","c","_default"),Err(EngineError::SchemaValidation(reason)) if reason=="cannot remove default partition '_default'")
        );
        let manifest =
            crate::storage::manifest::Manifest::read(&collection.join("manifest.json")).unwrap();
        assert_eq!(manifest.current_seq, 1);
        assert_eq!(manifest.partitions["_default"].data_files.len(), 1);
        assert!(wal_bytes().is_empty());
        db.close();
        drop(db);
        let db = test_db(root.path());
        assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
        db.close();
    }

    #[test]
    fn partition_creation_registers_only_safe_prepared_directories_and_rejects_unowned_files() {
        let root = tempfile::tempdir().unwrap();
        let db = test_db(root.path());
        db.create_collection("", "c", basic_schema()).unwrap();
        db.create_partition("", "c", "p1").unwrap();
        assert!(db.has_partition("", "c", "p1").unwrap());
        assert_eq!(db.list_partitions("", "c").unwrap().0, ["_default", "p1"]);
        assert!(matches!(
            db.create_partition("", "c", "p1"),
            Err(EngineError::PartitionAlreadyExists(_))
        ));
        for name in ["", ".", "..", "_all", "../escape", "a/b", "a\\b", "a\0b"] {
            assert!(matches!(
                db.create_partition("", "c", name),
                Err(EngineError::SchemaValidation(_))
            ));
            assert!(!db.has_partition("", "c", name).unwrap());
        }
        let occupied = root.path().join("collections/c/partitions/occupied");
        fs::create_dir_all(&occupied).unwrap();
        fs::write(occupied.join("canary"), b"owned").unwrap();
        assert!(db.create_partition("", "c", "occupied").is_err());
        assert!(!db.has_partition("", "c", "occupied").unwrap());
        assert_eq!(fs::read(occupied.join("canary")).unwrap(), b"owned");
        db.close();
    }

    #[test]
    fn partition_visibility_activation_keeps_real_columns_writes_flush_and_reopen_consistent() {
        let root = tempfile::tempdir().unwrap();
        let db = test_db(root.path());
        db.create_collection("", "c", basic_schema()).unwrap();
        let batch = |id, value| InsertBatch {
            num_rows: 1,
            columns: vec![
                ("id".into(), InputValues::Int(vec![id])),
                (
                    "v".into(),
                    InputValues::FloatVector {
                        dim: 2,
                        values: vec![value, 0.],
                    },
                ),
            ],
        };
        db.insert("", "c", batch(1, 1.)).unwrap();
        db.flush_collection("", "c").unwrap();
        db.delete("", "c", &[Pk::Int64(1)]).unwrap();
        db.insert("", "c", batch(1, 3.)).unwrap();
        let handle = db.collection("", "c").unwrap();
        handle.write().enable_partition_visibility().unwrap();
        let old = handle.read().read_partition_snapshot(Some(&[0])).unwrap();
        assert_eq!(
            old.query("", &["v".into()], None, 0).unwrap().rows.fields[0].1,
            crate::ColumnData::FloatVector {
                dim: 2,
                values: vec![3., 0.]
            }
        );
        assert_eq!(
            handle
                .read()
                .read_partition_snapshot(Some(&[1]))
                .unwrap()
                .count_filtered("")
                .unwrap(),
            0
        );
        db.insert("", "c", batch(2, 2.)).unwrap();
        assert_eq!(old.count_filtered("").unwrap(), 1);
        db.flush_collection("", "c").unwrap();
        assert_eq!(
            handle
                .read()
                .read_partition_snapshot(None)
                .unwrap()
                .count_filtered("")
                .unwrap(),
            2
        );
        db.delete("", "c", &[Pk::Int64(1)]).unwrap();
        db.flush_collection("", "c").unwrap();
        db.insert("", "c", batch(1, 5.)).unwrap();
        db.flush_collection("", "c").unwrap();
        assert_eq!(handle.read().count(), 2);
        db.close();
        drop(handle);
        drop(db);
        let reopened = test_db(root.path());
        let handle = reopened.collection("", "c").unwrap();
        assert_eq!(handle.read().count(), 2);
        assert_eq!(
            handle
                .read()
                .get(&[Pk::Int64(1)], &["v".into()])
                .unwrap()
                .rows
                .fields[0]
                .1,
            crate::ColumnData::FloatVector {
                dim: 2,
                values: vec![5., 0.]
            }
        );
        reopened.close();
    }

    #[test]
    fn failed_or_panicking_partition_index_transfer_blocks_service_and_preserves_wal() {
        for panic in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let db = test_db(root.path());
            db.create_collection("", "c", basic_schema()).unwrap();
            db.insert(
                "",
                "c",
                InsertBatch {
                    num_rows: 1,
                    columns: vec![
                        ("id".into(), InputValues::Int(vec![1])),
                        (
                            "v".into(),
                            InputValues::FloatVector {
                                dim: 2,
                                values: vec![1., 0.],
                            },
                        ),
                    ],
                },
            )
            .unwrap();
            let handle = db.collection("", "c").unwrap();
            let mut collection = handle.write();
            let persistent = collection.persistent_mut().unwrap();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                persistent.rebuild_partition_visibility(|_, _| {
                    if panic {
                        panic!("controlled partition index transfer panic");
                    }
                    Err("controlled partition index transfer error".into())
                })
            }));
            if panic {
                assert!(outcome.is_err());
            } else {
                assert!(outcome.unwrap().is_err());
            }
            assert!(persistent.poisoned());
            drop(collection);
            assert!(db.collection("", "c").is_err());
            db.close();
            drop(handle);
            drop(db);
            let reopened = test_db(root.path());
            assert_eq!(reopened.collection("", "c").unwrap().read().count(), 1);
            reopened.close();
        }
    }

    #[test]
    fn compaction_cleanup_does_not_delete_a_path_now_referenced_by_flush() {
        let root = tempfile::tempdir().unwrap();
        let db = test_db(root.path());
        db.create_collection("", "c", basic_schema()).unwrap();
        db.insert(
            "",
            "c",
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
        db.flush_collection("", "c").unwrap();
        let handle = db.collection("", "c").unwrap();
        let lock = handle.read().persistent().unwrap().flush_lock();
        let held = lock.lock();
        let path = {
            let mut collection = handle.write();
            let persistent = collection.persistent_mut().unwrap();
            let relative = persistent.manifest.partitions["_default"].data_files[0].clone();
            persistent
                .compaction_cleanup
                .push(PathBuf::from("_default").join(&relative));
            persistent.dir.join("partitions/_default").join(relative)
        };
        let before = fs::read(&path).unwrap();
        db.cleanup_compaction_files(&handle).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(handle.read().count(), 1);
        assert!(
            handle
                .read()
                .persistent()
                .unwrap()
                .compaction_cleanup
                .is_empty()
        );
        drop(held);
        db.close();
    }

    #[test]
    fn aliases_leaf_lock_covers_real_read_modify_replace_transactions() {
        use std::sync::mpsc;
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        let db = Arc::new(test_db(root.path()));
        for name in ["a", "b", "live"] {
            db.create_collection("", name, basic_schema()).unwrap();
        }
        for (target, alias) in [("a", "aa"), ("b", "bb"), ("live", "keep")] {
            db.create_alias("", target, alias).unwrap();
        }
        let path = root.path().join("aliases.json");
        let bytes = std::fs::read(&path).unwrap();
        let held = db.databases[DEFAULT_DATABASE].aliases.lock();
        let (tx, rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let mut workers = vec![];
        for name in ["a", "b"] {
            let db = Arc::clone(&db);
            let tx = tx.clone();
            let done = done_tx.clone();
            workers.push(std::thread::spawn(move || {
                tx.send(()).unwrap();
                remove_aliases_for(&db.databases[DEFAULT_DATABASE], name).unwrap();
                done.send(()).unwrap();
            }));
        }
        for _ in 0..2 {
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        drop(held);
        for worker in workers {
            worker.join().unwrap();
        }
        let aliases: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(aliases, serde_json::json!({"keep":"live"}));
        db.close();
    }
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

    fn key(name: &str) -> CollectionKey {
        CollectionKey {
            database: DEFAULT_DATABASE.to_owned(),
            name: name.to_owned(),
        }
    }

    fn test_db(path: &std::path::Path) -> Arc<Db> {
        Db::new(
            DataDir::open(path).unwrap(),
            Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
        )
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn all_close_paths_unlock_while_a_duplicated_descriptor_remains_open() {
        #[cfg(not(feature = "fault-injection"))]
        let modes = ["close", "nowait", "drop"];
        #[cfg(feature = "fault-injection")]
        let modes = ["close", "nowait", "drop", "spawn-failure"];
        for mode in modes {
            let root = tempfile::tempdir().unwrap();
            let dir = DataDir::open(root.path()).unwrap();
            // dup 与 fork 共享打开文件描述，保留这个引用能识别仅 close fd 的旧实现。
            let inherited = dir.duplicate_lock_for_test().unwrap();
            let db = Db::new(dir, Compute::new(std::num::NonZeroUsize::MIN).unwrap()).unwrap();
            assert!(matches!(
                DataDir::open(root.path()),
                Err(crate::data_dir::DataDirError::Locked { .. })
            ));
            match mode {
                "close" => db.close(),
                "nowait" => {
                    db.close_nowait();
                    db.close();
                }
                "drop" => drop(db),
                #[cfg(feature = "fault-injection")]
                "spawn-failure" => {
                    inject_close_worker_spawn_failure_once();
                    db.close_nowait();
                    db.close();
                }
                _ => unreachable!(),
            }
            let reopened = DataDir::open(root.path()).unwrap_or_else(|e| panic!("{mode}: {e}"));
            assert!(inherited.metadata().is_ok());
            drop(reopened);
            drop(inherited);
        }
    }

    #[cfg(unix)]
    #[test]
    fn unlock_failure_is_reported_by_synchronous_and_asynchronous_close() {
        #[cfg(not(feature = "fault-injection"))]
        let modes = ["close", "nowait"];
        #[cfg(feature = "fault-injection")]
        let modes = ["close", "nowait", "spawn-failure"];
        for mode in modes {
            // 解锁失败后只能靠关闭全部描述符释放锁，与库内派生子进程互斥。
            let _spawn = crate::data_dir::SUBPROCESS_SPAWN.read();
            let root = tempfile::tempdir().unwrap();
            crate::data_dir::inject_unlock_failure_for_next_open();
            let dir = DataDir::open(root.path()).unwrap();
            let inherited = dir.duplicate_lock_for_test().unwrap();
            let db = Db::new(dir, Compute::new(std::num::NonZeroUsize::MIN).unwrap()).unwrap();
            match mode {
                "close" => db.close(),
                "nowait" => db.close_nowait(),
                #[cfg(feature = "fault-injection")]
                "spawn-failure" => {
                    inject_close_worker_spawn_failure_once();
                    db.close_nowait();
                }
                _ => unreachable!(),
            }
            db.close();
            assert!(db.shutdown_failed(), "{mode}");
            assert!(matches!(db.enter(), Err(EngineError::Closed)));
            assert!(matches!(
                DataDir::open(root.path()),
                Err(crate::data_dir::DataDirError::Locked { .. })
            ));
            drop(inherited);
            DataDir::open(root.path()).unwrap();
        }
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
        handle.write().load().unwrap();
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
    fn empty_and_default_database_names_share_one_collection_identity() {
        let root = tempfile::tempdir().unwrap();
        let db = test_db(root.path());
        db.create_collection("default", "c", basic_schema())
            .unwrap();
        let by_empty = db.collection("", "c").unwrap();
        let by_default = db.collection("default", "c").unwrap();
        assert!(Arc::ptr_eq(&by_empty, &by_default));
        assert_eq!(
            db.collections.read().keys().cloned().collect::<Vec<_>>(),
            [key("c")]
        );
        assert!(Arc::ptr_eq(
            db.name_lease(&db.key("", "c").unwrap()).mutex(),
            db.name_lease(&db.key("default", "c").unwrap()).mutex()
        ));
        drop((by_empty, by_default));
        db.drop_collection("", "c").unwrap();
        assert!(db.collections.read().is_empty());
        assert!(matches!(
            db.collection("default", "c"),
            Err(EngineError::CollectionNotFound(_))
        ));
        db.close();
    }

    #[test]
    fn name_lock_identity_survives_waiters_and_reclaims_unused_names() {
        let root = tempfile::tempdir().unwrap();
        let db = test_db(root.path());
        let first = db.name_lease(&key("c"));
        let second = db.name_lease(&key("c"));
        assert!(Arc::ptr_eq(first.mutex(), second.mutex()));
        drop(first);
        let third = db.name_lease(&key("c"));
        assert!(Arc::ptr_eq(second.mutex(), third.mutex()));
        let other = db.name_lease(&key("other"));
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
    fn name_lock_pairs_take_names_in_byte_order_from_either_side() {
        let root = tempfile::tempdir().unwrap();
        let db = test_db(root.path());
        let (b, a) = (db.name_lease(&key("b")), db.name_lease(&key("a")));
        for (x, y) in [(&a, &b), (&b, &a)] {
            let guards = NameLease::lock_pair(x, y);
            assert!(a.mutex().try_lock().is_none() && b.mutex().try_lock().is_none());
            drop(guards);
        }
        #[cfg(debug_assertions)]
        {
            let c = db.name_lease(&key("c"));
            let guards = NameLease::lock_pair(&a, &b);
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(c.mutex().lock())))
                    .is_err()
            );
            drop(guards);
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop(NameLease::lock_pair(&a, &a))
                }))
                .is_err()
            );
        }
        drop((a, b));
        assert!(db.name_locks.lock().is_empty());
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
                    let lease = db.name_lease(&key("same-name"));
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
            drop(db.name_lease(&key("c")));
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
            let _lease = db.name_lease(&key("c"));
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
            assert!(
                db.open_failures.read().contains_key(&key("c")),
                "{kind}: {first}"
            );
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
                assert!(!db.open_failures.read().contains_key(&key("c")));
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
            assert!(db.open_failures.read().contains_key(&key("c")));
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
            assert!(!db.open_failures.read().contains_key(&key("c")));
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
            db.flush_handle(&old, true),
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
                    assert!(db.open_failures.read().contains_key(&key("c")));
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
                        assert!(!db.collections.read().contains_key(&key("c")));
                    }
                }
                assert!(!db.open_failures.read().contains_key(&key("c")));
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
        assert!(!db.open_failures.read().contains_key(&key("c")));
        db.create_collection("", "c", basic_schema()).unwrap();
        assert_eq!(db.collection("", "c").unwrap().read().count(), 0);
        db.close();
    }

    #[test]
    fn blocked_close_only_syncs_wal_and_leaves_manifest_untouched() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("blocked.db");
        let compute = || Compute::new(std::num::NonZeroUsize::MIN).unwrap();
        let db = Db::new(DataDir::open(&path).unwrap(), compute()).unwrap();
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
        assert!(db.shutdown_failed());
        assert!(!path.join("collections/c/manifest.json").exists());
        assert!(
            path.join("collections/c/wal/wal_data_000001.arrow")
                .exists()
        );
        let reopened = Db::new(DataDir::open(&path).unwrap(), compute()).unwrap();
        assert_eq!(reopened.collection("", "c").unwrap().read().count(), 1);
        reopened.close();
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn blocked_close_reports_failure_with_or_without_pending_maintenance() {
        use std::time::{Duration, Instant};

        for pending in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let db = test_db(root.path());
            db.create_collection("", "c", basic_schema()).unwrap();
            let before = db
                .wait_for_maintenance("", "c", Duration::from_secs(10))
                .unwrap();
            assert_eq!(before.accepted, before.completed);
            assert_eq!(before.failed, 0);
            db.insert(
                "",
                "c",
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
            let wal = root.path().join("collections/c/wal/wal_data_000001.arrow");
            let wal_before = fs::read(&wal).unwrap();
            let handle = db.collection("", "c").unwrap();
            // 维护不能越过闸门；关闭开始后才放行，固定积压任务的失败时机。
            let mut blocked = db.write_gate.write();
            *blocked = true;
            if pending {
                db.maintenance.enqueue(&handle).unwrap();
            }
            let closing = Arc::clone(&db);
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                closing.close();
                done_tx.send(()).unwrap();
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            while !db.gate.lock().cleanup_started {
                assert!(Instant::now() < deadline, "close did not start");
                std::thread::yield_now();
            }
            drop(blocked);
            done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            worker.join().unwrap();
            assert!(db.shutdown_failed(), "pending={pending}");
            let after = db
                .wait_for_maintenance("", "c", Duration::from_secs(10))
                .unwrap();
            assert_eq!(after.accepted, before.accepted + u64::from(pending));
            assert_eq!(after.completed, after.accepted);
            assert_eq!(after.failed, u64::from(pending));
            assert!(!root.path().join("collections/c/manifest.json").exists());
            assert_eq!(fs::read(&wal).unwrap(), wal_before);
            let reopened = test_db(root.path());
            assert_eq!(reopened.collection("", "c").unwrap().read().count(), 1);
            reopened.close();
            assert!(!reopened.shutdown_failed());
        }
    }

    #[test]
    fn stale_handle_cannot_flush_after_drop_moved_its_directory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("stale.db");
        let compute = Compute::new(std::num::NonZeroUsize::MIN).unwrap();
        let db = Db::new(DataDir::open(&path).unwrap(), compute).unwrap();
        db.create_collection("", "c", basic_schema()).unwrap();
        let stale = db.collection("", "c").unwrap();
        db.drop_collection("", "c").unwrap();
        assert!(matches!(
            db.flush_handle(&stale, true),
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
        )
        .unwrap();
        db.create_collection("", "c", basic_schema()).unwrap();
        db.close();
        let db = Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
        )
        .unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let opener = Arc::clone(&db);
        let opening = std::thread::spawn(move || {
            pause_open_cleanup_once(entered_tx, resume_rx);
            opener.collection("", "c").unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!db.write_gate.available());
        assert!(db.namespace.available());
        let lease = db.name_lease(&key("c"));
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
            let db = Db::new(
                DataDir::open(root.path()).unwrap(),
                Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
            )
            .unwrap();
            db.create_collection("", "writer", basic_schema()).unwrap();
            db.create_collection("", "victim", basic_schema()).unwrap();
            // 建表会排入后台维护，维护轮次也短暂持有写入许可；先等它们结束，下面观察到的
            // 许可占用才能归因于插入线程。
            let statuses = ["writer", "victim"].map(|name| {
                db.wait_for_maintenance("", name, Duration::from_secs(5))
                    .unwrap();
                let handle = db.collection("", name).unwrap();
                Arc::clone(&handle.read().persistent().unwrap().maintenance_status)
            });
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
            while db.write_gate.available() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
            // 主线程持有 writer 的状态写锁，不能再经 Db 取读锁；用事先取得的维护状态核对。
            for status in &statuses {
                assert!(
                    status.wait(Duration::ZERO).is_ok(),
                    "a maintenance round may hold the write permission"
                );
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
            assert!(db.namespace.available());
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
    #[cfg(feature = "fault-injection")]
    #[test]
    fn metadata_writes_wait_for_an_in_flight_writer_and_hold_the_exclusive_permit() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        type Change = fn(&Db) -> Result<()>;
        let changes: [(Change, Change); 2] = [
            (
                |db| db.create_alias("", "writer", "a"),
                |db| {
                    assert_eq!(db.describe_alias("", "a").unwrap(), "writer");
                    assert_eq!(db.collection("", "a").unwrap().read().count(), 1);
                    Ok(())
                },
            ),
            (
                |db| db.alter_collection("", "writer", &[("k".into(), "v".into())], &[]),
                |db| {
                    let handle = db.collection("", "writer").unwrap();
                    let collection = handle.read();
                    assert_eq!(collection.schema().properties, [("k".into(), "v".into())]);
                    assert_eq!(collection.count(), 1);
                    Ok(())
                },
            ),
        ];
        for (change, check) in changes {
            let root = tempfile::tempdir().unwrap();
            let db = Db::new(
                DataDir::open(root.path()).unwrap(),
                Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
            )
            .unwrap();
            db.create_collection("", "writer", basic_schema()).unwrap();
            db.wait_for_maintenance("", "writer", Duration::from_secs(5))
                .unwrap();
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
            while db.write_gate.available() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
            let changer = Arc::clone(&db);
            let (done_tx, done_rx) = mpsc::channel();
            let changing = std::thread::spawn(move || {
                done_tx.send(change(&changer)).unwrap();
            });
            // 写入者持共享许可期间元数据写入不能开始，叶子锁也未被占住，读请求照常解析。
            assert!(matches!(
                done_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            assert!(db.databases[DEFAULT_DATABASE].aliases.available());
            drop(state);
            writing.join().unwrap().unwrap();
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            changing.join().unwrap();
            check(&db).unwrap();
            db.close();
        }
    }
}
