//! 清空的持久化意图（0023 A「清空」）：私有文件，放在集合目录，记录旧 manifest、候选 manifest、待删的
//! snapshot 与旧索引侧车。当前实例的清理重试与重开时的处理都按它把清空收尾到「未清空」或「已清空」之一。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::fs_commit::{self, ReplaceError, ReplacePoint, guarded_io};
use super::manifest::Manifest;
use super::schema_file::SchemaFile;
use super::snapshot::{self, PinnedFiles};

pub const INTENT_FILE: &str = "milvus-lite-rust.truncate-intent.json";
const OPERATION: &str = "truncate_collection";

#[derive(Clone, Debug, PartialEq, Eq)]
struct SnapshotRef {
    name: String,
    id: String,
    /// 相对集合目录的 `partitions/<分区>/<data|delta|indexes>/<文件>`。
    files: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct TruncateIntent {
    /// 旧 `manifest.json` 的原文；`old` 为其解析结果。
    old_text: Option<String>,
    old: Option<Manifest>,
    candidate: Vec<u8>,
    snapshots: Vec<SnapshotRef>,
    index_files: Vec<String>,
}

/// 已提交、清理未完成的清空；`staging_wal` 是第 3 步移入 staging 的 `wal/`，只在当前实例内记录。
#[derive(Clone, Debug)]
pub(crate) struct Pending {
    pub(crate) intent: TruncateIntent,
    pub(crate) staging_wal: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    format_version: u32,
    operation: String,
    old_manifest: Option<String>,
    candidate_manifest: String,
    snapshots: Vec<SnapshotMarker>,
    index_files: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotMarker {
    name: String,
    id: String,
    files: Vec<String>,
}

#[derive(Debug)]
pub(crate) enum ReadError {
    /// 可随环境恢复，下次打开重试。
    Io(io::Error),
    /// 损坏或版本未知：不自动处理，需人工恢复。
    Invalid(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "cannot read truncate intent: {error}"),
            Self::Invalid(reason) => write!(f, "invalid truncate intent: {reason}"),
        }
    }
}

/// 清理各步骤的故障点。
#[derive(Clone, Copy, Debug)]
pub(crate) enum Step {
    Schema,
    SchemaAfterRename,
    Snapshots,
    Files,
    Prev,
    PrevAfterRename,
    IntentRemove,
    IntentRemoveAfter,
    /// 重开时确认 `manifest.json` 与集合目录落盘之前。
    ManifestSync,
}

pub(crate) enum CleanupError {
    /// 可在当前实例重试或重开时补做。
    Pending(String),
    /// `schema.json` 已替换，集合目录的同步未确认。
    SchemaUnconfirmed(String),
}

/// 第 4 步的候选：各分区文件列表清空、`version` 加 1、`current_seq` 归零、`active_wal_number` 置空，其余字段原样。
fn candidate(old: Option<&Manifest>) -> Result<Manifest, String> {
    let mut candidate = old.cloned().unwrap_or_default();
    for files in candidate.partitions.values_mut() {
        files.data_files.clear();
        files.delta_files.clear();
    }
    candidate.version = candidate
        .version
        .checked_add(1)
        .ok_or("manifest version space exhausted")?;
    candidate.current_seq = 0;
    candidate.active_wal_number = None;
    Ok(candidate)
}

