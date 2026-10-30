//! schema / manifest 的原子文件提交原语；调用方持有目录变更锁或 collection flush 锁。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::storage::manifest::Manifest;

#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsAction {
    SyncFile,
    SyncDir,
    Rename,
    Unlink,
}
#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone)]
pub struct FsEvent {
    pub action: FsAction,
    pub path: PathBuf,
}
#[cfg(feature = "fault-injection")]
thread_local! { static FS_TRACE: std::cell::RefCell<Option<Vec<FsEvent>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(feature = "fault-injection")]
pub fn begin_fs_trace() {
    FS_TRACE.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(Vec::new());
    });
}
#[cfg(feature = "fault-injection")]
pub fn finish_fs_trace() -> Vec<FsEvent> {
    FS_TRACE.with(|slot| match slot.borrow_mut().take() {
        Some(events) => events,
        None => panic!("filesystem trace was not started"),
    })
}
#[cfg(feature = "fault-injection")]
pub(crate) fn record_fs(action: FsAction, path: &Path) {
    FS_TRACE.with(|slot| {
        if let Some(events) = slot.borrow_mut().as_mut() {
            events.push(FsEvent {
                action,
                path: path.to_path_buf(),
            });
        }
    });
}

pub(crate) fn sync_file(file: &File, path: &Path) -> io::Result<()> {
    file.sync_all()?;
    #[cfg(feature = "fault-injection")]
    record_fs(FsAction::SyncFile, path);
    #[cfg(not(feature = "fault-injection"))]
    let _ = path;
    Ok(())
}

#[cfg(feature = "fault-injection")]
thread_local! {
    static FAIL_NEXT_MANIFEST_DIR_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static EXIT_AT_MANIFEST_REPLACE: std::cell::Cell<Option<CommitPhase>> = const { std::cell::Cell::new(None) };
    static PANIC_AT_MANIFEST_REPLACE: std::cell::Cell<Option<CommitPhase>> = const { std::cell::Cell::new(None) };
}

#[cfg(feature = "fault-injection")]
pub fn inject_manifest_panic_once(phase: CommitPhase) {
    PANIC_AT_MANIFEST_REPLACE.with(|slot| slot.set(Some(phase)));
}
#[cfg(feature = "fault-injection")]
fn panic_at_manifest_replace(phase: CommitPhase) {
    if PANIC_AT_MANIFEST_REPLACE.with(|slot| {
        if slot.get() == Some(phase) {
            slot.set(None);
            true
        } else {
            false
        }
    }) {
        panic!("injected manifest commit panic: {phase:?}");
    }
}

