//! 改名的持久化意图（0023 A「改名」）：私有文件，放在数据库根目录，记录旧名与新名。
//! 当前实例的重试与重开时的处理都按它把改名收尾到「改名前」或「改名后」之一。

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::fs_commit::{self, ReplaceError, ReplacePoint, guarded_io};

pub const INTENT_FILE: &str = "milvus-lite-rust.rename-intent.json";
const OPERATION: &str = "rename_collection";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RenameIntent {
    pub(crate) old: String,
    pub(crate) new: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    format_version: u32,
    operation: String,
    old: String,
    new: String,
}

#[derive(Debug)]
pub(crate) enum ReadError {
    /// 可随环境恢复，下一次受限操作重试。
    Io(io::Error),
    /// 损坏或版本未知：不自动处理，需人工恢复。
    Invalid(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "cannot read rename intent: {error}"),
            Self::Invalid(reason) => write!(f, "invalid rename intent: {reason}"),
        }
    }
}

pub(crate) fn read(root: &Path) -> Result<Option<RenameIntent>, ReadError> {
    let path = root.join(INTENT_FILE);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => return Err(ReadError::Invalid("not a regular file".to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ReadError::Io(error)),
    }
    let bytes = fs::read(&path).map_err(ReadError::Io)?;
    parse(&bytes).map(Some).map_err(ReadError::Invalid)
}

fn parse(bytes: &[u8]) -> Result<RenameIntent, String> {
    let marker: Marker = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if marker.format_version != 1 {
        return Err(format!(
            "unsupported format_version {}",
            marker.format_version
        ));
    }
    if marker.operation != OPERATION {
        return Err(format!("unknown operation {:?}", marker.operation));
    }
    for name in [&marker.old, &marker.new] {
        crate::db::validate_filesystem_name(name, "collection")
            .map_err(|error| error.to_string())?;
    }
    if marker.old == marker.new {
        return Err("old and new names are equal".to_owned());
    }
    Ok(RenameIntent {
        old: marker.old,
        new: marker.new,
    })
}

/// 经 [`fs_commit::replace_file`] 写入并同步数据库根目录。
pub(crate) fn write(
    root: &Path,
    intent: &RenameIntent,
    fault: impl Fn(ReplacePoint) -> io::Result<()>,
) -> Result<(), ReplaceError> {
    let bytes = serde_json::to_vec(&Marker {
        format_version: 1,
        operation: OPERATION.to_owned(),
        old: intent.old.clone(),
        new: intent.new.clone(),
    })
    .map_err(|error| ReplaceError::Unchanged(io::Error::other(error)))?;
    fs_commit::replace_file(root, INTENT_FILE, &bytes, fault)
}

/// 删除意图并同步数据库根目录；已不存在也同步，确认上一次未确认的删除。`fault` 的两点分别在删除之前与之后。
pub(crate) fn remove(
    root: &Path,
    fault: impl Fn(ReplacePoint) -> io::Result<()>,
) -> io::Result<()> {
    guarded_io(|| {
        fault(ReplacePoint::BeforeRename)?;
        match fs::remove_file(root.join(INTENT_FILE)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fault(ReplacePoint::AfterRename)?;
        fs_commit::sync_dir(root)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent() -> RenameIntent {
        RenameIntent {
            old: "a".into(),
            new: "中文".into(),
        }
    }

    #[test]
    fn written_intent_reads_back_and_removal_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        assert!(read(root.path()).unwrap().is_none());
        write(root.path(), &intent(), |_| Ok(())).unwrap();
        assert_eq!(read(root.path()).unwrap(), Some(intent()));
        remove(root.path(), |_| Ok(())).unwrap();
        remove(root.path(), |_| Ok(())).unwrap();
        assert!(read(root.path()).unwrap().is_none());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn every_malformed_field_is_invalid() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(INTENT_FILE);
        let long = "x".repeat(256);
        for text in [
            "",
            "{",
            "[]",
            r#"{"operation":"rename_collection","old":"a","new":"b"}"#,
            r#"{"format_version":1,"old":"a","new":"b"}"#,
            r#"{"format_version":1,"operation":"rename_collection","new":"b"}"#,
            r#"{"format_version":1,"operation":"rename_collection","old":"a"}"#,
            r#"{"format_version":1,"operation":"rename_collection","old":"a","new":"b","x":1}"#,
            r#"{"format_version":2,"operation":"rename_collection","old":"a","new":"b"}"#,
            r#"{"format_version":1.0,"operation":"rename_collection","old":"a","new":"b"}"#,
            r#"{"format_version":"1","operation":"rename_collection","old":"a","new":"b"}"#,
            r#"{"format_version":1,"operation":"truncate_collection","old":"a","new":"b"}"#,
            r#"{"format_version":1,"operation":"rename_collection","old":1,"new":"b"}"#,
            r#"{"format_version":1,"operation":"rename_collection","old":"a","new":"a"}"#,
            r#"{"format_version":1,"operation":"rename_collection","old":"","new":"b"}"#,
            r#"{"format_version":1,"operation":"rename_collection","old":"a","new":"../b"}"#,
            r#"{"format_version":1,"operation":"rename_collection","old":"..","new":"b"}"#,
            r#"{"format_version":1,"operation":"rename_collection","old":"a\\b","new":"b"}"#,
            r#"{"format_version":1,"operation":"rename_collection","old":"a\u0000","new":"b"}"#,
        ]
        .into_iter()
        .map(str::to_owned)
        .chain([format!(
            r#"{{"format_version":1,"operation":"rename_collection","old":"a","new":"{long}"}}"#
        )]) {
            std::fs::write(&path, &text).unwrap();
            assert!(
                matches!(read(root.path()), Err(ReadError::Invalid(_))),
                "{text}"
            );
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(read(root.path()), Err(ReadError::Invalid(_))));
    }

    #[test]
    fn faults_report_whether_the_intent_file_changed() {
        let root = tempfile::tempdir().unwrap();
        let before = write(root.path(), &intent(), |point| match point {
            ReplacePoint::BeforeRename => Err(io::Error::other("before")),
            ReplacePoint::AfterRename => Ok(()),
        });
        assert!(matches!(before, Err(ReplaceError::Unchanged(_))));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        let after = write(root.path(), &intent(), |point| match point {
            ReplacePoint::BeforeRename => Ok(()),
            ReplacePoint::AfterRename => panic!("after"),
        });
        assert!(matches!(after, Err(ReplaceError::Unconfirmed(_))));
        assert_eq!(read(root.path()).unwrap(), Some(intent()));

        assert!(
            remove(root.path(), |point| match point {
                ReplacePoint::BeforeRename => Err(io::Error::other("before")),
                ReplacePoint::AfterRename => Ok(()),
            })
            .is_err()
        );
        assert_eq!(read(root.path()).unwrap(), Some(intent()));
        assert!(
            remove(root.path(), |point| match point {
                ReplacePoint::BeforeRename => Ok(()),
                ReplacePoint::AfterRename => panic!("after"),
            })
            .is_err()
        );
        assert!(read(root.path()).unwrap().is_none());
    }
}