fn valid_snapshot_name(name: &str) -> bool {
    !matches!(name, "" | "." | "..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn valid_snapshot_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 意图中的文件路径：`partitions/<分区>/data|delta/<kind>_<数字>_<数字>.parquet` 或
/// `partitions/<分区>/indexes/<侧车>`；返回（分区，种类，文件名）。
fn split_path(path: &str) -> Option<(&str, &str, &str)> {
    let parts: Vec<_> = path.split('/').collect();
    let [root, partition, kind, file] = parts.as_slice() else {
        return None;
    };
    if *root != "partitions" || !super::partition_paths::valid_name(partition) {
        return None;
    }
    let valid = match *kind {
        "data" | "delta" => {
            file.starts_with(&format!("{kind}_"))
                && super::compaction::sequence_range(&format!("{kind}/{file}")).is_ok()
        }
        "indexes" => {
            !file.contains(['\\', ':']) && crate::index::parse_sidecar_name(file).is_some()
        }
        _ => false,
    };
    valid.then_some((*partition, *kind, *file))
}

fn manifest_files(manifest: &Manifest) -> impl Iterator<Item = (&str, String)> {
    manifest.partitions.iter().flat_map(|(partition, files)| {
        files
            .data_files
            .iter()
            .chain(&files.delta_files)
            .map(move |rel| (partition.as_str(), format!("partitions/{partition}/{rel}")))
    })
}

/// 列表内每个侧车的 stem 都须对应同一分区的某个 data 文件（`data` 给出可引用的 data 文件）。
fn check_files(
    label: &str,
    files: &[String],
    data: &BTreeSet<(String, String)>,
) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for path in files {
        let (partition, kind, file) =
            split_path(path).ok_or_else(|| format!("invalid {label} path {path:?}"))?;
        if !seen.insert(path) {
            return Err(format!("duplicate {label} path {path:?}"));
        }
        if kind == "indexes" {
            let stem = crate::index::parse_sidecar_name(file)
                .map(|name| name.source_stem)
                .unwrap_or_default();
            if !data.contains(&(partition.to_owned(), stem.to_owned())) {
                return Err(format!(
                    "{label} sidecar {path:?} has no matching data file"
                ));
            }
        }
    }
    Ok(())
}

fn data_stems<'a>(
    files: impl Iterator<Item = &'a str>,
) -> Result<BTreeSet<(String, String)>, String> {
    files
        .filter_map(|path| {
            let (partition, kind, file) = match split_path(path) {
                Some(parts) => parts,
                None => return Some(Err(format!("invalid file path {path:?}"))),
            };
            (kind == "data").then(|| {
                Ok((
                    partition.to_owned(),
                    file.trim_end_matches(".parquet").to_owned(),
                ))
            })
        })
        .collect()
}

fn parse(bytes: &[u8]) -> Result<TruncateIntent, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    // `Option` 字段缺失时 serde 会静默取 None；缺字段按损坏处理。
    for key in [
        "format_version",
        "operation",
        "old_manifest",
        "candidate_manifest",
        "snapshots",
        "index_files",
    ] {
        if value.get(key).is_none() {
            return Err(format!("missing field {key}"));
        }
    }
    let marker: Marker = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if marker.format_version != 1 {
        return Err(format!(
            "unsupported format_version {}",
            marker.format_version
        ));
    }
    if marker.operation != OPERATION {
        return Err(format!("unknown operation {:?}", marker.operation));
    }
    let old = marker
        .old_manifest
        .as_deref()
        .map(|text| Manifest::parse(text.as_bytes()))
        .transpose()
        .map_err(|e| format!("old manifest: {e}"))?;
    let parsed = Manifest::parse(marker.candidate_manifest.as_bytes())
        .map_err(|e| format!("candidate manifest: {e}"))?;
    let mut expected = candidate(old.as_ref())?;
    // 分区顺序只来自 JSON 键序，不参与比较。
    expected.partition_order.clone_from(&parsed.partition_order);
    if parsed != expected {
        return Err("candidate manifest does not follow from the old manifest".to_owned());
    }
    let old_data = match &old {
        Some(old) => {
            if old
                .partitions
                .keys()
                .any(|name| !super::partition_paths::valid_name(name))
            {
                return Err("old manifest has an invalid partition name".to_owned());
            }
            let files: Vec<_> = manifest_files(old).map(|(_, path)| path).collect();
            check_files("old manifest", &files, &BTreeSet::new())?;
            data_stems(files.iter().map(String::as_str))?
        }
        None => BTreeSet::new(),
    };
    check_files("index", &marker.index_files, &old_data)?;
    if let Some(path) = marker
        .index_files
        .iter()
        .find(|path| split_path(path).is_some_and(|(_, kind, _)| kind != "indexes"))
    {
        return Err(format!("index path {path:?} is not a sidecar"));
    }
    let mut names = BTreeSet::new();
    let mut snapshots = Vec::new();
    for snap in marker.snapshots {
        if !valid_snapshot_name(&snap.name) || !valid_snapshot_id(&snap.id) {
            return Err(format!("invalid snapshot {:?}", snap.name));
        }
        if !names.insert(snap.name.clone()) {
            return Err(format!("duplicate snapshot {:?}", snap.name));
        }
        let data = data_stems(snap.files.iter().map(String::as_str))?;
        check_files("snapshot", &snap.files, &data)?;
        snapshots.push(SnapshotRef {
            name: snap.name,
            id: snap.id,
            files: snap.files,
        });
    }
    Ok(TruncateIntent {
        old_text: marker.old_manifest,
        old,
        candidate: marker.candidate_manifest.into_bytes(),
        snapshots,
        index_files: marker.index_files,
    })
}

