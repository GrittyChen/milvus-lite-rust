//! schema / manifest 的原子文件提交原语；调用方持有目录变更锁或 collection flush 锁。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::storage::manifest::Manifest;

#[cfg(feature = "fault-injection")]
thread_local! {
    static FAIL_NEXT_MANIFEST_DIR_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static EXIT_AT_MANIFEST_REPLACE: std::cell::Cell<Option<CommitPhase>> = const { std::cell::Cell::new(None) };
}

/// 仅测试构建可用；发布 wheel 默认不含故障注入点。
#[cfg(feature = "fault-injection")]
pub fn inject_manifest_dir_sync_failure_once() {
    FAIL_NEXT_MANIFEST_DIR_SYNC.with(|flag| flag.set(true));
}

/// 仅测试子进程使用：模拟进程在 manifest 的逻辑提交点两侧立即消失。
#[cfg(feature = "fault-injection")]
pub fn inject_manifest_process_exit_once(phase: CommitPhase) {
    EXIT_AT_MANIFEST_REPLACE.with(|slot| slot.set(Some(phase)));
}

#[cfg(feature = "fault-injection")]
fn exit_at_manifest_replace(phase: CommitPhase) {
    let injected = EXIT_AT_MANIFEST_REPLACE.with(|slot| {
        if slot.get() == Some(phase) {
            slot.set(None);
            true
        } else {
            false
        }
    });
    if injected {
        std::process::exit(86);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitPhase {
    BeforeReplace,
    AfterReplace,
}

#[derive(Debug, thiserror::Error)]
#[error("file commit failed at {path:?} ({phase:?}): {source}")]
pub struct CommitError {
    pub path: PathBuf,
    pub phase: CommitPhase,
    #[source]
    pub source: io::Error,
}

impl CommitError {
    fn before(path: &Path, source: io::Error) -> Self {
        Self {
            path: path.to_path_buf(),
            phase: CommitPhase::BeforeReplace,
            source,
        }
    }

    fn after(path: &Path, source: io::Error) -> Self {
        Self {
            path: path.to_path_buf(),
            phase: CommitPhase::AfterReplace,
            source,
        }
    }
}

/// Linux/macOS 上目录项的持久化边界；不把 EINVAL/ENOTSUP 当成功。
pub fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// 逐级创建缺失的目录，并在每次创建之后同步其父目录。
pub fn create_dir_synced(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => return Ok(()),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "path is not a directory",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "directory has no parent"))?;
    if !parent.is_dir() {
        create_dir_synced(parent)?;
    }
    fs::create_dir(path)?;
    sync_dir(parent)
}

/// 新 schema 的 tmp → replace → 父目录 sync；通常在 staging 中调用。
pub fn write_new_schema(path: &Path, bytes: &[u8]) -> Result<(), CommitError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            return Err(CommitError::before(
                path,
                io::Error::new(io::ErrorKind::AlreadyExists, "schema already exists"),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(CommitError::before(path, error)),
    }
    let tmp = path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|source| CommitError::before(&tmp, source))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| CommitError::before(&tmp, source))?;
    drop(file);
    fs::rename(&tmp, path).map_err(|source| CommitError::before(path, source))?;
    let parent = path.parent().ok_or_else(|| {
        CommitError::after(
            path,
            io::Error::new(io::ErrorKind::InvalidInput, "schema has no parent"),
        )
    })?;
    sync_dir(parent).map_err(|source| CommitError::after(parent, source))
}

#[derive(Debug)]
pub struct ManifestCommit {
    /// 3.x 的 `.prev` 是辅助备份；复制失败不影响 manifest 的唯一真值，但必须记录。
    pub prev_copy_error: Option<io::Error>,
}

/// 与 3.x 相同的五步提交，replace 是逻辑提交点；目录 sync 失败单独标为提交后错误。
pub fn commit_manifest(dir: &Path, manifest: &Manifest) -> Result<ManifestCommit, CommitError> {
    let target = dir.join("manifest.json");
    let prev = dir.join("manifest.json.prev");
    let tmp = dir.join("manifest.json.tmp");
    let bytes = manifest
        .to_bytes()
        .map_err(|source| CommitError::before(&tmp, io::Error::other(source)))?;
    match fs::symlink_metadata(&tmp) {
        Ok(meta) if meta.file_type().is_file() => {
            fs::remove_file(&tmp).map_err(|source| CommitError::before(&tmp, source))?;
        }
        Ok(_) => {
            return Err(CommitError::before(
                &tmp,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "manifest tmp is not a regular file",
                ),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(CommitError::before(&tmp, error)),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|source| CommitError::before(&tmp, source))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| CommitError::before(&tmp, source))?;
    drop(file);
    let prev_copy_error = match fs::symlink_metadata(&target) {
        Ok(meta) if meta.file_type().is_file() => {
            match fs::symlink_metadata(&prev) {
                Ok(meta) if meta.file_type().is_file() => {}
                Ok(_) => {
                    return Err(CommitError::before(
                        &prev,
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "manifest prev is not a regular file",
                        ),
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(CommitError::before(&prev, error)),
            }
            fs::copy(&target, &prev).err()
        }
        Ok(_) => {
            return Err(CommitError::before(
                &target,
                io::Error::new(io::ErrorKind::InvalidData, "manifest is not a regular file"),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(CommitError::before(&target, error)),
    };
    #[cfg(feature = "fault-injection")]
    exit_at_manifest_replace(CommitPhase::BeforeReplace);
    fs::rename(&tmp, &target).map_err(|source| CommitError::before(&target, source))?;
    #[cfg(feature = "fault-injection")]
    exit_at_manifest_replace(CommitPhase::AfterReplace);
    #[cfg(feature = "fault-injection")]
    if FAIL_NEXT_MANIFEST_DIR_SYNC.with(|flag| flag.replace(false)) {
        return Err(CommitError::after(
            dir,
            io::Error::other("injected manifest directory sync failure"),
        ));
    }
    sync_dir(dir).map_err(|source| CommitError::after(dir, source))?;
    Ok(ManifestCommit { prev_copy_error })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_replace_preserves_prev_and_versions() {
        let root = tempfile::tempdir().unwrap();
        let mut manifest = Manifest {
            version: 1,
            ..Manifest::default()
        };
        commit_manifest(root.path(), &manifest).unwrap();
        manifest.version = 2;
        manifest.current_seq = 7;
        let report = commit_manifest(root.path(), &manifest).unwrap();
        assert!(report.prev_copy_error.is_none());
        assert_eq!(
            Manifest::read(&root.path().join("manifest.json"))
                .unwrap()
                .version,
            2
        );
        assert_eq!(
            Manifest::read(&root.path().join("manifest.json.prev"))
                .unwrap()
                .version,
            1
        );
    }

    #[test]
    fn schema_is_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("schema.json");
        write_new_schema(&target, b"first").unwrap();
        // staging 目录本来是唯一新建的；再次写同一 schema 是错误，不能改写已提交文件。
        assert!(write_new_schema(&target, b"second").is_err());
        assert_eq!(fs::read(target).unwrap(), b"first");
    }

    #[cfg(unix)]
    #[test]
    fn manifest_tmp_symlink_is_rejected_without_touching_target() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim");
        fs::write(&victim, b"unchanged").unwrap();
        std::os::unix::fs::symlink(&victim, root.path().join("manifest.json.tmp")).unwrap();
        let error = commit_manifest(root.path(), &Manifest::default()).unwrap_err();
        assert_eq!(error.phase, CommitPhase::BeforeReplace);
        assert_eq!(fs::read(victim).unwrap(), b"unchanged");
    }
}
