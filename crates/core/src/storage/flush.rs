//! 一个冻结 WAL 代的同步 flush 原语。调用方须按代串行提交，并负责失败后的内存状态。

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use arrow_array::{Array, RecordBatch, UInt64Array};

use crate::storage::fs_commit::{self, CommitPhase};
use crate::storage::manifest::Manifest;
use crate::storage::parquet_file;
use crate::storage::snapshot;

#[cfg(feature = "fault-injection")]
thread_local! {
    static FAIL_NEXT_WAL_UNLINK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static INHERITED_SYNC_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(feature = "fault-injection")]
pub fn inject_wal_unlink_failure_once() {
    FAIL_NEXT_WAL_UNLINK.with(|flag| flag.set(true));
}

#[cfg(feature = "fault-injection")]
pub fn inherited_sync_count() -> usize {
    INHERITED_SYNC_CALLS.with(std::cell::Cell::get)
}

#[derive(Debug, thiserror::Error)]
#[error("flush failed at {path:?} ({phase:?}): {reason}")]
pub struct FlushError {
    pub path: PathBuf,
    pub phase: CommitPhase,
    pub reason: String,
    /// replace 已成功时带上新 manifest，调用方不得把它当作未提交重做。
    pub committed_manifest: Option<Box<Manifest>>,
    /// manifest 已完整提交；失败只发生在后续 WAL 清理。
    pub cleanup_failed: bool,
}

impl FlushError {
    fn before(path: &Path, reason: impl ToString) -> Self {
        Self {
            path: path.to_path_buf(),
            phase: CommitPhase::BeforeReplace,
            reason: reason.to_string(),
            committed_manifest: None,
            cleanup_failed: false,
        }
    }

    fn after(path: &Path, reason: impl ToString, manifest: Manifest) -> Self {
        Self {
            path: path.to_path_buf(),
            phase: CommitPhase::AfterReplace,
            reason: reason.to_string(),
            committed_manifest: Some(Box::new(manifest)),
            cleanup_failed: false,
        }
    }

    fn cleanup(path: &Path, reason: impl ToString, manifest: Manifest) -> Self {
        Self {
            path: path.to_path_buf(),
            phase: CommitPhase::AfterReplace,
            reason: reason.to_string(),
            committed_manifest: Some(Box::new(manifest)),
            cleanup_failed: true,
        }
    }
}

/// `data` 与 `delta` 的 Arrow schema 分别是 3.x 的 data / delta Parquet schema。
/// 只在 manifest 提交前写新文件；提交后才清 WAL。重试时的孤儿文件由上层先按 snapshot 钉住集合清理。
pub fn flush_generation(
    collection_dir: &Path,
    old_manifest: &Manifest,
    generation: u64,
    frozen_max_seq: u64,
    data: Option<&RecordBatch>,
    delta: Option<&RecordBatch>,
) -> Result<Manifest, FlushError> {
    flush_generation_with_inherited_sync(
        collection_dir,
        old_manifest,
        generation,
        frozen_max_seq,
        data,
        delta,
        true,
    )
}

