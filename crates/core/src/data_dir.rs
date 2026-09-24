//! 数据目录的打开：创建目录、独占 `LOCK`、拒绝尚不支持读取的已有内容。

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

pub const LOCK_FILENAME: &str = "LOCK";

/// 被 3.x 打开过、但没写过数据的目录里只会有这几个空目录（见 0003「同路径复用」）。
/// 它们非空时一律视为已有数据：`.database-staging/` 有残留条目意味着 3.x 的数据库操作中途崩溃过。
const EMPTY_DIRS_LEFT_BY_3X: [&str; 3] = ["collections", "databases", ".database-staging"];

#[derive(Debug, thiserror::Error)]
pub enum DataDirError {
    #[error("failed to open data directory {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("another process holds the lock on {path:?}")]
    Locked { path: PathBuf },
    #[error(
        "data directory {path:?} contains existing data ({entry:?}); \
         this build keeps data in memory only and cannot read existing data yet"
    )]
    ExistingData { path: PathBuf, entry: String },
}

/// 持有期间独占数据目录；析构时关闭 `LOCK` 的 fd，锁随之释放。
///
/// 永不删除或重建 `LOCK` 文件：否则两个进程会锁在不同的 inode 上（0002 约束 10）。
#[derive(Debug)]
pub struct DataDir {
    path: PathBuf,
    _lock: File,
}

impl DataDir {
    pub fn open(path: &Path) -> Result<Self, DataDirError> {
        let io_err = |source| DataDirError::Io {
            path: path.to_path_buf(),
            source,
        };
        fs::create_dir_all(path).map_err(io_err)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.join(LOCK_FILENAME))
            .map_err(io_err)?;
        // std 在 Unix 上用 flock、在 Windows 上用覆盖第 0 字节的 LockFileEx，
        // 与 3.x 的 fcntl.flock / msvcrt.locking 互斥（0002 约束 10）。
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(DataDirError::Locked {
                    path: path.to_path_buf(),
                });
            }
            Err(TryLockError::Error(e)) => return Err(io_err(e)),
        }
        // 加锁之后再检查，检查期间不会有 3.x 进程同时写入。
        check_no_existing_data(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            _lock: lock,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn check_no_existing_data(path: &Path) -> Result<(), DataDirError> {
    let io_err = |source| DataDirError::Io {
        path: path.to_path_buf(),
        source,
    };
    for entry in fs::read_dir(path).map_err(io_err)? {
        let entry = entry.map_err(io_err)?;
        let name = entry.file_name();
        // DirEntry::file_type 不跟随符号链接：链接出去的目录一律按已有数据处理，fail-closed。
        let file_type = entry.file_type().map_err(io_err)?;
        let allowed = if name == LOCK_FILENAME {
            file_type.is_file()
        } else if EMPTY_DIRS_LEFT_BY_3X.iter().any(|d| name == *d) {
            file_type.is_dir() && fs::read_dir(entry.path()).map_err(io_err)?.next().is_none()
        } else {
            false
        };
        if !allowed {
            return Err(DataDirError::ExistingData {
                path: path.to_path_buf(),
                entry: name.to_string_lossy().into_owned(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn creates_missing_dir_with_only_lock() {
        let root = tmp();
        let path = root.path().join("x.db");
        let dir = DataDir::open(&path).unwrap();
        assert_eq!(dir.path(), path);
        let names: Vec<_> = fs::read_dir(&path)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [LOCK_FILENAME]);
    }

    #[test]
    fn accepts_empty_dir_left_by_3x() {
        let root = tmp();
        for d in EMPTY_DIRS_LEFT_BY_3X {
            fs::create_dir(root.path().join(d)).unwrap();
        }
        File::create(root.path().join(LOCK_FILENAME)).unwrap();
        DataDir::open(root.path()).unwrap();
    }

    #[test]
    fn rejects_non_empty_whitelisted_dirs() {
        for d in EMPTY_DIRS_LEFT_BY_3X {
            let root = tmp();
            fs::create_dir_all(root.path().join(d).join("leftover")).unwrap();
            let err = DataDir::open(root.path()).unwrap_err();
            assert!(
                matches!(&err, DataDirError::ExistingData { entry, .. } if entry == d),
                "{err}"
            );
        }
    }

    #[test]
    fn rejects_other_entries() {
        let root = tmp();
        File::create(root.path().join("aliases.json")).unwrap();
        let err = DataDir::open(root.path()).unwrap_err();
        assert!(matches!(err, DataDirError::ExistingData { .. }), "{err}");
    }

    #[test]
    fn rejects_whitelisted_name_with_wrong_type() {
        let root = tmp();
        File::create(root.path().join("collections")).unwrap();
        let err = DataDir::open(root.path()).unwrap_err();
        assert!(matches!(err, DataDirError::ExistingData { .. }), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_whitelisted_dir() {
        let root = tmp();
        let target = tmp();
        std::os::unix::fs::symlink(target.path(), root.path().join("collections")).unwrap();
        let err = DataDir::open(root.path()).unwrap_err();
        assert!(matches!(err, DataDirError::ExistingData { .. }), "{err}");
    }

    #[test]
    fn second_open_in_same_process_is_locked_until_first_dropped() {
        let root = tmp();
        let first = DataDir::open(root.path()).unwrap();
        let err = DataDir::open(root.path()).unwrap_err();
        assert!(matches!(err, DataDirError::Locked { .. }), "{err}");
        drop(first);
        DataDir::open(root.path()).unwrap();
    }

    #[test]
    fn existing_data_error_releases_lock() {
        let root = tmp();
        File::create(root.path().join("schema.json")).unwrap();
        DataDir::open(root.path()).unwrap_err();
        let lock = File::open(root.path().join(LOCK_FILENAME)).unwrap();
        lock.try_lock().unwrap();
    }

    #[test]
    fn path_is_a_file() {
        let root = tmp();
        let file = root.path().join("x.db");
        File::create(&file).unwrap();
        let err = DataDir::open(&file).unwrap_err();
        assert!(matches!(err, DataDirError::Io { .. }), "{err}");
    }
}
