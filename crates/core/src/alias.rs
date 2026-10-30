//! 每个数据库 `aliases.json` 的缓存与原子写入（0023 A「别名」）。数据目录由 LOCK 独占，只有本进程写这个文件，
//! 因此加载后的缓存与磁盘一致；文件替换之后才更新缓存，替换之前失败时两者都不变。

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{EngineError, Result};
use crate::storage::fs_commit;

/// 别名到真实集合名；按码点有序，与 3.x 的 `sorted` 和 `sort_keys` 一致。
pub(crate) type Aliases = BTreeMap<String, String>;

static TMP_NUMBER: AtomicU64 = AtomicU64::new(1);

/// 首次使用时读取；读取或解析失败不缓存，下次重试（3.x 的 `_aliases` 同样只缓存成功的加载）。
pub(crate) fn load<'a>(cache: &'a mut Option<Aliases>, root: &Path) -> Result<&'a mut Aliases> {
    let aliases = match cache.take() {
        Some(aliases) => aliases,
        None => read(root)?,
    };
    Ok(cache.insert(aliases))
}

/// 3.x `_load_aliases`：文件不存在或顶层不是对象时为空表，只保留字符串目标；无法解析时报错。
fn read(root: &Path) -> Result<Aliases> {
    let bytes = match fs::read(root.join("aliases.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Aliases::new()),
        Err(error) => return Err(EngineError::Storage(error.to_string())),
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| EngineError::Storage(error.to_string()))?;
    let mut aliases = Aliases::new();
    if let serde_json::Value::Object(map) = value {
        for (alias, target) in map {
            if let serde_json::Value::String(target) = target {
                aliases.insert(alias, target);
            }
        }
    }
    Ok(aliases)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WritePoint {
    BeforeRename,
    AfterRename,
}

#[derive(Debug)]
pub(crate) enum WriteError {
    /// 替换之前失败：磁盘上仍是原文件。
    Unchanged(std::io::Error),
    /// 文件已替换，数据库根目录的同步未确认（含 panic）。
    Unconfirmed(std::io::Error),
}

/// 写 tmp、fsync、改名、fsync 数据库根目录。两段分别兜住 panic：改名之后的失败必须让调用方按
/// 「已替换」收尾，不能与替换之前的失败混为一谈。
pub(crate) fn write(
    root: &Path,
    aliases: &Aliases,
    fault: impl Fn(WritePoint) -> std::io::Result<()>,
) -> std::result::Result<(), WriteError> {
    crate::db::guarded_io(|| replace(root, aliases, &fault)).map_err(WriteError::Unchanged)?;
    crate::db::guarded_io(|| {
        fault(WritePoint::AfterRename)?;
        fs_commit::sync_dir(root)
    })
    .map_err(WriteError::Unconfirmed)
}

fn replace(
    root: &Path,
    aliases: &Aliases,
    fault: &impl Fn(WritePoint) -> std::io::Result<()>,
) -> std::io::Result<()> {
    fault(WritePoint::BeforeRename)?;
    let bytes =
        crate::pyjson::dump_file(&serde_json::to_value(aliases).map_err(std::io::Error::other)?)
            .into_bytes();
    let path = root.join("aliases.json");
    let tmp = root.join(format!(
        "aliases.json.tmp.{}.{}",
        std::process::id(),
        TMP_NUMBER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        drop(file);
        remove_tmp(&tmp);
        return Err(error);
    }
    drop(file);
    if let Err(error) = fs::rename(&tmp, &path) {
        remove_tmp(&tmp);
        return Err(error);
    }
    Ok(())
}

fn remove_tmp(tmp: &Path) {
    if let Err(cleanup) = fs::remove_file(tmp) {
        eprintln!("cannot clean failed aliases temp file {tmp:?}: {cleanup}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_failures_are_not_cached_and_successful_loads_are() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("aliases.json");
        let mut cache = None;
        assert!(load(&mut cache, root.path()).unwrap().is_empty());
        std::fs::write(&path, br#"{"a":"c"}"#).unwrap();
        // 已缓存：本进程是唯一写者，不再读盘。
        assert!(load(&mut cache, root.path()).unwrap().is_empty());

        let mut cache = None;
        std::fs::write(&path, b"{").unwrap();
        assert!(matches!(
            load(&mut cache, root.path()),
            Err(EngineError::Storage(_))
        ));
        assert!(cache.is_none());
        std::fs::write(&path, br#"{"b":"t","a":"c","n":1,"x":null}"#).unwrap();
        assert_eq!(
            load(&mut cache, root.path()).unwrap(),
            &Aliases::from([("a".into(), "c".into()), ("b".into(), "t".into())])
        );

        for non_object in [&b"[]"[..], b"\"s\"", b"1", b"null"] {
            std::fs::write(&path, non_object).unwrap();
            assert!(load(&mut None, root.path()).unwrap().is_empty());
        }
    }

    #[test]
    fn write_reports_whether_the_file_was_replaced() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("aliases.json");
        std::fs::write(&path, b"old").unwrap();
        let aliases = Aliases::from([("z".into(), "c".into()), ("a".into(), "中文".into())]);
        let before = write(root.path(), &aliases, |point| match point {
            WritePoint::BeforeRename => Err(std::io::Error::other("before")),
            WritePoint::AfterRename => Ok(()),
        });
        assert!(matches!(before, Err(WriteError::Unchanged(_))));
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        let panicked = write(root.path(), &aliases, |point| match point {
            WritePoint::BeforeRename => Ok(()),
            WritePoint::AfterRename => panic!("after"),
        });
        assert!(matches!(panicked, Err(WriteError::Unconfirmed(_))));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"a\": \"中文\",\n  \"z\": \"c\"\n}"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        write(root.path(), &Aliases::new(), |_| Ok(())).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
    }
}
