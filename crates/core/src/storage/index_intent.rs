//! 索引删除的持久化意图；只有已证明安全的清理允许在当前实例重试。

use super::{fs_commit, index_platform};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const INTENT_FILE: &str = "milvus-lite-rust.index-intent.json";
const INTENT_TMP: &str = "milvus-lite-rust.index-intent.json.tmp";

#[derive(Clone, Debug, Default)]
pub(crate) enum IntentState {
    #[default]
    Clear,
    Unconfirmed(String),
    CleanupPending(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootSource {
    LinuxKernelProcfs,
    DarwinKernBootsessionuuid,
}

#[derive(Clone, Debug, Default)]
pub struct BootContext {
    pub boot_id: Option<String>,
    pub boot_source: Option<BootSource>,
    pub local: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    format_version: u32,
    operation: String,
    field_names: Vec<String>,
    files: Vec<String>,
    boot_id: Option<String>,
    boot_source: Option<BootSource>,
    auto_recovery: bool,
}

impl Marker {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        for key in [
            "format_version",
            "operation",
            "field_names",
            "files",
            "boot_id",
            "boot_source",
            "auto_recovery",
        ] {
            if value.get(key).is_none() {
                return Err(format!("missing field {key}"));
            }
        }
        let marker: Self = serde_json::from_value(value).map_err(|e| e.to_string())?;
        if marker.format_version != 1
            || !matches!(marker.operation.as_str(), "create_index" | "drop_index")
            || marker.field_names.is_empty()
            || marker.field_names.iter().any(String::is_empty)
            || marker.files.is_empty()
            || marker.files.iter().any(|p| !valid_sidecar_path(p))
            || marker.boot_id.is_some() != marker.boot_source.is_some()
            || marker.boot_id.as_ref().is_some_and(|id| !valid_uuid(id))
            || (marker.auto_recovery && marker.boot_id.is_none())
        {
            return Err("invalid index deletion intent fields".to_owned());
        }
        Ok(marker)
    }
}

pub(crate) fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}

fn valid_sidecar_path(path: &str) -> bool {
    let parts: Vec<_> = path.split('/').collect();
    parts.len() == 4
        && parts[0] == "partitions"
        && super::partition_paths::valid_name(parts[1])
        && parts[2] == "indexes"
        && !parts[3].contains(['\\', ':'])
        && crate::index::parse_sidecar_name(parts[3]).is_some()
}

