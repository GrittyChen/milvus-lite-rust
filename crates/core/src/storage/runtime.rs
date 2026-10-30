//! collection 写路径的持久化状态；读视图由 collection 的列式块维护。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::data::ColumnData;
use crate::data::Pk;
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

#[cfg(feature = "fault-injection")]
thread_local! {
    static BEFORE_COMMIT_PAUSE: std::cell::RefCell<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>> = const { std::cell::RefCell::new(None) };
    static PREPARE_PANIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(feature = "fault-injection")]
pub fn inject_flush_prepare_panic_once() {
    PREPARE_PANIC.with(|slot| slot.set(true));
}

#[cfg(feature = "fault-injection")]
pub fn pause_flush_before_commit_once(
    entered: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
) {
    BEFORE_COMMIT_PAUSE.with(|slot| *slot.borrow_mut() = Some((entered, resume)));
}

#[cfg(feature = "fault-injection")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetainedWalSyncFault {
    Data,
    Delta,
    Directory,
}
#[cfg(feature = "fault-injection")]
thread_local! { static RETAINED_WAL_SYNC_FAULT: std::cell::Cell<Option<RetainedWalSyncFault>> = const { std::cell::Cell::new(None) }; }
#[cfg(feature = "fault-injection")]
thread_local! { static RETAINED_WAL_SYNC_PANIC: std::cell::Cell<Option<RetainedWalSyncFault>> = const { std::cell::Cell::new(None) }; }
#[cfg(feature = "fault-injection")]
pub fn inject_retained_wal_sync_panic_once(point: RetainedWalSyncFault) {
    RETAINED_WAL_SYNC_PANIC.with(|slot| slot.set(Some(point)));
}
#[cfg(feature = "fault-injection")]
pub fn inject_retained_wal_sync_failure_once(fault: RetainedWalSyncFault) {
    RETAINED_WAL_SYNC_FAULT.with(|slot| slot.set(Some(fault)));
}
#[cfg(feature = "fault-injection")]
fn retained_wal_sync_fault(point: RetainedWalSyncFault) -> Result<(), String> {
    if RETAINED_WAL_SYNC_PANIC.with(|slot| {
        if slot.get() == Some(point) {
            slot.set(None);
            true
        } else {
            false
        }
    }) {
        panic!("injected retained WAL sync panic: {point:?}");
    }
    if RETAINED_WAL_SYNC_FAULT.with(|slot| {
        if slot.get() == Some(point) {
            slot.set(None);
            true
        } else {
            false
        }
    }) {
        Err("injected retained WAL sync failure".into())
    } else {
        Ok(())
    }
}

