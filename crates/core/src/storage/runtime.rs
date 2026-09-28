//! collection 写路径的持久化状态；读视图由 collection 的列式块维护。

use std::collections::VecDeque;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::data::ColumnData;
use crate::schema::CollectionSchema;
use crate::storage::arrow_data::{self, DecodedData};
use crate::storage::flush::{self, FlushError};
use crate::storage::fs_commit::{self, CommitPhase};
use crate::storage::manifest::IndexSpec as StoredIndexSpec;
use crate::storage::manifest::Manifest;
use crate::storage::memtable::{FrozenMemTable, MemTable};
use crate::storage::recovery::RecoveredCollection;
use crate::storage::visibility::{SourceId, Visibility};
use crate::storage::wal::{WalKind, WalWriter};

pub struct FrozenGeneration {
    pub number: u64,
    pub table: FrozenMemTable,
}

pub struct FlushOutcome {
    pub manifest: Manifest,
    pub mapping: Vec<Option<usize>>,
    pub data_rows: usize,
    pub post_commit_error: Option<PostCommitError>,
}

pub enum PostCommitError {
    Durability(String),
    WalCleanup(String),
}

pub struct PersistentCollection {
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub visibility: Visibility,
    pub active: MemTable,
    pub generation: u64,
    next_source: u64,
    data_wal: Option<WalWriter>,
    pending: VecDeque<Arc<FrozenGeneration>>,
    retry_manifest: bool,
    failed_flush: bool,
    last_published_generation: Option<u64>,
    last_failure: Option<(Instant, String)>,
    inherited_synced: bool,
    poisoned: bool,
    /// 与状态写锁分开：Parquet 写入、fsync 和 manifest 提交不阻塞读者。
    flush_lock: Arc<Mutex<()>>,
}

impl std::fmt::Debug for PersistentCollection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistentCollection")
            .field("dir", &self.dir)
            .field("generation", &self.generation)
            .field("pending", &self.pending.len())
            .field("retry_manifest", &self.retry_manifest)
            .field("poisoned", &self.poisoned)
            .finish()
    }
}