fn snapshot_paths(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    let root = dir.join("snapshots");
    (
        root.join("metadata").join(format!("{name}.json")),
        root.join("manifests").join(name),
    )
}

/// 元数据文件中的 `id`；文件不存在时为 None。
fn metadata_id(path: &Path) -> Result<Option<String>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => return Err(format!("snapshot metadata {path:?} is not a regular file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{path:?}: {error}")),
    }
    let bytes = fs::read(path).map_err(|e| format!("{path:?}: {e}"))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("{path:?}: {e}"))?;
    value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(|id| Some(id.to_owned()))
        .ok_or_else(|| format!("snapshot metadata {path:?} has no id"))
}

impl TruncateIntent {
    /// 由清空开始时的磁盘状态构造意图与候选 manifest；序列化结果先按重开时的规则校验一遍。
    pub(crate) fn prepare(
        dir: &Path,
        old_manifest: Option<Vec<u8>>,
    ) -> Result<(Self, Manifest), String> {
        let old_manifest = old_manifest
            .map(|bytes| String::from_utf8(bytes).map_err(|_| "manifest.json is not UTF-8"))
            .transpose()?;
        let old = old_manifest
            .as_deref()
            .map(|text| Manifest::parse(text.as_bytes()))
            .transpose()
            .map_err(|e| e.to_string())?;
        let next = candidate(old.as_ref())?;
        let candidate_manifest = String::from_utf8(next.to_bytes().map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        let mut snapshots = Vec::new();
        for snap in snapshot::index_snapshots(dir).map_err(|e| e.to_string())? {
            // 清理按 `<name>.json` 定位元数据；名字与文件不对应时不能确认删除的是同一个 snapshot。
            let (metadata, _) = snapshot_paths(dir, &snap.name);
            if metadata_id(&metadata)?.as_deref() != Some(snap.id.as_str()) {
                return Err(format!(
                    "snapshot {:?} does not match its metadata file",
                    snap.name
                ));
            }
            let mut files = Vec::new();
            for map in [&snap.data_files, &snap.delta_files, &snap.index_files] {
                for (partition, paths) in map {
                    files.extend(
                        paths
                            .iter()
                            .map(|rel| format!("partitions/{partition}/{rel}")),
                    );
                }
            }
            files.sort();
            files.dedup();
            snapshots.push(SnapshotMarker {
                name: snap.name,
                id: snap.id,
                files,
            });
        }
        let mut index_files = Vec::new();
        if let Some(old) = &old {
            for (partition, files) in &old.partitions {
                let stems: BTreeSet<_> = files
                    .data_files
                    .iter()
                    .filter_map(|rel| Path::new(rel).file_stem().and_then(|s| s.to_str()))
                    .collect();
                let indexes = dir.join("partitions").join(partition).join("indexes");
                let entries = match fs::read_dir(&indexes) {
                    Ok(entries) => entries,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(format!("{indexes:?}: {error}")),
                };
                for entry in entries {
                    let entry = entry.map_err(|e| format!("{indexes:?}: {e}"))?;
                    if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                        continue;
                    }
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    if crate::index::parse_sidecar_name(name)
                        .is_some_and(|sidecar| stems.contains(sidecar.source_stem))
                    {
                        index_files.push(format!("partitions/{partition}/indexes/{name}"));
                    }
                }
            }
        }
        index_files.sort();
        let marker = Marker {
            format_version: 1,
            operation: OPERATION.to_owned(),
            old_manifest,
            candidate_manifest,
            snapshots,
            index_files,
        };
        let bytes = serde_json::to_vec(&marker).map_err(|e| e.to_string())?;
        Ok((parse(&bytes)?, next))
    }

