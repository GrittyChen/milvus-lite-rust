//! 跨 segment / memtable 的按主键有效位图；查询只读位，不做逐行版本判定。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::data::Pk;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowLocation {
    pub source: SourceId,
    pub row: usize,
    pub seq: u64,
}

#[derive(Debug, Clone)]
struct SourceBits {
    len: usize,
    bits: Arc<Vec<u64>>,
}

impl SourceBits {
    fn new(len: usize) -> Self {
        Self {
            len,
            bits: Arc::new(vec![0; len.div_ceil(64)]),
        }
    }

    fn get(&self, row: usize) -> bool {
        self.bits[row / 64] & (1_u64 << (row % 64)) != 0
    }

    fn set(&mut self, row: usize, live: bool) {
        let word = &mut Arc::make_mut(&mut self.bits)[row / 64];
        let mask = 1_u64 << (row % 64);
        if live {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }
}

#[derive(Debug, Clone)]
pub struct VisibilitySnapshot {
    sources: BTreeMap<SourceId, SourceBits>,
    pub source_order: Vec<SourceId>,
    pub live_count: usize,
}

impl VisibilitySnapshot {
    pub fn is_live(&self, source: SourceId, row: usize) -> Option<bool> {
        let bits = self.sources.get(&source)?;
        (row < bits.len).then(|| bits.get(row))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VisibilityError {
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
    sources: BTreeMap<SourceId, SourceBits>,
    source_order: Vec<SourceId>,
    /// 最新的数据行；即使已被 tombstone 隐藏，仍保留它的 `_seq` 以处理乱序回放。
    latest_data: HashMap<Pk, RowLocation>,
    /// 已提交或活跃的删除中的最大 `_seq`。
    latest_delete: HashMap<Pk, u64>,
    live_count: usize,
}

impl Visibility {
    pub fn add_source(&mut self, id: SourceId, rows: usize) -> Result<(), VisibilityError> {
        if self.sources.contains_key(&id) {
            return Err(VisibilityError::DuplicateSource(id));
        }
        self.sources.insert(id, SourceBits::new(rows));
        self.source_order.push(id);
        Ok(())
    }

    pub fn extend_source(&mut self, id: SourceId, new_len: usize) -> Result<(), VisibilityError> {
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

    pub fn snapshot(&self) -> VisibilitySnapshot {
        VisibilitySnapshot {
            sources: self.sources.clone(),
            source_order: self.source_order.clone(),
            live_count: self.live_count,
        }
    }

    pub fn latest_data(&self, pk: &Pk) -> Option<RowLocation> {
        self.latest_data.get(pk).copied()
    }

    pub fn latest_delete(&self, pk: &Pk) -> Option<u64> {
        self.latest_delete.get(pk).copied()
    }

    pub fn is_live(&self, source: SourceId, row: usize) -> Option<bool> {
        let bits = self.sources.get(&source)?;
        (row < bits.len).then(|| bits.get(row))
    }

    pub fn live_count(&self) -> usize {
        self.live_count
    }

    /// 冻结 memtable 变成已提交 segment 时，在状态写锁内一次性换源；旧快照继续持旧位图。
    pub fn replace_source(
        &mut self,
        old: SourceId,
        new: SourceId,
        new_rows: usize,
        mapping: &[Option<usize>],
    ) -> Result<(), VisibilityError> {
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
        self.latest_data.retain(|_, location| {
            if location.source != old {
                return true;
            }
            if let Some(target) = mapping[location.row] {
                location.source = new;
                location.row = target;
                true
            } else {
                false
            }
        });
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
        vis.replace_source(SourceId(1), SourceId(3), 2, &[Some(0), Some(1), None])
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
