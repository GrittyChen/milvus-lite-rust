//! collection 的只读恢复阶段：先验证全部引用文件，再把行与 tombstone 纳入有效位图。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::data::{ColumnData, Pk};
use crate::schema::CollectionSchema;
use crate::storage::arrow_data::{self, DecodedData, DecodedDelta};
use crate::storage::manifest::Manifest;
use crate::storage::memtable::{MemTable, MemTableError};
use crate::storage::parquet_file;
use crate::storage::schema_file::SchemaFile;
use crate::storage::snapshot::{self, PinnedFiles};
use crate::storage::visibility::{SourceId, Visibility};
use crate::storage::wal;

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("schema error: {0}")]
    Schema(#[from] crate::storage::schema_file::SchemaFileError),
    #[error("manifest error: {0}")]
    Manifest(#[from] crate::storage::manifest::ManifestError),
    #[error("Parquet error: {0}")]
    Parquet(#[from] parquet_file::ParquetFileError),
    #[error("Arrow data error: {0}")]
    Arrow(#[from] arrow_data::ArrowDataError),
    #[error("visibility error: {0}")]
    Visibility(#[from] crate::storage::visibility::VisibilityError),
    #[error("memtable replay error: {0}")]
    MemTable(#[from] MemTableError),
    #[error("WAL error: {0}")]
    Wal(#[from] wal::WalError),
    #[error("snapshot metadata error: {0}")]
    Snapshot(#[from] snapshot::SnapshotError),
    #[error("collection file {path:?} cannot be inspected: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("collection content outside M2 scope: {0}")]
    Unsupported(String),
    #[error("manifest references empty {kind} Parquet file {path:?}")]
    EmptyReferencedFile { path: PathBuf, kind: &'static str },
}

#[derive(Debug)]
pub struct RecoveredSegment {
    pub source: SourceId,
    pub relative_path: String,
    pub batches: Vec<DecodedData>,
}

#[derive(Debug)]
pub struct RecoveredCollection {
    pub schema_file: SchemaFile,
    pub schema: CollectionSchema,
    pub manifest: Manifest,
    pub segments: Vec<RecoveredSegment>,
    pub pinned_files: PinnedFiles,
    pub active_memtable: MemTable,
    pub visibility: Visibility,
    pub next_seq: u64,
    pub next_wal_number: u64,
    pub wal_warnings: Vec<String>,
}

enum WalOperation {
    Data(DecodedData),
    Delete(DecodedDelta),
}

fn filter_committed_rows(
    schema: &CollectionSchema,
    batch: DecodedData,
    committed: &Visibility,
) -> Result<DecodedData, RecoveryError> {
    let mut seqs = Vec::new();
    let mut columns = schema
        .fields
        .iter()
        .map(ColumnData::empty)
        .collect::<Vec<_>>();
    let mut meta = batch.meta.as_ref().map(|_| Vec::new());
    for row in 0..batch.seqs.len() {
        let pk = pk_at(schema, &batch.columns, row)?;
        let seq = batch.seqs[row];
        let seg_max = committed.latest_data(&pk).map(|loc| loc.seq);
        let delta_max = committed.latest_delete(&pk);
        if seg_max.is_some_and(|old| seq <= old) || delta_max.is_some_and(|deleted| seq < deleted) {
            continue;
        }
        seqs.push(seq);
        for (dst, src) in columns.iter_mut().zip(&batch.columns) {
            dst.push_row(src, row);
        }
        if let Some(meta) = &mut meta {
            let value = batch
                .meta
                .as_ref()
                .and_then(|all| all.get(row))
                .ok_or_else(|| RecoveryError::Unsupported("WAL $meta row is missing".to_owned()))?;
            meta.push(value.clone());
        }
    }
    Ok(DecodedData {
        seqs,
        columns,
        meta,
    })
}

/// 不做孤儿清理；任何读取或判定失败时，目录保持原样。
pub fn load_committed(collection_dir: &Path) -> Result<RecoveredCollection, RecoveryError> {
    let schema_path = collection_dir.join("schema.json");
    regular_file(&schema_path)?;
    let schema_file = SchemaFile::read(&schema_path)?;
    let dir_name = collection_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if schema_file.collection_name != dir_name {
        return Err(RecoveryError::Unsupported(format!(
            "schema collection name {:?} differs from directory {dir_name:?}",
            schema_file.collection_name
        )));
    }
    let schema = schema_file.to_core()?;
    let manifest_path = collection_dir.join("manifest.json");
    let prev_path = collection_dir.join("manifest.json.prev");
    let manifest = match fs::symlink_metadata(&manifest_path) {
        Ok(_) => {
            regular_file(&manifest_path)?;
            Manifest::read(&manifest_path)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if fs::symlink_metadata(&prev_path).is_ok() {
                return Err(RecoveryError::Unsupported(
                    "manifest.json is missing but manifest.json.prev exists".to_owned(),
                ));
            }
            Manifest::default()
        }
        Err(source) => {
            return Err(RecoveryError::Io {
                path: manifest_path,
                source,
            });
        }
    };
    if manifest.partitions.len() != 1 || !manifest.partitions.contains_key("_default") {
        return Err(RecoveryError::Unsupported(
            "non-default partitions".to_owned(),
        ));
    }
    let pinned_files = snapshot::load_pins(collection_dir)?;
    let files = &manifest.partitions["_default"];
    let partition_dir = collection_dir.join("partitions").join("_default");
    let mut visibility = Visibility::default();
    let mut committed_max_seq = 0_u64;
    let mut segments = Vec::with_capacity(files.data_files.len());
    for (index, relative_path) in files.data_files.iter().enumerate() {
        let path = partition_dir.join(relative_path);
        regular_file(&path)?;
        let batches = parquet_file::read_all(&path)?;
        if !batches.iter().any(|batch| batch.num_rows() > 0) {
            return Err(RecoveryError::EmptyReferencedFile { path, kind: "data" });
        }
        let decoded = batches
            .iter()
            .map(|batch| arrow_data::decode_data_batch(&schema, batch))
            .collect::<Result<Vec<_>, _>>()?;
        let source = SourceId(index as u64 + 1);
        let row_count = decoded.iter().try_fold(0_usize, |rows, batch| {
            rows.checked_add(batch.seqs.len()).ok_or_else(|| {
                RecoveryError::Unsupported(format!("row count overflows in {path:?}"))
            })
        })?;
        visibility.add_source(source, row_count)?;
        let mut global_row = 0;
        for batch in &decoded {
            for row in 0..batch.seqs.len() {
                let pk = pk_at(&schema, &batch.columns, row)?;
                committed_max_seq = committed_max_seq.max(batch.seqs[row]);
                visibility.apply_row(pk, batch.seqs[row], source, global_row)?;
                global_row += 1;
            }
        }
        segments.push(RecoveredSegment {
            source,
            relative_path: relative_path.clone(),
            batches: decoded,
        });
    }
    // 数据与 delta 各自可能乱序；位图与主键索引只比较 `_seq`，不依赖文件处理顺序。
    for relative_path in &files.delta_files {
        let path = partition_dir.join(relative_path);
        regular_file(&path)?;
        let batches = parquet_file::read_all(&path)?;
        if !batches.iter().any(|batch| batch.num_rows() > 0) {
            return Err(RecoveryError::EmptyReferencedFile {
                path,
                kind: "delta",
            });
        }
        for batch in &batches {
            let DecodedDelta { pks, seqs } = arrow_data::decode_delta_batch(&schema, batch)?;
            for (pk, seq) in pks.into_iter().zip(seqs) {
                committed_max_seq = committed_max_seq.max(seq);
                visibility.apply_delete(pk, seq)?;
            }
        }
    }
    if manifest.current_seq < committed_max_seq {
        return Err(RecoveryError::Unsupported(format!(
            "manifest current_seq {} is below committed file sequence {committed_max_seq}",
            manifest.current_seq
        )));
    }
    let committed = visibility.clone();
    let mut wal_warnings = Vec::new();
    let wal_paths = scan_wal(&collection_dir.join("wal"))?;
    let max_wal_number = wal_paths.keys().next_back().copied().unwrap_or(0);
    let active_source = SourceId(segments.len() as u64 + 1);
    visibility.add_source(active_source, 0)?;
    let mut active_memtable = MemTable::new(active_source);
    for (_number, (data_path, delta_path)) in wal_paths {
        let mut operations = Vec::new();
        if let Some(path) = data_path {
            regular_file(&path)?;
            let recovered = wal::read_prefix(&path)?;
            if let Some(reason) = recovered.stopped_at {
                wal_warnings.push(format!("{path:?}: {reason}"));
            }
            for batch in recovered.batches {
                let decoded = arrow_data::decode_wal_data_batch(&schema, &batch)?;
                if let Some(&first_seq) = decoded.seqs.first() {
                    operations.push((first_seq, WalOperation::Data(decoded)));
                }
            }
        }
        if let Some(path) = delta_path {
            regular_file(&path)?;
            let recovered = wal::read_prefix(&path)?;
            if let Some(reason) = recovered.stopped_at {
                wal_warnings.push(format!("{path:?}: {reason}"));
            }
            for batch in recovered.batches {
                let decoded = arrow_data::decode_wal_delta_batch(&schema, &batch)?;
                if let Some(&first_seq) = decoded.seqs.first() {
                    operations.push((first_seq, WalOperation::Delete(decoded)));
                }
            }
        }
        // 参照实现按每个编号内 batch 的首个 `_seq` 稳定排序；data 同序号时先于 delta。
        operations.sort_by_key(|(first_seq, _)| *first_seq);
        for (_, operation) in operations {
            match operation {
                WalOperation::Data(decoded) => {
                    let filtered = filter_committed_rows(&schema, decoded, &committed)?;
                    if !filtered.seqs.is_empty() {
                        active_memtable.apply_data(&schema, filtered, &mut visibility)?;
                    }
                }
                WalOperation::Delete(delta) => {
                    for (pk, seq) in delta.pks.into_iter().zip(delta.seqs) {
                        let seg_max = committed.latest_data(&pk).map(|loc| loc.seq).unwrap_or(0);
                        let delta_max = committed.latest_delete(&pk).unwrap_or(0);
                        if seq > seg_max.max(delta_max) {
                            active_memtable.apply_delete(&[pk], seq, &mut visibility)?;
                        }
                    }
                }
            }
        }
    }
    let next_seq = manifest
        .current_seq
        .max(active_memtable.max_seq())
        .checked_add(1)
        .ok_or_else(|| RecoveryError::Unsupported("_seq space exhausted".to_owned()))?;
    let next_wal_number = max_wal_number
        .checked_add(1)
        .ok_or_else(|| RecoveryError::Unsupported("WAL number space exhausted".to_owned()))?
        .max(manifest.active_wal_number.unwrap_or(1));
    Ok(RecoveredCollection {
        schema_file,
        schema,
        manifest,
        segments,
        pinned_files,
        active_memtable,
        visibility,
        next_seq,
        next_wal_number,
        wal_warnings,
    })
}

type WalPaths = (Option<PathBuf>, Option<PathBuf>);

fn scan_wal(wal_dir: &Path) -> Result<BTreeMap<u64, WalPaths>, RecoveryError> {
    let entries = match fs::read_dir(wal_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(source) => {
            return Err(RecoveryError::Io {
                path: wal_dir.to_path_buf(),
                source,
            });
        }
    };
    let mut paths: BTreeMap<u64, WalPaths> = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|source| RecoveryError::Io {
            path: wal_dir.to_path_buf(),
            source,
        })?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".arrow") else {
            continue;
        };
        let (kind, number) = if let Some(number) = stem.strip_prefix("wal_data_") {
            (0, number)
        } else if let Some(number) = stem.strip_prefix("wal_delta_") {
            (1, number)
        } else {
            continue;
        };
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let number = number.parse::<u64>().map_err(|_| {
            RecoveryError::Unsupported(format!("WAL number exceeds u64 in {name:?}"))
        })?;
        let canonical = if kind == 0 {
            format!("wal_data_{number:06}.arrow")
        } else {
            format!("wal_delta_{number:06}.arrow")
        };
        if name != canonical {
            continue;
        }
        let slot = paths.entry(number).or_default();
        if kind == 0 {
            slot.0 = Some(entry.path());
        } else {
            slot.1 = Some(entry.path());
        }
    }
    Ok(paths)
}

fn regular_file(path: &Path) -> Result<(), RecoveryError> {
    let meta = fs::symlink_metadata(path).map_err(|source| RecoveryError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !meta.file_type().is_file() {
        return Err(RecoveryError::Unsupported(format!(
            "referenced path {path:?} is not a regular file"
        )));
    }
    Ok(())
}

fn pk_at(
    schema: &CollectionSchema,
    columns: &[ColumnData],
    row: usize,
) -> Result<Pk, RecoveryError> {
    let pos = schema
        .primary_position()
        .ok_or_else(|| RecoveryError::Unsupported("no primary key".to_owned()))?;
    match &columns[pos] {
        ColumnData::Int64(values) => Ok(Pk::Int64(values[row])),
        ColumnData::VarChar(values) => Ok(Pk::VarChar(values[row].clone())),
        _ => Err(RecoveryError::Unsupported(
            "unsupported primary key dtype".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wal_collection(root: &Path) -> (PathBuf, CollectionSchema) {
        let collection = root.join("c");
        fs::create_dir_all(collection.join("wal")).unwrap();
        fs::write(
            collection.join("schema.json"),
            include_bytes!("../../tests/fixtures/schema_3x.json"),
        )
        .unwrap();
        let schema = SchemaFile::read(&collection.join("schema.json"))
            .unwrap()
            .to_core()
            .unwrap();
        (collection, schema)
    }

    fn append_data(writer: &mut wal::WalWriter, schema: &CollectionSchema, seq: u64, id: i64) {
        let columns = [
            ColumnData::Int64(vec![id]),
            ColumnData::FloatVector {
                dim: 2,
                values: vec![1.0, 2.0],
            },
        ];
        let batch = arrow_data::wal_data_batch(schema, &[seq], "_default", &columns, None).unwrap();
        writer.append(&batch).unwrap();
    }

    fn frozen_replay_ids(recovered: RecoveredCollection) -> (Vec<i64>, Vec<u64>) {
        let frozen = recovered.active_memtable.freeze(&recovered.visibility);
        let materialized = frozen.materialize(&recovered.schema).unwrap();
        let decoded =
            arrow_data::decode_data_batch(&recovered.schema, materialized.data.as_ref().unwrap())
                .unwrap();
        let ColumnData::Int64(ids) = &decoded.columns[0] else {
            panic!("fixture primary key type changed");
        };
        (ids.clone(), decoded.seqs)
    }

    #[test]
    fn wal_replay_interleaves_data_and_delta_batches_by_seq() {
        let root = tempfile::tempdir().unwrap();
        let (collection, schema) = wal_collection(root.path());
        let mut data_writer = wal::WalWriter::create(
            &collection.join("wal"),
            wal::WalKind::Data,
            1,
            &arrow_data::wal_data_schema(&schema).unwrap(),
        )
        .unwrap();
        append_data(&mut data_writer, &schema, 1, 1);
        append_data(&mut data_writer, &schema, 2, 2);
        append_data(&mut data_writer, &schema, 4, 1);
        let mut delta_writer = wal::WalWriter::create(
            &collection.join("wal"),
            wal::WalKind::Delta,
            1,
            &arrow_data::wal_delta_schema(&schema).unwrap(),
        )
        .unwrap();
        let delta = arrow_data::wal_delta_batch(&schema, &[Pk::Int64(1)], &[3], "_all").unwrap();
        delta_writer.append(&delta).unwrap();
        for file in [data_writer.into_file(), delta_writer.into_file()] {
            file.sync_all().unwrap();
        }

        let recovered = load_committed(&collection).unwrap();
        assert_eq!(recovered.visibility.live_count(), 2);
        assert_eq!(recovered.next_seq, 5);
        assert_eq!(frozen_replay_ids(recovered), (vec![2, 1], vec![2, 4]));
    }

    #[test]
    fn wal_replay_merges_generations_with_first_pk_position() {
        let root = tempfile::tempdir().unwrap();
        let (collection, schema) = wal_collection(root.path());
        for (number, rows) in [(1, vec![(1, 1), (2, 2)]), (2, vec![(3, 1)])] {
            let mut writer = wal::WalWriter::create(
                &collection.join("wal"),
                wal::WalKind::Data,
                number,
                &arrow_data::wal_data_schema(&schema).unwrap(),
            )
            .unwrap();
            for (seq, id) in rows {
                append_data(&mut writer, &schema, seq, id);
            }
            writer.into_file().sync_all().unwrap();
        }
        let recovered = load_committed(&collection).unwrap();
        assert_eq!(recovered.next_wal_number, 3);
        assert_eq!(frozen_replay_ids(recovered), (vec![1, 2], vec![3, 2]));
    }

    #[test]
    fn wal_replay_overwrite_keeps_first_pk_position_within_one_generation() {
        let root = tempfile::tempdir().unwrap();
        let (collection, schema) = wal_collection(root.path());
        let mut writer = wal::WalWriter::create(
            &collection.join("wal"),
            wal::WalKind::Data,
            1,
            &arrow_data::wal_data_schema(&schema).unwrap(),
        )
        .unwrap();
        for (seq, id) in [(1, 1), (2, 2), (3, 1)] {
            append_data(&mut writer, &schema, seq, id);
        }
        writer.into_file().sync_all().unwrap();
        let recovered = load_committed(&collection).unwrap();
        assert_eq!(frozen_replay_ids(recovered), (vec![1, 2], vec![3, 2]));
    }

    fn assert_empty_referenced_file_fails_closed(kind: &'static str) {
        let root = tempfile::tempdir().unwrap();
        let collection = root.path().join("c");
        let file_dir = collection.join("partitions/_default").join(kind);
        let wal_dir = collection.join("wal");
        fs::create_dir_all(&file_dir).unwrap();
        fs::create_dir_all(&wal_dir).unwrap();
        fs::write(
            collection.join("schema.json"),
            include_bytes!("../../tests/fixtures/schema_3x.json"),
        )
        .unwrap();
        let schema = SchemaFile::read(&collection.join("schema.json"))
            .unwrap()
            .to_core()
            .unwrap();
        let mut manifest = Manifest {
            current_seq: if kind == "delta" { 3 } else { 1 },
            ..Manifest::default()
        };
        if kind == "data" {
            let empty = arrow_data::data_batch(
                &schema,
                &[],
                &[
                    ColumnData::Int64(Vec::new()),
                    ColumnData::FloatVector {
                        dim: 2,
                        values: Vec::new(),
                    },
                ],
                None,
            )
            .unwrap();
            let name = "data_000001_000001.parquet";
            parquet_file::write_new(&file_dir.join(name), &empty).unwrap();
            manifest
                .partitions
                .get_mut("_default")
                .unwrap()
                .data_files
                .push(format!("data/{name}"));
        } else {
            let data_dir = collection.join("partitions/_default/data");
            fs::create_dir_all(&data_dir).unwrap();
            fs::write(
                data_dir.join("data_000001_000002.parquet"),
                include_bytes!("../../tests/fixtures/data_3x.parquet"),
            )
            .unwrap();
            manifest
                .partitions
                .get_mut("_default")
                .unwrap()
                .data_files
                .push("data/data_000001_000002.parquet".to_owned());
            let empty = arrow_data::delta_batch(&schema, &[], &[]).unwrap();
            let name = "delta_000001_000001.parquet";
            parquet_file::write_new(&file_dir.join(name), &empty).unwrap();
            manifest
                .partitions
                .get_mut("_default")
                .unwrap()
                .delta_files
                .push(format!("delta/{name}"));
        }
        fs::write(
            collection.join("manifest.json"),
            manifest.to_bytes().unwrap(),
        )
        .unwrap();
        let wal_path = wal_dir.join("wal_data_000002.arrow");
        fs::write(
            &wal_path,
            include_bytes!("../../tests/fixtures/wal_data_3x.arrow"),
        )
        .unwrap();
        let error = load_committed(&collection).unwrap_err();
        assert!(
            matches!(error, RecoveryError::EmptyReferencedFile { kind: actual, .. } if actual == kind)
        );
        assert!(
            file_dir
                .join(format!("{kind}_000001_000001.parquet"))
                .exists()
        );
        if kind == "delta" {
            assert!(
                collection
                    .join("partitions/_default/data/data_000001_000002.parquet")
                    .exists()
            );
        }
        assert!(wal_path.exists());
    }

    #[test]
    fn empty_referenced_data_file_fails_closed() {
        assert_empty_referenced_file_fails_closed("data");
    }

    #[test]
    fn empty_referenced_delta_file_fails_closed() {
        assert_empty_referenced_file_fails_closed("delta");
    }

    #[test]
    fn restores_committed_data_and_delta_without_touching_files() {
        let root = tempfile::tempdir().unwrap();
        let collection = root.path().join("c");
        let data_dir = collection.join("partitions/_default/data");
        let delta_dir = collection.join("partitions/_default/delta");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&delta_dir).unwrap();
        fs::write(
            collection.join("schema.json"),
            include_bytes!("../../tests/fixtures/schema_3x.json"),
        )
        .unwrap();
        fs::write(
            data_dir.join("data_000001_000002.parquet"),
            include_bytes!("../../tests/fixtures/data_3x.parquet"),
        )
        .unwrap();
        fs::write(
            delta_dir.join("delta_000003_000003.parquet"),
            include_bytes!("../../tests/fixtures/delta_3x.parquet"),
        )
        .unwrap();
        let mut manifest = Manifest {
            version: 2,
            current_seq: 3,
            ..Manifest::default()
        };
        manifest
            .partitions
            .get_mut("_default")
            .unwrap()
            .data_files
            .push("data/data_000001_000002.parquet".to_owned());
        manifest
            .partitions
            .get_mut("_default")
            .unwrap()
            .delta_files
            .push("delta/delta_000003_000003.parquet".to_owned());
        fs::write(
            collection.join("manifest.json"),
            manifest.to_bytes().unwrap(),
        )
        .unwrap();

        let recovered = load_committed(&collection).unwrap();
        assert_eq!(recovered.segments.len(), 1);
        assert_eq!(recovered.visibility.live_count(), 1);
        assert_eq!(recovered.visibility.is_live(SourceId(1), 0), Some(false));
        assert_eq!(recovered.visibility.is_live(SourceId(1), 1), Some(true));
        assert_eq!(recovered.visibility.latest_delete(&Pk::Int64(10)), Some(3));
        assert!(data_dir.join("data_000001_000002.parquet").exists());
    }

    #[test]
    fn missing_manifest_with_prev_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let collection = root.path().join("c");
        fs::create_dir(&collection).unwrap();
        fs::write(
            collection.join("schema.json"),
            include_bytes!("../../tests/fixtures/schema_3x.json"),
        )
        .unwrap();
        fs::write(
            collection.join("manifest.json.prev"),
            include_bytes!("../../tests/fixtures/manifest_3x.json"),
        )
        .unwrap();
        assert!(load_committed(&collection).is_err());
    }

    #[test]
    fn replays_3x_wal_data_and_delta_after_process_exit() {
        let root = tempfile::tempdir().unwrap();
        let collection = root.path().join("c");
        let wal_dir = collection.join("wal");
        fs::create_dir_all(&wal_dir).unwrap();
        fs::write(
            collection.join("schema.json"),
            include_bytes!("../../tests/fixtures/schema_3x.json"),
        )
        .unwrap();
        fs::write(
            wal_dir.join("wal_data_000001.arrow"),
            include_bytes!("../../tests/fixtures/wal_data_3x.arrow"),
        )
        .unwrap();
        fs::write(
            wal_dir.join("wal_delta_000003.arrow"),
            include_bytes!("../../tests/fixtures/wal_delta_3x.arrow"),
        )
        .unwrap();
        let recovered = load_committed(&collection).unwrap();
        assert_eq!(recovered.active_memtable.physical_rows(), 2);
        assert_eq!(recovered.active_memtable.size(), 2);
        assert_eq!(recovered.visibility.live_count(), 1);
        assert_eq!(recovered.visibility.is_live(SourceId(1), 0), Some(false));
        assert_eq!(recovered.visibility.is_live(SourceId(1), 1), Some(true));
        assert_eq!(recovered.next_seq, 4);
        assert_eq!(recovered.next_wal_number, 4);
        let frozen = recovered.active_memtable.freeze(&recovered.visibility);
        let materialized = frozen.materialize(&recovered.schema).unwrap();
        let data =
            arrow_data::decode_data_batch(&recovered.schema, materialized.data.as_ref().unwrap())
                .unwrap();
        let delta =
            arrow_data::decode_delta_batch(&recovered.schema, materialized.delta.as_ref().unwrap())
                .unwrap();
        assert_eq!(data.columns[0], ColumnData::Int64(vec![20]));
        assert_eq!(delta.pks, [Pk::Int64(10)]);
        assert_eq!(delta.seqs, [3]);
    }

    #[test]
    fn committed_wal_rows_are_not_replayed_twice() {
        let root = tempfile::tempdir().unwrap();
        let collection = root.path().join("c");
        let wal_dir = collection.join("wal");
        let data_dir = collection.join("partitions/_default/data");
        fs::create_dir_all(&wal_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(
            collection.join("schema.json"),
            include_bytes!("../../tests/fixtures/schema_3x.json"),
        )
        .unwrap();
        fs::write(
            data_dir.join("data_000001_000002.parquet"),
            include_bytes!("../../tests/fixtures/data_3x.parquet"),
        )
        .unwrap();
        fs::write(
            wal_dir.join("wal_data_000001.arrow"),
            include_bytes!("../../tests/fixtures/wal_data_3x.arrow"),
        )
        .unwrap();
        let mut manifest = Manifest {
            current_seq: 2,
            ..Manifest::default()
        };
        manifest
            .partitions
            .get_mut("_default")
            .unwrap()
            .data_files
            .push("data/data_000001_000002.parquet".to_owned());
        fs::write(
            collection.join("manifest.json"),
            manifest.to_bytes().unwrap(),
        )
        .unwrap();
        let recovered = load_committed(&collection).unwrap();
        assert_eq!(recovered.active_memtable.physical_rows(), 0);
        assert_eq!(recovered.visibility.live_count(), 2);
        assert_eq!(recovered.next_seq, 3);
        assert_eq!(recovered.next_wal_number, 2);
    }

    #[test]
    fn only_canonical_wal_names_are_replayed() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("wal_data_1.arrow"), b"noncanonical").unwrap();
        fs::write(root.path().join("wal_data_000001.arrow"), b"canonical").unwrap();
        let paths = scan_wal(root.path()).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(
            paths[&1].0.as_ref().unwrap().file_name().unwrap(),
            "wal_data_000001.arrow"
        );
    }
}