    pub(crate) fn candidate_bytes(&self) -> &[u8] {
        &self.candidate
    }

    fn marker_bytes(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(&Marker {
            format_version: 1,
            operation: OPERATION.to_owned(),
            old_manifest: self.old_text.clone(),
            candidate_manifest: String::from_utf8(self.candidate.clone())
                .map_err(|e| e.to_string())?,
            snapshots: self
                .snapshots
                .iter()
                .map(|snap| SnapshotMarker {
                    name: snap.name.clone(),
                    id: snap.id.clone(),
                    files: snap.files.clone(),
                })
                .collect(),
            index_files: self.index_files.clone(),
        })
        .map_err(|e| e.to_string())
    }

    /// 待删文件：旧 manifest 的 data / delta、旧索引侧车与所列 snapshot 引用的文件，减去当前 manifest
    /// （连同其 data 文件的侧车）与剩余 snapshot 引用的文件。
    fn deletions(&self, current: &Manifest, pins: &PinnedFiles) -> BTreeSet<String> {
        let mut files: BTreeSet<String> = self.index_files.iter().cloned().collect();
        if let Some(old) = &self.old {
            files.extend(manifest_files(old).map(|(_, path)| path));
        }
        for snap in &self.snapshots {
            files.extend(snap.files.iter().cloned());
        }
        let mut keep: BTreeSet<String> = manifest_files(current).map(|(_, path)| path).collect();
        for set in [&pins.data, &pins.delta, &pins.index] {
            keep.extend(
                set.iter()
                    .map(|(partition, rel)| format!("partitions/{partition}/{rel}")),
            );
        }
        let live_stems: BTreeMap<&str, BTreeSet<&str>> =
            current
                .partitions
                .iter()
                .fold(BTreeMap::new(), |mut stems, (partition, files)| {
                    stems.entry(partition.as_str()).or_default().extend(
                        files
                            .data_files
                            .iter()
                            .filter_map(|rel| Path::new(rel).file_stem().and_then(|s| s.to_str())),
                    );
                    stems
                });
        files.retain(|path| {
            if keep.contains(path) {
                return false;
            }
            match split_path(path) {
                Some((partition, "indexes", file)) => !crate::index::parse_sidecar_name(file)
                    .is_some_and(|sidecar| {
                        live_stems
                            .get(partition)
                            .is_some_and(|stems| stems.contains(sidecar.source_stem))
                    }),
                _ => true,
            }
        });
        files
    }
}

pub(crate) fn read(dir: &Path) -> Result<Option<TruncateIntent>, ReadError> {
    let path = dir.join(INTENT_FILE);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => return Err(ReadError::Invalid("not a regular file".to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ReadError::Io(error)),
    }
    let bytes = fs::read(&path).map_err(ReadError::Io)?;
    parse(&bytes).map(Some).map_err(ReadError::Invalid)
}

/// 经 [`fs_commit::replace_file`] 写入并同步集合目录。
pub(crate) fn write(
    dir: &Path,
    intent: &TruncateIntent,
    fault: impl Fn(ReplacePoint) -> io::Result<()>,
) -> Result<(), ReplaceError> {
    let bytes = intent
        .marker_bytes()
        .map_err(|error| ReplaceError::Unchanged(io::Error::other(error)))?;
    fs_commit::replace_file(dir, INTENT_FILE, &bytes, fault)
}

