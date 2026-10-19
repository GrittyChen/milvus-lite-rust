//! Drop 分区时把全局删除证据与移除分区作为同一次 manifest 提交。
use super::{compaction, flush, fs_commit, manifest::Manifest, snapshot::PinnedFiles};
use std::fs::{self, OpenOptions};
use std::path::Path;

#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFault {
    Write,
    FileSync,
    DirectorySync,
}
#[cfg(feature = "fault-injection")]
thread_local! { static COPY_FAULT: std::cell::Cell<Option<CopyFault>> = const { std::cell::Cell::new(None) }; }
#[cfg(feature = "fault-injection")]
pub fn inject_copy_fault_once(point: CopyFault) {
    COPY_FAULT.with(|slot| slot.set(Some(point)));
}
#[cfg(feature = "fault-injection")]
fn fault(point: CopyFault) -> Result<(), String> {
    if COPY_FAULT.with(|slot| {
        if slot.get() == Some(point) {
            slot.set(None);
            true
        } else {
            false
        }
    }) {
        Err(format!("injected partition delta copy failure: {point:?}"))
    } else {
        Ok(())
    }
}

pub(crate) fn prepare(
    dir: &Path,
    old: &Manifest,
    pins: &PinnedFiles,
    name: &str,
) -> Result<Manifest, String> {
    let mut next = old.clone();
    let dropped = next
        .partitions
        .remove(name)
        .ok_or("partition missing at drop")?;
    next.partition_order.retain(|part| part != name);
    next.version = next
        .version
        .checked_add(1)
        .ok_or("manifest version exhausted")?;
    let destination = dir.join("partitions/_default/delta");
    if !dropped.delta_files.is_empty() {
        fs_commit::create_dir_synced(&destination).map_err(|e| e.to_string())?;
    }
    for relative in dropped.delta_files {
        let source = dir.join("partitions").join(name).join(&relative);
        if !fs::symlink_metadata(&source)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_file()
        {
            return Err(format!("drop delta is not a regular file: {source:?}"));
        }
        let (min, max) = compaction::sequence_range(&relative).map_err(|e| e.to_string())?;
        let defaults = next
            .partitions
            .get_mut("_default")
            .ok_or("default partition missing")?;
        let file_name = flush::output_name(
            &destination,
            "delta",
            min,
            max,
            &defaults.delta_files,
            &pins.delta,
            "_default",
        )
        .map_err(|e| e.to_string())?;
        let path = destination.join(&file_name);
        let mut input = fs::File::open(&source).map_err(|e| e.to_string())?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        #[cfg(feature = "fault-injection")]
        fault(CopyFault::Write)?;
        std::io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
        #[cfg(feature = "fault-injection")]
        fault(CopyFault::FileSync)?;
        fs_commit::sync_file(&output, &path).map_err(|e| e.to_string())?;
        defaults.delta_files.push(format!("delta/{file_name}"));
    }
    if !next.partitions["_default"].delta_files.is_empty() {
        #[cfg(feature = "fault-injection")]
        fault(CopyFault::DirectorySync)?;
        fs_commit::sync_dir(&destination).map_err(|e| e.to_string())?;
    }
    Ok(next)
}
