//! 数据库：多 collection 的生命周期与引擎的关闭。M1 只有 `default` 一个数据库（0004「范围」）。

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::{Condvar, Mutex, RwLock};

use crate::collection::{Collection, LoadState};
use crate::compute::Compute;
use crate::data_dir::DataDir;
use crate::error::{EngineError, Result, py_repr};
use crate::schema::CollectionSchema;

/// 每个 collection 一把读写锁：写入互斥、读取并发，仍是「单 collection 单写者」（不变量 7）。
pub type CollectionHandle = Arc<RwLock<Collection>>;

pub const DEFAULT_DATABASE: &str = "default";

#[derive(Debug)]
pub struct Db {
    /// 按名字排序，list 的顺序与 3.x 的 `sorted(names)`（码点序）一致。
    collections: RwLock<BTreeMap<String, CollectionHandle>>,
    gate: Mutex<Gate>,
    idle: Condvar,
    /// 服务的计算池（0005）。服务的运行状态与每个在途的 RPC 都持有 `Arc<Db>`，
    /// 所以它在最后一个在途任务结束之后才析构。
    compute: Compute,
}

/// 关闭闸门（0003「进程退出」第 2、3 步）：关闭后不再接受新任务，在途任务全部结束后才释放 LOCK，
/// 保证 LOCK 释放时没有任何线程还在读写引擎状态。
#[derive(Debug)]
struct Gate {
    closed: bool,
    in_flight: usize,
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
            gate.data_dir = None;
            self.db.idle.notify_all();
        }
    }
}

impl Db {
    pub fn new(data_dir: DataDir, compute: Compute) -> Self {
        Self {
            collections: RwLock::new(BTreeMap::new()),
            gate: Mutex::new(Gate {
                closed: false,
                in_flight: 0,
                data_dir: Some(data_dir),
            }),
            idle: Condvar::new(),
            compute,
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
        while gate.in_flight > 0 {
            self.idle.wait(&mut gate);
        }
        gate.data_dir = None;
    }

    /// 不阻塞的关闭：拒绝新任务；没有在途任务时立即释放 LOCK，否则由最后一个任务结束时释放。
    pub fn close_nowait(&self) {
        let mut gate = self.gate.lock();
        gate.closed = true;
        if gate.in_flight == 0 {
            gate.data_dir = None;
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
        let collection = Collection::new(name, schema)?;
        let mut collections = self.collections.write();
        if collections.contains_key(name) {
            return Err(EngineError::CollectionAlreadyExists(name.to_owned()));
        }
        collections.insert(name.to_owned(), Arc::new(RwLock::new(collection)));
        Ok(())
    }

    /// collection 不存在时是空操作（3.x 相同）。
    pub fn drop_collection(&self, database: &str, name: &str) -> Result<()> {
        require_database(database)?;
        self.collections.write().remove(name);
        Ok(())
    }

    pub fn collection(&self, database: &str, name: &str) -> Result<CollectionHandle> {
        require_database(database)?;
        self.collections
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| EngineError::CollectionNotFound(name.to_owned()))
    }

    /// 全部 collection 的名字与 load 状态。先在表读锁内取出句柄，放开表锁后再读各自的状态：
    /// 等待 collection 锁时若仍持有表锁，排队的写者会让所有 RPC 都停在表锁上。
    /// 取出之后被并发 drop 的 collection 仍会列出，不会报错。
    pub fn list_collections(&self, database: &str) -> Result<Vec<(String, LoadState)>> {
        require_database(database)?;
        let handles: Vec<(String, CollectionHandle)> = self
            .collections
            .read()
            .iter()
            .map(|(name, handle)| (name.clone(), Arc::clone(handle)))
            .collect();
        Ok(handles
            .into_iter()
            .map(|(name, handle)| {
                let state = handle.read().load_state();
                (name, state)
            })
            .collect())
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