/// 删除意图并同步集合目录；已不存在也同步，确认上一次未确认的删除。`fault` 的两点分别在删除之前与之后。
pub(crate) fn remove(dir: &Path, fault: impl Fn(ReplacePoint) -> io::Result<()>) -> io::Result<()> {
    guarded_io(|| {
        fault(ReplacePoint::BeforeRename)?;
        match fs::remove_file(dir.join(INTENT_FILE)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fault(ReplacePoint::AfterRename)?;
        fs_commit::sync_dir(dir)
    })
}

/// 内容已是 `bytes` 时只同步所在目录（确认上一次未确认的替换），否则整体替换。
fn ensure_file(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    fault: impl Fn(ReplacePoint) -> io::Result<()>,
) -> Result<(), ReplaceError> {
    match fs::read(dir.join(name)) {
        Ok(current) if current == bytes => guarded_io(|| {
            fault(ReplacePoint::BeforeRename)?;
            fs_commit::sync_dir(dir)
        })
        .map_err(ReplaceError::Unchanged),
        Ok(_) => fs_commit::replace_file(dir, name, bytes, fault),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs_commit::replace_file(dir, name, bytes, fault)
        }
        Err(error) => Err(ReplaceError::Unchanged(error)),
    }
}

/// 第 6 步（不含之后的内存收尾）：`schema.json` 规范重写 → 删除名字与 id 都匹配的 snapshot → 删除待删文件
/// → 以候选字节重写 `manifest.json.prev` → 删除意图。每一步幂等，文件已不存在视为已删除；不触碰 `wal/`。
/// `name` 是集合目录名，写入 `schema.json` 的 `collection_name`。
pub(crate) fn cleanup(
    dir: &Path,
    name: &str,
    intent: &TruncateIntent,
    fault: impl Fn(Step) -> io::Result<()>,
) -> Result<(), CleanupError> {
    let pending = |error: &dyn std::fmt::Display| CleanupError::Pending(error.to_string());
    let replace_fault = |before: Step, after: Step| {
        let fault = &fault;
        move |point| match point {
            ReplacePoint::BeforeRename => fault(before),
            ReplacePoint::AfterRename => fault(after),
        }
    };
    let current = fs::read(dir.join("schema.json")).map_err(|e| pending(&e))?;
    let bytes = SchemaFile::with_name(&current, name).map_err(|e| pending(&e))?;
    match ensure_file(
        dir,
        "schema.json",
        &bytes,
        replace_fault(Step::Schema, Step::SchemaAfterRename),
    ) {
        Ok(()) => {}
        Err(ReplaceError::Unchanged(error)) => return Err(pending(&error)),
        Err(ReplaceError::Unconfirmed(error)) => {
            return Err(CleanupError::SchemaUnconfirmed(error.to_string()));
        }
    }
    guarded_io(|| {
        fault(Step::Snapshots)?;
        let mut synced = BTreeSet::new();
        for snap in &intent.snapshots {
            let (metadata, manifests) = snapshot_paths(dir, &snap.name);
            match metadata_id(&metadata).map_err(io::Error::other)? {
                Some(id) if id == snap.id => fs::remove_file(&metadata)?,
                // 同名的新 snapshot（3.x 之后建立）：不是意图所列的那一个。
                Some(_) => continue,
                None => {}
            }
            if fs::symlink_metadata(&manifests).is_ok_and(|meta| meta.file_type().is_dir()) {
                fs::remove_dir_all(&manifests)?;
            }
            synced.extend(metadata.parent().map(Path::to_owned));
            synced.extend(manifests.parent().map(Path::to_owned));
        }
        for path in synced {
            if path.is_dir() {
                fs_commit::sync_dir(&path)?;
            }
        }
        Ok(())
    })
    .map_err(|e| pending(&e))?;
    let pins = snapshot::load_pins(dir).map_err(|e| pending(&e))?;
    let manifest = Manifest::read(&dir.join("manifest.json")).map_err(|e| pending(&e))?;
    guarded_io(|| {
        let mut parents = BTreeSet::new();
        for path in intent.deletions(&manifest, &pins) {
            fault(Step::Files)?;
            let path = dir.join(path);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            parents.extend(path.parent().map(Path::to_owned));
        }
        for parent in parents {
            if parent.is_dir() {
                fs_commit::sync_dir(&parent)?;
            }
        }
        Ok(())
    })
    .map_err(|e| pending(&e))?;
    // 否则 3.x 在 manifest 解析失败时会回退到指向已删文件的旧版。
    ensure_file(
        dir,
        "manifest.json.prev",
        &intent.candidate,
        replace_fault(Step::Prev, Step::PrevAfterRename),
    )
    .map_err(|error| match error {
        ReplaceError::Unchanged(error) | ReplaceError::Unconfirmed(error) => pending(&error),
    })?;
    remove(
        dir,
        replace_fault(Step::IntentRemove, Step::IntentRemoveAfter),
    )
    .map_err(|e| pending(&e))
}

