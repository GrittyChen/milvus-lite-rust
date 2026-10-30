//! 每个数据库 `aliases.json` 的缓存与原子写入（0023 A「别名」）。数据目录由 LOCK 独占，只有本进程写这个文件，
//! 因此加载后的缓存与磁盘一致；文件替换之后才更新缓存，替换之前失败时两者都不变。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::error::{EngineError, Result};
use crate::storage::fs_commit::{self, ReplaceError, ReplacePoint};

/// 别名到真实集合名；按码点有序，与 3.x 的 `sorted` 和 `sort_keys` 一致。
pub(crate) type Aliases = BTreeMap<String, String>;

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

/// 写入 `aliases.json`（[`fs_commit::replace_file`]），同步数据库根目录。
pub(crate) fn write(
    root: &Path,
    aliases: &Aliases,
    fault: impl Fn(ReplacePoint) -> std::io::Result<()>,
) -> std::result::Result<(), ReplaceError> {
    let value = serde_json::to_value(aliases)
        .map_err(|error| ReplaceError::Unchanged(std::io::Error::other(error)))?;
    let bytes = crate::pyjson::dump_file(&value).into_bytes();
    fs_commit::replace_file(root, "aliases.json", &bytes, fault)
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
            ReplacePoint::BeforeRename => Err(std::io::Error::other("before")),
            ReplacePoint::AfterRename => Ok(()),
        });
        assert!(matches!(before, Err(ReplaceError::Unchanged(_))));
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        let panicked = write(root.path(), &aliases, |point| match point {
            ReplacePoint::BeforeRename => Ok(()),
            ReplacePoint::AfterRename => panic!("after"),
        });
        assert!(matches!(panicked, Err(ReplaceError::Unconfirmed(_))));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"a\": \"中文\",\n  \"z\": \"c\"\n}"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        write(root.path(), &Aliases::new(), |_| Ok(())).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
    }
}
