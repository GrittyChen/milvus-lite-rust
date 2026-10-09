//! 已提交数据的有界合并；选择、物理行映射与提交协议见决策 0010。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::collection::Chunk;
use crate::data::{ColumnData, Pk};
use crate::error::{EngineError, Result};
use crate::schema::CollectionSchema;
use crate::storage::arrow_data::{self, DecodedData};
use crate::storage::visibility::SourceId;
use crate::storage::{fs_commit, manifest::Manifest, parquet_file, snapshot::PinnedFiles};

pub(crate) struct View {
    pub dir: PathBuf,
    pub schema: CollectionSchema,
    pub manifest: Manifest,
    pub pins: PinnedFiles,
    pub inputs: Vec<Input>,
    pub sync_inherited: bool,
}

const INPUT_ROWS: usize = 200_000;
const OUTPUT_ROWS: usize = 100_000;
static TEMP_NUMBER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub(crate) struct Input {
    pub relative: String,
    pub source: SourceId,
    pub rank: usize,
    pub chunks: Vec<Chunk>,
}

impl Input {
    pub fn rows(&self) -> usize {
        self.chunks.iter().map(|c| c.seqs.len()).sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Target {
    pub output: usize,
    pub row: usize,
}

#[derive(Debug)]
pub(crate) struct Output {
    pub data: DecodedData,
    pub norms: Vec<f32>,
    pub temp: PathBuf,
    pub relative: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Prepared {
    pub inputs: Vec<Input>,
    pub mapping: Vec<Vec<Option<Target>>>,
    pub outputs: Vec<Output>,
    pub input_rows: usize,
}

impl Drop for Prepared {
    fn drop(&mut self) {
        // 最终名可能已提交或被后续 flush 使用；此处只清理不会被复用的临时名。
        for output in &self.outputs {
            if let Err(error) = fs::remove_file(&output.temp)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!(
                    "cannot remove compaction temporary file {:?}: {error}",
                    output.temp
                );
            }
        }
    }
}

fn storage(reason: impl std::fmt::Display) -> EngineError {
    EngineError::Storage(reason.to_string())
}

fn bucket(size: u64) -> usize {
    [1_000_000, 10_000_000, 100_000_000]
        .iter()
        .position(|bound| size < *bound)
        .unwrap_or(3)
}

fn cap(candidates: &[usize], counts: &[usize]) -> Vec<usize> {
    let mut selected = Vec::new();
    let mut rows = 0;
    for &index in candidates {
        let n = counts[index];
        if n >= INPUT_ROWS {
            continue;
        }
        if n > INPUT_ROWS - rows {
            break;
        }
        selected.push(index);
        rows += n;
    }
    selected
}

pub(crate) fn select(dir: &Path, inputs: Vec<Input>) -> Result<Vec<Input>> {
    if inputs.len() < 4 {
        return Ok(Vec::new());
    }
    let mut buckets: [Vec<usize>; 4] = std::array::from_fn(|_| Vec::new());
    for (index, input) in inputs.iter().enumerate() {
        let path = dir.join("partitions/_default").join(&input.relative);
        let meta = fs::symlink_metadata(&path).map_err(storage)?;
        if !meta.file_type().is_file() {
            return Err(storage("compaction input is not a regular file"));
        }
        let (min, max) = sequence_range(&input.relative)?;
        if input
            .chunks
            .iter()
            .flat_map(|c| &c.seqs)
            .any(|seq| *seq < min || *seq > max)
        {
            return Err(storage(
                "compaction input filename does not cover its content",
            ));
        }
        buckets[bucket(meta.len())].push(index);
    }
    let counts: Vec<_> = inputs.iter().map(Input::rows).collect();
    let mut selected = Vec::new();
    for entries in &buckets {
        if entries.len() >= 4 {
            let picked = cap(entries, &counts);
            if picked.len() >= 4 {
                selected = picked;
                break;
            }
        }
    }
    if selected.is_empty() && inputs.len() > 32 {
        let all: Vec<_> = buckets.into_iter().flatten().collect();
        let picked = cap(&all, &counts);
        if picked.len() >= 2 {
            selected = picked;
        }
    }
    Ok(selected.into_iter().map(|i| inputs[i].clone()).collect())
}

pub(crate) fn primary(chunk: &Chunk, pk_pos: usize, row: usize) -> Result<Pk> {
    match chunk.columns.get(pk_pos) {
        Some(ColumnData::Int64(v)) => v.get(row).copied().map(Pk::Int64),
        Some(ColumnData::VarChar(v)) => v.get(row).cloned().map(Pk::VarChar),
        _ => None,
    }
    .ok_or_else(|| storage("compaction row has no primary key"))
}

pub(crate) fn reject_external_sequence_ties(
    all: &[Input],
    selected: &[Input],
    pk_pos: usize,
) -> Result<()> {
    let mut winners = HashMap::<Pk, u64>::new();
    for input in selected {
        for chunk in &input.chunks {
            for row in 0..chunk.seqs.len() {
                let pk = primary(chunk, pk_pos, row)?;
                let seq = chunk.seqs[row];
                winners
                    .entry(pk)
                    .and_modify(|old| *old = (*old).max(seq))
                    .or_insert(seq);
            }
        }
    }
    for input in all
        .iter()
        .filter(|input| !selected.iter().any(|s| s.source == input.source))
    {
        for chunk in &input.chunks {
            for row in 0..chunk.seqs.len() {
                let pk = primary(chunk, pk_pos, row)?;
                if winners.get(&pk) == Some(&chunk.seqs[row]) {
                    return Err(storage(
                        "compaction deferred: equal primary sequence in an unselected source",
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn prepare(
    dir: &Path,
    schema: &CollectionSchema,
    inputs: Vec<Input>,
    deletes: &HashMap<Pk, u64>,
    cancelled: impl Fn() -> bool,
) -> Result<Prepared> {
    let pk_pos = schema
        .primary_position()
        .ok_or_else(|| storage("compaction schema has no primary key"))?;
    let mut winners: HashMap<Pk, (u64, usize, usize, usize, usize)> = HashMap::new();
    for (i, input) in inputs.iter().enumerate() {
        let mut physical = 0;
        for (b, chunk) in input.chunks.iter().enumerate() {
            if cancelled() {
                return Err(EngineError::Closed);
            }
            if chunk.row_start != physical {
                return Err(storage("compaction input blocks are not contiguous"));
            }
            for row in 0..chunk.seqs.len() {
                let pk = primary(chunk, pk_pos, row)?;
                let value = (chunk.seqs[row], i, b, row, physical + row);
                let replace = winners.get(&pk).is_none_or(|old| {
                    value.0 > old.0
                        || (value.0 == old.0 && (input.rank, value.4) < (inputs[old.1].rank, old.4))
                });
                if replace {
                    winners.insert(pk, value);
                }
            }
            physical += chunk.seqs.len();
        }
    }
    let mut retained: Vec<_> = winners
        .into_iter()
        .filter(|(pk, row)| deletes.get(pk).is_none_or(|seq| *seq <= row.0))
        .map(|(_, row)| row)
        .collect();
    retained.sort_by_key(|r| (r.0, inputs[r.1].rank, r.4));
    let input_rows = inputs.iter().map(Input::rows).sum();
    let mut prepared = Prepared {
        mapping: inputs
            .iter()
            .map(|input| vec![None; input.rows()])
            .collect(),
        inputs,
        outputs: Vec::new(),
        input_rows,
    };
    for (output, rows) in retained.chunks(OUTPUT_ROWS).enumerate() {
        if cancelled() {
            return Err(EngineError::Closed);
        }
        let mut data = DecodedData {
            columns: schema.fields.iter().map(ColumnData::empty).collect(),
            seqs: Vec::with_capacity(rows.len()),
            meta: schema
                .enable_dynamic_field
                .then(|| Vec::with_capacity(rows.len())),
        };
        let mut norms = Vec::with_capacity(rows.len());
        for (target, &(seq, i, block, row, physical)) in rows.iter().enumerate() {
            let chunk = &prepared.inputs[i].chunks[block];
            for (dst, src) in data.columns.iter_mut().zip(&chunk.columns) {
                dst.push_row(src, row);
            }
            data.seqs.push(seq);
            norms.push(
                *chunk
                    .norms
                    .get(row)
                    .ok_or_else(|| storage("compaction vector norm missing"))?,
            );
            if let Some(meta) = &mut data.meta {
                meta.push(
                    chunk
                        .meta
                        .as_ref()
                        .and_then(|v| v.get(row))
                        .ok_or_else(|| storage("compaction meta missing"))?
                        .clone(),
                );
            }
            prepared.mapping[i][physical] = Some(Target {
                output,
                row: target,
            });
        }
        let batch = arrow_data::data_batch(schema, &data.seqs, &data.columns, data.meta.as_deref())
            .map_err(storage)?;
        let temp = dir.join("partitions/_default/data").join(format!(
            ".compaction-{}-{}.tmp",
            std::process::id(),
            TEMP_NUMBER.fetch_add(1, Ordering::Relaxed)
        ));
        // write_new 在返回前完成 Parquet 收尾和文件 fsync；提交锁只负责目录与 manifest。
        prepared.outputs.push(Output {
            data,
            norms,
            temp,
            relative: None,
        });
        parquet_file::write_new(&prepared.outputs[output].temp, &batch).map_err(storage)?;
        if cancelled() {
            return Err(EngineError::Closed);
        }
    }
    Ok(prepared)
}

pub(crate) fn sequence_range(relative: &str) -> Result<(u64, u64)> {
    let name = Path::new(relative)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| storage("invalid segment filename"))?;
    let stem = name
        .strip_suffix(".parquet")
        .ok_or_else(|| storage("invalid segment filename"))?;
    let mut parts = stem.split('_');
    let kind = parts.next();
    let min = parts.next().and_then(|s| s.parse::<u64>().ok());
    let max = parts.next().and_then(|s| s.parse::<u64>().ok());
    match (kind, min, max, parts.next()) {
        (Some("data" | "delta"), Some(min), Some(max), None) if min <= max => Ok((min, max)),
        _ => Err(storage("invalid segment sequence range")),
    }
}

pub(crate) fn promote(dir: &Path, prepared: &mut Prepared, pins: &PinnedFiles) -> Result<()> {
    let partition = dir.join("partitions/_default");
    for output in &mut prepared.outputs {
        let min = *output
            .data
            .seqs
            .first()
            .ok_or_else(|| storage("compaction output is empty"))?;
        let mut max = *output
            .data
            .seqs
            .last()
            .ok_or_else(|| storage("compaction output is empty"))?;
        let mut chosen = None;
        for _ in 0..10_000 {
            let relative = format!("data/data_{min:06}_{max:06}.parquet");
            let path = partition.join(&relative);
            let exists = match fs::symlink_metadata(&path) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(storage(error)),
            };
            if !exists
                && !pins.data.contains(&("_default".into(), relative.clone()))
                && !super::flush::has_index_sidecar(
                    &partition.join("data"),
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .ok_or_else(|| storage("invalid output stem"))?,
                )
                .map_err(storage)?
            {
                fs::rename(&output.temp, &path).map_err(storage)?;
                // 后续输出或同步失败时，调用方仍须知道此文件已成为最终名孤儿。
                output.relative = Some(relative);
                #[cfg(feature = "fault-injection")]
                fs_commit::record_fs(fs_commit::FsAction::Rename, &path);
                chosen = Some(());
                break;
            }
            max = max
                .checked_add(1)
                .ok_or_else(|| storage("compaction filename sequence exhausted"))?;
        }
        chosen.ok_or_else(|| storage("compaction filename attempts exhausted"))?;
    }
    if !prepared.outputs.is_empty() {
        fs_commit::sync_dir(&partition.join("data")).map_err(storage)?;
    }
    Ok(())
}

pub(crate) fn candidate(
    current: &Manifest,
    prepared: &Prepared,
    pins: &PinnedFiles,
) -> Result<(Manifest, Option<u64>, Vec<PathBuf>)> {
    let mut next = current.clone();
    next.version = next
        .version
        .checked_add(1)
        .ok_or_else(|| storage("manifest version exhausted"))?;
    let files = next
        .partitions
        .get_mut("_default")
        .ok_or_else(|| storage("default partition missing"))?;
    for input in &prepared.inputs {
        if !files.data_files.contains(&input.relative) {
            return Err(storage("compaction input is no longer referenced"));
        }
    }
    let mut obsolete = Vec::new();
    files.data_files.retain(|relative| {
        let remove = prepared
            .inputs
            .iter()
            .any(|input| input.relative == *relative);
        if remove && !pins.data.contains(&("_default".into(), relative.clone())) {
            obsolete.push(PathBuf::from(relative));
        }
        !remove
    });
    for output in &prepared.outputs {
        files.data_files.push(
            output
                .relative
                .clone()
                .ok_or_else(|| storage("compaction output has no final name"))?,
        );
    }
    let threshold = files
        .data_files
        .iter()
        .map(|name| sequence_range(name).map(|range| range.0))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .min();
    let mut delta = Vec::new();
    for relative in &files.delta_files {
        let max = sequence_range(relative)?.1;
        if threshold.is_none_or(|t| max < t)
            && !pins.delta.contains(&("_default".into(), relative.clone()))
        {
            obsolete.push(PathBuf::from(relative));
        } else {
            delta.push(relative.clone());
        }
    }
    files.delta_files = delta;
    Ok((next, threshold, obsolete))
}

pub(crate) fn cleanup(
    dir: &Path,
    obsolete: &[PathBuf],
    pins: &PinnedFiles,
    cancelled: impl Fn() -> bool,
) -> Result<bool> {
    let partition = dir.join("partitions/_default");
    let mut paths: Vec<_> = obsolete.iter().map(|p| partition.join(p)).collect();
    let index_dir = partition.join("indexes");
    match fs::symlink_metadata(&index_dir) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(storage(error)),
        Ok(_) => {
            return Err(storage(
                "compaction index cleanup path is not a regular directory",
            ));
        }
    }
    match fs::read_dir(&index_dir) {
        Ok(entries) => {
            for entry in entries {
                let path = entry.map_err(storage)?.path();
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !name.ends_with(".idx") && !name.ends_with(".sidx") {
                    continue;
                }
                if obsolete.iter().filter(|p| p.starts_with("data")).any(|p| {
                    p.file_stem()
                        .and_then(|s| s.to_str())
                        .is_some_and(|stem| name.starts_with(&format!("{stem}.")))
                }) && !pins
                    .index
                    .contains(&("_default".into(), format!("indexes/{name}")))
                {
                    paths.push(path);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(storage(error)),
    }
    // 先验证完整集合，类型损坏不触发前半段的破坏性清理。
    for path in &paths {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_file() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => return Err(storage("compaction cleanup path is not a regular file")),
            Err(error) => return Err(storage(error)),
        }
    }
    for path in paths {
        if cancelled() {
            return Ok(false);
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                #[cfg(feature = "fault-injection")]
                fs_commit::record_fs(fs_commit::FsAction::Unlink, &path);
                fs_commit::sync_dir(
                    path.parent()
                        .ok_or_else(|| storage("cleanup path has no parent"))?,
                )
                .map_err(storage)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(storage(error)),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_promotion_retains_every_owned_final_path_for_retry_cleanup() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("partitions/_default/data")).unwrap();
        let first = root.path().join("partitions/_default/data/.first.tmp");
        fs::write(&first, b"owned file").unwrap();
        let mut prepared = Prepared {
            inputs: Vec::new(),
            mapping: Vec::new(),
            input_rows: 2,
            outputs: vec![
                Output {
                    data: DecodedData {
                        columns: Vec::new(),
                        meta: None,
                        seqs: vec![1],
                    },
                    norms: Vec::new(),
                    temp: first.clone(),
                    relative: None,
                },
                Output {
                    data: DecodedData {
                        columns: Vec::new(),
                        meta: None,
                        seqs: vec![2],
                    },
                    norms: Vec::new(),
                    temp: root.path().join("missing.tmp"),
                    relative: None,
                },
            ],
        };
        assert!(promote(root.path(), &mut prepared, &PinnedFiles::default()).is_err());
        let final_path = root
            .path()
            .join("partitions/_default")
            .join(prepared.outputs[0].relative.as_ref().unwrap());
        assert!(prepared.outputs[1].relative.is_none());
        assert_eq!(fs::read(&final_path).unwrap(), b"owned file");
        assert!(!first.exists());
        drop(prepared);
        assert!(final_path.exists());
    }

    #[test]
    fn row_budget_skips_terminal_files_and_stops_at_the_first_oversized_prefix() {
        assert_eq!(cap(&[0, 1, 2, 3], &[50_000; 4]), vec![0, 1, 2, 3]);
        assert_eq!(cap(&[0, 1, 2, 3], &[200_000, 199_999, 1, 1]), vec![1, 2]);
        assert_eq!(cap(&[0, 1, 2], &[199_999, 2, 1]), vec![0]);
    }

    #[test]
    fn buckets_and_sequence_ranges_keep_strict_boundaries() {
        assert_eq!(
            [999_999, 1_000_000, 10_000_000, 100_000_000].map(bucket),
            [0, 1, 2, 3]
        );
        assert_eq!(
            sequence_range("data/data_000003_000007.parquet").unwrap(),
            (3, 7)
        );
        assert!(sequence_range("data/data_7_3.parquet").is_err());
        assert!(sequence_range("delta/delta_1_2_extra.parquet").is_err());
    }
}