pub(crate) fn flush_generation_with_inherited_sync(
    collection_dir: &Path,
    old_manifest: &Manifest,
    generation: u64,
    frozen_max_seq: u64,
    data: Option<&RecordBatch>,
    delta: Option<&RecordBatch>,
    sync_inherited_files: bool,
) -> Result<Manifest, FlushError> {
    let next_wal = generation
        .checked_add(1)
        .ok_or_else(|| FlushError::before(collection_dir, "WAL number space exhausted"))?;
    let data_range = data
        .filter(|batch| batch.num_rows() > 0)
        .map(|batch| seq_range(batch, "_seq"))
        .transpose()?;
    let delta_range = delta
        .filter(|batch| batch.num_rows() > 0)
        .map(|batch| seq_range(batch, "_seq"))
        .transpose()?;
    if data_range.is_some_and(|(_, max)| max > frozen_max_seq)
        || delta_range.is_some_and(|(_, max)| max > frozen_max_seq)
    {
        return Err(FlushError::before(
            collection_dir,
            "frozen max_seq is below a row written by this generation",
        ));
    }
    let partition = collection_dir.join("partitions/_default");
    let data_dir = partition.join("data");
    let delta_dir = partition.join("delta");
    if (data.is_some_and(|batch| batch.num_rows() > 0)
        || delta.is_some_and(|batch| batch.num_rows() > 0))
        && collect_wal(&collection_dir.join("wal"), generation)
            .map_err(|error| FlushError::before(collection_dir, error))?
            .is_empty()
    {
        return Err(FlushError::before(
            collection_dir,
            "nonempty flush has no WAL for recovery",
        ));
    }
    fs_commit::create_dir_synced(&data_dir)
        .map_err(|error| FlushError::before(&data_dir, error))?;
    fs_commit::create_dir_synced(&delta_dir)
        .map_err(|error| FlushError::before(&delta_dir, error))?;
    let pinned = snapshot::load_pins(collection_dir)
        .map_err(|error| FlushError::before(collection_dir, error))?;
    if sync_inherited_files {
        sync_inherited(collection_dir, old_manifest)?;
    }
    let mut candidate = old_manifest.clone();
    let files = candidate
        .partitions
        .get_mut("_default")
        .ok_or_else(|| FlushError::before(collection_dir, "default partition missing"))?;
    if let (Some(batch), Some((min, max))) = (data, data_range) {
        let name = output_name(&data_dir, "data", min, max, &files.data_files, &pinned.data)?;
        let path = data_dir.join(&name);
        parquet_file::write_new(&path, batch).map_err(|error| FlushError::before(&path, error))?;
        files.data_files.push(format!("data/{name}"));
    }
    if let (Some(batch), Some((min, max))) = (delta, delta_range) {
        let name = output_name(
            &delta_dir,
            "delta",
            min,
            max,
            &files.delta_files,
            &pinned.delta,
        )?;
        let path = delta_dir.join(&name);
        parquet_file::write_new(&path, batch).map_err(|error| FlushError::before(&path, error))?;
        files.delta_files.push(format!("delta/{name}"));
    }
    fs_commit::sync_dir(&data_dir).map_err(|error| FlushError::before(&data_dir, error))?;
    fs_commit::sync_dir(&delta_dir).map_err(|error| FlushError::before(&delta_dir, error))?;
    candidate.current_seq = old_manifest.current_seq.max(frozen_max_seq);
    candidate.active_wal_number = Some(next_wal);
    candidate.version = candidate
        .version
        .checked_add(1)
        .ok_or_else(|| FlushError::before(collection_dir, "manifest version space exhausted"))?;
    match fs_commit::commit_manifest(collection_dir, &candidate) {
        Ok(report) => {
            if let Some(error) = report.prev_copy_error {
                // `.prev` 不是真值；调用方必须记录这项非致命 I/O 故障。
                eprintln!("manifest backup copy failed for {collection_dir:?}: {error}");
            }
        }
        Err(error) if error.phase == CommitPhase::BeforeReplace => {
            return Err(FlushError::before(&error.path, error.source));
        }
        Err(error) => {
            let reason = match sync_retained_wal(&collection_dir.join("wal"), generation) {
                Ok(()) => error.source.to_string(),
                Err(wal_error) => {
                    format!("{}; retained WAL sync failed: {wal_error}", error.source)
                }
            };
            return Err(FlushError::after(&error.path, reason, candidate));
        }
    }
    if let Err(error) = cleanup_wal(&collection_dir.join("wal"), generation) {
        return Err(FlushError::cleanup(collection_dir, error, candidate));
    }
    Ok(candidate)
}

