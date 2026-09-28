//! 活跃/冻结代的列式 memtable：不可变 batch、主键位次与 tombstone 顺序。

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::RecordBatch;
use serde_json::{Map, Value};

use crate::data::{ColumnData, Pk};
use crate::schema::{CollectionSchema, DataType};
use crate::storage::arrow_data::{self, DecodedData};
use crate::storage::visibility::{SourceId, Visibility, VisibilitySnapshot};

#[derive(Debug, thiserror::Error)]
pub enum MemTableError {
    #[error("Arrow data error: {0}")]
    Arrow(#[from] arrow_data::ArrowDataError),
    #[error("visibility error: {0}")]
    Visibility(#[from] crate::storage::visibility::VisibilityError),
    #[error("memtable position space exhausted")]
    PositionExhausted,
    #[error("row {row} has no valid primary key")]
    PrimaryKey { row: usize },
    #[error("decoded data does not match field {0:?}")]
    Column(String),
    #[error("decoded data has missing or mismatched $meta rows")]
    Meta,
    #[error("frozen visibility has no row {row} in source {source_id:?}")]
    MissingFrozenRow { source_id: SourceId, row: usize },
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    batch: usize,
    row: usize,
    global_row: usize,
    seq: u64,
    position: u64,
}

#[derive(Debug, Clone, Copy)]
struct Tombstone {
    seq: u64,
    position: u64,
}

#[derive(Debug)]
pub struct Materialized {
    pub data: Option<RecordBatch>,
    pub delta: Option<RecordBatch>,
    /// 原 source 行号 → data Parquet 行号；发布时转换有效位图与主键引用。
    pub mapping: Vec<Option<usize>>,
}

#[derive(Debug)]
pub struct MemTable {
    pub source: SourceId,
    batches: Vec<Arc<DecodedData>>,
    physical_rows: usize,
    active: HashMap<Pk, Entry>,
    tombstones: HashMap<Pk, Tombstone>,
    next_position: u64,
    next_tombstone_position: u64,
    max_seq: u64,
}

/// 冻结时固定的行可见性；新一代写入只改变全局位图，不改变这一代写出的数据。
#[derive(Debug)]
pub struct FrozenMemTable {
    memtable: MemTable,
    live_at_freeze: VisibilitySnapshot,
}

impl FrozenMemTable {
    pub fn max_seq(&self) -> u64 {
        self.memtable.max_seq
    }

    pub fn source(&self) -> SourceId {
        self.memtable.source
    }

    pub fn materialize(&self, schema: &CollectionSchema) -> Result<Materialized, MemTableError> {
        self.memtable.materialize_with(schema, &self.live_at_freeze)
    }
}

impl MemTable {
    pub fn new(source: SourceId) -> Self {
        Self {
            source,
            batches: Vec::new(),
            physical_rows: 0,
            active: HashMap::new(),
            tombstones: HashMap::new(),
            next_position: 0,
            next_tombstone_position: 0,
            max_seq: 0,
        }
    }

    pub fn size(&self) -> usize {
        self.active.len() + self.tombstones.len()
    }

    pub fn physical_rows(&self) -> usize {
        self.physical_rows
    }

    pub fn max_seq(&self) -> u64 {
        self.max_seq
    }

    pub fn freeze(self, visibility: &Visibility) -> FrozenMemTable {
        FrozenMemTable {
            memtable: self,
            live_at_freeze: visibility.snapshot(),
        }
    }

    /// 打开时只为构建查询视图借用活跃代；写入状态仍留在 memtable 中。
    pub fn materialize(
        &self,
        schema: &CollectionSchema,
        visibility: &Visibility,
    ) -> Result<Materialized, MemTableError> {
        self.materialize_with(schema, &visibility.snapshot())
    }

    /// 调用方先追加 WAL；这里之后只更新内存。重复或旧 seq 的物理行不进入主键表。
    pub fn apply_data(
        &mut self,
        schema: &CollectionSchema,
        decoded: DecodedData,
        visibility: &mut Visibility,
    ) -> Result<(), MemTableError> {
        let n = decoded.seqs.len();
        validate_decoded(schema, &decoded)?;
        self.next_position
            .checked_add(u64::try_from(n).map_err(|_| MemTableError::PositionExhausted)?)
            .ok_or(MemTableError::PositionExhausted)?;
        let pks = (0..n)
            .map(|row| primary_key(schema, &decoded.columns, row))
            .collect::<Result<Vec<_>, _>>()?;
        let new_len = self
            .physical_rows
            .checked_add(n)
            .ok_or(MemTableError::PositionExhausted)?;
        visibility.extend_source(self.source, new_len)?;
        let batch_index = self.batches.len();
        for (row, pk) in pks.into_iter().enumerate() {
            let seq = decoded.seqs[row];
            let global_row = self.physical_rows + row;
            let old_seq = visibility.latest_data(&pk).map(|entry| entry.seq);
            let deleted_seq = visibility.latest_delete(&pk);
            // 同号的本地删除先到时必须拦截；已提交 delta 同号仍按跨源严格大于放行。
            let locally_deleted_at_same_seq = self
                .tombstones
                .get(&pk)
                .is_some_and(|tombstone| tombstone.seq >= seq);
            if old_seq.is_some_and(|old| old >= seq)
                || deleted_seq.is_some_and(|deleted| deleted > seq)
                || locally_deleted_at_same_seq
            {
                continue;
            }
            let position = if let Some(old) = self.active.get(&pk) {
                old.position
            } else {
                let position = self.next_position;
                self.next_position = self
                    .next_position
                    .checked_add(1)
                    .ok_or(MemTableError::PositionExhausted)?;
                position
            };
            self.tombstones.remove(&pk);
            visibility.apply_row(pk.clone(), seq, self.source, global_row)?;
            self.max_seq = self.max_seq.max(seq);
            self.active.insert(
                pk,
                Entry {
                    batch: batch_index,
                    row,
                    global_row,
                    seq,
                    position,
                },
            );
        }
        self.physical_rows = new_len;
        self.batches.push(Arc::new(decoded));
        Ok(())
    }

    pub fn apply_delete(
        &mut self,
        pks: &[Pk],
        seq: u64,
        visibility: &mut Visibility,
    ) -> Result<(), MemTableError> {
        if pks.is_empty() {
            return Ok(());
        }
        self.next_tombstone_position
            .checked_add(u64::try_from(pks.len()).map_err(|_| MemTableError::PositionExhausted)?)
            .ok_or(MemTableError::PositionExhausted)?;
        for pk in pks {
            if self.active.get(pk).is_some_and(|entry| entry.seq >= seq)
                || self
                    .tombstones
                    .get(pk)
                    .is_some_and(|tombstone| tombstone.seq >= seq)
            {
                continue;
            }
            let position = if let Some(old) = self.tombstones.get(pk) {
                old.position
            } else {
                let position = self.next_tombstone_position;
                self.next_tombstone_position = self
                    .next_tombstone_position
                    .checked_add(1)
                    .ok_or(MemTableError::PositionExhausted)?;
                position
            };
            self.active.remove(pk);
            visibility.apply_delete(pk.clone(), seq)?;
            self.max_seq = self.max_seq.max(seq);
            self.tombstones
                .insert(pk.clone(), Tombstone { seq, position });
        }
        Ok(())
    }

    /// 冻结时按 Python dict 的位次输出；冻结后发生的新覆盖只会改变发布时的位图。
    fn materialize_with(
        &self,
        schema: &CollectionSchema,
        live_at_freeze: &VisibilitySnapshot,
    ) -> Result<Materialized, MemTableError> {
        let mut active: Vec<_> = self.active.values().collect();
        active.sort_by_key(|entry| entry.position);
        let mut columns: Vec<ColumnData> = schema.fields.iter().map(ColumnData::empty).collect();
        let mut seqs = Vec::new();
        let mut meta: Option<Vec<Map<String, Value>>> = schema.enable_dynamic_field.then(Vec::new);
        let mut mapping = vec![None; self.physical_rows];
        for entry in active {
            match live_at_freeze.is_live(self.source, entry.global_row) {
                Some(false) => continue,
                Some(true) => {}
                None => {
                    return Err(MemTableError::MissingFrozenRow {
                        source_id: self.source,
                        row: entry.global_row,
                    });
                }
            }
            let batch = &self.batches[entry.batch];
            let new_row = seqs.len();
            seqs.push(entry.seq);
            mapping[entry.global_row] = Some(new_row);
            for (dst, src) in columns.iter_mut().zip(&batch.columns) {
                dst.push_row(src, entry.row);
            }
            if let Some(meta) = &mut meta {
                let value = batch
                    .meta
                    .as_ref()
                    .and_then(|all| all.get(entry.row))
                    .ok_or(MemTableError::Meta)?;
                meta.push(value.clone());
            }
        }
        let data = if seqs.is_empty() {
            None
        } else {
            Some(arrow_data::data_batch(
                schema,
                &seqs,
                &columns,
                meta.as_deref(),
            )?)
        };
        let mut tombstones: Vec<_> = self.tombstones.iter().collect();
        tombstones.sort_by_key(|(_, tombstone)| tombstone.position);
        let delta = if tombstones.is_empty() {
            None
        } else {
            let pks = tombstones
                .iter()
                .map(|(pk, _)| (*pk).clone())
                .collect::<Vec<_>>();
            let seqs = tombstones
                .iter()
                .map(|(_, tombstone)| tombstone.seq)
                .collect::<Vec<_>>();
            Some(arrow_data::delta_batch(schema, &pks, &seqs)?)
        };
        Ok(Materialized {
            data,
            delta,
            mapping,
        })
    }
}

fn primary_key(
    schema: &CollectionSchema,
    columns: &[ColumnData],
    row: usize,
) -> Result<Pk, MemTableError> {
    let pos = schema
        .primary_position()
        .ok_or(MemTableError::PrimaryKey { row })?;
    match columns.get(pos).ok_or(MemTableError::PrimaryKey { row })? {
        ColumnData::Int64(values) => values
            .get(row)
            .copied()
            .map(Pk::Int64)
            .ok_or(MemTableError::PrimaryKey { row }),
        ColumnData::VarChar(values) => values
            .get(row)
            .cloned()
            .map(Pk::VarChar)
            .ok_or(MemTableError::PrimaryKey { row }),
        _ => Err(MemTableError::PrimaryKey { row }),
    }
}

fn validate_decoded(schema: &CollectionSchema, decoded: &DecodedData) -> Result<(), MemTableError> {
    if decoded.columns.len() != schema.fields.len() {
        return Err(MemTableError::Column("field count".to_owned()));
    }
    let n = decoded.seqs.len();
    for (field, column) in schema.fields.iter().zip(&decoded.columns) {
        let matches_type = matches!(
            (field.data_type, column),
            (DataType::Bool, ColumnData::Bool(_))
                | (DataType::Int8, ColumnData::Int8(_))
                | (DataType::Int16, ColumnData::Int16(_))
                | (DataType::Int32, ColumnData::Int32(_))
                | (DataType::Int64, ColumnData::Int64(_))
                | (DataType::Float, ColumnData::Float(_))
                | (DataType::Double, ColumnData::Double(_))
                | (DataType::VarChar, ColumnData::VarChar(_))
                | (DataType::Json, ColumnData::Json(_))
                | (DataType::FloatVector, ColumnData::FloatVector { .. })
        );
        if !matches_type || column.len() != n {
            return Err(MemTableError::Column(field.name.clone()));
        }
        if let ColumnData::FloatVector { dim, .. } = column
            && field.dim != i64::try_from(*dim).ok()
        {
            return Err(MemTableError::Column(field.name.clone()));
        }
    }
    if schema.enable_dynamic_field != decoded.meta.is_some()
        || decoded.meta.as_ref().is_some_and(|meta| meta.len() != n)
    {
        return Err(MemTableError::Meta);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::schema::{DataType, FieldSchema};

    use super::*;

    fn schema() -> CollectionSchema {
        let mut pk = FieldSchema::new("id", DataType::Int64);
        pk.is_primary = true;
        let mut vector = FieldSchema::new("vector", DataType::FloatVector);
        vector.dim = Some(2);
        CollectionSchema {
            fields: vec![pk, vector],
            enable_dynamic_field: false,
            properties: Vec::new(),
        }
    }

    fn data(ids: &[i64], seqs: &[u64]) -> DecodedData {
        DecodedData {
            seqs: seqs.to_vec(),
            columns: vec![
                ColumnData::Int64(ids.to_vec()),
                ColumnData::FloatVector {
                    dim: 2,
                    values: [1.0, 2.0].repeat(ids.len()),
                },
            ],
            meta: None,
        }
    }

    #[test]
    fn overwrite_keeps_position_and_reinsert_moves_to_end() {
        let schema = schema();
        let mut vis = Visibility::default();
        vis.add_source(SourceId(1), 0).unwrap();
        let mut mem = MemTable::new(SourceId(1));
        mem.apply_data(&schema, data(&[1, 2, 3], &[1, 2, 3]), &mut vis)
            .unwrap();
        mem.apply_data(&schema, data(&[1], &[4]), &mut vis).unwrap();
        mem.apply_delete(&[Pk::Int64(2)], 5, &mut vis).unwrap();
        mem.apply_data(&schema, data(&[2], &[6]), &mut vis).unwrap();
        assert_eq!(mem.size(), 3);
        let materialized = mem.freeze(&vis).materialize(&schema).unwrap();
        let data = materialized.data.unwrap();
        let decoded = arrow_data::decode_data_batch(&schema, &data).unwrap();
        assert_eq!(decoded.columns[0], ColumnData::Int64(vec![1, 3, 2]));
        assert_eq!(decoded.seqs, [4, 3, 6]);
        assert!(materialized.delta.is_none());
    }

    #[test]
    fn delete_nonexistent_counts_tombstone_but_empty_delete_does_not() {
        let schema = schema();
        let mut vis = Visibility::default();
        vis.add_source(SourceId(1), 0).unwrap();
        let mut mem = MemTable::new(SourceId(1));
        mem.apply_delete(&[], 1, &mut vis).unwrap();
        assert_eq!(mem.size(), 0);
        mem.apply_delete(&[Pk::Int64(99)], 2, &mut vis).unwrap();
        assert_eq!(mem.size(), 1);
        let mat = mem.freeze(&vis).materialize(&schema).unwrap();
        let delta = mat.delta.unwrap();
        let decoded = arrow_data::decode_delta_batch(&schema, &delta).unwrap();
        assert_eq!(decoded.pks, [Pk::Int64(99)]);
        assert_eq!(decoded.seqs, [2]);
    }

    #[test]
    fn missing_freeze_source_is_an_error_not_an_empty_file() {
        let schema = schema();
        let mut vis = Visibility::default();
        vis.add_source(SourceId(1), 0).unwrap();
        let mut mem = MemTable::new(SourceId(1));
        mem.apply_data(&schema, data(&[1], &[1]), &mut vis).unwrap();
        let wrong = Visibility::default();
        assert!(matches!(
            mem.freeze(&wrong).materialize(&schema),
            Err(MemTableError::MissingFrozenRow {
                source_id: SourceId(1),
                row: 0
            })
        ));
    }

    #[test]
    fn equal_seq_in_one_memtable_preserves_first_operation() {
        let schema = schema();
        for delete_first in [false, true] {
            let mut visibility = Visibility::default();
            visibility.add_source(SourceId(1), 0).unwrap();
            let mut memtable = MemTable::new(SourceId(1));
            if delete_first {
                memtable
                    .apply_delete(&[Pk::Int64(1)], 7, &mut visibility)
                    .unwrap();
            }
            memtable
                .apply_data(&schema, data(&[1], &[7]), &mut visibility)
                .unwrap();
            if !delete_first {
                memtable
                    .apply_delete(&[Pk::Int64(1)], 7, &mut visibility)
                    .unwrap();
            }
            assert_eq!(visibility.live_count(), usize::from(!delete_first));
            assert_eq!(memtable.size(), 1);
        }
    }

    #[test]
    fn committed_delta_at_equal_seq_does_not_hide_replayed_data() {
        let schema = schema();
        let mut visibility = Visibility::default();
        visibility.add_source(SourceId(1), 0).unwrap();
        visibility.apply_delete(Pk::Int64(1), 7).unwrap();
        let mut memtable = MemTable::new(SourceId(1));
        memtable
            .apply_data(&schema, data(&[1], &[7]), &mut visibility)
            .unwrap();
        assert_eq!(visibility.live_count(), 1);
    }
}