fn manual(dir: &Path, reason: &dyn std::fmt::Display) -> String {
    format!(
        "{reason}; stop the service, remove the snapshots listed in {path:?} if manifest.json is already the emptied candidate, then remove that file and reopen",
        path = dir.join(INTENT_FILE)
    )
}

/// 重开时在读取 WAL 与孤儿清理之前处理清空意图：当前 manifest 等于候选则先确认其落盘再清理，否则删除意图、
/// 不删除任何其他文件。与 0014 的删除意图同时存在、或意图损坏时不处理（fail-closed）。
pub(crate) fn settle(
    dir: &Path,
    name: &str,
    fault: impl Fn(Step) -> io::Result<()>,
) -> Result<(), String> {
    let intent = match read(dir) {
        Ok(None) => return Ok(()),
        Ok(Some(intent)) => intent,
        Err(error @ ReadError::Io(_)) => return Err(error.to_string()),
        Err(error @ ReadError::Invalid(_)) => return Err(manual(dir, &error)),
    };
    if fs::symlink_metadata(dir.join(super::index_intent::INTENT_FILE)).is_ok() {
        return Err(manual(
            dir,
            &"an index deletion intent exists alongside the truncate intent",
        ));
    }
    SchemaFile::read(&dir.join("schema.json")).map_err(|e| e.to_string())?;
    let manifest_path = dir.join("manifest.json");
    let current = match fs::read(&manifest_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("{manifest_path:?}: {error}")),
    };
    if current.as_deref() != Some(intent.candidate_bytes()) {
        return remove(dir, |point| {
            fault(match point {
                ReplacePoint::BeforeRename => Step::IntentRemove,
                ReplacePoint::AfterRename => Step::IntentRemoveAfter,
            })
        })
        .map_err(|e| e.to_string());
    }
    // 进程崩溃时 manifest 的改名可能仍只在页缓存中，断电会回滚；确认之前不能删除旧文件。
    guarded_io(|| {
        fault(Step::ManifestSync)?;
        fs_commit::sync_file(&fs::File::open(&manifest_path)?, &manifest_path)?;
        fs_commit::sync_dir(dir)
    })
    .map_err(|e| e.to_string())?;
    cleanup(dir, name, &intent, fault).map_err(|error| match error {
        CleanupError::Pending(reason) | CleanupError::SchemaUnconfirmed(reason) => reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn old_manifest() -> String {
        r#"{"version":3,"current_seq":9,"active_wal_number":4,"partitions":{"_default":{"data_files":["data/data_000001_000004.parquet"],"delta_files":["delta/delta_000005_000005.parquet"]},"p":{"data_files":["data/data_000006_000009.parquet"]}},"index_specs":{}}"#.to_owned()
    }

    fn marker(old: Option<&str>) -> serde_json::Value {
        let parsed = old.map(|text| Manifest::parse(text.as_bytes()).unwrap());
        let next = candidate(parsed.as_ref()).unwrap();
        serde_json::json!({
            "format_version": 1,
            "operation": "truncate_collection",
            "old_manifest": old,
            "candidate_manifest": String::from_utf8(next.to_bytes().unwrap()).unwrap(),
            "snapshots": [{
                "name": "s1",
                "id": ID,
                "files": [
                    "partitions/_default/data/data_000001_000002.parquet",
                    "partitions/_default/indexes/data_000001_000002.vector.HNSW.idx",
                ],
            }],
            "index_files": ["partitions/p/indexes/data_000006_000009.vector.HNSW.idx"],
        })
    }

    #[test]
    fn valid_markers_parse_with_and_without_an_old_manifest() {
        parse(&serde_json::to_vec(&marker(Some(&old_manifest()))).unwrap()).unwrap();
        let mut empty = marker(None);
        empty["index_files"] = serde_json::json!([]);
        let intent = parse(&serde_json::to_vec(&empty).unwrap()).unwrap();
        let next = Manifest::parse(intent.candidate_bytes()).unwrap();
        assert_eq!((next.version, next.current_seq), (1, 0));
        // 两个 snapshot 之间、snapshot 与 index_files 之间可以引用同一路径。
        let mut shared = marker(Some(&old_manifest()));
        let copy = shared["snapshots"][0].clone();
        let mut second = copy.clone();
        second["name"] = "s2".into();
        second["files"]
            .as_array_mut()
            .unwrap()
            .push("partitions/p/indexes/data_000006_000009.vector.HNSW.idx".into());
        second["files"]
            .as_array_mut()
            .unwrap()
            .push("partitions/p/data/data_000006_000009.parquet".into());
        shared["snapshots"] = serde_json::json!([copy, second]);
        parse(&serde_json::to_vec(&shared).unwrap()).unwrap();
    }

    #[test]
    fn every_malformed_field_fails_closed() {
        let old = old_manifest();
        let base = marker(Some(&old));
        let mut cases: Vec<serde_json::Value> = Vec::new();
        for key in base.as_object().unwrap().keys() {
            let mut value = base.clone();
            value.as_object_mut().unwrap().remove(key);
            cases.push(value);
        }
        let edits: Vec<(&str, serde_json::Value)> = vec![
            ("format_version", 2.into()),
            ("format_version", 1.0.into()),
            ("format_version", "1".into()),
            ("operation", "rename_collection".into()),
            ("old_manifest", "{".into()),
            ("candidate_manifest", "{}".into()),
            ("extra", true.into()),
            (
                "index_files",
                serde_json::json!(["partitions/p/indexes/data_000001_000004.vector.HNSW.idx"]),
            ),
            (
                "index_files",
                serde_json::json!(["partitions/p/data/data_000006_000009.parquet"]),
            ),
            (
                "index_files",
                serde_json::json!([
                    "partitions/p/indexes/data_000006_000009.vector.HNSW.idx",
                    "partitions/p/indexes/data_000006_000009.vector.HNSW.idx"
                ]),
            ),
            (
                "index_files",
                serde_json::json!(["partitions/../indexes/data_000006_000009.vector.HNSW.idx"]),
            ),
            (
                "index_files",
                serde_json::json!(["partitions/p/indexes/../../x.vector.HNSW.idx"]),
            ),
        ];
        for (key, value) in edits {
            let mut case = base.clone();
            case[key] = value;
            cases.push(case);
        }
        // 候选与旧 manifest 的关系不符。
        for change in [
            r#""version":5"#,
            r#""current_seq":1"#,
            r#""active_wal_number":4"#,
        ] {
            let mut next: serde_json::Value =
                serde_json::from_str(base["candidate_manifest"].as_str().unwrap()).unwrap();
            let (k, v) = change.split_once(':').unwrap();
            next[k.trim_matches('"')] = serde_json::from_str(v).unwrap();
            let mut case = base.clone();
            case["candidate_manifest"] = next.to_string().into();
            cases.push(case);
        }
        let mut next: serde_json::Value =
            serde_json::from_str(base["candidate_manifest"].as_str().unwrap()).unwrap();
        next["partitions"]["q"] = serde_json::json!({});
        let mut case = base.clone();
        case["candidate_manifest"] = next.to_string().into();
        cases.push(case);
        for snap in [
            serde_json::json!({"name": "../x", "id": ID, "files": []}),
            serde_json::json!({"name": "..", "id": ID, "files": []}),
            serde_json::json!({"name": "s", "id": "ABC", "files": []}),
            serde_json::json!({"name": "s", "id": ID.to_uppercase(), "files": []}),
            serde_json::json!({"name": "s", "id": ID, "files": [], "extra": 1}),
            serde_json::json!({"name": "s", "id": ID}),
            serde_json::json!({"name": "s", "id": ID, "files": ["partitions/_default/indexes/data_000009_000009.vector.HNSW.idx"]}),
            serde_json::json!({"name": "s", "id": ID, "files": ["partitions/_default/data/x.parquet"]}),
            serde_json::json!({"name": "s", "id": ID, "files": ["partitions/_default/delta/data_000001_000001.parquet"]}),
            serde_json::json!({"name": "s", "id": ID, "files": ["partitions/_all/data/data_000001_000001.parquet"]}),
            serde_json::json!({"name": "s", "id": ID, "files": ["partitions/_default/data/data_000001_000001.parquet", "partitions/_default/data/data_000001_000001.parquet"]}),
        ] {
            let mut case = base.clone();
            case["snapshots"] = serde_json::json!([snap]);
            cases.push(case);
        }
        let mut duplicate = base.clone();
        let snap = base["snapshots"][0].clone();
        duplicate["snapshots"] = serde_json::json!([snap.clone(), snap]);
        cases.push(duplicate);
        for case in cases {
            assert!(
                parse(&serde_json::to_vec(&case).unwrap()).is_err(),
                "{case}"
            );
        }
    }

    #[test]
    fn deletions_keep_files_still_referenced() {
        let mut value = marker(Some(&old_manifest()));
        value["snapshots"][0]["files"]
            .as_array_mut()
            .unwrap()
            .push("partitions/_default/delta/delta_000003_000003.parquet".into());
        let intent = parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        let mut current = Manifest::parse(intent.candidate_bytes()).unwrap();
        current.partitions.get_mut("p").unwrap().data_files =
            vec!["data/data_000006_000009.parquet".into()];
        let mut pins = PinnedFiles::default();
        pins.delta.insert((
            "_default".into(),
            "delta/delta_000003_000003.parquet".into(),
        ));
        let deleted: Vec<_> = intent.deletions(&current, &pins).into_iter().collect();
        assert_eq!(
            deleted,
            [
                "partitions/_default/data/data_000001_000002.parquet",
                "partitions/_default/data/data_000001_000004.parquet",
                "partitions/_default/delta/delta_000005_000005.parquet",
                "partitions/_default/indexes/data_000001_000002.vector.HNSW.idx",
            ]
        );
    }

    #[test]
    fn written_intent_reads_back_and_removal_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        assert!(read(root.path()).unwrap().is_none());
        let intent = parse(&serde_json::to_vec(&marker(Some(&old_manifest()))).unwrap()).unwrap();
        write(root.path(), &intent, |_| Ok(())).unwrap();
        let back = read(root.path()).unwrap().unwrap();
        assert_eq!(back.marker_bytes().unwrap(), intent.marker_bytes().unwrap());
        remove(root.path(), |_| Ok(())).unwrap();
        remove(root.path(), |_| Ok(())).unwrap();
        assert!(read(root.path()).unwrap().is_none());
        fs::create_dir(root.path().join(INTENT_FILE)).unwrap();
        assert!(matches!(read(root.path()), Err(ReadError::Invalid(_))));
    }
}