fn read_marker(dir: &Path) -> Result<Option<Vec<u8>>, String> {
    let path = dir.join(INTENT_FILE);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => fs::read(&path)
            .map(Some)
            .map_err(|e| format!("{path:?}: {e}")),
        Ok(_) => Err(format!(
            "index deletion intent is not a regular file: {path:?}"
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{path:?}: {e}")),
    }
}

fn blocked(dir: &Path, reason: &str) -> String {
    format!(
        "index changes are blocked by {:?}: {reason}; after the system hosting the data directory has rebooted, stop the service, remove this marker and reopen the service",
        dir.join(INTENT_FILE)
    )
}

impl IntentState {
    pub(crate) fn check(&mut self, dir: &Path) -> Result<(), String> {
        match self {
            Self::Unconfirmed(reason) => return Err(reason.clone()),
            Self::CleanupPending(_) => return self.cleanup(dir),
            Self::Clear => {}
        }
        let bytes = match read_marker(dir) {
            Ok(None) => {
                clean_tmp(dir)?;
                return Ok(());
            }
            Ok(Some(bytes)) => bytes,
            Err(e) => {
                let reason = blocked(dir, &e);
                *self = Self::Unconfirmed(reason.clone());
                return Err(reason);
            }
        };
        let marker = match Marker::parse(&bytes) {
            Ok(marker) => marker,
            Err(e) => {
                let reason = blocked(dir, &format!("invalid marker: {e}"));
                *self = Self::Unconfirmed(reason.clone());
                return Err(reason);
            }
        };
        let current = boot_context(dir, marker.files.iter().map(|path| dir.join(path)));
        if marker.auto_recovery
            && current.local
            && marker.boot_source == current.boot_source
            && current.boot_id.is_some()
            && marker.boot_id != current.boot_id
        {
            *self = Self::CleanupPending(bytes);
            self.cleanup(dir)
        } else {
            let reason = blocked(
                dir,
                "index deletion durability is unconfirmed; no trusted kernel reboot has been established",
            );
            *self = Self::Unconfirmed(reason.clone());
            Err(reason)
        }
    }

    pub(crate) fn persist(
        &mut self,
        dir: &Path,
        operation: &str,
        fields: Vec<String>,
        files: &[PathBuf],
    ) -> Result<(), String> {
        let context = boot_context(dir, files.iter().cloned());
        let marker = Marker {
            format_version: 1,
            operation: operation.to_owned(),
            field_names: fields,
            files: files
                .iter()
                .map(|p| {
                    p.strip_prefix(dir)
                        .map_err(|e| e.to_string())
                        .and_then(|r| {
                            r.to_str()
                                .map(str::to_owned)
                                .ok_or_else(|| "index path is not UTF-8".to_owned())
                        })
                })
                .collect::<Result<_, _>>()?,
            auto_recovery: context.local && context.boot_id.is_some(),
            boot_id: context.boot_id,
            boot_source: context.boot_source,
        };
        let bytes = serde_json::to_vec(&marker).map_err(|e| e.to_string())?;
        Marker::parse(&bytes)?;
        let tmp = dir.join(INTENT_TMP);
        let target = dir.join(INTENT_FILE);
        let mut owned = false;
        let result = guarded(|| {
            clean_tmp(dir)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|e| format!("{tmp:?}: {e}"))?;
            owned = true;
            #[cfg(feature = "fault-injection")]
            fault(IndexPoint::MarkerWrite).map_err(|e| e.to_string())?;
            file.write_all(&bytes).map_err(|e| e.to_string())?;
            #[cfg(feature = "fault-injection")]
            fault(IndexPoint::MarkerSync).map_err(|e| e.to_string())?;
            fs_commit::sync_file(&file, &tmp).map_err(|e| e.to_string())?;
            drop(file);
            index_platform::check_replace(dir, &tmp, &target)?;
            #[cfg(feature = "fault-injection")]
            fault(IndexPoint::MarkerRename).map_err(|e| e.to_string())?;
            fs::rename(&tmp, &target).map_err(|e| e.to_string())?;
            owned = false;
            *self = Self::CleanupPending(bytes.clone());
            #[cfg(feature = "fault-injection")]
            fs_commit::record_fs(fs_commit::FsAction::Rename, &target);
            #[cfg(feature = "fault-injection")]
            fault(IndexPoint::MarkerDirSync).map_err(|e| e.to_string())?;
            fs_commit::sync_dir(dir).map_err(|e| e.to_string())?;
            Ok(())
        });
        if owned && let Err(cleanup) = fs::remove_file(&tmp) {
            return Err(format!(
                "{}; intent tmp cleanup failed: {cleanup}",
                result
                    .err()
                    .unwrap_or_else(|| "intent preparation failed".to_owned())
            ));
        }
        result
    }

    pub(crate) fn delete(&mut self, dir: &Path, files: &[PathBuf]) -> Result<(), String> {
        let bytes = match self {
            Self::CleanupPending(bytes) => bytes.clone(),
            _ => return Err("index deletion intent has not been persisted".to_owned()),
        };
        *self = Self::Unconfirmed(blocked(dir, "index deletion durability is unconfirmed"));
        let directories: std::collections::BTreeSet<_> = files
            .iter()
            .map(|path| {
                path.parent()
                    .map(Path::to_owned)
                    .ok_or_else(|| "index path has no parent".to_owned())
            })
            .collect::<Result<_, _>>()?;
        guarded(|| {
            #[cfg(feature = "fault-injection")]
            {
                let f = BEFORE_DELETE.with(|s| s.borrow_mut().take());
                if let Some(f) = f {
                    f();
                }
            }
            #[cfg(feature = "fault-injection")]
            fault(IndexPoint::CapabilitySync).map_err(|e| e.to_string())?;
            for index_dir in &directories {
                fs_commit::sync_dir(index_dir).map_err(|e| e.to_string())?;
            }
            // 删除位次只用于隔离构建中的逐文件故障注入。
            #[cfg_attr(
                not(feature = "fault-injection"),
                allow(clippy::unused_enumerate_index)
            )]
            for (_i, path) in files.iter().enumerate() {
                #[cfg(feature = "fault-injection")]
                fault(IndexPoint::Delete(_i)).map_err(|e| e.to_string())?;
                match fs::remove_file(path) {
                    Ok(()) => {
                        #[cfg(feature = "fault-injection")]
                        fs_commit::record_fs(fs_commit::FsAction::Unlink, path);
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(format!("{path:?}: {e}")),
                }
            }
            #[cfg(feature = "fault-injection")]
            fault(IndexPoint::AfterLastDelete).map_err(|e| e.to_string())?;
            #[cfg(feature = "fault-injection")]
            fault(IndexPoint::DeleteSync).map_err(|e| e.to_string())?;
            for index_dir in &directories {
                fs_commit::sync_dir(index_dir).map_err(|e| e.to_string())?;
            }
            *self = Self::CleanupPending(bytes);
            Ok(())
        })
    }

    pub(crate) fn cleanup(&mut self, dir: &Path) -> Result<(), String> {
        let expected = match self {
            Self::CleanupPending(bytes) => bytes.clone(),
            _ => return Ok(()),
        };
        let result = guarded(|| {
            #[cfg(feature = "fault-injection")]
            fault(IndexPoint::CleanupRead).map_err(|e| e.to_string())?;
            match read_marker(dir) {
                Ok(Some(bytes)) if bytes == expected => {
                    #[cfg(feature = "fault-injection")]
                    fault(IndexPoint::CleanupUnlink).map_err(|e| e.to_string())?;
                    fs::remove_file(dir.join(INTENT_FILE)).map_err(|e| e.to_string())?;
                    #[cfg(feature = "fault-injection")]
                    fs_commit::record_fs(fs_commit::FsAction::Unlink, &dir.join(INTENT_FILE));
                }
                Ok(None) => {}
                Ok(Some(_)) => {
                    let reason = blocked(dir, "intent marker differs from the confirmed operation");
                    *self = Self::Unconfirmed(reason.clone());
                    return Err(reason);
                }
                Err(e) => {
                    if fs::symlink_metadata(dir.join(INTENT_FILE))
                        .is_ok_and(|m| !m.file_type().is_file())
                    {
                        *self = Self::Unconfirmed(blocked(dir, &e));
                    }
                    return Err(e);
                }
            }
            #[cfg(feature = "fault-injection")]
            fault(IndexPoint::CleanupSync).map_err(|e| e.to_string())?;
            fs_commit::sync_dir(dir).map_err(|e| e.to_string())?;
            *self = Self::Clear;
            Ok(())
        });
        result.map_err(|e| {
            if let Self::Unconfirmed(reason) = self {
                return reason.clone();
            }
            format!(
                "index intent cleanup failed at {:?}: {e}; retry the index change in this service instance after resolving the cleanup error",
                dir.join(INTENT_FILE)
            )
        })
    }
}