fn output_name(
    dir: &Path,
    prefix: &str,
    seq_min: u64,
    seq_max: u64,
    referenced: &[String],
    pinned: &std::collections::HashSet<(String, String)>,
) -> Result<String, FlushError> {
    let mut upper = seq_max;
    loop {
        let name = format!("{prefix}_{seq_min:06}_{upper:06}.parquet");
        let relative = format!("{prefix}/{name}");
        let path = dir.join(&name);
        if referenced.contains(&relative) || pinned.contains(&("_default".to_owned(), relative)) {
            upper = upper
                .checked_add(1)
                .ok_or_else(|| FlushError::before(&path, "segment filename sequence exhausted"))?;
            continue;
        }
        if has_index_sidecar(dir, name.trim_end_matches(".parquet"))? {
            // 3.x 会按 data 文件的 stem 加载侧车索引；复用孤儿 Parquet 的名字会加载旧索引。
            upper = upper
                .checked_add(1)
                .ok_or_else(|| FlushError::before(&path, "segment filename sequence exhausted"))?;
            continue;
        }
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(name),
            Err(error) => return Err(FlushError::before(&path, error)),
            Ok(meta) if !meta.file_type().is_file() => {
                return Err(FlushError::before(
                    &path,
                    "output path is not a regular file",
                ));
            }
            Ok(_) => {
                // 上一次提交前失败留下的孤儿。已验证所有 snapshot 元数据，故可删后重写。
                fs::remove_file(&path).map_err(|error| FlushError::before(&path, error))?;
                return Ok(name);
            }
        }
    }
}

