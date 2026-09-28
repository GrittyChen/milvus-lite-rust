//! 数据目录的打开：创建目录、独占 `LOCK`。collection 的格式在首次访问时校验。

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

pub const LOCK_FILENAME: &str = "LOCK";

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
        cleanup_staging(path).map_err(io_err)?;
        Ok(Self {
            path: path.to_path_buf(),
            _lock: lock,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn cleanup_staging(data_dir: &Path) -> io::Result<()> {
    let staging = data_dir.join(".database-staging");
    match fs::symlink_metadata(&staging) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "database staging path is not a directory",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    for entry in fs::read_dir(&staging)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    crate::storage::fs_commit::sync_dir(&staging)
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
        for d in ["collections", "databases", ".database-staging"] {
            fs::create_dir(root.path().join(d)).unwrap();
        }
        File::create(root.path().join(LOCK_FILENAME)).unwrap();
        DataDir::open(root.path()).unwrap();
    }

    #[test]
    fn removes_staging_crash_residue_after_taking_lock() {
        let root = tmp();
        let staging = root.path().join(".database-staging");
        fs::create_dir_all(staging.join("database-x/collections")).unwrap();
        File::create(staging.join("orphan.tmp")).unwrap();
        DataDir::open(root.path()).unwrap();
        assert!(fs::read_dir(&staging).unwrap().next().is_none());
    }

    #[test]
    fn rejects_staging_path_that_is_not_a_directory() {
        let root = tmp();
        File::create(root.path().join(".database-staging")).unwrap();
        assert!(matches!(
            DataDir::open(root.path()),
            Err(DataDirError::Io { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn staging_cleanup_unlinks_symlinks_without_touching_their_targets() {
        let root = tmp();
        let outside = tmp();
        let sentinel = outside.path().join("keep");
        File::create(&sentinel).unwrap();
        let staging = root.path().join(".database-staging");
        fs::create_dir(&staging).unwrap();
        std::os::unix::fs::symlink(outside.path(), staging.join("link")).unwrap();
        DataDir::open(root.path()).unwrap();
        assert!(sentinel.exists());
        assert!(fs::read_dir(&staging).unwrap().next().is_none());
    }

    #[test]
    fn existing_entries_are_left_for_lazy_collection_validation() {
        let root = tmp();
        File::create(root.path().join("aliases.json")).unwrap();
        File::create(root.path().join("collections")).unwrap();
        DataDir::open(root.path()).unwrap();
        assert!(root.path().join("aliases.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn leaves_symlinked_entries_untouched() {
        let root = tmp();
        let target = tmp();
        std::os::unix::fs::symlink(target.path(), root.path().join("collections")).unwrap();
        DataDir::open(root.path()).unwrap();
        assert!(
            fs::symlink_metadata(root.path().join("collections"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
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
    fn lock_release_does_not_depend_on_existing_entries() {
        let root = tmp();
        File::create(root.path().join("schema.json")).unwrap();
        drop(DataDir::open(root.path()).unwrap());
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