fn clean_tmp(dir: &Path) -> Result<(), String> {
    let tmp = dir.join(INTENT_TMP);
    match fs::symlink_metadata(&tmp) {
        Ok(m) if m.file_type().is_file() => {
            fs::remove_file(&tmp).map_err(|e| format!("{tmp:?}: {e}"))
        }
        Ok(_) => Err(format!("intent tmp is not a regular file: {tmp:?}")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{tmp:?}: {e}")),
    }
}

pub(crate) fn guarded<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => Err(format!(
            "index operation panicked: {}",
            crate::error::panic_description(payload.as_ref())
        )),
    }
}

fn boot_context(dir: &Path, mut files: impl Iterator<Item = PathBuf>) -> BootContext {
    #[cfg(feature = "fault-injection")]
    if let Some(context) = BOOT_OVERRIDE.with(|s| s.borrow().clone()) {
        return context;
    }
    let mut context = index_platform::boot_context(dir);
    context.local &= files.all(|path| path.parent().is_some_and(index_platform::local_fs));
    context
}

#[cfg(feature = "fault-injection")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexPoint {
    InheritedSync,
    ManifestWrite,
    ManifestSync,
    MarkerWrite,
    MarkerSync,
    MarkerRename,
    MarkerDirSync,
    CapabilitySync,
    Delete(usize),
    AfterLastDelete,
    DeleteSync,
    BeforeCommit,
    CleanupRead,
    CleanupUnlink,
    CleanupSync,
}
#[cfg(feature = "fault-injection")]
#[derive(Clone, Copy, Debug)]
pub enum FaultKind {
    Error,
    Panic,
    Exit,
}
#[cfg(feature = "fault-injection")]
thread_local! {
    static FAULT: std::cell::RefCell<Vec<(IndexPoint, FaultKind)>> = const { std::cell::RefCell::new(Vec::new()) };
    static BOOT_OVERRIDE: std::cell::RefCell<Option<BootContext>> = const { std::cell::RefCell::new(None) };
    static BEFORE_COMMIT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static BEFORE_DELETE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(feature = "fault-injection")]