#[cfg(feature = "fault-injection")]
thread_local! { static HOLD_RETRY_WINDOW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

/// 避免故障窗口断言依赖机器速度；只冻结测试线程的窗口，不改变生产的一秒节流。
#[cfg(feature = "fault-injection")]
#[must_use]
pub struct FlushRetryWindowGuard {
    previous: bool,
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}
#[cfg(feature = "fault-injection")]
pub fn hold_flush_retry_window() -> FlushRetryWindowGuard {
    FlushRetryWindowGuard {
        previous: HOLD_RETRY_WINDOW.with(|cell| cell.replace(true)),
        _not_send: std::marker::PhantomData,
    }
}
#[cfg(feature = "fault-injection")]
impl Drop for FlushRetryWindowGuard {
    fn drop(&mut self) {
        HOLD_RETRY_WINDOW.with(|cell| cell.set(self.previous));
    }
}

const TRUNCATE_CLEANUP_PENDING: &str =
    "the collection was truncated, but its cleanup must finish before another write";

pub struct FrozenGeneration {
    pub number: u64,
    pub table: FrozenMemTable,
    pub(crate) partitions: Vec<(u32, String)>,
    pub(crate) partitioned: bool,
    wal_files: Mutex<Vec<(WalKind, File)>>,
}

impl FrozenGeneration {
    fn sync_and_close_wal(&self, dir: &Path) -> Result<(), String> {
        let mut files = self.wal_files.lock();
        if files.is_empty() {
            return Ok(());
        }
        for (kind, file) in files.iter() {
            #[cfg(feature = "fault-injection")]
            retained_wal_sync_fault(match kind {
                WalKind::Data => RetainedWalSyncFault::Data,
                WalKind::Delta => RetainedWalSyncFault::Delta,
            })?;
            fs_commit::sync_file(file, &dir.join("wal").join(kind.file_name(self.number)))
                .map_err(err)?;
        }
        #[cfg(feature = "fault-injection")]
        retained_wal_sync_fault(RetainedWalSyncFault::Directory)?;
        fs_commit::sync_dir(&dir.join("wal")).map_err(err)?;
        files.clear();
        Ok(())
    }
}

pub struct FlushOutcome {
    pub(crate) partitioned: Option<PartitionFlushOutcome>,
    pub manifest: Manifest,
    pub mapping: Vec<Option<usize>>,
    /// 与实际写出 Parquet 相同的批次，锁外解码后交给读取视图。
    pub data: Option<DecodedData>,
    pub norms: Vec<f32>,
    pub post_commit_error: Option<PostCommitError>,
}

pub(crate) struct FlushedPartition {
    pub id: u32,
    pub data: DecodedData,
    pub norms: Vec<f32>,
}
pub(crate) struct PartitionFlushOutcome {
    pub mapping: Vec<Option<(u32, usize)>>,
    pub data: Vec<FlushedPartition>,
}

pub enum PostCommitError {
    Durability(String),
    WalCleanup(String),
}

pub struct PersistentCollection {
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub(crate) partitions: super::partition_paths::Registry,
    pub visibility: Visibility,
    pub active: MemTable,
    pub generation: u64,
    pub(crate) data_sources: BTreeMap<(String, String), SourceId>,
    pub(crate) pinned_files: crate::storage::snapshot::PinnedFiles,
    retained_old_wal: bool,
    pub(crate) maintenance_status: Arc<crate::maintenance::Status>,
    pub(crate) compaction_cleanup: Vec<PathBuf>,
    pub(crate) partition_cleanup: bool,
    pub(crate) committed_deletes: HashMap<Pk, u64>,
    next_source: u64,
    data_wal: Option<WalWriter>,
    delta_wal: Option<WalWriter>,
    pending: VecDeque<Arc<FrozenGeneration>>,
    retry_manifest: bool,
    failed_flush: bool,
    last_published_generation: Option<u64>,
    last_failure: Option<(Instant, String)>,
    inherited_synced: bool,
    pub(crate) index_intent: super::index_intent::IntentState,
    /// 清空已提交、清理未完成（0023 A「清空」第 7 步）：期间拒绝改写 manifest 的写入，暂停后台维护。
    pub(crate) truncate_cleanup: Option<super::truncate_intent::Pending>,
    poisoned: bool,
    /// 与状态写锁分开：Parquet 写入、fsync 和 manifest 提交不阻塞读者。
    flush_lock: Arc<crate::lock_order::Mutex<(), 4>>,
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
        Self::with_manifest(dir, Manifest::default())
    }

    pub(crate) fn with_manifest(dir: PathBuf, manifest: Manifest) -> Result<Self, String> {
        let mut visibility = if manifest.partitions.len() > 1 {
            Visibility::new_partitioned()
        } else {
            Visibility::default()
        };
        visibility.add_source(SourceId(1), 0).map_err(err)?;
        Ok(Self {
            dir,
            partitions: super::partition_paths::Registry::from_manifest(&manifest)?,
            manifest,
            visibility,
            active: MemTable::new(SourceId(1)),
            generation: 1,
            data_sources: BTreeMap::new(),
            pinned_files: Default::default(),
            retained_old_wal: false,
            maintenance_status: Arc::new(Default::default()),
            compaction_cleanup: Vec::new(),
            partition_cleanup: false,
            committed_deletes: HashMap::new(),
            next_source: 2,
            data_wal: None,
            delta_wal: None,
            pending: VecDeque::new(),
            retry_manifest: false,
            failed_flush: false,
            last_published_generation: None,
            last_failure: None,
            inherited_synced: false,
            index_intent: Default::default(),
            truncate_cleanup: None,
            poisoned: false,
            flush_lock: Arc::new(crate::lock_order::Mutex::new(())),
        })
    }

    pub(crate) fn recovered(
        dir: PathBuf,
        recovered: RecoveredCollection,
        data_sources: BTreeMap<(String, String), SourceId>,
    ) -> Self {
        let next_source = recovered.active_memtable.source.0 + 1;
        Self {
            dir,
            partitions: recovered.partitions,
            manifest: recovered.manifest,
            visibility: recovered.visibility,
            active: recovered.active_memtable,
            generation: recovered.next_wal_number,
            data_sources,
            pinned_files: recovered.pinned_files,
            retained_old_wal: recovered.retained_old_wal,
            maintenance_status: Arc::new(Default::default()),
            compaction_cleanup: Vec::new(),
            partition_cleanup: false,
            committed_deletes: recovered.committed_deletes,
            next_source,
            data_wal: None,
            delta_wal: None,
            pending: VecDeque::new(),
            retry_manifest: false,
            failed_flush: false,
            last_published_generation: None,
            last_failure: None,
            inherited_synced: false,
            index_intent: Default::default(),
            truncate_cleanup: None,
            poisoned: false,
            flush_lock: Arc::new(crate::lock_order::Mutex::new(())),
        }
    }

    pub fn flush_lock(&self) -> Arc<crate::lock_order::Mutex<(), 4>> {
        Arc::clone(&self.flush_lock)
    }

    pub(crate) fn can_compact(&self) -> Result<bool, String> {
        Ok(!self.poisoned
            && !self.needs_retry()
            && self.truncate_cleanup.is_none()
            && !self.retained_old_wal
            && !self.active.has_legacy_operations(self.manifest.current_seq)
            && !self
                .pending
                .iter()
                .any(|g| g.table.has_legacy_operations(self.manifest.current_seq)))
    }

    pub(crate) fn committed_wal_limit(&self) -> Option<u64> {
        self.last_published_generation
    }

    pub(crate) fn compaction_sources(&self, count: usize) -> Result<Vec<SourceId>, String> {
        let end = self
            .next_source
            .checked_add(u64::try_from(count).map_err(err)?)
            .ok_or("source ID space exhausted")?;
        Ok((self.next_source..end).map(SourceId).collect())
    }

    pub(crate) fn publish_compaction(
        &mut self,
        prepared: &crate::storage::compaction::Prepared,
        manifest: Manifest,
        ids: &[SourceId],
        rows: &[(SourceId, Vec<crate::storage::visibility::CompactionRow>)],
        threshold: Option<u64>,
        durability_error: Option<String>,
    ) -> Result<(), String> {
        let mut sources = self.data_sources.clone();
        for input in &prepared.inputs {
            sources.remove(&(input.partition.clone(), input.relative.clone()));
        }
        for (output, source) in prepared.outputs.iter().zip(ids) {
            sources.insert(
                (
                    prepared.partition.clone(),
                    output
                        .relative
                        .clone()
                        .ok_or("compaction output name missing")?,
                ),
                *source,
            );
        }
        // 参照缓存保留未合并 segment 的相对顺序，再追加新输出；冻结代及活跃源在其后。
        let committed: std::collections::HashSet<_> = self.data_sources.values().copied().collect();
        let mut order: Vec<_> = self
            .visibility
            .source_order()
            .iter()
            .copied()
            .filter(|source| {
                committed.contains(source) && !prepared.inputs.iter().any(|i| i.source == *source)
            })
            .collect();
        order.extend_from_slice(ids);
        order.extend(
            self.visibility
                .source_order()
                .iter()
                .copied()
                .filter(|source| !committed.contains(source)),
        );
        let outputs: Vec<_> = prepared
            .outputs
            .iter()
            .zip(ids)
            .map(|(o, id)| (*id, o.data.seqs.len()))
            .collect();
        self.visibility.compacted(rows, &outputs, order)?;
        self.next_source = self
            .next_source
            .checked_add(u64::try_from(ids.len()).map_err(err)?)
            .ok_or("source ID space exhausted")?;
        self.data_sources = sources;
        self.manifest = manifest;
        self.inherited_synced = true;
        self.retry_manifest = durability_error.is_some();
        if let Some(reason) = durability_error {
            self.record_failure(reason);
        } else {
            self.visibility
                .gc_deletes(threshold, self.manifest.current_seq)?;
            self.committed_deletes
                .retain(|_, seq| threshold.is_some_and(|t| *seq >= t));
            // 删除重负载后不能让已过期表长期保留峰值桶容量。
            if self.committed_deletes.len() < self.committed_deletes.capacity() / 4 {
                self.committed_deletes.shrink_to_fit();
            }
        }
        Ok(())
    }

    pub fn needs_retry(&self) -> bool {
        self.failed_flush || self.retry_manifest
    }

    pub fn throttled_failure(&self) -> Option<&str> {
        self.last_failure
            .as_ref()
            .filter(|(at, _)| {
                #[cfg(feature = "fault-injection")]
                if HOLD_RETRY_WINDOW.with(|cell| cell.get()) {
                    return true;
                }
                at.elapsed() < Duration::from_secs(1)
            })
            .map(|(_, reason)| reason.as_str())
    }

    pub fn record_failure(&mut self, reason: impl Into<String>) {
        self.last_failure = Some((Instant::now(), reason.into()));
    }

    pub fn has_pending_generation(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(crate) fn pending_generations(&self) -> impl Iterator<Item = &Arc<FrozenGeneration>> {
        self.pending.iter()
    }

    pub(crate) fn rebuild_partition_visibility(
        &mut self,
        build: impl FnOnce(
            &Self,
            crate::storage::visibility::partitions::PartitionVisibility,
        ) -> Result<
            crate::storage::visibility::partitions::PartitionVisibility,
            String,
        >,
    ) -> Result<(), String> {
        if self.poisoned {
            return Err("collection state is unusable after a storage failure".into());
        }
        if self.visibility.is_partitioned() {
            return Ok(());
        }
        // 重建全程持状态写锁；先封锁失败/展开路径，避免同时保留两份全库主键表。
        self.poisoned = true;
        let index = self.visibility.release_version_index_for_rebuild();
        let visibility =
            crate::storage::visibility::partitions::PartitionVisibility::with_reused_index(index);
        let visibility = build(self, visibility)?;
        self.visibility.enable_partitions(visibility);
        self.poisoned = false;
        Ok(())
    }

    pub fn inherited_synced(&self) -> bool {
        self.inherited_synced
    }

    pub(crate) fn poison(&mut self) {
        self.poisoned = true;
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
        self.append_partition(schema, seqs, columns, meta, 0)
    }

    pub fn append_partition(
        &mut self,
        schema: &CollectionSchema,
        seqs: &[u64],
        columns: &[ColumnData],
        meta: Option<&[Map<String, Value>]>,
        partition: u32,
    ) -> Result<(), String> {
        let name = self.write_partition_name(partition)?;
        let batch = arrow_data::wal_data_batch(schema, seqs, name, columns, meta).map_err(err)?;
        let decoded = DecodedData {
            seqs: seqs.to_vec(),
            columns: columns.to_vec(),
            meta: meta.map(<[Map<String, Value>]>::to_vec),
        };
        self.append_prepared_partition(schema, batch, decoded, partition)
    }

    pub(crate) fn append_normalized_partition(
        &mut self,
        seqs: &[u64],
        columns: arrow_data::NormalizedColumns<'_>,
        meta: Option<&[Map<String, Value>]>,
        partition: u32,
    ) -> Result<(), String> {
        let name = self.write_partition_name(partition)?;
        let batch = columns.wal_batch(seqs, name, meta).map_err(err)?;
        let decoded = DecodedData {
            seqs: seqs.to_vec(),
            columns: columns.columns().to_vec(),
            meta: meta.map(<[Map<String, Value>]>::to_vec),
        };
        self.append_prepared_partition(columns.schema(), batch, decoded, partition)
    }

    fn write_partition_name(&self, partition: u32) -> Result<&str, String> {
        if self.poisoned {
            return Err("collection state is unusable after a storage failure".to_owned());
        }
        if self.needs_retry() {
            return Err("a failed flush must complete before another write".to_owned());
        }
        if self.truncate_cleanup.is_some() {
            return Err(TRUNCATE_CLEANUP_PENDING.to_owned());
        }
        let name = self
            .partitions
            .name(partition)
            .ok_or("write partition missing")?;
        if !self.manifest.partitions.contains_key(name) {
            return Err("write partition is not registered".into());
        }
        Ok(name)
    }

    /// 调用方已完成全部桶的编码；这里仅追加既有 WAL stream 并发布当前桶。
    pub(crate) fn append_prepared_partition(
        &mut self,
        schema: &CollectionSchema,
        batch: arrow_array::RecordBatch,
        decoded: DecodedData,
        partition: u32,
    ) -> Result<(), String> {
        if self.poisoned {
            return Err("collection state is unusable after a storage failure".to_owned());
        }
        if self.needs_retry() {
            return Err("a failed flush must complete before another write".to_owned());
        }
        if self.truncate_cleanup.is_some() {
            return Err(TRUNCATE_CLEANUP_PENDING.to_owned());
        }
        if self.data_wal.is_none() {
            let wal_dir = self.dir.join("wal");
            fs_commit::create_dir_synced(&wal_dir).map_err(err)?;
            let wal_schema = batch.schema();
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
        if let Err(error) =
            self.active
                .apply_partition_data(schema, decoded, &mut self.visibility, &[partition])
        {
            // WAL 已追加成功。不能继续在可能只更新一半的内存状态上服务请求。
            self.poisoned = true;
            return Err(error.to_string());
        }
        Ok(())
    }

    pub fn append_delete(
        &mut self,
        schema: &CollectionSchema,
        pks: &[Pk],
        seq: u64,
    ) -> Result<(), String> {
        self.append_partition_delete(schema, pks, seq, None)
    }

    pub fn append_partition_delete(
        &mut self,
        schema: &CollectionSchema,
        pks: &[Pk],
        seq: u64,
        target: Option<u32>,
    ) -> Result<(), String> {
        if pks.is_empty() {
            return Ok(());
        }
        if self.poisoned {
            return Err("collection state is unusable after a storage failure".to_owned());
        }
        if self.needs_retry() {
            return Err("a failed flush must complete before another write".to_owned());
        }
        if self.truncate_cleanup.is_some() {
            return Err(TRUNCATE_CLEANUP_PENDING.to_owned());
        }
        let seqs = vec![seq; pks.len()];
        let name = if let Some(part) = target {
            self.partitions
                .name(part)
                .filter(|name| self.manifest.partitions.contains_key(*name))
                .ok_or("delete partition is not registered")?
        } else {
            "_all"
        };
        let batch = arrow_data::wal_delta_batch(schema, pks, &seqs, name).map_err(err)?;
        if self.delta_wal.is_none() {
            let wal_dir = self.dir.join("wal");
            fs_commit::create_dir_synced(&wal_dir).map_err(err)?;
            let wal_schema = arrow_data::wal_delta_schema(schema).map_err(err)?;
            match WalWriter::create(&wal_dir, WalKind::Delta, self.generation, &wal_schema) {
                Ok(writer) => self.delta_wal = Some(writer),
                Err(error) => {
                    self.freeze_active(true)?;
                    self.failed_flush = true;
                    return Err(error.to_string());
                }
            }
        }
        let writer = self
            .delta_wal
            .as_mut()
            .ok_or("delta WAL writer is missing")?;
        if let Err(error) = writer.append(&batch) {
            self.freeze_active(true)?;
            self.failed_flush = true;
            return Err(error.to_string());
        }
        if let Err(error) =
            self.active
                .apply_partition_delete(pks, seq, target, &mut self.visibility)
        {
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
        let partitions: Vec<_> = self
            .manifest
            .partitions
            .keys()
            .map(|name| {
                self.partitions
                    .id(name)
                    .map(|id| (id, name.clone()))
                    .ok_or_else(|| "freeze partition metadata missing".to_owned())
            })
            .collect::<Result<_, _>>()?;
        let partitioned = self.visibility.is_partitioned();
        let next_generation = self
            .generation
            .checked_add(1)
            .ok_or("WAL number space exhausted")?;
        let new_source = SourceId(self.next_source);
        self.next_source = self
            .next_source
            .checked_add(1)
            .ok_or("source ID space exhausted")?;
        self.visibility
            .freeze_source(self.active.source)
            .map_err(err)?;
        self.visibility.add_source(new_source, 0).map_err(err)?;
        let old = std::mem::replace(&mut self.active, MemTable::new(new_source));
        // 视图构建若 panic，原 writer 必须仍由 self 持有，不能随局部 File 容器析构。
        let frozen = old.freeze(&self.visibility);
        #[cfg(feature = "fault-injection")]
        crate::db::state_panic(crate::db::StatePanicPoint::Freeze);
        let mut wal_files = Vec::new();
        if let Some(writer) = self.data_wal.take() {
            wal_files.push((WalKind::Data, writer.into_file()));
        }
        if let Some(writer) = self.delta_wal.take() {
            wal_files.push((WalKind::Delta, writer.into_file()));
        }
        self.pending.push_back(Arc::new(FrozenGeneration {
            partitions,
            partitioned,
            number: self.generation,
            table: frozen,
            wal_files: Mutex::new(wal_files),
        }));
        self.generation = next_generation;
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
                #[cfg(feature = "fault-injection")]
                crate::db::state_panic(crate::db::StatePanicPoint::Recommit);
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

    pub(crate) fn mark_inherited_synced(&mut self) {
        self.inherited_synced = true;
    }

    /// 清空第 3 步的前提：flush 之后没有冻结代与打开的 WAL 写者，当前代编号严格大于 `wal/` 中所有规范 WAL，
    /// 因而 `wal/` 中的行全部已在 manifest 中。
    pub(crate) fn truncate_wal_ready(&self) -> Result<(), String> {
        if !self.pending.is_empty() || self.data_wal.is_some() || self.delta_wal.is_some() {
            return Err("collection still has unflushed WAL generations".to_owned());
        }
        let wal = crate::storage::recovery::scan_wal(&self.dir.join("wal")).map_err(err)?;
        match wal.keys().next_back() {
            Some(&last) if last >= self.generation => Err(format!(
                "WAL generation {last} is not older than the active generation {}",
                self.generation
            )),
            _ => Ok(()),
        }
    }

    /// 清空第 5 步按重开路径重建之后，接续原实例：WAL 代编号与 SourceId 计数在进程内继续单调递增，
    /// flush 锁与维护状态沿用原来的 `Arc`（等待者与维护队列持有的是它们）。
    pub(crate) fn continue_from(&mut self, old: &Self) -> Result<(), String> {
        self.generation = self.generation.max(
            old.generation
                .checked_add(1)
                .ok_or("WAL number space exhausted")?,
        );
        self.next_source = self.next_source.max(
            old.next_source
                .checked_add(1)
                .ok_or("source ID space exhausted")?,
        );
        self.flush_lock = Arc::clone(&old.flush_lock);
        self.maintenance_status = Arc::clone(&old.maintenance_status);
        Ok(())
    }

    /// manifest 已替换、集合目录同步未确认：沿用 flush 的重新提交路径。
    pub(crate) fn mark_manifest_unconfirmed(&mut self, reason: String) {
        self.retry_manifest = true;
        self.record_failure(reason);
    }

    pub(crate) fn index_manifest(
        &self,
        change: &crate::index::IndexChange,
    ) -> Result<Manifest, String> {
        if self.poisoned || self.needs_retry() || self.truncate_cleanup.is_some() {
            return Err("collection has unfinished storage work".to_owned());
        }
        let mut candidate = self.manifest.clone();
        candidate.version = candidate
            .version
            .checked_add(1)
            .ok_or("manifest version space exhausted")?;
        match change {
            crate::index::IndexChange::Create(spec) => {
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
            }
            crate::index::IndexChange::Drop(specs) => {
                for spec in specs {
                    candidate.index_specs.remove(&spec.field_name);
                }
            }
        }
        Ok(candidate)
    }

    pub(crate) fn commit_index_manifest(
        &mut self,
        candidate: Manifest,
        prepared: &mut fs_commit::PreparedManifest,
    ) -> Result<(), String> {
        if self.poisoned || self.needs_retry() {
            return Err("collection has unfinished storage work".to_owned());
        }
        match prepared.replace() {
            Ok(report) => {
                if let Some(error) = report.prev_copy_error {
                    eprintln!(
                        "index manifest prev copy failed for {:?}: {error}",
                        self.dir
                    );
                }
            }
            Err(error) if error.phase == CommitPhase::AfterReplace => {
                self.retry_manifest = true;
                self.record_failure(error.to_string());
                eprintln!(
                    "index definition committed but directory sync failed for {:?}: {error}",
                    self.dir
                );
            }
            Err(error) => return Err(error.to_string()),
        }
        self.manifest = candidate;
        Ok(())
    }

    pub(crate) fn validate_partition_commit(&self) -> Result<(), String> {
        if self.poisoned || self.needs_retry() || self.truncate_cleanup.is_some() {
            return Err("collection has unfinished storage work".into());
        }
        Ok(())
    }

    pub(crate) fn publish_partition_manifest(
        &mut self,
        candidate: Manifest,
        registry: super::partition_paths::Registry,
        committed: Result<fs_commit::ManifestCommit, fs_commit::CommitError>,
    ) -> Result<(), String> {
        let durability = match committed {
            Ok(report) => {
                if let Some(error) = report.prev_copy_error {
                    eprintln!(
                        "partition manifest backup copy failed for {:?}: {error}",
                        self.dir
                    );
                }
                None
            }
            Err(error) if error.phase == CommitPhase::BeforeReplace => {
                return Err(error.to_string());
            }
            Err(error) => Some(error.to_string()),
        };
        // 正常写者由独占许可排除；若先前维护的异常收尾已封锁实例，不能发布半状态。
        if let Err(error) = self.validate_partition_commit() {
            self.poisoned = true;
            return Err(error);
        }
        self.manifest = candidate;
        self.partitions = registry;
        if let Some(error) = durability {
            self.retry_manifest = true;
            self.record_failure(error.clone());
            return Err(error);
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
        source_rows: &[(Pk, usize)],
        after_replace_error: bool,
    ) -> Result<SourceId, String> {
        if !self
            .pending
            .front()
            .is_some_and(|front| Arc::ptr_eq(front, generation))
        {
            self.poisoned = true;
            return Err("flush generation changed before publication".to_owned());
        }
        let segment_source = SourceId(self.next_source);
        let new_file = if data_rows > 0 {
            let old = &self.manifest.partitions["_default"].data_files;
            let files: Vec<_> = manifest.partitions["_default"]
                .data_files
                .iter()
                .filter(|f| !old.contains(f))
                .cloned()
                .collect();
            if files.len() != 1 {
                return Err("published data filename is incomplete".into());
            }
            files.into_iter().next()
        } else {
            None
        };
        self.next_source = self
            .next_source
            .checked_add(1)
            .ok_or("source ID space exhausted")?;
        if let Err(error) = self.visibility.replace_source_known_rows(
            generation.table.source(),
            segment_source,
            data_rows,
            mapping,
            source_rows,
        ) {
            self.poisoned = true;
            return Err(error.to_string());
        }
        if let Some(file) = new_file {
            self.data_sources
                .insert(("_default".into(), file), segment_source);
        }
        self.manifest = manifest;
        self.pending.pop_front();
        self.last_published_generation = Some(generation.number);
        for (pk, seq) in generation.table.delete_entries() {
            self.committed_deletes
                .entry(pk.clone())
                .and_modify(|old| *old = (*old).max(seq))
                .or_insert(seq);
        }
        // 此代编号大于打开时扫描到的所有 WAL；维护入场还须确认这些 WAL 清理成功。
        self.retained_old_wal = false;
        self.inherited_synced = true;
        self.failed_flush = false;
        self.retry_manifest |= after_replace_error;
        if !self.retry_manifest {
            self.last_failure = None;
        }
        Ok(segment_source)
    }

    pub(crate) fn publish_partitioned(
        &mut self,
        generation: &Arc<FrozenGeneration>,
        manifest: Manifest,
        publication: &PartitionFlushOutcome,
        source_rows: &[(Pk, usize, u64)],
        after_replace_error: bool,
    ) -> Result<Vec<(u32, SourceId)>, String> {
        if !self
            .pending
            .front()
            .is_some_and(|front| Arc::ptr_eq(front, generation))
        {
            return Err("flush generation changed before publication".into());
        }
        let mut targets = Vec::new();
        let mut names = Vec::new();
        let mut sources = Vec::new();
        for data in &publication.data {
            let name = self
                .partitions
                .name(data.id)
                .ok_or("published partition missing")?;
            let old = &self
                .manifest
                .partitions
                .get(name)
                .ok_or("old partition missing")?
                .data_files;
            let new = &manifest
                .partitions
                .get(name)
                .ok_or("new partition missing")?
                .data_files;
            let added: Vec<_> = new
                .iter()
                .filter(|file| !old.contains(file))
                .cloned()
                .collect();
            if added.len() != 1 {
                return Err("published partition filename is incomplete".into());
            }
            let source = SourceId(self.next_source);
            self.next_source = self
                .next_source
                .checked_add(1)
                .ok_or("source ID space exhausted")?;
            targets.push((data.id, source));
            sources.push((
                source,
                data.data.seqs.len(),
                crate::storage::visibility::partitions::SourceKind::Immutable(data.id),
            ));
            names.push(((name.to_owned(), added[0].clone()), source));
        }
        let mut mapping = Vec::with_capacity(source_rows.len());
        for (pk, row, seq) in source_rows {
            let target = match publication
                .mapping
                .get(*row)
                .ok_or("partition source mapping missing")?
            {
                None => None,
                Some((part, row)) => Some(crate::storage::visibility::RowLocation {
                    source: targets
                        .iter()
                        .find(|(id, _)| id == part)
                        .map(|(_, source)| *source)
                        .ok_or("partition target source missing")?,
                    row: *row,
                    seq: *seq,
                }),
            };
            mapping.push((pk.clone(), *row, target));
        }
        let mut order = Vec::new();
        for &source in self.visibility.source_order() {
            if source == generation.table.source() {
                order.extend(targets.iter().map(|(_, source)| *source));
            } else {
                order.push(source);
            }
        }
        self.visibility
            .replace_partition_source(generation.table.source(), mapping, &sources, &order)
            .map_err(err)?;
        self.data_sources.extend(names);
        self.manifest = manifest;
        self.pending.pop_front();
        self.last_published_generation = Some(generation.number);
        for (pk, seq) in generation.table.delete_entries() {
            self.committed_deletes
                .entry(pk.clone())
                .and_modify(|old| *old = (*old).max(seq))
                .or_insert(seq);
        }
        self.retained_old_wal = false;
        self.inherited_synced = true;
        self.failed_flush = false;
        self.retry_manifest |= after_replace_error;
        if !self.retry_manifest {
            self.last_failure = None;
        }
        Ok(targets)
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

    pub(crate) fn retained_old_wal(&self) -> bool {
        self.retained_old_wal
    }

    pub fn sync_retained_wal(&mut self) -> Result<(), String> {
        // 全部同步成功之前保留 writer；drop 同步失败后仍须允许原 collection 写入。
        let dir = self.dir.join("wal");
        match fs::metadata(&dir) {
            Ok(_) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && self.data_wal.is_none()
                    && self.delta_wal.is_none()
                    && self
                        .pending
                        .iter()
                        .all(|generation| generation.wal_files.lock().is_empty()) =>
            {
                return Ok(());
            }
            Err(error) => return Err(err(error)),
        }
        for entry in fs::read_dir(&dir).map_err(err)? {
            let entry = entry.map_err(err)?;
            if entry.file_type().map_err(err)?.is_file() {
                #[cfg(feature = "fault-injection")]
                {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with("wal_data_") {
                        retained_wal_sync_fault(RetainedWalSyncFault::Data)?;
                    }
                    if name.starts_with("wal_delta_") {
                        retained_wal_sync_fault(RetainedWalSyncFault::Delta)?;
                    }
                }
                let path = entry.path();
                File::open(&path)
                    .and_then(|file| fs_commit::sync_file(&file, &path))
                    .map_err(err)?;
            }
        }
        #[cfg(feature = "fault-injection")]
        retained_wal_sync_fault(RetainedWalSyncFault::Directory)?;
        fs_commit::sync_dir(&dir).map_err(err)?;
        self.data_wal = None;
        self.delta_wal = None;
        for generation in &self.pending {
            generation.wal_files.lock().clear();
        }
        Ok(())
    }

    /// 外部移走整个 collection 后只能同步仍持有的原句柄；不创建目录，也不追踪移走后的路径。
    /// 这不放宽正常关闭对 wal/ 缺失的判定，调用方须先确认原 collection 目录已不存在。
    pub(crate) fn sync_displaced_wal(&mut self) -> Result<(), String> {
        for (kind, writer) in [
            (WalKind::Data, &self.data_wal),
            (WalKind::Delta, &self.delta_wal),
        ] {
            if let Some(writer) = writer {
                #[cfg(feature = "fault-injection")]
                retained_wal_sync_fault(match kind {
                    WalKind::Data => RetainedWalSyncFault::Data,
                    WalKind::Delta => RetainedWalSyncFault::Delta,
                })?;
                #[cfg(not(feature = "fault-injection"))]
                let _kind = kind;
                writer.sync().map_err(err)?;
            }
        }
        for generation in &self.pending {
            let files = generation.wal_files.lock();
            for (kind, file) in files.iter() {
                #[cfg(feature = "fault-injection")]
                retained_wal_sync_fault(match kind {
                    WalKind::Data => RetainedWalSyncFault::Data,
                    WalKind::Delta => RetainedWalSyncFault::Delta,
                })?;
                #[cfg(not(feature = "fault-injection"))]
                let _kind = kind;
                fs_commit::sync_file(
                    file,
                    &self.dir.join("wal").join(kind.file_name(generation.number)),
                )
                .map_err(err)?;
            }
        }
        self.data_wal = None;
        self.delta_wal = None;
        for generation in &self.pending {
            generation.wal_files.lock().clear();
        }
        Ok(())
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
    if generation.partitioned {
        return flush_one_partitioned(dir, manifest, generation, schema, sync_inherited_files);
    }
    let (materialized, data, norms) =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(feature = "fault-injection")]
            if PREPARE_PANIC.with(|slot| slot.replace(false)) {
                panic!("injected flush preparation panic");
            }
            let materialized =
                generation
                    .table
                    .materialize(schema)
                    .map_err(|error| FlushError {
                        path: dir.to_path_buf(),
                        phase: CommitPhase::BeforeReplace,
                        reason: error.to_string(),
                        committed_manifest: None,
                        cleanup_failed: false,
                    })?;
            let data = materialized
                .data
                .as_ref()
                .map(|batch| arrow_data::decode_data_batch(schema, batch))
                .transpose()
                .map_err(|error| FlushError {
                    path: dir.to_path_buf(),
                    phase: CommitPhase::BeforeReplace,
                    reason: error.to_string(),
                    committed_manifest: None,
                    cleanup_failed: false,
                })?;
            let norms = match &data {
                None => Vec::new(),
                Some(decoded) => decoded
                    .columns
                    .iter()
                    .find_map(|column| {
                        if let ColumnData::FloatVector { dim, values } = column {
                            Some(values.chunks_exact(*dim).map(crate::flat::norm).collect())
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| FlushError {
                        path: dir.to_path_buf(),
                        phase: CommitPhase::BeforeReplace,
                        reason: "published data has no dense vector column".to_owned(),
                        committed_manifest: None,
                        cleanup_failed: false,
                    })?,
            };
            // 钩子的诊断失败必须落在提交点之前，不能跳过已提交数据的锁内发布。
            #[cfg(feature = "fault-injection")]
            BEFORE_COMMIT_PAUSE.with(|slot| -> Result<(), FlushError> {
                if let Some((entered, resume)) = slot.borrow_mut().take() {
                    let error = |reason: String| FlushError {
                        path: dir.to_path_buf(),
                        phase: CommitPhase::BeforeReplace,
                        reason,
                        committed_manifest: None,
                        cleanup_failed: false,
                    };
                    entered.send(()).map_err(|cause| error(cause.to_string()))?;
                    resume.recv().map_err(|cause| error(cause.to_string()))?;
                }
                Ok(())
            })?;
            generation
                .sync_and_close_wal(dir)
                .map_err(|reason| FlushError {
                    path: dir.join("wal"),
                    phase: CommitPhase::BeforeReplace,
                    reason,
                    committed_manifest: None,
                    cleanup_failed: false,
                })?;
            Ok::<_, FlushError>((materialized, data, norms))
        }))
        .unwrap_or_else(|payload| {
            Err(FlushError {
                path: dir.to_path_buf(),
                phase: CommitPhase::BeforeReplace,
                reason: format!(
                    "flush preparation panicked: {}",
                    crate::error::panic_description(payload.as_ref())
                ),
                committed_manifest: None,
                cleanup_failed: false,
            })
        })?;
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
            partitioned: None,
            manifest,
            mapping: materialized.mapping,
            data,
            norms,
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
                partitioned: None,
                manifest: (**manifest).clone(),
                mapping: materialized.mapping,
                data,
                norms,
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

fn prepare_error(dir: &Path, reason: impl ToString) -> FlushError {
    FlushError {
        path: dir.to_path_buf(),
        phase: CommitPhase::BeforeReplace,
        reason: reason.to_string(),
        committed_manifest: None,
        cleanup_failed: false,
    }
}

fn flush_one_partitioned(
    dir: &Path,
    manifest: &Manifest,
    generation: &FrozenGeneration,
    schema: &CollectionSchema,
    sync_inherited_files: bool,
) -> Result<FlushOutcome, FlushError> {
    let (materialized, data) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        #[cfg(feature = "fault-injection")]
        if PREPARE_PANIC.with(|slot| slot.replace(false)) {
            panic!("injected flush preparation panic");
        }
        let known: Vec<_> = generation.partitions.iter().map(|(id, _)| *id).collect();
        let materialized = generation
            .table
            .materialize_partitions(schema, &known)
            .map_err(|error| prepare_error(dir, error))?;
        let mut data = Vec::new();
        for (&id, batch) in &materialized.batches {
            if let Some(batch) = &batch.data {
                let decoded = arrow_data::decode_data_batch(schema, batch)
                    .map_err(|error| prepare_error(dir, error))?;
                let norms = decoded
                    .columns
                    .iter()
                    .find_map(|column| {
                        if let ColumnData::FloatVector { dim, values } = column {
                            Some(values.chunks_exact(*dim).map(crate::flat::norm).collect())
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| {
                        prepare_error(dir, "published data has no dense vector column")
                    })?;
                data.push(FlushedPartition {
                    id,
                    data: decoded,
                    norms,
                });
            }
        }
        #[cfg(feature = "fault-injection")]
        BEFORE_COMMIT_PAUSE.with(|slot| -> Result<(), FlushError> {
            if let Some((entered, resume)) = slot.borrow_mut().take() {
                entered
                    .send(())
                    .map_err(|error| prepare_error(dir, error))?;
                resume.recv().map_err(|error| prepare_error(dir, error))?;
            }
            Ok(())
        })?;
        generation
            .sync_and_close_wal(dir)
            .map_err(|error| prepare_error(dir.join("wal").as_path(), error))?;
        Ok::<_, FlushError>((materialized, data))
    }))
    .unwrap_or_else(|panic| {
        Err(prepare_error(
            dir,
            format!(
                "flush preparation panicked: {}",
                crate::error::panic_description(panic.as_ref())
            ),
        ))
    })?;
    let mut plans = Vec::new();
    for (id, batch) in &materialized.batches {
        let name = generation
            .partitions
            .iter()
            .find(|(part, _)| part == id)
            .map(|(_, name)| name.as_str())
            .ok_or_else(|| prepare_error(dir, "frozen partition name missing"))?;
        plans.push(flush::PartitionFlush {
            name,
            data: batch.data.as_ref(),
            delta: batch.delta.as_ref(),
        });
    }
    if plans.is_empty() {
        plans.push(flush::PartitionFlush {
            name: "_default",
            data: None,
            delta: None,
        });
    }
    let result = flush::flush_partitions_generation(
        dir,
        manifest,
        generation.number,
        generation.table.max_seq(),
        &plans,
        sync_inherited_files,
    );
    let (manifest, post_commit_error) = match result {
        Ok(manifest) => (manifest, None),
        Err(error) if error.phase == CommitPhase::AfterReplace => {
            let manifest = error.committed_manifest.ok_or_else(|| {
                prepare_error(dir, "committed manifest missing from post-replace error")
            })?;
            (
                *manifest,
                Some(if error.cleanup_failed {
                    PostCommitError::WalCleanup(error.reason)
                } else {
                    PostCommitError::Durability(error.reason)
                }),
            )
        }
        Err(error) => return Err(error),
    };
    Ok(FlushOutcome {
        manifest,
        partitioned: Some(PartitionFlushOutcome {
            mapping: materialized.mapping,
            data,
        }),
        mapping: Vec::new(),
        data: None,
        norms: Vec::new(),
        post_commit_error,
    })
}

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(all(test, feature = "fault-injection"))]
mod retained_wal_tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn displaced_collection_syncs_active_and_frozen_original_files_before_closing_any() {
        use crate::{CollectionSchema, DataType, FieldSchema};
        let root = tempfile::tempdir().unwrap();
        let mut pk = FieldSchema::new("id", DataType::Int64);
        pk.is_primary = true;
        let schema = CollectionSchema {
            fields: vec![pk],
            enable_dynamic_field: false,
            properties: vec![],
        };
        for active in [false, true] {
            for fault in [RetainedWalSyncFault::Data, RetainedWalSyncFault::Delta] {
                let dir = root.path().join(format!("{active}-{fault:?}"));
                fs::create_dir_all(dir.join("wal")).unwrap();
                let mut persistent = PersistentCollection::new(dir.clone()).unwrap();
                persistent.data_wal = Some(
                    WalWriter::create(
                        &dir.join("wal"),
                        WalKind::Data,
                        1,
                        &arrow_data::wal_data_schema(&schema).unwrap(),
                    )
                    .unwrap(),
                );
                persistent.delta_wal = Some(
                    WalWriter::create(
                        &dir.join("wal"),
                        WalKind::Delta,
                        1,
                        &arrow_data::wal_delta_schema(&schema).unwrap(),
                    )
                    .unwrap(),
                );
                persistent.freeze_active(true).unwrap();
                let frozen = persistent.front().unwrap();
                if active {
                    persistent.data_wal = Some(
                        WalWriter::create(
                            &dir.join("wal"),
                            WalKind::Data,
                            2,
                            &arrow_data::wal_data_schema(&schema).unwrap(),
                        )
                        .unwrap(),
                    );
                    persistent.delta_wal = Some(
                        WalWriter::create(
                            &dir.join("wal"),
                            WalKind::Delta,
                            2,
                            &arrow_data::wal_delta_schema(&schema).unwrap(),
                        )
                        .unwrap(),
                    );
                }
                fs::remove_dir_all(&dir).unwrap();
                inject_retained_wal_sync_failure_once(fault);
                assert!(persistent.sync_displaced_wal().is_err());
                assert_eq!(frozen.wal_files.lock().len(), 2);
                assert_eq!(persistent.data_wal.is_some(), active);
                assert_eq!(persistent.delta_wal.is_some(), active);
                persistent.sync_displaced_wal().unwrap();
                assert!(frozen.wal_files.lock().is_empty());
                assert!(persistent.data_wal.is_none() && persistent.delta_wal.is_none());
                assert!(!dir.exists());
            }
        }
    }
    #[test]
    fn failed_sync_keeps_both_active_writers() {
        use crate::{CollectionSchema, DataType, FieldSchema};
        let root = tempfile::tempdir().unwrap();
        let mut pk = FieldSchema::new("id", DataType::Int64);
        pk.is_primary = true;
        let schema = CollectionSchema {
            fields: vec![pk],
            enable_dynamic_field: false,
            properties: Vec::new(),
        };
        for fault in [
            RetainedWalSyncFault::Data,
            RetainedWalSyncFault::Delta,
            RetainedWalSyncFault::Directory,
        ] {
            let dir = root.path().join(format!("{fault:?}"));
            std::fs::create_dir(&dir).unwrap();
            let mut persistent = PersistentCollection::new(dir.clone()).unwrap();
            std::fs::create_dir(dir.join("wal")).unwrap();
            persistent.data_wal = Some(
                WalWriter::create(
                    &dir.join("wal"),
                    WalKind::Data,
                    1,
                    &arrow_data::wal_data_schema(&schema).unwrap(),
                )
                .unwrap(),
            );
            persistent.delta_wal = Some(
                WalWriter::create(
                    &dir.join("wal"),
                    WalKind::Delta,
                    1,
                    &arrow_data::wal_delta_schema(&schema).unwrap(),
                )
                .unwrap(),
            );
            inject_retained_wal_sync_failure_once(fault);
            assert!(persistent.sync_retained_wal().is_err());
            assert!(persistent.data_wal.is_some());
            assert!(persistent.delta_wal.is_some());
            persistent.sync_retained_wal().unwrap();
            assert!(persistent.data_wal.is_none());
            assert!(persistent.delta_wal.is_none());
        }
    }
    #[test]
    fn frozen_generation_keeps_original_files_until_all_syncs_succeed() {
        use crate::{CollectionSchema, DataType, FieldSchema};
        let root = tempfile::tempdir().unwrap();
        let mut pk = FieldSchema::new("id", DataType::Int64);
        pk.is_primary = true;
        let schema = CollectionSchema {
            fields: vec![pk],
            enable_dynamic_field: false,
            properties: Vec::new(),
        };
        for fault in [
            RetainedWalSyncFault::Data,
            RetainedWalSyncFault::Delta,
            RetainedWalSyncFault::Directory,
        ] {
            for panic in [false, true] {
                for close_all in [false, true] {
                    let dir = root.path().join(format!("{fault:?}-{close_all}-{panic}"));
                    std::fs::create_dir_all(dir.join("wal")).unwrap();
                    let mut persistent = PersistentCollection::new(dir.clone()).unwrap();
                    persistent.data_wal = Some(
                        WalWriter::create(
                            &dir.join("wal"),
                            WalKind::Data,
                            1,
                            &arrow_data::wal_data_schema(&schema).unwrap(),
                        )
                        .unwrap(),
                    );
                    persistent.delta_wal = Some(
                        WalWriter::create(
                            &dir.join("wal"),
                            WalKind::Delta,
                            1,
                            &arrow_data::wal_delta_schema(&schema).unwrap(),
                        )
                        .unwrap(),
                    );
                    persistent.freeze_active(true).unwrap();
                    let frozen = persistent.front().unwrap();
                    assert!(persistent.data_wal.is_none() && persistent.delta_wal.is_none());
                    assert_eq!(frozen.wal_files.lock().len(), 2);
                    if panic {
                        inject_retained_wal_sync_panic_once(fault);
                    } else {
                        inject_retained_wal_sync_failure_once(fault);
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if close_all {
                            persistent.sync_retained_wal()
                        } else {
                            frozen.sync_and_close_wal(&dir)
                        }
                    }));
                    if panic {
                        assert!(result.is_err());
                    } else {
                        assert!(result.unwrap().is_err());
                    }
                    assert_eq!(frozen.wal_files.lock().len(), 2);
                    if close_all {
                        persistent.sync_retained_wal().unwrap();
                    } else {
                        frozen.sync_and_close_wal(&dir).unwrap();
                    }
                    assert!(frozen.wal_files.lock().is_empty());
                }
            }
        }
    }
    #[cfg(unix)]
    #[test]
    fn missing_wal_directory_only_succeeds_when_no_owned_handles_remain() {
        use crate::{CollectionSchema, ColumnData, DataType, FieldSchema};
        let root = tempfile::tempdir().unwrap();
        let mut pk = FieldSchema::new("id", DataType::Int64);
        pk.is_primary = true;
        let schema = CollectionSchema {
            fields: vec![pk],
            enable_dynamic_field: false,
            properties: Vec::new(),
        };
        for frozen in [false, true] {
            let dir = root.path().join(format!("{frozen}"));
            std::fs::create_dir(&dir).unwrap();
            let mut persistent = PersistentCollection::new(dir.clone()).unwrap();
            assert!(persistent.sync_retained_wal().is_ok());
            persistent
                .append(&schema, &[1], &[ColumnData::Int64(vec![1])], None)
                .unwrap();
            persistent
                .append_delete(&schema, &[Pk::Int64(1)], 2)
                .unwrap();
            if frozen {
                persistent.freeze_active(true).unwrap();
            }
            std::fs::remove_dir_all(dir.join("wal")).unwrap();
            assert!(persistent.sync_retained_wal().is_err());
            if frozen {
                assert_eq!(persistent.front().unwrap().wal_files.lock().len(), 2);
            } else {
                assert!(persistent.data_wal.is_some() && persistent.delta_wal.is_some());
            }
        }
        let mut empty = PersistentCollection::new(root.path().join("empty")).unwrap();
        empty.freeze_active(true).unwrap();
        assert!(empty.sync_retained_wal().is_ok());
    }

    #[test]
    fn retry_window_guard_restores_thread_state_and_does_not_create_a_failure() {
        let root = tempfile::tempdir().unwrap();
        let mut persistent = PersistentCollection::new(root.path().to_path_buf()).unwrap();
        assert!(!HOLD_RETRY_WINDOW.with(|cell| cell.get()));
        {
            let _guard = hold_flush_retry_window();
            assert!(persistent.throttled_failure().is_none());
            persistent.last_failure = Some((
                Instant::now() - Duration::from_secs(2),
                "known failure".into(),
            ));
            assert_eq!(persistent.throttled_failure(), Some("known failure"));
            {
                let _inner = hold_flush_retry_window();
                assert!(HOLD_RETRY_WINDOW.with(|cell| cell.get()));
            }
            assert!(HOLD_RETRY_WINDOW.with(|cell| cell.get()));
        }
        assert!(!HOLD_RETRY_WINDOW.with(|cell| cell.get()));
        assert!(persistent.throttled_failure().is_none());
    }
}