fn has_index_sidecar(data_or_delta_dir: &Path, stem: &str) -> Result<bool, FlushError> {
    let partition = data_or_delta_dir.parent().ok_or_else(|| {
        FlushError::before(
            data_or_delta_dir,
            "segment directory has no partition parent",
        )
    })?;
    let index_dir = partition.join("indexes");
    let meta = match fs::symlink_metadata(&index_dir) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(FlushError::before(&index_dir, error)),
    };
    if !meta.file_type().is_dir() {
        return Err(FlushError::before(
            &index_dir,
            "indexes path is not a regular directory",
        ));
    }
    let prefix = format!("{stem}.");
    for entry in fs::read_dir(&index_dir).map_err(|error| FlushError::before(&index_dir, error))? {
        let entry = entry.map_err(|error| FlushError::before(&index_dir, error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(&prefix) && (name.ends_with(".idx") || name.ends_with(".sidx")) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn seq_range(batch: &RecordBatch, name: &str) -> Result<(u64, u64), FlushError> {
    let index = batch
        .schema()
        .index_of(name)
        .map_err(|error| FlushError::before(Path::new(name), error))?;
    let seqs = batch
        .column(index)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| FlushError::before(Path::new(name), "_seq is not uint64"))?;
    if seqs.null_count() != 0 || seqs.is_empty() {
        return Err(FlushError::before(
            Path::new(name),
            "_seq is empty or nullable",
        ));
    }
    let min = seqs
        .values()
        .iter()
        .copied()
        .min()
        .ok_or_else(|| FlushError::before(Path::new(name), "_seq is empty"))?;
    let max = seqs
        .values()
        .iter()
        .copied()
        .max()
        .ok_or_else(|| FlushError::before(Path::new(name), "_seq is empty"))?;
    Ok((min, max))
}

pub(crate) fn sync_inherited(collection_dir: &Path, manifest: &Manifest) -> Result<(), FlushError> {
    #[cfg(feature = "fault-injection")]
    INHERITED_SYNC_CALLS.with(|count| count.set(count.get() + 1));
    let partition = collection_dir.join("partitions/_default");
    let files = manifest
        .partitions
        .get("_default")
        .ok_or_else(|| FlushError::before(collection_dir, "default partition missing"))?;
    for relative in files.data_files.iter().chain(&files.delta_files) {
        if !crate::storage::manifest::safe_relative_path(relative) {
            return Err(FlushError::before(
                collection_dir,
                format!("unsafe manifest path {relative:?}"),
            ));
        }
        let path = partition.join(relative);
        let meta = fs::symlink_metadata(&path).map_err(|error| FlushError::before(&path, error))?;
        if !meta.file_type().is_file() {
            return Err(FlushError::before(&path, "referenced file is not regular"));
        }
        File::open(&path)
            .and_then(|file| file.sync_all())
            .map_err(|error| FlushError::before(&path, error))?;
    }
    let schema = collection_dir.join("schema.json");
    let meta = fs::symlink_metadata(&schema).map_err(|error| FlushError::before(&schema, error))?;
    if !meta.file_type().is_file() {
        return Err(FlushError::before(&schema, "schema is not a regular file"));
    }
    File::open(&schema)
        .and_then(|file| file.sync_all())
        .map_err(|error| FlushError::before(&schema, error))?;
    // 3.x 不同步 Parquet 及多层目录；第一次由本实现提交前须从数据目录根的上级补齐。
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut current = Some(collection_dir);
    for _ in 0..4 {
        if let Some(dir) = current {
            dirs.push(dir.to_path_buf());
            current = dir.parent();
        }
    }
    dirs.reverse();
    dirs.extend([
        partition.clone(),
        partition.join("data"),
        partition.join("delta"),
    ]);
    for dir in dirs {
        if dir.is_dir() {
            fs_commit::sync_dir(&dir).map_err(|error| FlushError::before(&dir, error))?;
        }
    }
    Ok(())
}

/// 一趟升序删除已提交代的 WAL；每个 unlink 后同步目录，断电时仍保留编号后缀。
pub fn cleanup_wal(wal_dir: &Path, up_to: u64) -> io::Result<()> {
    for path in collect_wal(wal_dir, up_to)? {
        File::open(&path)?.sync_all()?;
        #[cfg(feature = "fault-injection")]
        if FAIL_NEXT_WAL_UNLINK.with(|flag| flag.replace(false)) {
            return Err(io::Error::other("injected WAL unlink failure"));
        }
        fs::remove_file(&path)?;
        fs_commit::sync_dir(wal_dir)?;
    }
    Ok(())
}

fn sync_retained_wal(wal_dir: &Path, up_to: u64) -> io::Result<()> {
    for path in collect_wal(wal_dir, up_to)? {
        File::open(&path)?.sync_all()?;
    }
    if wal_dir.is_dir() {
        fs_commit::sync_dir(wal_dir)?;
    }
    Ok(())
}

fn collect_wal(wal_dir: &Path, up_to: u64) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(wal_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".arrow") else {
            continue;
        };
        let (order, number) = if let Some(number) = stem.strip_prefix("wal_data_") {
            (0_u8, number)
        } else if let Some(number) = stem.strip_prefix("wal_delta_") {
            (1_u8, number)
        } else {
            continue;
        };
        let Ok(number) = number.parse::<u64>() else {
            continue;
        };
        let canonical = if order == 0 {
            format!("wal_data_{number:06}.arrow")
        } else {
            format!("wal_delta_{number:06}.arrow")
        };
        if name != canonical {
            continue;
        }
        if number <= up_to {
            files.push((number, order, entry.path()));
        }
    }
    files.sort_by_key(|(number, order, _)| (*number, *order));
    let mut paths = Vec::with_capacity(files.len());
    for (_, _, path) in files {
        let meta = fs::symlink_metadata(&path)?;
        if !meta.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WAL is not a regular file",
            ));
        }
        paths.push(path);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use arrow_array::{Int64Array, UInt64Array};
    use arrow_schema::{DataType, Field, Schema};

    use crate::storage::recovery::load_committed;

    use super::*;

    #[test]
    fn flush_data_and_delete_then_recover() {
        let root = tempfile::tempdir().unwrap();
        let collection = root.path().join("x.db/collections/c");
        fs_commit::create_dir_synced(&collection).unwrap();
        fs_commit::write_new_schema(
            &collection.join("schema.json"),
            include_bytes!("../../tests/fixtures/schema_3x.json"),
        )
        .unwrap();
        let source = crate::storage::parquet_file::read_all(&{
            let path = root.path().join("source.parquet");
            fs::write(
                &path,
                include_bytes!("../../tests/fixtures/data_3x.parquet"),
            )
            .unwrap();
            path
        })
        .unwrap();
        let data = source[0].clone();
        let delta_schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("_seq", DataType::UInt64, false),
        ]));
        let delta = RecordBatch::try_new(
            delta_schema,
            vec![
                Arc::new(Int64Array::from(vec![10])),
                Arc::new(UInt64Array::from(vec![3])),
            ],
        )
        .unwrap();
        let wal_dir = collection.join("wal");
        fs_commit::create_dir_synced(&wal_dir).unwrap();
        fs::write(
            wal_dir.join("wal_data_000001.arrow"),
            include_bytes!("../../tests/fixtures/wal_data_3x.arrow"),
        )
        .unwrap();
        let manifest = flush_generation(
            &collection,
            &Manifest::default(),
            1,
            3,
            Some(&data),
            Some(&delta),
        )
        .unwrap();
        assert_eq!(manifest.current_seq, 3);
        assert!(!wal_dir.join("wal_data_000001.arrow").exists());
        let recovered = load_committed(&collection).unwrap();
        assert_eq!(recovered.visibility.live_count(), 1);
        assert_eq!(recovered.next_seq, 4);
    }

    #[cfg(unix)]
    #[test]
    fn pre_replace_error_keeps_wal_recoverable() {
        let root = tempfile::tempdir().unwrap();
        let collection = root.path().join("x.db/collections/c");
        fs_commit::create_dir_synced(&collection).unwrap();
        fs_commit::write_new_schema(
            &collection.join("schema.json"),
            include_bytes!("../../tests/fixtures/schema_3x.json"),
        )
        .unwrap();
        let wal_dir = collection.join("wal");
        fs_commit::create_dir_synced(&wal_dir).unwrap();
        let wal_path = wal_dir.join("wal_data_000001.arrow");
        fs::write(
            &wal_path,
            include_bytes!("../../tests/fixtures/wal_data_3x.arrow"),
        )
        .unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, b"unchanged").unwrap();
        std::os::unix::fs::symlink(&outside, collection.join("manifest.json.tmp")).unwrap();
        let error =
            flush_generation(&collection, &Manifest::default(), 1, 0, None, None).unwrap_err();
        assert_eq!(error.phase, CommitPhase::BeforeReplace);
        assert!(error.committed_manifest.is_none());
        assert!(wal_path.exists());
        assert_eq!(fs::read(outside).unwrap(), b"unchanged");
        assert_eq!(
            load_committed(&collection).unwrap().visibility.live_count(),
            2
        );
    }

    #[test]
    fn snapshot_pinned_name_is_never_reused() {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("_default/data");
        fs::create_dir_all(&data_dir).unwrap();
        let old = data_dir.join("data_000001_000001.parquet");
        fs::write(&old, b"pinned").unwrap();
        let mut pins = HashSet::new();
        pins.insert((
            "_default".to_owned(),
            "data/data_000001_000001.parquet".to_owned(),
        ));
        let new_name = output_name(&data_dir, "data", 1, 1, &[], &pins).unwrap();
        assert_eq!(new_name, "data_000001_000002.parquet");
        assert_eq!(fs::read(old).unwrap(), b"pinned");
    }

    #[test]
    fn unreferenced_orphan_can_be_rewritten() {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("_default/data");
        fs::create_dir_all(&data_dir).unwrap();
        let orphan = data_dir.join("data_000001_000001.parquet");
        fs::write(&orphan, b"orphan").unwrap();
        let name = output_name(&data_dir, "data", 1, 1, &[], &HashSet::new()).unwrap();
        assert_eq!(name, "data_000001_000001.parquet");
        assert!(!orphan.exists());
    }

    #[test]
    fn stale_index_sidecar_prevents_reusing_orphan_stem() {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("_default/data");
        let index_dir = root.path().join("_default/indexes");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&index_dir).unwrap();
        let orphan = data_dir.join("data_000001_000001.parquet");
        fs::write(&orphan, b"old data").unwrap();
        fs::write(
            index_dir.join("data_000001_000001.vector.hnsw.idx"),
            b"old index",
        )
        .unwrap();
        let name = output_name(&data_dir, "data", 1, 1, &[], &HashSet::new()).unwrap();
        assert_eq!(name, "data_000001_000002.parquet");
        assert_eq!(fs::read(orphan).unwrap(), b"old data");
    }

    #[test]
    fn nonempty_flush_without_wal_is_rejected_before_writing_files() {
        let root = tempfile::tempdir().unwrap();
        let collection = root.path().join("x.db/collections/c");
        fs_commit::create_dir_synced(&collection).unwrap();
        fs_commit::write_new_schema(
            &collection.join("schema.json"),
            include_bytes!("../../tests/fixtures/schema_3x.json"),
        )
        .unwrap();
        let fixture = root.path().join("source.parquet");
        fs::write(
            &fixture,
            include_bytes!("../../tests/fixtures/data_3x.parquet"),
        )
        .unwrap();
        let data = crate::storage::parquet_file::read_all(&fixture).unwrap();
        let error = flush_generation(
            &collection,
            &Manifest::default(),
            1,
            2,
            Some(&data[0]),
            None,
        )
        .unwrap_err();
        assert_eq!(error.phase, CommitPhase::BeforeReplace);
        assert!(!collection.join("manifest.json").exists());
    }

    #[test]
    fn manifest_uses_frozen_max_seq_not_output_file_max() {
        let root = tempfile::tempdir().unwrap();
        let collection = root.path().join("x.db/collections/c");
        fs_commit::create_dir_synced(&collection).unwrap();
        fs_commit::write_new_schema(
            &collection.join("schema.json"),
            include_bytes!("../../tests/fixtures/schema_3x.json"),
        )
        .unwrap();
        let wal_dir = collection.join("wal");
        fs_commit::create_dir_synced(&wal_dir).unwrap();
        fs::write(
            wal_dir.join("wal_data_000001.arrow"),
            include_bytes!("../../tests/fixtures/wal_data_3x.arrow"),
        )
        .unwrap();
        let fixture = root.path().join("source.parquet");
        fs::write(
            &fixture,
            include_bytes!("../../tests/fixtures/data_3x.parquet"),
        )
        .unwrap();
        let data = crate::storage::parquet_file::read_all(&fixture).unwrap();
        let too_low = flush_generation(
            &collection,
            &Manifest::default(),
            1,
            1,
            Some(&data[0]),
            None,
        )
        .unwrap_err();
        assert_eq!(too_low.phase, CommitPhase::BeforeReplace);
        assert!(!collection.join("manifest.json").exists());
        let committed = flush_generation(
            &collection,
            &Manifest::default(),
            1,
            9,
            Some(&data[0]),
            None,
        )
        .unwrap();
        assert_eq!(committed.current_seq, 9);
        assert_eq!(load_committed(&collection).unwrap().next_seq, 10);
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn post_replace_failure_keeps_wal_until_full_recommit() {
        let root = tempfile::tempdir().unwrap();
        let collection = root.path().join("x.db/collections/c");
        fs_commit::create_dir_synced(&collection).unwrap();
        fs_commit::write_new_schema(
            &collection.join("schema.json"),
            include_bytes!("../../tests/fixtures/schema_3x.json"),
        )
        .unwrap();
        let wal_dir = collection.join("wal");
        fs_commit::create_dir_synced(&wal_dir).unwrap();
        let wal_path = wal_dir.join("wal_data_000001.arrow");
        fs::write(
            &wal_path,
            include_bytes!("../../tests/fixtures/wal_data_3x.arrow"),
        )
        .unwrap();
        let fixture = root.path().join("source.parquet");
        fs::write(
            &fixture,
            include_bytes!("../../tests/fixtures/data_3x.parquet"),
        )
        .unwrap();
        let data = crate::storage::parquet_file::read_all(&fixture).unwrap();

        fs_commit::inject_manifest_dir_sync_failure_once();
        let error = flush_generation(
            &collection,
            &Manifest::default(),
            1,
            2,
            Some(&data[0]),
            None,
        )
        .unwrap_err();
        assert_eq!(error.phase, CommitPhase::AfterReplace);
        assert!(wal_path.exists());
        let mut committed = *error.committed_manifest.unwrap();
        assert_eq!(
            Manifest::read(&collection.join("manifest.json")).unwrap(),
            committed
        );
        assert_eq!(
            load_committed(&collection).unwrap().visibility.live_count(),
            2
        );

        committed.version += 1;
        fs_commit::commit_manifest(&collection, &committed).unwrap();
        cleanup_wal(&wal_dir, 1).unwrap();
        assert!(!wal_path.exists());
        assert_eq!(
            load_committed(&collection).unwrap().visibility.live_count(),
            2
        );
    }
}