#[cfg(feature = "fault-injection")]
type ManifestPause = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
#[cfg(feature = "fault-injection")]
thread_local! { static AFTER_REPLACE_PAUSE: std::cell::RefCell<Option<ManifestPause>> = const { std::cell::RefCell::new(None) }; }
#[cfg(feature = "fault-injection")]
pub fn pause_manifest_after_replace_once(
    entered: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
) {
    AFTER_REPLACE_PAUSE.with(|slot| *slot.borrow_mut() = Some((entered, resume)));
}
#[cfg(feature = "fault-injection")]
fn manifest_after_replace_pause() -> io::Result<()> {
    let pause = AFTER_REPLACE_PAUSE.with(|slot| slot.borrow_mut().take());
    if let Some((entered, resume)) = pause {
        entered.send(()).map_err(io::Error::other)?;
        resume
            .recv_timeout(std::time::Duration::from_secs(30))
            .map_err(io::Error::other)?;
    }
    Ok(())
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

/// 把 panic 转成 I/O 错误，让调用方按所在阶段（替换之前 / 之后）收尾。
pub(crate) fn guarded_io<T>(action: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)).unwrap_or_else(|payload| {
        Err(io::Error::other(format!(
            "directory operation panicked: {}",
            crate::error::panic_description(payload.as_ref())
        )))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplacePoint {
    BeforeRename,
    AfterRename,
}

#[derive(Debug)]
pub(crate) enum ReplaceError {
    /// 替换之前失败：磁盘上仍是原文件。
    Unchanged(io::Error),
    /// 文件已替换，所在目录的同步未确认（含 panic）。
    Unconfirmed(io::Error),
}

static REPLACE_TMP_NUMBER: AtomicU64 = AtomicU64::new(1);

/// 共享 JSON 文件（`aliases.json`、`schema.json`）的替换：写 tmp、fsync、改名、fsync 所在目录。
/// 两段分别兜住 panic：改名之后的失败必须让调用方按「已替换」收尾，不能与替换之前的失败混为一谈。
pub(crate) fn replace_file(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    fault: impl Fn(ReplacePoint) -> io::Result<()>,
) -> Result<(), ReplaceError> {
    guarded_io(|| {
        fault(ReplacePoint::BeforeRename)?;
        let tmp = dir.join(format!(
            "{name}.tmp.{}.{}",
            std::process::id(),
            REPLACE_TMP_NUMBER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        if let Err(error) = file.write_all(bytes).and_then(|()| sync_file(&file, &tmp)) {
            drop(file);
            remove_tmp(&tmp);
            return Err(error);
        }
        drop(file);
        if let Err(error) = fs::rename(&tmp, dir.join(name)) {
            remove_tmp(&tmp);
            return Err(error);
        }
        Ok(())
    })
    .map_err(ReplaceError::Unchanged)?;
    guarded_io(|| {
        fault(ReplacePoint::AfterRename)?;
        sync_dir(dir)
    })
    .map_err(ReplaceError::Unconfirmed)
}

fn remove_tmp(tmp: &Path) {
    if let Err(cleanup) = fs::remove_file(tmp) {
        eprintln!("cannot clean failed temp file {tmp:?}: {cleanup}");
    }
}

/// Linux/macOS 上目录项的持久化边界；不把 EINVAL/ENOTSUP 当成功。
pub fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()?;
    #[cfg(feature = "fault-injection")]
    record_fs(FsAction::SyncDir, path);
    Ok(())
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
        .and_then(|()| sync_file(&file, &tmp))
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

/// 与 3.x 相同的五步提交；调用方也可在删除旧索引之前单独完成准备。
pub fn commit_manifest(dir: &Path, manifest: &Manifest) -> Result<ManifestCommit, CommitError> {
    let mut prepared = prepare_manifest(dir, manifest)?;
    match prepared.replace() {
        Ok(report) => Ok(report),
        Err(error) => Err(prepared.abort(error)),
    }
}

#[derive(Debug)]
pub struct PreparedManifest {
    dir: PathBuf,
    tmp_owned: bool,
    prev_copy_error: Option<io::Error>,
}

pub fn prepare_manifest(dir: &Path, manifest: &Manifest) -> Result<PreparedManifest, CommitError> {
    let mut prepared = PreparedManifest {
        dir: dir.to_owned(),
        tmp_owned: false,
        prev_copy_error: None,
    };
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prepared.prepare(manifest)));
    match result {
        Ok(Ok(())) => Ok(prepared),
        Ok(Err(error)) => Err(prepared.abort(error)),
        Err(payload) => {
            let error = CommitError::before(
                &dir.join("manifest.json.tmp"),
                io::Error::other(format!(
                    "manifest preparation panicked: {}",
                    crate::error::panic_description(payload.as_ref())
                )),
            );
            Err(prepared.abort(error))
        }
    }
}

impl PreparedManifest {
    pub fn tmp_path(&self) -> PathBuf {
        self.dir.join("manifest.json.tmp")
    }

    fn prepare(&mut self, manifest: &Manifest) -> Result<(), CommitError> {
        let target = self.dir.join("manifest.json");
        let prev = self.dir.join("manifest.json.prev");
        let tmp = self.tmp_path();
        let bytes = manifest
            .to_bytes()
            .map_err(|e| CommitError::before(&tmp, io::Error::other(e)))?;
        let target_exists = regular_optional(&target)?;
        if target_exists {
            regular_optional(&prev)?;
        }
        if regular_optional(&tmp)? {
            fs::remove_file(&tmp).map_err(|e| CommitError::before(&tmp, e))?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| CommitError::before(&tmp, e))?;
        self.tmp_owned = true;
        #[cfg(feature = "fault-injection")]
        super::index_intent::fault(super::index_intent::IndexPoint::ManifestWrite)
            .map_err(|e| CommitError::before(&tmp, e))?;
        file.write_all(&bytes)
            .map_err(|e| CommitError::before(&tmp, e))?;
        #[cfg(feature = "fault-injection")]
        super::index_intent::fault(super::index_intent::IndexPoint::ManifestSync)
            .map_err(|e| CommitError::before(&tmp, e))?;
        sync_file(&file, &tmp).map_err(|e| CommitError::before(&tmp, e))?;
        drop(file);
        if target_exists {
            self.prev_copy_error = fs::copy(target, prev).err();
        }
        Ok(())
    }

    pub fn replace(&mut self) -> Result<ManifestCommit, CommitError> {
        let mut replaced = false;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let target = self.dir.join("manifest.json");
            #[cfg(feature = "fault-injection")]
            exit_at_manifest_replace(CommitPhase::BeforeReplace);
            #[cfg(feature = "fault-injection")]
            panic_at_manifest_replace(CommitPhase::BeforeReplace);
            fs::rename(self.tmp_path(), &target).map_err(|e| CommitError::before(&target, e))?;
            replaced = true;
            self.tmp_owned = false;
            #[cfg(feature = "fault-injection")]
            record_fs(FsAction::Rename, &target);
            #[cfg(feature = "fault-injection")]
            exit_at_manifest_replace(CommitPhase::AfterReplace);
            #[cfg(feature = "fault-injection")]
            panic_at_manifest_replace(CommitPhase::AfterReplace);
            #[cfg(feature = "fault-injection")]
            if FAIL_NEXT_MANIFEST_DIR_SYNC.with(|flag| flag.replace(false)) {
                return Err(CommitError::after(
                    &self.dir,
                    io::Error::other("injected manifest directory sync failure"),
                ));
            }
            #[cfg(feature = "fault-injection")]
            manifest_after_replace_pause().map_err(|e| CommitError::after(&self.dir, e))?;
            sync_dir(&self.dir).map_err(|e| CommitError::after(&self.dir, e))?;
            Ok(ManifestCommit {
                prev_copy_error: self.prev_copy_error.take(),
            })
        }));
        match result {
            Ok(result) => result,
            Err(payload) => Err(CommitError {
                path: self.dir.join("manifest.json"),
                phase: if replaced {
                    CommitPhase::AfterReplace
                } else {
                    CommitPhase::BeforeReplace
                },
                source: io::Error::other(format!(
                    "manifest commit panicked: {}",
                    crate::error::panic_description(payload.as_ref())
                )),
            }),
        }
    }

    pub fn cleanup(&mut self) -> io::Result<()> {
        if self.tmp_owned {
            let tmp = self.tmp_path();
            match fs::remove_file(&tmp) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            self.tmp_owned = false;
        }
        Ok(())
    }

    fn abort(&mut self, mut error: CommitError) -> CommitError {
        if let Err(cleanup) = self.cleanup() {
            error.source = io::Error::other(format!(
                "{}; manifest tmp cleanup failed: {cleanup}",
                error.source
            ));
        }
        error
    }
}

