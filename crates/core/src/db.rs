//! 数据库：多 collection 的生命周期。M1 只有 `default` 一个数据库（0004「范围」）。

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::collection::Collection;
use crate::data_dir::DataDir;
use crate::error::{EngineError, Result, py_repr};
use crate::schema::CollectionSchema;

/// 每个 collection 一把读写锁：写入互斥、读取并发，仍是「单 collection 单写者」（不变量 7）。
pub type CollectionHandle = Arc<RwLock<Collection>>;

#[derive(Debug)]
pub struct Db {
    /// 持有期间独占数据目录的 LOCK。
    _data_dir: DataDir,
    /// 按名字排序，list 的顺序与 3.x 的 `sorted(names)`（码点序）一致。
    collections: RwLock<BTreeMap<String, CollectionHandle>>,
}

impl Db {
    pub fn new(data_dir: DataDir) -> Self {
        Self {
            _data_dir: data_dir,
            collections: RwLock::new(BTreeMap::new()),
        }
    }

    /// 检查顺序与 3.x 相同：名字合法性、schema 校验、是否重名。
    pub fn create_collection(&self, name: &str, schema: CollectionSchema) -> Result<()> {
        validate_name(name)?;
        let collection = Collection::new(name, schema)?;
        let mut collections = self.collections.write();
        if collections.contains_key(name) {
            return Err(EngineError::CollectionAlreadyExists(name.to_owned()));
        }
        collections.insert(name.to_owned(), Arc::new(RwLock::new(collection)));
        Ok(())
    }

    /// 不存在时是空操作（3.x 相同）。
    pub fn drop_collection(&self, name: &str) {
        self.collections.write().remove(name);
    }

    pub fn collection(&self, name: &str) -> Result<CollectionHandle> {
        self.collections
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| EngineError::CollectionNotFound(name.to_owned()))
    }

    pub fn has_collection(&self, name: &str) -> bool {
        self.collections.read().contains_key(name)
    }

    pub fn list_collections(&self) -> Vec<String> {
        self.collections.read().keys().cloned().collect()
    }
}

/// 3.x 的 `_validate_filesystem_name`：以 ValueError 抛出，因此是 InvalidArgument。
/// 名字按原样区分大小写（0004「决策」）。
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(EngineError::invalid("collection name must not be empty"));
    }
    if name.contains(['/', '\\', '\0']) || name == "." || name == ".." {
        return Err(EngineError::invalid(format!(
            "invalid collection name: {}",
            py_repr(name)
        )));
    }
    if name.len() > 255 {
        return Err(EngineError::invalid(format!(
            "collection name too long ({} bytes, max 255)",
            name.len()
        )));
    }
    Ok(())
}
