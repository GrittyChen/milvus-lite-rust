//! 跨 segment / memtable 的按主键有效位图；查询只读位，不做逐行版本判定。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::data::{Pk, PkMap};

pub mod partitions;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowLocation {
    pub source: SourceId,
    pub row: usize,
    pub seq: u64,
}

pub(crate) struct CompactionRow {
    pub pk: Pk,
    pub seq: u64,
    pub target: Option<(SourceId, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceBits {
    pub(crate) len: usize,
    bits: Arc<Vec<u64>>,
}

impl SourceBits {
    pub(crate) fn new(len: usize) -> Self {
        Self {
            len,
            bits: Arc::new(vec![0; len.div_ceil(64)]),
        }
    }

    pub(crate) fn from_live(live: &[bool]) -> Self {
        let mut bits = Self::new(live.len());
        for (row, &value) in live.iter().enumerate() {
            bits.set(row, value);
        }
        bits
    }

    pub(crate) fn get(&self, row: usize) -> bool {
        self.bits[row / 64] & (1_u64 << (row % 64)) != 0
    }

    pub(crate) fn any_live(&self) -> bool {
        self.bits.iter().any(|word| *word != 0)
    }

    pub(crate) fn count_live(&self) -> usize {
        self.bits
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    pub(crate) fn live_rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.bits.iter().enumerate().flat_map(|(index, &word)| {
            let mut remaining = word;
            std::iter::from_fn(move || {
                if remaining == 0 {
                    return None;
                }
                let row = index * 64 + remaining.trailing_zeros() as usize;
                remaining &= remaining - 1;
                Some(row)
            })
        })
    }

    /// 每个 and 操作数遍历原有效位，不能用累计掩码屏蔽后续操作数的转换错误。
    pub(crate) fn intersect_matching(&mut self, live: &Self, test: impl Fn(usize) -> bool) {
        for (index, &word) in live.bits.iter().enumerate() {
            let mut matches = 0_u64;
            if word == u64::MAX {
                // 完整活字直接连续读列；稀疏字仍只访问活行，尾部填充位保持为零。
                for bit in 0..64 {
                    matches |= u64::from(test(index * 64 + bit)) << bit;
                }
            } else {
                let mut remaining = word;
                while remaining != 0 {
                    let bit = remaining.trailing_zeros() as usize;
                    matches |= u64::from(test(index * 64 + bit)) << bit;
                    remaining &= remaining - 1;
                }
            }
            let matches = matches & self.bits[index];
            if matches != self.bits[index] {
                Arc::make_mut(&mut self.bits)[index] = matches;
            }
        }
    }

    pub(crate) fn set(&mut self, row: usize, live: bool) {
        let word = &mut Arc::make_mut(&mut self.bits)[row / 64];
        let mask = 1_u64 << (row % 64);
        if live {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }

    pub(crate) fn push(&mut self, valid: bool) {
        if self.len.is_multiple_of(64) {
            Arc::make_mut(&mut self.bits).push(0);
        }
        let row = self.len;
        self.len += 1;
        if valid {
            self.set(row, true);
        }
    }

    pub(crate) fn intersect(&mut self, other: &Self) {
        assert_eq!(
            self.len, other.len,
            "source masks refer to the same physical rows"
        );
        for (word, other) in Arc::make_mut(&mut self.bits)
            .iter_mut()
            .zip(other.bits.iter())
        {
            *word &= other;
        }
    }

    pub(crate) fn slice(&self, start: usize, len: usize) -> Option<Self> {
        if start.checked_add(len)? > self.len {
            return None;
        }
        if start == 0 && len == self.len {
            return Some(self.clone());
        }
        let mut result = Self::new(len);
        let shift = start % 64;
        for (index, target) in Arc::make_mut(&mut result.bits).iter_mut().enumerate() {
            let word = start / 64 + index;
            *target = self.bits[word] >> shift;
            if shift != 0 {
                // 最末字可能没有高侧邻居；不存在的部分仅为有效范围之外的填充位。
                *target |= self
                    .bits
                    .get(word + 1)
                    .map_or(0, |word| *word << (64 - shift));
            }
        }
        if !len.is_multiple_of(64)
            && let Some(last) = Arc::make_mut(&mut result.bits).last_mut()
        {
            *last &= (1 << (len % 64)) - 1;
        }
        Some(result)
    }
}

#[derive(Debug, Clone, Default)]
pub struct VisibilitySnapshot {
    sources: BTreeMap<SourceId, SourceBits>,
    // 单分区零结果省略位图，但仍区分已知源的无效行与未知/越界位置。
    empty_sources: BTreeMap<SourceId, usize>,
    pub source_order: Vec<SourceId>,
    pub live_count: usize,
}

impl VisibilitySnapshot {
    pub(crate) fn source_bits(&self, source: SourceId) -> Option<&SourceBits> {
        self.sources.get(&source)
    }
    pub fn is_live(&self, source: SourceId, row: usize) -> Option<bool> {
        if let Some(bits) = self.sources.get(&source) {
            return (row < bits.len).then(|| bits.get(row));
        }
        (row < *self.empty_sources.get(&source)?).then_some(false)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VisibilityError {
    #[error("partition {0} requires partition visibility")]
    PartitionModeRequired(u32),
    #[error(transparent)]
    Partition(#[from] partitions::PartitionVisibilityError),
    #[error("source {0:?} already exists")]
    DuplicateSource(SourceId),
    #[error("source {0:?} does not exist")]
    UnknownSource(SourceId),
    #[error("row {row} is outside source {source_id:?}")]
    RowOutOfRange { source_id: SourceId, row: usize },
    #[error("source {source_id:?} cannot shrink from {old} to {new}")]
    SourceShrink {
        source_id: SourceId,
        old: usize,
        new: usize,
    },
    #[error(
        "replacement mapping has {actual} entries for source {source_id:?}, expected {expected}"
    )]
    MappingLength {
        source_id: SourceId,
        actual: usize,
        expected: usize,
    },
    #[error("replacement omits live row {row} of source {source_id:?}")]
    MissingLiveRow { source_id: SourceId, row: usize },
    #[error("replacement maps two rows to target row {row}")]
    DuplicateTarget { row: usize },
}

#[derive(Debug, Default, Clone)]
pub struct Visibility {
    partitions: Option<Box<partitions::PartitionVisibility>>,
    sources: BTreeMap<SourceId, SourceBits>,
    source_order: Vec<SourceId>,
    /// 最新的数据行；即使已被 tombstone 隐藏，仍保留它的 `_seq` 以处理乱序回放。
    latest_data: PkMap<RowLocation>,
    /// 已提交或活跃的删除中的最大 `_seq`。
    latest_delete: HashMap<Pk, u64>,
    live_count: usize,
}

impl Visibility {
    pub(crate) fn new_partitioned() -> Self {
        Self {
            partitions: Some(Box::default()),
            ..Self::default()
        }
    }
    pub(crate) fn add_partition_source(
        &mut self,
        id: SourceId,
        rows: usize,
        partition: u32,
    ) -> Result<(), VisibilityError> {
        if let Some(parts) = self.partitions.as_mut() {
            return parts
                .register_source(id, rows, partitions::SourceKind::Immutable(partition))
                .map_err(Into::into);
        }
        if partition != 0 {
            return Err(VisibilityError::PartitionModeRequired(partition));
        }
        self.add_source(id, rows)
    }
    pub(crate) fn replace_partition_source(
        &mut self,
        old: SourceId,
        inputs: Vec<(Pk, usize, Option<RowLocation>)>,
        outputs: &[(SourceId, usize, partitions::SourceKind)],
        order: &[SourceId],
    ) -> Result<(), VisibilityError> {
        let parts = self
            .partitions
            .as_mut()
            .ok_or(VisibilityError::PartitionModeRequired(0))?;
        parts.commit_deletes(old)?;
        parts.replace_sources(
            &[partitions::SourceReplacement {
                source: old,
                rows: inputs,
            }],
            outputs,
            order,
            partitions::Removal::Replace,
        )?;
        Ok(())
    }
    pub(crate) fn check_active_source(&self, source: SourceId) -> Result<(), VisibilityError> {
        if let Some(parts) = &self.partitions {
            parts.check_active(source)?;
        }
        Ok(())
    }
    pub(crate) fn release_version_index_for_rebuild(&mut self) -> PkMap<RowLocation> {
        let mut index = std::mem::take(&mut self.latest_data);
        index.clear();
        self.latest_delete = HashMap::new();
        index
    }
    pub fn is_partitioned(&self) -> bool {
        self.partitions.is_some()
    }

    pub(crate) fn enable_partitions(&mut self, partitions: partitions::PartitionVisibility) {
        self.sources.clear();
        self.source_order.clear();
        self.latest_data = PkMap::default();
        self.latest_delete = HashMap::new();
        self.live_count = 0;
        self.partitions = Some(Box::new(partitions));
    }

    pub fn selected_snapshot(
        &self,
        selected: Option<&[partitions::PartitionId]>,
    ) -> VisibilitySnapshot {
        if let Some(parts) = &self.partitions {
            return parts.snapshot(selected);
        }
        let mut snapshot = self.snapshot();
        if selected.is_some_and(|parts| !parts.contains(&0)) {
            snapshot.empty_sources = snapshot
                .sources
                .iter()
                .map(|(id, bits)| (*id, bits.len))
                .collect();
            snapshot.sources.clear();
            snapshot.live_count = 0;
        }
        snapshot
    }

    pub(crate) fn freeze_source(&mut self, source: SourceId) -> Result<(), VisibilityError> {
        if let Some(parts) = self.partitions.as_mut() {
            parts.freeze(source)?;
        }
        Ok(())
    }

    pub fn apply_local_delete(
        &mut self,
        source: SourceId,
        pk: Pk,
        seq: u64,
    ) -> Result<(), VisibilityError> {
        if let Some(parts) = self.partitions.as_mut() {
            return parts.delete(source, pk, seq).map_err(Into::into);
        }
        self.apply_delete(pk, seq)
    }

    pub fn apply_partition_row(
        &mut self,
        pk: Pk,
        seq: u64,
        source: SourceId,
        row: usize,
        partition: partitions::PartitionId,
    ) -> Result<(), VisibilityError> {
        if let Some(parts) = self.partitions.as_mut() {
            return parts
                .insert(
                    pk,
                    partitions::PartitionRow {
                        partition,
                        location: RowLocation { source, row, seq },
                    },
                )
                .map_err(Into::into);
        }
        if partition != 0 {
            return Err(VisibilityError::PartitionModeRequired(partition));
        }
        self.apply_row(pk, seq, source, row)
    }

    pub(crate) fn source_order(&self) -> &[SourceId] {
        if let Some(parts) = &self.partitions {
            return &parts.global.source_order;
        }
        &self.source_order
    }

    pub(crate) fn delete_snapshot(&self) -> HashMap<Pk, u64> {
        if let Some(parts) = &self.partitions {
            return parts.delete_snapshot();
        }
        self.latest_delete.clone()
    }

    pub(crate) fn drop_partition_sources(
        &mut self,
        partition: u32,
        inputs: &[partitions::SourceReplacement],
    ) -> Result<(), String> {
        let parts = self
            .partitions
            .as_mut()
            .ok_or("partition visibility missing")?;
        let removed: HashSet<_> = inputs.iter().map(|input| input.source).collect();
        let order: Vec<_> = parts
            .global
            .source_order
            .iter()
            .copied()
            .filter(|id| !removed.contains(id))
            .collect();
        parts
            .replace_sources(
                inputs,
                &[],
                &order,
                partitions::Removal::DropPartition(partition),
            )
            .map_err(|e| e.to_string())
    }

    pub(crate) fn compacted(
        &mut self,
        inputs: &[(SourceId, Vec<CompactionRow>)],
        outputs: &[(SourceId, usize)],
        order: Vec<SourceId>,
    ) -> Result<(), String> {
        if let Some(parts) = &mut self.partitions {
            let partition = inputs
                .first()
                .and_then(|(id, _)| parts.source_partition(*id))
                .ok_or("compaction partition missing")?;
            let sources: Vec<_> = inputs
                .iter()
                .map(|(source, rows)| partitions::SourceReplacement {
                    source: *source,
                    rows: rows
                        .iter()
                        .enumerate()
                        .map(|(row, input)| {
                            (
                                input.pk.clone(),
                                row,
                                input.target.map(|(source, row)| RowLocation {
                                    source,
                                    row,
                                    seq: input.seq,
                                }),
                            )
                        })
                        .collect(),
                })
                .collect();
            let outputs: Vec<_> = outputs
                .iter()
                .map(|&(source, rows)| (source, rows, partitions::SourceKind::Immutable(partition)))
                .collect();
            return parts
                .replace_sources(&sources, &outputs, &order, partitions::Removal::Replace)
                .map_err(|error| error.to_string());
        }
        let removed: HashSet<_> = inputs.iter().map(|(id, _)| *id).collect();
        if removed.len() != inputs.len() {
            return Err("duplicate compaction source".into());
        }
        let mut bits = BTreeMap::new();
        for &(id, rows) in outputs {
            if self.sources.contains_key(&id) || bits.insert(id, SourceBits::new(rows)).is_some() {
                return Err("compaction target source already exists".into());
            }
        }
        let expected: HashSet<_> = self
            .sources
            .keys()
            .filter(|id| !removed.contains(id))
            .copied()
            .chain(bits.keys().copied())
            .collect();
        if order.len() != expected.len()
            || order.iter().copied().collect::<HashSet<_>>() != expected
        {
            return Err("compaction source order is incomplete".into());
        }
        let mut targets = HashSet::new();
        for (id, rows) in inputs {
            let old = self.sources.get(id).ok_or("compaction source missing")?;
            if old.len != rows.len() {
                return Err("compaction source row count changed".into());
            }
            for (row, data) in rows.iter().enumerate() {
                if let Some((source, target)) = data.target {
                    let dst = bits
                        .get_mut(&source)
                        .ok_or("compaction target source missing")?;
                    if target >= dst.len || !targets.insert((source, target)) {
                        return Err("invalid compaction target row".into());
                    }
                    if old.get(row) {
                        dst.set(target, true);
                    }
                } else if old.get(row) {
                    return Err("compaction omits a live row".into());
                }
                if data.target.is_none()
                    && self
                        .latest_data(&data.pk)
                        .is_some_and(|at| at.source == *id && at.row == row)
                    && self
                        .latest_delete(&data.pk)
                        .is_none_or(|seq| seq <= data.seq)
                {
                    return Err("removed primary winner has no protecting delete".into());
                }
            }
        }
        if targets.len() != outputs.iter().map(|(_, n)| *n).sum::<usize>() {
            return Err("compaction targets are incomplete".into());
        }
        // 全部边界已校验；只更新选中来源的主键，未选中/活跃引用保持原样。
        for (id, rows) in inputs {
            for (row, data) in rows.iter().enumerate() {
                if self
                    .latest_data(&data.pk)
                    .is_some_and(|at| at.source == *id && at.row == row)
                {
                    if let Some((source, row)) = data.target {
                        self.latest_data.insert(
                            data.pk.clone(),
                            RowLocation {
                                source,
                                row,
                                seq: data.seq,
                            },
                        );
                    } else {
                        self.latest_data.remove(&data.pk);
                    }
                }
            }
            self.sources.remove(id);
        }
        self.sources.extend(bits);
        self.source_order = order;
        Ok(())
    }

    pub(crate) fn gc_deletes(
        &mut self,
        threshold: Option<u64>,
        committed: u64,
    ) -> Result<(), String> {
        if let Some(parts) = self.partitions.as_mut() {
            return parts
                .gc_committed_before(threshold)
                .map_err(|error| error.to_string());
        }
        self.latest_delete
            .retain(|_, seq| *seq > committed || threshold.is_some_and(|t| *seq >= t));
        Ok(())
    }

    pub fn add_source(&mut self, id: SourceId, rows: usize) -> Result<(), VisibilityError> {
        if let Some(parts) = self.partitions.as_mut() {
            return parts
                .register_source(
                    id,
                    rows,
                    if rows == 0 {
                        partitions::SourceKind::Active
                    } else {
                        partitions::SourceKind::Immutable(0)
                    },
                )
                .map_err(Into::into);
        }
        if self.sources.contains_key(&id) {
            return Err(VisibilityError::DuplicateSource(id));
        }
        self.sources.insert(id, SourceBits::new(rows));
        self.source_order.push(id);
        Ok(())
    }

    pub fn extend_source(&mut self, id: SourceId, new_len: usize) -> Result<(), VisibilityError> {
        if let Some(parts) = self.partitions.as_mut() {
            return parts.extend_source(id, new_len).map_err(Into::into);
        }
        let bits = self.source_mut(id)?;
        if new_len < bits.len {
            return Err(VisibilityError::SourceShrink {
                source_id: id,
                old: bits.len,
                new: new_len,
            });
        }
        Arc::make_mut(&mut bits.bits).resize(new_len.div_ceil(64), 0);
        bits.len = new_len;
        Ok(())
    }

    /// 数据源的物理行数，包括已被删除隐藏的行。
    pub(crate) fn source_len(&self, id: SourceId) -> Option<usize> {
        if let Some(parts) = &self.partitions {
            return parts.global.sources.get(&id).map(|bits| bits.len);
        }
        self.sources.get(&id).map(|bits| bits.len)
    }

    pub fn snapshot(&self) -> VisibilitySnapshot {
        if let Some(parts) = &self.partitions {
            return parts.snapshot(None);
        }
        VisibilitySnapshot {
            sources: self.sources.clone(),
            empty_sources: BTreeMap::new(),
            source_order: self.source_order.clone(),
            live_count: self.live_count,
        }
    }

    pub(crate) fn visible_data(&self, pk: &Pk, selected: Option<&[u32]>) -> Option<RowLocation> {
        if let Some(parts) = &self.partitions {
            return parts.visible_data(pk, selected);
        }
        if selected.is_some_and(|ids| !ids.contains(&0)) {
            return None;
        }
        let at = self.latest_data(pk)?;
        self.is_live(at.source, at.row)?.then_some(at)
    }

    pub fn latest_data(&self, pk: &Pk) -> Option<RowLocation> {
        if let Some(parts) = &self.partitions {
            return parts.latest_data(pk);
        }
        self.latest_data.get(pk).copied()
    }

    pub fn latest_delete(&self, pk: &Pk) -> Option<u64> {
        if let Some(parts) = &self.partitions {
            return parts.latest_delete(pk);
        }
        self.latest_delete.get(pk).copied()
    }

    pub fn is_live(&self, source: SourceId, row: usize) -> Option<bool> {
        if let Some(parts) = &self.partitions {
            return parts.global.is_live(source, row);
        }
        let bits = self.sources.get(&source)?;
        (row < bits.len).then(|| bits.get(row))
    }

    pub fn live_count(&self) -> usize {
        if let Some(parts) = &self.partitions {
            return parts.global.live_count;
        }
        self.live_count
    }

    /// 发布调用方提供冻结源的物理主键，不扫描无关数据源的全量主键表。
    /// 已知行集合须涵盖该源实际写入的物理行；当前主键引用所在行不可省略。
    pub fn replace_source_known_rows(
        &mut self,
        old: SourceId,
        new: SourceId,
        new_rows: usize,
        mapping: &[Option<usize>],
        rows: &[(Pk, usize)],
    ) -> Result<(), VisibilityError> {
        if let Some(parts) = self.partitions.as_mut() {
            if mapping.len() != parts.global.sources.get(&old).map_or(0, |bits| bits.len) {
                return Err(VisibilityError::MappingLength {
                    source_id: old,
                    actual: mapping.len(),
                    expected: parts.global.sources.get(&old).map_or(0, |bits| bits.len),
                });
            }
            let mut source_rows = Vec::with_capacity(rows.len());
            for (pk, row) in rows {
                let target = if let Some(target) = mapping.get(*row).copied().flatten() {
                    let seq = parts.source_row_seq(pk, old, *row).ok_or(
                        VisibilityError::MissingLiveRow {
                            source_id: old,
                            row: *row,
                        },
                    )?;
                    Some(RowLocation {
                        source: new,
                        row: target,
                        seq,
                    })
                } else {
                    None
                };
                source_rows.push((pk.clone(), *row, target));
            }
            let inputs = [partitions::SourceReplacement {
                source: old,
                rows: source_rows,
            }];
            let order: Vec<_> = parts
                .global
                .source_order
                .iter()
                .map(|&id| if id == old { new } else { id })
                .collect();
            parts.commit_deletes(old)?;
            return parts
                .replace_sources(
                    &inputs,
                    &[(new, new_rows, partitions::SourceKind::Immutable(0))],
                    &order,
                    partitions::Removal::Replace,
                )
                .map_err(Into::into);
        }
        if self.sources.contains_key(&new) {
            return Err(VisibilityError::DuplicateSource(new));
        }
        let old_bits = self.source(old)?;
        if mapping.len() != old_bits.len {
            return Err(VisibilityError::MappingLength {
                source_id: old,
                actual: mapping.len(),
                expected: old_bits.len,
            });
        }
        // 已知行集合也是调用方的输入；边界校验必须在换源之前完成。
        for (_, row) in rows {
            if mapping.get(*row).is_none() {
                return Err(VisibilityError::RowOutOfRange {
                    source_id: old,
                    row: *row,
                });
            }
        }
        let mut used = HashSet::new();
        let mut new_bits = SourceBits::new(new_rows);
        for (row, mapped) in mapping.iter().enumerate() {
            match mapped {
                Some(target) if *target >= new_rows => {
                    return Err(VisibilityError::RowOutOfRange {
                        source_id: new,
                        row: *target,
                    });
                }
                Some(target) if !used.insert(*target) => {
                    return Err(VisibilityError::DuplicateTarget { row: *target });
                }
                Some(target) if old_bits.get(row) => new_bits.set(*target, true),
                None if old_bits.get(row) => {
                    return Err(VisibilityError::MissingLiveRow {
                        source_id: old,
                        row,
                    });
                }
                _ => {}
            }
        }
        let rank = self
            .source_order
            .iter()
            .position(|id| *id == old)
            .ok_or(VisibilityError::UnknownSource(old))?;
        self.sources.remove(&old);
        self.sources.insert(new, new_bits);
        self.source_order[rank] = new;
        for (pk, row) in rows {
            if let Some(location) = self.latest_data.get_mut(pk)
                && location.source == old
                && location.row == *row
            {
                if let Some(target) = mapping.get(*row).copied().flatten() {
                    location.source = new;
                    location.row = target;
                } else {
                    self.latest_data.remove(pk);
                }
            }
        }
        Ok(())
    }

    /// 只有更大的 `_seq` 才覆盖旧行；同一 (pk, seq) 的第一份物理行赢。
    pub fn apply_row(
        &mut self,
        pk: Pk,
        seq: u64,
        source: SourceId,
        row: usize,
    ) -> Result<(), VisibilityError> {
        if let Some(parts) = self.partitions.as_mut() {
            return parts
                .insert(
                    pk,
                    partitions::PartitionRow {
                        partition: 0,
                        location: RowLocation { source, row, seq },
                    },
                )
                .map_err(Into::into);
        }
        self.check_row(source, row)?;
        if let Some(old) = self.latest_data.get(&pk)
            && (old.seq > seq || (old.seq == seq && !self.before(source, row, *old)))
        {
            return Ok(());
        }
        if let Some(old) = self.latest_data.get(&pk).copied()
            && self.source(old.source)?.get(old.row)
        {
            self.source_mut(old.source)?.set(old.row, false);
            self.live_count -= 1;
        }
        let deleted_after = self.latest_delete.get(&pk).is_some_and(|t| *t > seq);
        if !deleted_after {
            self.source_mut(source)?.set(row, true);
            self.live_count += 1;
        }
        self.latest_data
            .insert(pk, RowLocation { source, row, seq });
        Ok(())
    }

    /// 已提交 delta 与数据行同 seq 时数据可见（3.x DeltaIndex.is_deleted 用严格大于）；
    /// 同一 MemTable 内的平局由 MemTable 先按操作顺序拦截。
    pub fn apply_delete(&mut self, pk: Pk, seq: u64) -> Result<(), VisibilityError> {
        if let Some(parts) = self.partitions.as_mut() {
            return parts.committed_delete(pk, seq).map_err(Into::into);
        }
        if self.latest_delete.get(&pk).is_some_and(|old| *old >= seq) {
            return Ok(());
        }
        self.latest_delete.insert(pk.clone(), seq);
        if let Some(row) = self.latest_data.get(&pk).copied()
            && seq > row.seq
            && self.source(row.source)?.get(row.row)
        {
            self.source_mut(row.source)?.set(row.row, false);
            self.live_count -= 1;
        }
        Ok(())
    }

    fn check_row(&self, source: SourceId, row: usize) -> Result<(), VisibilityError> {
        let bits = self.source(source)?;
        if row >= bits.len {
            return Err(VisibilityError::RowOutOfRange {
                source_id: source,
                row,
            });
        }
        Ok(())
    }

    fn source(&self, id: SourceId) -> Result<&SourceBits, VisibilityError> {
        self.sources
            .get(&id)
            .ok_or(VisibilityError::UnknownSource(id))
    }

    fn source_mut(&mut self, id: SourceId) -> Result<&mut SourceBits, VisibilityError> {
        self.sources
            .get_mut(&id)
            .ok_or(VisibilityError::UnknownSource(id))
    }

    fn before(&self, source: SourceId, row: usize, old: RowLocation) -> bool {
        let new_rank = self.source_order.iter().position(|id| *id == source);
        let old_rank = self.source_order.iter().position(|id| *id == old.source);
        match (new_rank, old_rank) {
            (Some(new_rank), Some(old_rank)) => (new_rank, row) < (old_rank, old.row),
            // `apply_row` 已校验新 source，旧 source 由本表写入；缺失只可能是内部状态损坏。
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_slices_match_physical_ranges_across_word_and_tail_boundaries() {
        let live: Vec<_> = (0..257).map(|i| i % 7 == 0 || i % 11 == 3).collect();
        let bits = SourceBits::from_live(&live);
        for start in 0..=live.len() {
            for len in [0, 1, 2, 63, 64, 65, 127, 128, 129, 257] {
                if start + len > live.len() {
                    assert!(bits.slice(start, len).is_none());
                    continue;
                }
                let sliced = bits.slice(start, len).unwrap();
                assert_eq!(sliced.len, len);
                assert_eq!(
                    sliced.live_rows().collect::<Vec<_>>(),
                    live[start..start + len]
                        .iter()
                        .enumerate()
                        .filter_map(|(row, &yes)| yes.then_some(row))
                        .collect::<Vec<_>>()
                );
            }
        }
        assert!(bits.slice(usize::MAX, 1).is_none());
    }

    #[test]
    fn invalid_known_row_rejects_before_changing_visibility_or_primary_locations() {
        let mut vis = Visibility::default();
        vis.add_source(SourceId(1), 1).unwrap();
        vis.apply_row(Pk::Int64(7), 1, SourceId(1), 0).unwrap();
        let old = vis.snapshot();
        let at = vis.latest_data(&Pk::Int64(7));
        assert!(matches!(
            vis.replace_source_known_rows(
                SourceId(1),
                SourceId(2),
                1,
                &[Some(0)],
                &[(Pk::Int64(7), 99)]
            ),
            Err(VisibilityError::RowOutOfRange {
                source_id: SourceId(1),
                row: 99
            })
        ));
        assert_eq!(vis.snapshot().source_order, old.source_order);
        assert_eq!(vis.latest_data(&Pk::Int64(7)), at);
        assert_eq!(vis.is_live(SourceId(1), 0), Some(true));
        assert_eq!(vis.is_live(SourceId(2), 0), None);
    }

    #[test]
    fn packed_read_bits_cover_word_boundaries_and_copy_once_per_owner() {
        for len in [0_usize, 1, 63, 64, 65, 129] {
            let live: Vec<_> = (0..len).map(|row| row % 3 == 0).collect();
            let mut bits = SourceBits::from_live(&live);
            let snapshot = bits.clone();
            assert_eq!(bits.bits.len(), len.div_ceil(64));
            assert!(Arc::ptr_eq(&bits.bits, &snapshot.bits));
            for (row, expected) in live.iter().enumerate() {
                assert_eq!(bits.get(row), *expected);
            }
            if len > 0 {
                bits.set(0, false);
                assert!(!Arc::ptr_eq(&bits.bits, &snapshot.bits));
                let copied = Arc::as_ptr(&bits.bits);
                for (row, expected) in live.iter().enumerate() {
                    bits.set(row, !expected);
                    assert_eq!(Arc::as_ptr(&bits.bits), copied);
                    assert_eq!(snapshot.get(row), *expected);
                    assert_eq!(bits.get(row), !expected);
                }
            }
        }
    }

    #[test]
    fn packed_filter_count_and_iteration_preserve_tail_bits_and_original_candidates() {
        for len in [0_usize, 1, 63, 64, 65, 127, 129] {
            for divisor in [1, 3, 70] {
                let live = SourceBits::from_live(
                    &(0..len).map(|row| row % divisor == 0).collect::<Vec<_>>(),
                );
                let original: Vec<_> = (0..len).filter(|row| row % divisor == 0).collect();
                assert_eq!(live.live_rows().collect::<Vec<_>>(), original);
                assert_eq!(live.count_live(), original.len());
                let mut filtered = live.clone();
                filtered.intersect_matching(&live, |row| row % 2 == 0);
                let expected: Vec<_> = original
                    .iter()
                    .copied()
                    .filter(|row| row % 2 == 0)
                    .collect();
                assert_eq!(filtered.live_rows().collect::<Vec<_>>(), expected);
                assert_eq!(filtered.count_live(), expected.len());
                let visited = std::cell::RefCell::new(Vec::new());
                filtered.intersect_matching(&live, |row| {
                    visited.borrow_mut().push(row);
                    false
                });
                assert_eq!(*visited.borrow(), original);
                assert_eq!(filtered.count_live(), 0);
                assert_eq!(live.live_rows().collect::<Vec<_>>(), original);
            }
        }
    }

    #[test]
    fn dense_and_sparse_words_visit_original_live_rows_once_without_reading_padding() {
        for len in [63, 64, 65, 127, 128, 129, 255, 256, 257] {
            let values: Vec<_> = (0..len)
                .map(|row| match row / 64 % 3 {
                    0 => true,
                    1 => row % 2 == 0,
                    _ => false,
                })
                .collect();
            let live = SourceBits::from_live(&values);
            let expected: Vec<_> = (0..len).filter(|&row| values[row]).collect();
            let mut filtered = live.clone();
            for divisor in [3, 2, 1] {
                let visited = std::cell::RefCell::new(Vec::new());
                filtered.intersect_matching(&live, |row| {
                    assert!(row < len && values[row]);
                    visited.borrow_mut().push(row);
                    row % divisor == 0
                });
                assert_eq!(*visited.borrow(), expected);
                let matches: Vec<_> = expected
                    .iter()
                    .copied()
                    .filter(|row| row % 3 == 0 && (divisor == 3 || row % 2 == 0))
                    .collect();
                assert_eq!(filtered.live_rows().collect::<Vec<_>>(), matches);
                assert_eq!(filtered.count_live(), matches.len());
                assert_eq!(live.live_rows().collect::<Vec<_>>(), expected);
            }
        }
    }

    fn pk(id: i64) -> Pk {
        Pk::Int64(id)
    }

    #[test]
    fn newer_seq_and_tombstone_determine_visibility() {
        let mut vis = Visibility::default();
        vis.add_source(SourceId(1), 3).unwrap();
        vis.add_source(SourceId(2), 3).unwrap();
        vis.apply_row(pk(7), 1, SourceId(1), 0).unwrap();
        assert_eq!(vis.live_count(), 1);
        vis.apply_delete(pk(7), 3).unwrap();
        assert_eq!(vis.live_count(), 0);
        vis.apply_row(pk(7), 2, SourceId(1), 1).unwrap();
        assert_eq!(vis.live_count(), 0);
        vis.apply_row(pk(7), 4, SourceId(2), 0).unwrap();
        assert_eq!(vis.live_count(), 1);
        assert_eq!(vis.latest_data(&pk(7)).unwrap().seq, 4);
        assert_eq!(vis.latest_delete(&pk(7)), Some(3));
        assert_eq!(vis.is_live(SourceId(2), 0), Some(true));
    }

    #[test]
    fn equal_seq_duplicate_and_tie_keep_first_data_row() {
        let mut vis = Visibility::default();
        vis.add_source(SourceId(1), 2).unwrap();
        vis.apply_row(pk(1), 5, SourceId(1), 0).unwrap();
        vis.apply_delete(pk(1), 5).unwrap();
        vis.apply_row(pk(1), 5, SourceId(1), 1).unwrap();
        assert_eq!(vis.live_count(), 1);
        assert_eq!(vis.is_live(SourceId(1), 0), Some(true));
        assert_eq!(vis.is_live(SourceId(1), 1), Some(false));
    }

    #[test]
    fn equal_seq_chooses_first_physical_row_even_if_applied_later() {
        let mut vis = Visibility::default();
        vis.add_source(SourceId(10), 1).unwrap();
        vis.add_source(SourceId(2), 1).unwrap();
        vis.apply_row(pk(1), 5, SourceId(2), 0).unwrap();
        vis.apply_row(pk(1), 5, SourceId(10), 0).unwrap();
        assert_eq!(vis.is_live(SourceId(10), 0), Some(true));
        assert_eq!(vis.is_live(SourceId(2), 0), Some(false));
        assert_eq!(vis.live_count(), 1);
    }

    #[test]
    fn snapshot_is_unchanged_after_cow_update() {
        let mut vis = Visibility::default();
        vis.add_source(SourceId(1), 1).unwrap();
        vis.apply_row(pk(1), 1, SourceId(1), 0).unwrap();
        let before = vis.snapshot();
        vis.apply_delete(pk(1), 2).unwrap();
        assert_eq!(before.is_live(SourceId(1), 0), Some(true));
        assert_eq!(before.live_count, 1);
        assert_eq!(vis.is_live(SourceId(1), 0), Some(false));
        assert_eq!(vis.live_count(), 0);
    }

    #[test]
    fn extending_active_source_does_not_change_old_snapshot() {
        let mut vis = Visibility::default();
        vis.add_source(SourceId(1), 1).unwrap();
        vis.apply_row(pk(1), 1, SourceId(1), 0).unwrap();
        let before = vis.snapshot();
        vis.extend_source(SourceId(1), 2).unwrap();
        vis.apply_row(pk(2), 2, SourceId(1), 1).unwrap();
        assert_eq!(before.is_live(SourceId(1), 1), None);
        assert_eq!(vis.is_live(SourceId(1), 1), Some(true));
    }

    #[test]
    fn publishing_frozen_source_preserves_newer_updates_and_old_snapshot() {
        let mut vis = Visibility::default();
        vis.add_source(SourceId(1), 3).unwrap();
        vis.apply_row(pk(1), 1, SourceId(1), 0).unwrap();
        vis.apply_row(pk(2), 2, SourceId(1), 1).unwrap();
        let frozen_snapshot = vis.snapshot();
        vis.add_source(SourceId(2), 1).unwrap();
        vis.apply_row(pk(1), 3, SourceId(2), 0).unwrap();
        vis.replace_source_known_rows(
            SourceId(1),
            SourceId(3),
            2,
            &[Some(0), Some(1), None],
            &[(pk(1), 0), (pk(2), 1)],
        )
        .unwrap();
        assert_eq!(frozen_snapshot.is_live(SourceId(1), 0), Some(true));
        assert_eq!(vis.is_live(SourceId(3), 0), Some(false));
        assert_eq!(vis.is_live(SourceId(3), 1), Some(true));
        assert_eq!(vis.latest_data(&pk(1)).unwrap().source, SourceId(2));
        assert_eq!(vis.latest_data(&pk(2)).unwrap().source, SourceId(3));
        assert_eq!(vis.live_count(), 2);
        assert_eq!(vis.snapshot().source_order, [SourceId(3), SourceId(2)]);
    }

    #[test]
    fn randomized_seq_model_matches_every_bitmap() {
        let mut vis = Visibility::default();
        vis.add_source(SourceId(1), 300).unwrap();
        vis.add_source(SourceId(2), 300).unwrap();
        let mut rows: Vec<(SourceId, usize, i64, u64)> = Vec::new();
        let mut deletes: Vec<(i64, u64)> = Vec::new();
        let mut next_row = [0_usize; 2];
        let mut random = 0x70d6_a517_u64;
        for step in 0..500 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let id = (random % 17) as i64;
            let seq = 1 + ((random >> 9) % 73);
            if random & 7 == 0 {
                vis.apply_delete(pk(id), seq).unwrap();
                deletes.push((id, seq));
            } else {
                let index = ((random >> 5) & 1) as usize;
                let source = SourceId(index as u64 + 1);
                let row = next_row[index];
                next_row[index] += 1;
                vis.apply_row(pk(id), seq, source, row).unwrap();
                rows.push((source, row, id, seq));
            }
            if step % 10 != 0 {
                continue;
            }
            let mut expected_count = 0;
            for &(source, row, id, seq) in &rows {
                let best = rows
                    .iter()
                    .filter(|candidate| candidate.2 == id)
                    .max_by(|left, right| {
                        left.3.cmp(&right.3).then_with(|| {
                            // 对同 seq，物理位置更早者赢。
                            (right.0, right.1).cmp(&(left.0, left.1))
                        })
                    })
                    .unwrap();
                let deleted_at = deletes
                    .iter()
                    .filter(|candidate| candidate.0 == id)
                    .map(|candidate| candidate.1)
                    .max();
                let expected = best.0 == source
                    && best.1 == row
                    && best.3 == seq
                    && !deleted_at.is_some_and(|deleted| deleted > seq);
                assert_eq!(
                    vis.is_live(source, row),
                    Some(expected),
                    "step {step}, pk {id}"
                );
                expected_count += usize::from(expected);
            }
            assert_eq!(vis.live_count(), expected_count, "step {step}");
        }
    }
}
