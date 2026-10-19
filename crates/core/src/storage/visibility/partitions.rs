//! 按选择集增量维护可见性；只保存主键位置，列与向量由引擎既有读块持有。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use super::{RowLocation, SourceBits, SourceId, VisibilitySnapshot};
use crate::data::{Pk, PkMap};
use crate::schema::DataType;

pub type PartitionId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Immutable(PartitionId),
    Active,
    Frozen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartitionRow {
    pub partition: PartitionId,
    pub location: RowLocation,
}

#[derive(Debug, Clone)]
struct Source {
    kind: SourceKind,
    rank: usize,
    candidates: usize,
    row_partitions: Option<Vec<PartitionId>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Winner {
    row: PartitionRow,
    rank: usize,
}

impl Winner {
    fn priority(self) -> (u64, std::cmp::Reverse<usize>, std::cmp::Reverse<usize>) {
        (
            self.row.location.seq,
            std::cmp::Reverse(self.rank),
            std::cmp::Reverse(self.row.location.row),
        )
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid partition visibility operation: {0}")]
pub struct PartitionVisibilityError(String);

type Result<T> = std::result::Result<T, PartitionVisibilityError>;
type SparseWords = BTreeMap<usize, u64>;

#[derive(Debug, Clone)]
struct LocalBits {
    bitmap: SourceBits,
    live: usize,
}

#[derive(Debug, Clone, Default)]
pub struct PartitionVisibility {
    pub(super) global: VisibilitySnapshot,
    sources: BTreeMap<SourceId, Source>,
    data: PkMap<RowLocation>,
    extra: PkMap<Vec<RowLocation>>,
    active: Option<SourceId>,
    pending_deletes: BTreeMap<SourceId, PkMap<u64>>,
    committed_deletes: HashMap<Pk, u64>,
    local: BTreeMap<(SourceId, PartitionId), LocalBits>,
    shadows: BTreeMap<(SourceId, PartitionId), Arc<SparseWords>>,
    stats: PartitionVisibilityStats,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PartitionVisibilityStats {
    pub keys: usize,
    pub relations: usize,
    pub shadow_words: usize,
    pub local_masks: usize,
    pub relation_edits: u64,
    pub version_comparisons: u64,
}

pub struct SourceReplacement {
    pub source: SourceId,
    pub rows: Vec<(Pk, usize, Option<RowLocation>)>,
}

#[derive(Clone, Copy)]
pub enum Removal {
    Replace,
    DropPartition(PartitionId),
}

impl PartitionVisibility {
    pub(super) fn check_active(&self, source: SourceId) -> Result<()> {
        if self.active == Some(source) {
            Ok(())
        } else {
            Err(error("source is not active"))
        }
    }
    pub(crate) fn with_reused_index(index: PkMap<RowLocation>) -> Self {
        Self {
            data: index,
            ..Self::default()
        }
    }

    fn locations<'a>(&'a self, pk: &'a Pk) -> impl Iterator<Item = RowLocation> + 'a {
        self.data
            .get(pk)
            .copied()
            .into_iter()
            .chain(self.extra.get(pk).into_iter().flatten().copied())
    }
    pub(super) fn source_partition(&self, id: SourceId) -> Option<PartitionId> {
        match self.sources.get(&id)?.kind {
            SourceKind::Immutable(part) => Some(part),
            _ => None,
        }
    }

    pub(super) fn source_row_seq(&self, pk: &Pk, source: SourceId, row: usize) -> Option<u64> {
        self.locations(pk)
            .find(|at| at.source == source && at.row == row)
            .map(|at| at.seq)
    }

    pub(super) fn latest_delete(&self, pk: &Pk) -> Option<u64> {
        self.deleted(pk)
    }

    pub(super) fn delete_snapshot(&self) -> HashMap<Pk, u64> {
        let mut result = self.committed_deletes.clone();
        for map in self.pending_deletes.values().cloned() {
            for (pk, seq) in map.into_entries() {
                result
                    .entry(pk)
                    .and_modify(|old| *old = (*old).max(seq))
                    .or_insert(seq);
            }
        }
        result
    }

    pub(crate) fn load_pending_delete(&mut self, source: SourceId, pk: Pk, seq: u64) -> Result<()> {
        if self
            .sources
            .get(&source)
            .is_none_or(|meta| matches!(meta.kind, SourceKind::Immutable(_)))
        {
            return Err(error("pending delete source missing"));
        }
        let old = self.data_rows(&pk);
        let old_delete = self.deleted(&pk);
        self.pending_deletes
            .entry(source)
            .or_default()
            .insert(pk.clone(), seq);
        self.update(&pk, old.clone(), old, old_delete)
    }

    pub(crate) fn load_candidate(&mut self, pk: Pk, row: PartitionRow) -> Result<()> {
        self.insert_impl(pk, row, true)
    }
    pub fn reserve(&mut self, kind: DataType, keys: usize) {
        self.data.reserve(kind, keys);
    }

    pub fn register_source(&mut self, id: SourceId, rows: usize, kind: SourceKind) -> Result<()> {
        if self.sources.contains_key(&id) {
            return Err(error("duplicate source"));
        }
        if kind == SourceKind::Active && self.active.is_some() {
            return Err(error("active source already exists"));
        }
        if kind == SourceKind::Active {
            self.active = Some(id);
        }
        self.sources.insert(
            id,
            Source {
                kind,
                rank: self.global.source_order.len(),
                candidates: 0,
                row_partitions: None,
            },
        );
        self.global.source_order.push(id);
        self.global.sources.insert(id, SourceBits::new(rows));
        Ok(())
    }

    pub fn extend_source(&mut self, id: SourceId, rows: usize) -> Result<()> {
        let bits = self
            .global
            .sources
            .get_mut(&id)
            .ok_or_else(|| error("unknown source"))?;
        if rows < bits.len {
            return Err(error("source cannot shrink"));
        }
        Arc::make_mut(&mut bits.bits).resize(rows.div_ceil(64), 0);
        bits.len = rows;
        if let Some(parts) = self
            .sources
            .get_mut(&id)
            .and_then(|source| source.row_partitions.as_mut())
        {
            parts.resize(rows, 0);
        }
        for (_, local) in self.local.range_mut((id, 0)..=(id, u32::MAX)) {
            Arc::make_mut(&mut local.bitmap.bits).resize(rows.div_ceil(64), 0);
            local.bitmap.len = rows;
        }
        Ok(())
    }

    fn check(&self, row: PartitionRow) -> Result<()> {
        let source = self
            .sources
            .get(&row.location.source)
            .ok_or_else(|| error("unknown source"))?;
        if row.location.row >= self.global.sources[&row.location.source].len {
            return Err(error("row out of range"));
        }
        if matches!(source.kind, SourceKind::Immutable(partition) if partition != row.partition) {
            return Err(error("source partition differs"));
        }
        Ok(())
    }

    fn winners(&self, rows: &[PartitionRow]) -> Vec<Winner> {
        let mut result = BTreeMap::<PartitionId, Winner>::new();
        for &row in rows {
            let winner = Winner {
                row,
                rank: self.sources[&row.location.source].rank,
            };
            if let Some(old) = result.get_mut(&row.partition) {
                if winner.priority() > old.priority() {
                    *old = winner;
                }
            } else {
                result.insert(row.partition, winner);
            }
        }
        result.into_values().collect()
    }

    fn deleted(&self, pk: &Pk) -> Option<u64> {
        self.committed_deletes
            .get(pk)
            .copied()
            .into_iter()
            .chain(
                self.pending_deletes
                    .values()
                    .filter_map(|map| map.get(pk).copied()),
            )
            .max()
    }

    fn data_rows(&self, pk: &Pk) -> Vec<PartitionRow> {
        self.locations(pk)
            .map(|location| PartitionRow {
                location,
                partition: self.row_partition(location),
            })
            .collect()
    }

    fn row_partition(&self, row: RowLocation) -> PartitionId {
        let source = &self.sources[&row.source];
        match source.kind {
            SourceKind::Immutable(partition) => partition,
            _ => source
                .row_partitions
                .as_ref()
                .map_or(0, |parts| parts[row.row]),
        }
    }

    fn shadow(&mut self, row: PartitionRow, other: PartitionId, set: bool) {
        let key = (row.location.source, other);
        let word = row.location.row / 64;
        let bit = 1 << (row.location.row % 64);
        if set {
            let words = self.shadows.entry(key).or_default();
            *Arc::make_mut(words).entry(word).or_default() |= bit;
        } else if let Some(words) = self.shadows.get_mut(&key) {
            let words = Arc::make_mut(words);
            if let Some(value) = words.get_mut(&word) {
                *value &= !bit;
                if *value == 0 {
                    words.remove(&word);
                }
            }
            if words.is_empty() {
                self.shadows.remove(&key);
            }
        }
        self.stats.relation_edits += 1;
    }

    fn local_bit(&mut self, row: PartitionRow, live: bool) {
        let key = (row.location.source, row.partition);
        if live {
            let len = self.global.sources[&row.location.source].len;
            self.local.entry(key).or_insert_with(|| LocalBits {
                bitmap: SourceBits::new(len),
                live: 0,
            });
            if let Some(local) = self.local.get_mut(&key)
                && !local.bitmap.get(row.location.row)
            {
                local.bitmap.set(row.location.row, true);
                local.live += 1;
            }
        } else if let Some(bits) = self.local.get_mut(&key)
            && bits.bitmap.get(row.location.row)
        {
            if bits.live == 1 {
                self.local.remove(&key);
            } else {
                bits.bitmap.set(row.location.row, false);
                bits.live -= 1;
            }
        }
    }

    fn global_bit(&mut self, row: PartitionRow, live: bool) -> Result<()> {
        let bits = self
            .global
            .sources
            .get_mut(&row.location.source)
            .ok_or_else(|| error("global source missing"))?;
        let old = bits.get(row.location.row);
        if old != live {
            bits.set(row.location.row, live);
            if live {
                self.global.live_count += 1;
            } else {
                self.global.live_count -= 1;
            }
        }
        Ok(())
    }

    fn update(
        &mut self,
        pk: &Pk,
        old: Vec<PartitionRow>,
        new: Vec<PartitionRow>,
        old_delete: Option<u64>,
    ) -> Result<()> {
        let old_winners = self.winners(&old);
        self.publish_update(pk, old, new, old_winners, old_delete)
    }

    fn publish_update(
        &mut self,
        pk: &Pk,
        old: Vec<PartitionRow>,
        new: Vec<PartitionRow>,
        old_winners: Vec<Winner>,
        old_delete: Option<u64>,
    ) -> Result<()> {
        let new_winners = self.winners(&new);
        let new_delete = self.deleted(pk);
        let mut changed: Vec<_> = old_winners
            .iter()
            .chain(&new_winners)
            .map(|w| w.row.partition)
            .collect();
        changed.sort_unstable();
        changed.dedup();
        changed.retain(|part| {
            old_winners
                .binary_search_by_key(part, |w| w.row.partition)
                .ok()
                .map(|i| old_winners[i].row)
                != new_winners
                    .binary_search_by_key(part, |w| w.row.partition)
                    .ok()
                    .map(|i| new_winners[i].row)
        });
        for old in &old_winners {
            if changed.contains(&old.row.partition) {
                self.local_bit(old.row, false);
                for other in &old_winners {
                    if other.row.partition != old.row.partition {
                        self.shadow(old.row, other.row.partition, false);
                    }
                }
            } else {
                for &part in &changed {
                    self.shadow(old.row, part, false);
                }
            }
        }
        for new in &new_winners {
            if changed.contains(&new.row.partition) || old_delete != new_delete {
                self.local_bit(
                    new.row,
                    new_delete.is_none_or(|seq| seq <= new.row.location.seq),
                );
            }
        }
        for (i, new) in new_winners.iter().enumerate() {
            if !changed.contains(&new.row.partition) {
                continue;
            }
            for (j, other) in new_winners.iter().enumerate() {
                if i == j || (j < i && changed.contains(&other.row.partition)) {
                    continue;
                }
                self.stats.version_comparisons += 1;
                if new.priority() > other.priority() {
                    self.shadow(other.row, new.row.partition, true);
                } else {
                    self.shadow(new.row, other.row.partition, true);
                }
            }
        }
        let old_live = old_winners
            .iter()
            .max_by_key(|w| w.priority())
            .filter(|w| old_delete.is_none_or(|seq| seq <= w.row.location.seq))
            .map(|w| w.row);
        let new_live = new_winners
            .iter()
            .max_by_key(|w| w.priority())
            .filter(|w| new_delete.is_none_or(|seq| seq <= w.row.location.seq))
            .map(|w| w.row);
        if old_live != new_live {
            if let Some(old) = old_live {
                self.global_bit(old, false)?;
            }
            if let Some(new) = new_live {
                self.global_bit(new, true)?;
            }
        }
        for row in &old {
            self.sources
                .get_mut(&row.location.source)
                .ok_or_else(|| error("candidate source missing"))?
                .candidates -= 1;
        }
        for row in &new {
            self.sources
                .get_mut(&row.location.source)
                .ok_or_else(|| error("candidate source missing"))?
                .candidates += 1;
        }
        if let Some(winner) = new_winners.iter().max_by_key(|winner| winner.priority()) {
            self.data.insert(pk.clone(), winner.row.location);
            let extra: Vec<_> = new
                .into_iter()
                .map(|row| row.location)
                .filter(|row| *row != winner.row.location)
                .collect();
            if extra.is_empty() {
                self.extra.remove(pk);
            } else {
                self.extra.insert(pk.clone(), extra);
            }
        } else {
            self.data.remove(pk);
            self.extra.remove(pk);
        }
        Ok(())
    }

    pub fn insert(&mut self, pk: Pk, row: PartitionRow) -> Result<()> {
        self.insert_impl(pk, row, false)
    }

    fn insert_impl(&mut self, pk: Pk, row: PartitionRow, loading: bool) -> Result<()> {
        self.check(row)?;
        if !loading && self.sources[&row.location.source].kind == SourceKind::Frozen {
            return Err(error("cannot append to frozen source"));
        }
        let old = self.data_rows(&pk);
        let active = self.sources[&row.location.source].kind == SourceKind::Active;
        if active
            && self
                .pending_deletes
                .get(&row.location.source)
                .and_then(|map| map.get(&pk))
                .is_some_and(|seq| *seq >= row.location.seq)
        {
            return Ok(());
        }
        if old
            .iter()
            .find(|old| old.location.source == row.location.source)
            .is_some_and(|old| {
                old.location.seq > row.location.seq
                    || (old.location.seq == row.location.seq
                        && old.location.row <= row.location.row)
            })
        {
            return Ok(());
        }
        let old_delete = self.deleted(&pk);
        let mut new = old.clone();
        new.retain(|old| old.location.source != row.location.source);
        new.push(row);
        let len = self.global.sources[&row.location.source].len;
        let source = self
            .sources
            .get_mut(&row.location.source)
            .ok_or_else(|| error("source missing"))?;
        if !matches!(source.kind, SourceKind::Immutable(_))
            && (row.partition != 0 || source.row_partitions.is_some())
        {
            source.row_partitions.get_or_insert_with(|| vec![0; len])[row.location.row] =
                row.partition;
        }
        if active && let Some(deletes) = self.pending_deletes.get_mut(&row.location.source) {
            deletes.remove(&pk);
        }
        self.update(&pk, old, new, old_delete)?;
        Ok(())
    }

    pub fn delete(&mut self, source: SourceId, pk: Pk, seq: u64) -> Result<()> {
        if self.active != Some(source) {
            return Err(error("delete source is not active"));
        }
        let old = self.data_rows(&pk);
        if old
            .iter()
            .any(|row| row.location.source == source && row.location.seq >= seq)
            || self
                .pending_deletes
                .get(&source)
                .and_then(|map| map.get(&pk))
                .is_some_and(|old| *old >= seq)
        {
            return Ok(());
        }
        let old_delete = self.deleted(&pk);
        self.pending_deletes
            .entry(source)
            .or_default()
            .insert(pk.clone(), seq);
        let new = old
            .iter()
            .filter(|row| row.location.source != source)
            .copied()
            .collect();
        self.update(&pk, old, new, old_delete)?;
        Ok(())
    }

    pub fn committed_delete(&mut self, pk: Pk, seq: u64) -> Result<()> {
        if self
            .committed_deletes
            .get(&pk)
            .is_some_and(|old| *old >= seq)
        {
            return Ok(());
        }
        let old = self.data_rows(&pk);
        let old_delete = self.deleted(&pk);
        self.committed_deletes.insert(pk.clone(), seq);
        self.update(&pk, old.clone(), old, old_delete)
    }

    pub fn freeze(&mut self, source: SourceId) -> Result<()> {
        if self.active != Some(source) {
            return Err(error("freeze source is not active"));
        }
        self.sources
            .get_mut(&source)
            .ok_or_else(|| error("unknown source"))?
            .kind = SourceKind::Frozen;
        self.active = None;
        Ok(())
    }

    pub fn commit_deletes(&mut self, source: SourceId) -> Result<()> {
        if self
            .sources
            .get(&source)
            .is_none_or(|meta| meta.kind != SourceKind::Frozen)
        {
            return Err(error("commit source is not frozen"));
        }
        if let Some(deletes) = self.pending_deletes.remove(&source) {
            for (pk, seq) in deletes.into_entries() {
                self.committed_deletes
                    .entry(pk)
                    .and_modify(|old| *old = (*old).max(seq))
                    .or_insert(seq);
            }
        }
        Ok(())
    }

    /// 物理行映射由持有原列的发布者提供；候选计数确保隐身的分区版本也没有遗漏。
    pub fn replace_sources(
        &mut self,
        inputs: &[SourceReplacement],
        outputs: &[(SourceId, usize, SourceKind)],
        order: &[SourceId],
        removal: Removal,
    ) -> Result<()> {
        let removed: HashSet<_> = inputs.iter().map(|input| input.source).collect();
        if removed.len() != inputs.len() {
            return Err(error("duplicate replacement source"));
        }
        let targets: BTreeMap<_, _> = outputs
            .iter()
            .map(|&(id, rows, kind)| (id, (rows, kind)))
            .collect();
        if targets.len() != outputs.len()
            || targets.keys().any(|id| self.sources.contains_key(id))
            || targets
                .values()
                .any(|(_, kind)| !matches!(kind, SourceKind::Immutable(_)))
        {
            return Err(error("invalid replacement target source"));
        }
        let expected: HashSet<_> = self
            .sources
            .keys()
            .filter(|id| !removed.contains(id))
            .chain(targets.keys())
            .copied()
            .collect();
        if order.len() != expected.len()
            || order.iter().copied().collect::<HashSet<_>>() != expected
        {
            return Err(error("incomplete replacement order"));
        }
        let old_remaining: Vec<_> = self
            .global
            .source_order
            .iter()
            .filter(|id| !removed.contains(id))
            .collect();
        let new_remaining: Vec<_> = order
            .iter()
            .filter(|id| !targets.contains_key(id))
            .collect();
        if old_remaining != new_remaining {
            return Err(error("replacement reorders unchanged sources"));
        }
        let mut mapping = BTreeMap::new();
        let mut used = HashSet::new();
        let mut affected = HashSet::new();
        for input in inputs {
            let meta = self
                .sources
                .get(&input.source)
                .ok_or_else(|| error("unknown replacement source"))?;
            if meta.kind == SourceKind::Active || self.pending_deletes.contains_key(&input.source) {
                return Err(error("replacement source has uncommitted state"));
            }
            if matches!(removal, Removal::DropPartition(part) if meta.kind != SourceKind::Immutable(part))
            {
                return Err(error("drop source partition differs"));
            }
            let len = self.global.sources[&input.source].len;
            if input.rows.len() != len {
                return Err(error("incomplete physical mapping"));
            }
            let mut candidates = 0;
            for (pk, row, target) in &input.rows {
                if *row >= len || mapping.insert((input.source, *row), *target).is_some() {
                    return Err(error("duplicate or invalid input row"));
                }
                if let Some(target) = target {
                    let (rows, _) = targets
                        .get(&target.source)
                        .ok_or_else(|| error("unknown mapped target"))?;
                    if target.row >= *rows || !used.insert((target.source, target.row)) {
                        return Err(error("invalid mapped target"));
                    }
                }
                if let Some(candidate) = self
                    .locations(pk)
                    .find(|candidate| candidate.source == input.source && candidate.row == *row)
                {
                    let candidate = PartitionRow {
                        location: candidate,
                        partition: self.row_partition(candidate),
                    };
                    candidates += 1;
                    affected.insert(pk.clone());
                    if let Some(target) = target {
                        if target.seq != candidate.location.seq
                            || targets[&target.source].1
                                != SourceKind::Immutable(candidate.partition)
                        {
                            return Err(error("replacement changes row identity"));
                        }
                    } else if matches!(removal, Removal::Replace)
                        && self
                            .winners(&self.data_rows(pk))
                            .iter()
                            .any(|winner| winner.row == candidate)
                        && self
                            .committed_deletes
                            .get(pk)
                            .is_none_or(|seq| *seq <= candidate.location.seq)
                    {
                        return Err(error("replacement omits an undeleted partition winner"));
                    }
                }
            }
            if candidates != meta.candidates {
                return Err(error("replacement misses candidate primary keys"));
            }
        }
        if used.len() != outputs.iter().map(|(_, n, _)| *n).sum::<usize>() {
            return Err(error("replacement target rows are incomplete"));
        }
        let before: Vec<_> = affected
            .into_iter()
            .map(|pk| {
                let rows = self.data_rows(&pk);
                let winners = self.winners(&rows);
                let delete = self.deleted(&pk);
                (pk, rows, winners, delete)
            })
            .collect();
        for &(id, rows, kind) in outputs {
            self.register_source(id, rows, kind)?;
        }
        for (rank, id) in order.iter().enumerate() {
            self.sources
                .get_mut(id)
                .ok_or_else(|| error("replacement source missing"))?
                .rank = rank;
        }
        for (pk, old, winners, delete) in before {
            let mut new = Vec::new();
            for candidate in &old {
                if removed.contains(&candidate.location.source) {
                    if let Some(location) = mapping
                        .get(&(candidate.location.source, candidate.location.row))
                        .copied()
                        .flatten()
                    {
                        new.push(PartitionRow {
                            location,
                            partition: candidate.partition,
                        });
                    }
                } else {
                    new.push(*candidate);
                }
            }
            self.publish_update(&pk, old, new, winners, delete)?;
        }
        for id in removed {
            self.global.sources.remove(&id);
            self.sources.remove(&id);
            self.local.retain(|(source, _), _| *source != id);
            self.shadows.retain(|(source, _), _| *source != id);
        }
        self.global.source_order = order.to_vec();
        Ok(())
    }

    pub fn gc_committed_before(&mut self, minimum: Option<u64>) -> Result<()> {
        let removed: Vec<_> = self
            .committed_deletes
            .iter()
            .filter(|(_, seq)| minimum.is_none_or(|min| **seq < min))
            .map(|(pk, seq)| (pk.clone(), *seq))
            .collect();
        for (pk, seq) in &removed {
            if self.locations(pk).any(|row| row.seq < *seq) {
                return Err(error("delete GC could revive a physical candidate"));
            }
        }
        for (pk, _) in removed {
            let old = self.data_rows(&pk);
            let old_delete = self.deleted(&pk);
            self.committed_deletes.remove(&pk);
            self.update(&pk, old.clone(), old, old_delete)?;
        }
        Ok(())
    }

    pub fn latest_data(&self, pk: &Pk) -> Option<RowLocation> {
        self.data.get(pk).copied()
    }

    pub fn visible_data(&self, pk: &Pk, selected: Option<&[PartitionId]>) -> Option<RowLocation> {
        let winner = self
            .winners(&self.data_rows(pk))
            .into_iter()
            .filter(|winner| selected.is_none_or(|parts| parts.contains(&winner.row.partition)))
            .max_by_key(|winner| winner.priority())?;
        self.deleted(pk)
            .is_none_or(|seq| seq <= winner.row.location.seq)
            .then_some(winner.row.location)
    }

    pub fn snapshot(&self, selected: Option<&[PartitionId]>) -> VisibilitySnapshot {
        let Some(selected) = selected else {
            return self.global.clone();
        };
        if selected.is_empty() {
            return VisibilitySnapshot {
                sources: BTreeMap::new(),
                empty_sources: self
                    .global
                    .sources
                    .iter()
                    .map(|(id, bits)| (*id, bits.len))
                    .collect(),
                source_order: self.global.source_order.clone(),
                live_count: 0,
            };
        }
        let mut result = VisibilitySnapshot {
            sources: BTreeMap::new(),
            empty_sources: BTreeMap::new(),
            source_order: self.global.source_order.clone(),
            live_count: 0,
        };
        for &source in &self.global.source_order {
            if selected.len() == 1 {
                if let Some(bits) = self.local.get(&(source, selected[0])) {
                    result.live_count += bits.live;
                    result.sources.insert(source, bits.bitmap.clone());
                } else {
                    result
                        .empty_sources
                        .insert(source, self.global.sources[&source].len);
                }
                continue;
            }
            let mut bits = SourceBits::new(self.global.sources[&source].len);
            for ((_, part), local) in self.local.range((source, 0)..=(source, u32::MAX)) {
                if selected.contains(part) {
                    for (word, local) in Arc::make_mut(&mut bits.bits)
                        .iter_mut()
                        .zip(local.bitmap.bits.iter())
                    {
                        *word |= local;
                    }
                }
            }
            for ((_, part), shadow) in self.shadows.range((source, 0)..=(source, u32::MAX)) {
                if selected.contains(part) {
                    for (&word, &shadow) in shadow.iter() {
                        Arc::make_mut(&mut bits.bits)[word] &= !shadow;
                    }
                }
            }
            result.live_count += bits.count_live();
            result.sources.insert(source, bits);
        }
        result
    }

    pub fn stats(&self) -> PartitionVisibilityStats {
        PartitionVisibilityStats {
            keys: self.data.len(),
            relations: self.shadows.len(),
            shadow_words: self.shadows.values().map(|words| words.len()).sum(),
            local_masks: self.local.len(),
            ..self.stats
        }
    }
}

fn error(message: &str) -> PartitionVisibilityError {
    PartitionVisibilityError(message.to_owned())
}