impl PersistentCollection {
    pub fn new(dir: PathBuf) -> Result<Self, String> {
        let mut visibility = Visibility::default();
        visibility.add_source(SourceId(1), 0).map_err(err)?;
        Ok(Self {
            dir,
            manifest: Manifest::default(),
            visibility,
            active: MemTable::new(SourceId(1)),
            generation: 1,
            next_source: 2,
            data_wal: None,
            pending: VecDeque::new(),
            retry_manifest: false,
            failed_flush: false,
            last_published_generation: None,
            last_failure: None,
            inherited_synced: false,
            poisoned: false,
            flush_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn recovered(dir: PathBuf, recovered: RecoveredCollection) -> Self {
        let next_source = recovered.active_memtable.source.0 + 1;
        Self {
            dir,
            manifest: recovered.manifest,
            visibility: recovered.visibility,
            active: recovered.active_memtable,
            generation: recovered.next_wal_number,
            next_source,
            data_wal: None,
            pending: VecDeque::new(),
            retry_manifest: false,
            failed_flush: false,
            last_published_generation: None,
            last_failure: None,
            inherited_synced: false,
            poisoned: false,
            flush_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn flush_lock(&self) -> Arc<Mutex<()>> {
        Arc::clone(&self.flush_lock)
    }

    pub fn needs_retry(&self) -> bool {
        self.failed_flush || self.retry_manifest
    }

    pub fn throttled_failure(&self) -> Option<&str> {
        self.last_failure
            .as_ref()
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(1))
            .map(|(_, reason)| reason.as_str())
    }

    pub fn record_failure(&mut self, reason: impl Into<String>) {
        self.last_failure = Some((Instant::now(), reason.into()));
    }

    pub fn has_pending_generation(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn inherited_synced(&self) -> bool {
        self.inherited_synced
    }

    pub fn poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn append(
        &mut self,
        schema: &CollectionSchema,
        seqs: &[u64],
        columns: &[ColumnData],
        meta: Option<&[Map<String, Value>]>,
    ) -> Result<(), String> {
        if self.poisoned {
            return Err("collection state is unusable after a storage failure".to_owned());
        }
        if self.needs_retry() {
            return Err("a failed flush must complete before another write".to_owned());
        }
        let batch =
            arrow_data::wal_data_batch(schema, seqs, "_default", columns, meta).map_err(err)?;
        if self.data_wal.is_none() {
            let wal_dir = self.dir.join("wal");
            fs_commit::create_dir_synced(&wal_dir).map_err(err)?;
            let wal_schema = arrow_data::wal_data_schema(schema).map_err(err)?;
            match WalWriter::create(&wal_dir, WalKind::Data, self.generation, &wal_schema) {
                Ok(writer) => self.data_wal = Some(writer),
                Err(error) => {
                    self.freeze_active(true)?;
                    self.failed_flush = true;
                    return Err(error.to_string());
                }
            }
        }
        let writer = self.data_wal.as_mut().ok_or("WAL writer is missing")?;
        if let Err(error) = writer.append(&batch) {
            self.freeze_active(true)?;
            self.failed_flush = true;
            return Err(error.to_string());
        }
        let decoded = DecodedData {
            seqs: seqs.to_vec(),
            columns: columns.to_vec(),
            meta: meta.map(<[Map<String, Value>]>::to_vec),
        };
        if let Err(error) = self
            .active
            .apply_data(schema, decoded, &mut self.visibility)
        {
            // WAL 已追加成功。不能继续在可能只更新一半的内存状态上服务请求。
            self.poisoned = true;
            return Err(error.to_string());
        }
        Ok(())
    }

    pub fn should_auto_flush(&self) -> bool {
        self.active.size() >= 10_000
    }

    pub fn freeze_active(&mut self, even_if_empty: bool) -> Result<(), String> {
        if self.active.size() == 0 && !even_if_empty {
            return Ok(());
        }
        let next_generation = self
            .generation
            .checked_add(1)
            .ok_or("WAL number space exhausted")?;
        let new_source = SourceId(self.next_source);
        self.next_source = self
            .next_source
            .checked_add(1)
            .ok_or("source ID space exhausted")?;
        self.visibility.add_source(new_source, 0).map_err(err)?;
        let old = std::mem::replace(&mut self.active, MemTable::new(new_source));
        self.pending.push_back(Arc::new(FrozenGeneration {
            number: self.generation,
            table: old.freeze(&self.visibility),
        }));
        self.generation = next_generation;
        self.data_wal = None;
        Ok(())
    }

    pub fn front(&self) -> Option<Arc<FrozenGeneration>> {
        self.pending.front().cloned()
    }

    pub fn retry_manifest(&self) -> bool {
        self.retry_manifest
    }

    pub fn recommit(&mut self) -> Result<(), String> {
        if !self.retry_manifest {
            return Ok(());
        }
        match fs_commit::commit_manifest(&self.dir, &self.manifest) {
            Ok(_) => {
                self.retry_manifest = false;
                self.last_failure = None;
                if let Err(error) = self.cleanup_committed_wal() {
                    // manifest 已完整提交；WAL 清理失败按已提交后的非阻断故障处理。
                    eprintln!(
                        "WAL cleanup after manifest recommit failed for {:?}: {error}",
                        self.dir
                    );
                }
                Ok(())
            }
            Err(error) => {
                self.record_failure(error.to_string());
                Err(error.to_string())
            }
        }
    }

    /// 索引定义与 manifest 同一提交点；replace 后的同步失败仍视为定义已生效。
    pub fn commit_index(&mut self, spec: &crate::index::IndexSpec) -> Result<(), String> {
        if self.poisoned || self.needs_retry() {
            return Err("collection has unfinished storage work".to_owned());
        }
        if !self.inherited_synced {
            flush::sync_inherited(&self.dir, &self.manifest).map_err(err)?;
            self.inherited_synced = true;
        }
        let mut candidate = self.manifest.clone();
        candidate.version = candidate
            .version
            .checked_add(1)
            .ok_or("manifest version space exhausted")?;
        candidate.index_specs.insert(
            spec.field_name.clone(),
            StoredIndexSpec {
                field_name: spec.field_name.clone(),
                index_type: spec.index_type.clone(),
                metric_type: spec.metric_type.clone(),
                build_params: spec.params.clone(),
                search_params: spec.search_params.clone(),
            },
        );
        match fs_commit::commit_manifest(&self.dir, &candidate) {
            Ok(_) => self.manifest = candidate,
            Err(error) if error.phase == CommitPhase::AfterReplace => {
                self.manifest = candidate;
                self.retry_manifest = true;
                self.record_failure(error.to_string());
                eprintln!(
                    "index definition committed but directory sync failed for {:?}: {error}",
                    self.dir
                );
            }
            Err(error) => return Err(error.to_string()),
        }
        Ok(())
    }

    /// flush 锁持有者提交一代，之后在状态写锁内发布位图映射。
    pub fn publish(
        &mut self,
        generation: &Arc<FrozenGeneration>,
        manifest: Manifest,
        mapping: &[Option<usize>],
        data_rows: usize,
        after_replace_error: bool,
    ) -> Result<(), String> {
        if !self
            .pending
            .front()
            .is_some_and(|front| Arc::ptr_eq(front, generation))
        {
            self.poisoned = true;
            return Err("flush generation changed before publication".to_owned());
        }
        let segment_source = SourceId(self.next_source);
        self.next_source = self
            .next_source
            .checked_add(1)
            .ok_or("source ID space exhausted")?;
        if let Err(error) = self.visibility.replace_source(
            generation.table.source(),
            segment_source,
            data_rows,
            mapping,
        ) {
            self.poisoned = true;
            return Err(error.to_string());
        }
        self.manifest = manifest;
        self.pending.pop_front();
        self.last_published_generation = Some(generation.number);
        self.inherited_synced = true;
        self.failed_flush = false;
        self.retry_manifest |= after_replace_error;
        if !self.retry_manifest {
            self.last_failure = None;
        }
        Ok(())
    }

    pub fn failed_before_replace(&mut self, reason: impl Into<String>) {
        self.failed_flush = true;
        self.record_failure(reason);
    }

    pub fn cleanup_committed_wal(&self) -> Result<(), String> {
        if let Some(last) = self.last_published_generation {
            flush::cleanup_wal(&self.dir.join("wal"), last).map_err(err)?;
        }
        Ok(())
    }

    pub fn sync_retained_wal(&mut self) -> Result<(), String> {
        // 活跃 writer 先交出 File，再按路径同步所有留下的编号和目录。
        self.data_wal = None;
        let dir = self.dir.join("wal");
        if !dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(&dir).map_err(err)? {
            let entry = entry.map_err(err)?;
            if entry.file_type().map_err(err)?.is_file() {
                File::open(entry.path())
                    .and_then(|file| file.sync_all())
                    .map_err(err)?;
            }
        }
        fs_commit::sync_dir(&dir).map_err(err)
    }

    pub fn has_wal(&self) -> Result<bool, String> {
        let dir = self.dir.join("wal");
        match fs::read_dir(dir) {
            Ok(mut entries) => Ok(entries.next().is_some()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.to_string()),
        }
    }
}

pub fn flush_one(
    dir: &Path,
    manifest: &Manifest,
    generation: &FrozenGeneration,
    schema: &CollectionSchema,
    sync_inherited_files: bool,
) -> Result<FlushOutcome, FlushError> {
    let materialized = generation
        .table
        .materialize(schema)
        .map_err(|error| FlushError {
            path: dir.to_path_buf(),
            phase: CommitPhase::BeforeReplace,
            reason: error.to_string(),
            committed_manifest: None,
            cleanup_failed: false,
        })?;
    let data_rows = materialized
        .data
        .as_ref()
        .map_or(0, |batch| batch.num_rows());
    match flush::flush_generation_with_inherited_sync(
        dir,
        manifest,
        generation.number,
        generation.table.max_seq(),
        materialized.data.as_ref(),
        materialized.delta.as_ref(),
        sync_inherited_files,
    ) {
        Ok(manifest) => Ok(FlushOutcome {
            manifest,
            mapping: materialized.mapping,
            data_rows,
            post_commit_error: None,
        }),
        Err(error) if error.phase == CommitPhase::AfterReplace => {
            let manifest = error
                .committed_manifest
                .as_ref()
                .ok_or_else(|| FlushError {
                    path: dir.to_path_buf(),
                    phase: CommitPhase::BeforeReplace,
                    reason: "committed manifest missing from post-replace error".to_owned(),
                    committed_manifest: None,
                    cleanup_failed: false,
                })?;
            Ok(FlushOutcome {
                manifest: (**manifest).clone(),
                mapping: materialized.mapping,
                data_rows,
                post_commit_error: Some(if error.cleanup_failed {
                    PostCommitError::WalCleanup(error.reason)
                } else {
                    PostCommitError::Durability(error.reason)
                }),
            })
        }
        Err(error) => Err(error),
    }
}

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}