pub fn inject_fault(point: IndexPoint, kind: FaultKind) {
    FAULT.with(|s| s.borrow_mut().push((point, kind)));
}
#[cfg(feature = "fault-injection")]
pub fn override_boot_context(context: Option<BootContext>) {
    BOOT_OVERRIDE.with(|s| *s.borrow_mut() = context);
}
#[cfg(feature = "fault-injection")]
pub fn before_commit_once(f: impl FnOnce() + 'static) {
    BEFORE_COMMIT.with(|s| *s.borrow_mut() = Some(Box::new(f)));
}
#[cfg(feature = "fault-injection")]
pub fn before_delete_once(f: impl FnOnce() + 'static) {
    BEFORE_DELETE.with(|s| *s.borrow_mut() = Some(Box::new(f)));
}
#[cfg(feature = "fault-injection")]
pub(crate) fn before_commit() -> io::Result<()> {
    let f = BEFORE_COMMIT.with(|s| s.borrow_mut().take());
    if let Some(f) = f {
        f();
    }
    fault(IndexPoint::BeforeCommit)
}
#[cfg(feature = "fault-injection")]
pub(crate) fn fault(point: IndexPoint) -> io::Result<()> {
    let action = FAULT.with(|s| {
        let mut faults = s.borrow_mut();
        let index = faults.iter().position(|(p, _)| *p == point)?;
        Some(faults.remove(index).1)
    });
    match action {
        None => Ok(()),
        Some(FaultKind::Error) => Err(io::Error::other(format!(
            "injected index failure: {point:?}"
        ))),
        Some(FaultKind::Panic) => panic!("injected index panic: {point:?}"),
        Some(FaultKind::Exit) => std::process::exit(86),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_boot_scope_uses_only_collection_and_involved_index_directories() {
        for extra_default in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path();
            let index = dir.join("partitions/p/indexes");
            fs::create_dir_all(&index).unwrap();
            let sidecar = index.join("data_1_1.v.hnsw.idx");
            fs::write(&sidecar, b"reference sidecar").unwrap();
            if extra_default {
                fs::create_dir_all(dir.join("partitions/_default/indexes")).unwrap();
            }
            let real = index_platform::boot_context(dir);
            let local = index_platform::local_fs(dir) && index_platform::local_fs(&index);
            let context = boot_context(dir, [sidecar.clone()].into_iter());
            assert_eq!(context.local, local);
            assert_eq!(context.boot_id, real.boot_id);
            assert_eq!(context.boot_source, real.boot_source);
            assert!(
                !boot_context(
                    dir,
                    [dir.join("partitions/missing/indexes/x.idx")].into_iter()
                )
                .local
            );
            assert!(!boot_context(&dir.join("missing"), [sidecar.clone()].into_iter()).local);
            let mut state = IntentState::Clear;
            state
                .persist(dir, "drop_index", vec!["v".into()], &[sidecar])
                .unwrap();
            let mut marker = Marker::parse(&fs::read(dir.join(INTENT_FILE)).unwrap()).unwrap();
            assert_eq!(marker.auto_recovery, local && context.boot_id.is_some());
            assert!(IntentState::Clear.check(dir).is_err());
            if context.boot_id.is_some() {
                marker.boot_id = Some(
                    if context.boot_id.as_deref() == Some("11111111-1111-4111-8111-111111111111") {
                        "22222222-2222-4222-8222-222222222222"
                    } else {
                        "11111111-1111-4111-8111-111111111111"
                    }
                    .into(),
                );
                fs::write(dir.join(INTENT_FILE), serde_json::to_vec(&marker).unwrap()).unwrap();
                assert_eq!(IntentState::Clear.check(dir).is_ok(), local);
                assert_eq!(dir.join(INTENT_FILE).exists(), !local);
            }
        }
    }
}