impl Drop for PreparedManifest {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!(
                "manifest tmp cleanup failed for {:?}: {error}",
                self.tmp_path()
            );
        }
    }
}

fn regular_optional(path: &Path) -> Result<bool, CommitError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => Ok(true),
        Ok(_) => Err(CommitError::before(
            path,
            io::Error::new(
                io::ErrorKind::InvalidData,
                "manifest path is not a regular file",
            ),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(CommitError::before(path, e)),
    }
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

    /// 替换之前的失败（钩子、改名）共用同一出口：磁盘上仍是原文件、不留临时文件；临时文件的打开、写入与同步
    /// 失败与改名同在 `replace_file` 的第一个闭包内、走同一分支，没有单独注入。只有改名之后的失败才是
    /// 「已替换、未确认」。清空清理与别名、改名、Alter 的重试语义都依赖这一区分。
    #[test]
    fn replace_file_reports_unchanged_before_the_rename_and_unconfirmed_after() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("f.json");
        fs::write(&target, b"old").unwrap();
        let leftovers = || {
            fs::read_dir(root.path())
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains(".tmp")
                })
                .count()
        };
        let hook = replace_file(root.path(), "f.json", b"new", |point| match point {
            ReplacePoint::BeforeRename => Err(io::Error::other("hook")),
            ReplacePoint::AfterRename => Ok(()),
        });
        assert!(matches!(hook, Err(ReplaceError::Unchanged(_))));
        assert_eq!(
            (fs::read(&target).unwrap(), leftovers()),
            (b"old".to_vec(), 0)
        );

        // 改名目标是非空目录：写入与同步都成功，改名失败，临时文件被清掉。
        let blocked = root.path().join("d.json");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("x"), b"x").unwrap();
        let renamed = replace_file(root.path(), "d.json", b"new", |_| Ok(()));
        assert!(matches!(renamed, Err(ReplaceError::Unchanged(_))));
        assert!(blocked.is_dir());
        assert_eq!(leftovers(), 0);

        let after = replace_file(root.path(), "f.json", b"new", |point| match point {
            ReplacePoint::BeforeRename => Ok(()),
            ReplacePoint::AfterRename => Err(io::Error::other("hook")),
        });
        assert!(matches!(after, Err(ReplaceError::Unconfirmed(_))));
        assert_eq!(
            (fs::read(&target).unwrap(), leftovers()),
            (b"new".to_vec(), 0)
        );
    }
}
