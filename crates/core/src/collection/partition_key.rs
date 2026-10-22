//! key 写入：请求前基线合并 → 全批校验 → 全桶编码 → 碰撞预检 → 逐桶追加。

use super::*;
use crate::partition_key::{BUCKETS, bucket, name};

#[cfg(all(test, feature = "fault-injection"))]
thread_local! {
    static EXIT_AFTER_ALLOCATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static EXIT_AFTER_APPLIED_BUCKET: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Collection 的非持久化入口也复用 A 的选择集组件；附加磁盘状态时立即释放。
#[derive(Debug)]
pub(super) struct MemoryKey {
    pub(super) registry: crate::storage::partition_paths::Registry,
    pub(super) visibility: crate::storage::visibility::Visibility,
}

impl MemoryKey {
    pub(super) fn new(schema: &CollectionSchema) -> Result<Self> {
        let mut manifest = crate::storage::manifest::Manifest::default();
        crate::partition_key::register(schema, &mut manifest);
        let registry = crate::storage::partition_paths::Registry::from_manifest(&manifest)
            .map_err(EngineError::Storage)?;
        let mut visibility = crate::storage::visibility::Visibility::new_partitioned();
        visibility
            .add_source(SourceId(1), 0)
            .map_err(|e| EngineError::Storage(e.to_string()))?;
        Ok(Self {
            registry,
            visibility,
        })
    }
}

struct MergedRow {
    fields: Vec<Option<InputValues>>,
    meta: Map<String, Value>,
}

struct PreparedBucket {
    rows: Vec<usize>,
    seqs: Vec<u64>,
    columns: Vec<ColumnData>,
    meta: Option<Vec<Map<String, Value>>>,
    norms: Vec<f32>,
    wal: Option<arrow_array::RecordBatch>,
    decoded: Option<DecodedData>,
    partition: u32,
}

impl Collection {
    pub(super) fn insert_key_batch(&mut self, batch: InsertBatch, upsert: bool) -> Result<Vec<Pk>> {
        if self.deleted {
            return Err(EngineError::CollectionNotFound(self.name.clone()));
        }
        let n = batch.num_rows;
        if n == 0 {
            return Ok(Vec::new());
        }
        for (field, values) in &batch.columns {
            check_column_len(field, values, n)?;
        }
        let pk_field = &self.schema.fields[self.pk_pos];
        if !upsert {
            let has = |name: &str| batch.columns.iter().any(|(field, _)| field == name);
            if !pk_field.auto_id && pk_field.default_value.is_none() && !has(&pk_field.name) {
                return Err(EngineError::schema(format!(
                    "primary key {} missing or None",
                    py_repr(&pk_field.name)
                )));
            }
            let vector = &self.schema.fields[self.vector_pos];
            if !has(&vector.name) {
                return Err(EngineError::schema(format!(
                    "vector field {} missing or None",
                    py_repr(&vector.name)
                )));
            }
        }
        let dynamic = self.decode_dynamic(&batch);
        let mut supplied = HashMap::new();
        let mut checks = HashMap::<&str, Vec<&InputValues>>::new();
        for (field, values) in &batch.columns {
            if field != META_FIELD
                && self.schema.field(field).is_some()
                && let Some(previous) = supplied.insert(field.as_str(), values)
            {
                checks.entry(field.as_str()).or_default().push(previous);
            }
        }
        retain_masked_json(&supplied, &mut checks);
        let mut merged = Vec::with_capacity(if upsert { n } else { 0 });
        // P1：仅访问请求前的主键表。不得把同批上一行放入这里的旧值候选。
        for row in 0..n {
            if upsert {
                let old = supplied
                    .get(pk_field.name.as_str())
                    .and_then(|input| input_pk(input, row))
                    .and_then(|pk| self.pk_index.get(&pk));
                let fields: Vec<_> = self
                    .schema
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(i, field)| {
                        supplied
                            .get(field.name.as_str())
                            .map(|input| input_row(input, row))
                            .or_else(|| {
                                old.map(|at| {
                                    input_from_column(&self.chunks[&at.chunk].columns[i], at.row)
                                })
                            })
                    })
                    .collect();
                let mut meta = old
                    .and_then(|at| {
                        self.chunks[&at.chunk]
                            .meta
                            .as_ref()
                            .map(|m| m[at.row].clone())
                    })
                    .unwrap_or_default();
                meta.extend(dynamic.maps[row].clone());
                let by_name = self
                    .schema
                    .fields
                    .iter()
                    .zip(&fields)
                    .filter_map(|(f, v)| v.as_ref().map(|v| (f.name.as_str(), v)))
                    .collect();
                let row_dynamic = Dynamic {
                    maps: vec![meta.clone()],
                    errors: vec![dynamic.errors[row].clone()],
                };
                self.validate_row(&by_name, &row_dynamic, 0, &checks, row)?;
                merged.push(MergedRow { fields, meta });
            } else {
                self.validate_row(&supplied, &dynamic, row, &checks, row)?;
            }
        }
        let field_input = |row: usize, field: usize| -> Option<(&InputValues, usize)> {
            if upsert {
                merged[row].fields[field].as_ref().map(|v| (v, 0))
            } else {
                supplied
                    .get(self.schema.fields[field].name.as_str())
                    .map(|v| (*v, row))
            }
        };
        let key_pos = self
            .schema
            .fields
            .iter()
            .position(|f| f.is_partition_key)
            .ok_or_else(|| EngineError::Storage("partition key missing".into()))?;
        let key = &self.schema.fields[key_pos];
        let generated: Vec<_> = (0..n)
            .map(|row| {
                pk_field.auto_id
                    && field_input(row, self.pk_pos).is_none_or(|(v, i)| input_is_null(v, i))
            })
            .collect();
        let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
        let mut positions = [None; BUCKETS];
        for row in 0..n {
            let input = field_input(row, key_pos);
            let (value, index) =
                resolved_input(key, input.map(|(v, _)| v), input.map_or(0, |(_, i)| i))?
                    .ok_or_else(|| {
                        EngineError::Storage("validated partition key lost field".into())
                    })?;
            let text = if input_is_null(&value, index) {
                "None".to_owned()
            } else {
                match input_pk(&value, index) {
                    Some(Pk::Int64(v)) => v.to_string(),
                    Some(Pk::VarChar(v)) => v,
                    None => {
                        return Err(EngineError::Storage(
                            "validated partition key lost type".into(),
                        ));
                    }
                }
            };
            let b = bucket(&text);
            let at = *positions[b].get_or_insert_with(|| {
                groups.push((b, Vec::new()));
                groups.len() - 1
            });
            groups[at].1.push(row);
        }
        // P2：序号仅为局部计划；所有桶（包括 WAL Arrow batch）完成后才改 next_seq。
        let next_seq = self
            .next_seq
            .checked_add(n as u64)
            .ok_or_else(|| EngineError::invalid("_seq space exhausted"))?;
        let mut sequence = self.next_seq;
        let mut prepared = Vec::with_capacity(groups.len());
        for (bucket, rows) in groups {
            let seqs: Vec<_> = (sequence..sequence + rows.len() as u64).collect();
            sequence += rows.len() as u64;
            let mut columns = Vec::with_capacity(self.schema.fields.len());
            for (i, field) in self.schema.fields.iter().enumerate() {
                let column = if field.data_type == DataType::Array {
                    let mut arrays = Vec::with_capacity(rows.len());
                    for &row in &rows {
                        let input = field_input(row, i);
                        let (v, index) = resolved_input(
                            field,
                            input.map(|(v, _)| v),
                            input.map_or(0, |(_, r)| r),
                        )?
                        .ok_or_else(|| EngineError::Storage("validated ARRAY lost field".into()))?;
                        arrays.push(
                            crate::data::array::row_input(&v, index)?
                                .map(std::borrow::Cow::into_owned),
                        );
                    }
                    crate::data::array::convert(field, &InputValues::Array(arrays))?
                } else {
                    let mut column = ColumnData::empty(field);
                    for (local, &row) in rows.iter().enumerate() {
                        let input = field_input(row, i);
                        let value = if i == self.pk_pos && generated[row] {
                            ColumnData::Int64(vec![
                                i64::try_from(seqs[local])
                                    .map_err(|_| EngineError::invalid("auto_id space exhausted"))?,
                            ])
                        } else {
                            let single = input.map(|(v, index)| input_row(v, index));
                            convert_optional(field, single.as_ref(), 1)?
                        };
                        column.push_row(&value, 0);
                    }
                    column
                };
                columns.push(column);
            }
            let mut normalized = None;
            let column_values = if self.persistent.is_some() {
                normalized
                    .insert(
                        crate::storage::arrow_data::normalize_data_json(&self.schema, &mut columns)
                            .map_err(|error| {
                                match error {
                        crate::storage::arrow_data::ArrowDataError::NonFiniteJsonNumber {
                            ..
                        } => EngineError::InvalidArgument(error.to_string()),
                        _ => EngineError::Storage(error.to_string()),
                    }
                            })?,
                    )
                    .columns()
            } else {
                &columns
            };
            let meta = self.schema.enable_dynamic_field.then(|| {
                rows.iter()
                    .map(|&row| {
                        if upsert {
                            merged[row].meta.clone()
                        } else {
                            dynamic.maps[row].clone()
                        }
                    })
                    .collect::<Vec<_>>()
            });
            let (dim, vectors) = vector_values(&column_values[self.vector_pos]);
            let norms = vectors.chunks_exact(dim).map(flat::norm).collect();
            let partition = self.resolve_write_partition(&name(bucket))?;
            let (wal, decoded) = if let Some(normalized) = normalized {
                let wal = normalized
                    .wal_batch(&seqs, &name(bucket), meta.as_deref())
                    .map_err(|e| EngineError::Storage(e.to_string()))?;
                let decoded = DecodedData {
                    seqs: seqs.clone(),
                    columns: normalized.columns().to_vec(),
                    meta: meta.clone(),
                };
                (Some(wal), Some(decoded))
            } else {
                (None, None)
            };
            prepared.push(PreparedBucket {
                rows,
                seqs,
                columns,
                meta,
                norms,
                wal,
                decoded,
                partition,
            });
        }
        // P3：按暂定 seq 递增检查真正自动生成的候选，不豁免同批的显式覆盖。
        for group in &prepared {
            for (i, &row) in group.rows.iter().enumerate() {
                if generated[row] {
                    let pk = pk_at(&group.columns[self.pk_pos], i);
                    if self.pk_index.get(&pk).is_some() {
                        return Err(EngineError::Storage(format!(
                            "auto-generated primary key {} would overwrite an existing row",
                            group.seqs[i]
                        )));
                    }
                }
            }
        }
        // P4：真实追加失败由 runtime 冻结/丢弃 writer；只发布已成功的桶并停止尾部。
        self.next_seq = next_seq;
        #[cfg(all(test, feature = "fault-injection"))]
        if EXIT_AFTER_ALLOCATION.with(|v| v.replace(false)) {
            std::process::exit(88);
        }
        let mut ids = vec![Pk::Int64(0); n];
        for group in prepared {
            let (source, row_start) = self.persistent.as_ref().map_or_else(
                || {
                    (
                        SourceId(1),
                        self.chunks
                            .last_key_value()
                            .map_or(0, |(_, c)| c.row_start + c.live.len),
                    )
                },
                |p| (p.active.source, p.active.physical_rows()),
            );
            if let Some(p) = self.persistent.as_mut() {
                let wal = group
                    .wal
                    .ok_or_else(|| EngineError::Storage("prepared WAL missing".into()))?;
                let decoded = group
                    .decoded
                    .ok_or_else(|| EngineError::Storage("prepared data missing".into()))?;
                p.append_prepared_partition(&self.schema, wal, decoded, group.partition)
                    .map_err(EngineError::Storage)?;
            }
            if let Some(memory) = self.memory_key.as_mut() {
                memory
                    .visibility
                    .extend_source(source, row_start + group.rows.len())
                    .map_err(|e| EngineError::Storage(e.to_string()))?;
                for i in 0..group.rows.len() {
                    memory
                        .visibility
                        .apply_partition_row(
                            pk_at(&group.columns[self.pk_pos], i),
                            group.seqs[i],
                            source,
                            row_start + i,
                            group.partition,
                        )
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                }
            }
            let bucket_ids = self.publish_insert(
                group.columns,
                group.meta,
                group.norms,
                group.seqs,
                source,
                row_start,
            );
            #[cfg(all(test, feature = "fault-injection"))]
            EXIT_AFTER_APPLIED_BUCKET.with(|v| match v.get() {
                Some(0) => std::process::exit(88),
                Some(n) => v.set(Some(n - 1)),
                None => {}
            });
            for (row, id) in group.rows.into_iter().zip(bucket_ids) {
                ids[row] = id;
            }
        }
        Ok(ids)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{DataDir, Db};
    use std::path::Path;

    fn schema(auto: bool) -> CollectionSchema {
        CollectionSchema {
            fields: vec![
                FieldSchema {
                    is_primary: true,
                    auto_id: auto,
                    ..FieldSchema::new("id", DataType::Int64)
                },
                FieldSchema {
                    is_partition_key: true,
                    nullable: true,
                    ..FieldSchema::new("k", DataType::Int64)
                },
                FieldSchema {
                    dim: Some(2),
                    ..FieldSchema::new("v", DataType::FloatVector)
                },
                FieldSchema {
                    element_type: Some(DataType::Int8),
                    max_capacity: Some(8),
                    ..FieldSchema::new("a", DataType::Array)
                },
            ],
            enable_dynamic_field: true,
            properties: vec![],
        }
    }
    fn db(root: &Path) -> Arc<Db> {
        Db::new(
            DataDir::open(root).unwrap(),
            Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
        )
        .unwrap()
    }
    fn batch(keys: &[i64], ids: Option<&[i64]>) -> InsertBatch {
        let mut columns = vec![
            ("k".into(), InputValues::Int(keys.to_vec())),
            (
                "v".into(),
                InputValues::FloatVector {
                    dim: 2,
                    values: vec![1.; keys.len() * 2],
                },
            ),
            (
                "a".into(),
                InputValues::Array(vec![Some(InputValues::Int(vec![1])); keys.len()]),
            ),
        ];
        if let Some(ids) = ids {
            columns.push(("id".into(), InputValues::Int(ids.to_vec())));
        }
        InsertBatch {
            num_rows: keys.len(),
            columns,
        }
    }
    fn files(root: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
        let mut result = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for e in std::fs::read_dir(dir).unwrap() {
                let e = e.unwrap();
                let path = e.path();
                if e.file_type().unwrap().is_dir() {
                    pending.push(path);
                } else {
                    result.insert(
                        path.strip_prefix(root).unwrap().into(),
                        std::fs::read(path).unwrap(),
                    );
                }
            }
        }
        result
    }

    #[test]
    fn generated_ids_follow_physical_bucket_seq_and_restore_input_positions() {
        let root = tempfile::tempdir().unwrap();
        let db = db(root.path());
        db.create_collection("", "c", schema(true)).unwrap();
        let before = files(root.path());
        assert!(
            db.create_partition("", "c", "_default")
                .unwrap_err()
                .to_string()
                .contains("cannot create manual partitions")
        );
        assert!(
            db.drop_partition("", "c", "unknown")
                .unwrap_err()
                .to_string()
                .contains("cannot drop partitions")
        );
        assert_eq!(files(root.path()), before);
        let ids = db
            .insert_partition_with_pk_type("", "c", batch(&[0, 1, 0], None), "../ignored")
            .unwrap()
            .pks;
        assert_eq!(ids, vec![Pk::Int64(1), Pk::Int64(3), Pk::Int64(2)]);
        let handle = db.collection("", "c").unwrap();
        let c = handle.read();
        assert_eq!(c.partition_count(), 17);
        assert_eq!(c.count_partition("_default").unwrap(), 0);
        assert_eq!(c.count_partition("_pk_10").unwrap(), 2);
        assert_eq!(c.count_partition("_pk_11").unwrap(), 1);
        let p = c.persistent.as_ref().unwrap();
        assert_eq!(p.active.physical_rows(), 3);
        let wal =
            crate::storage::wal::read_prefix(&p.dir.join("wal/wal_data_000001.arrow")).unwrap();
        assert_eq!(wal.batches.len(), 2);
        assert_eq!(c.next_seq, 4);
        drop(c);
        db.flush_collection("", "c").unwrap();
        db.close();
        drop(handle);
        drop(db);
        let reopened = self::db(root.path());
        let handle = reopened.collection("", "c").unwrap();
        assert_eq!(handle.read().count(), 3);
        assert_eq!(handle.read().next_seq, 4);
        reopened.close();
    }

    #[test]
    fn every_bucket_conversion_precedes_sequence_allocation_and_wal() {
        for keys in [[0, 1, 0], [1, 0, 0], [0, 0, 0]] {
            let root = tempfile::tempdir().unwrap();
            let db = db(root.path());
            db.create_collection("", "c", schema(true)).unwrap();
            let mut input = batch(&keys, None);
            input.columns[2].1 = InputValues::Array(vec![
                Some(InputValues::Int(vec![1])),
                Some(InputValues::Int(vec![128])),
                Some(InputValues::Int(vec![1])),
            ]);
            let before = files(root.path());
            let error = db.insert("", "c", input).unwrap_err();
            assert_eq!(
                error.to_string(),
                "Value 128 too large to fit in C integer type"
            );
            assert_eq!(files(root.path()), before);
            let handle = db.collection("", "c").unwrap();
            assert_eq!(handle.read().count(), 0);
            assert_eq!(handle.read().next_seq, 1);
            assert_eq!(
                db.insert("", "c", batch(&[0], None)).unwrap(),
                vec![Pk::Int64(1)]
            );
            db.close();
        }
    }

    #[test]
    fn key_upsert_uses_request_base_for_dynamic_keys_and_typed_columns() {
        for flushed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let db = db(root.path());
            db.create_collection("", "c", schema(false)).unwrap();
            let handle = db.collection("", "c").unwrap();
            let mut initial = batch(&[0], Some(&[1]));
            initial.columns.push((
                META_FIELD.into(),
                InputValues::Json(vec![br#"{"x":0,"y":0}"#.to_vec()]),
            ));
            db.insert("", "c", initial).unwrap();
            if flushed {
                db.flush_collection("", "c").unwrap();
            }
            let update = InsertBatch {
                num_rows: 2,
                columns: vec![
                    ("id".into(), InputValues::Int(vec![1, 1])),
                    (
                        META_FIELD.into(),
                        InputValues::Json(vec![br#"{"x":1}"#.to_vec(), br#"{"y":2}"#.to_vec()]),
                    ),
                ],
            };
            db.upsert_with_pk_type("", "c", update).unwrap();
            let c = handle.read();
            let rows = c
                .get(&[Pk::Int64(1)], &["x".into(), "y".into()])
                .unwrap()
                .rows;
            assert_eq!(
                rows.meta.unwrap(),
                vec![
                    serde_json::json!({"x":0,"y":2})
                        .as_object()
                        .unwrap()
                        .clone()
                ]
            );
            assert_eq!(c.count_partition("_pk_10").unwrap(), 1);
            drop(c);
            db.close();
        }
    }

    #[test]
    fn in_memory_key_collection_reuses_partition_visibility_and_delete() {
        let mut c = Collection::new("c", schema(true)).unwrap();
        assert_eq!(
            c.insert(batch(&[0, 1, 0], None)).unwrap(),
            vec![Pk::Int64(1), Pk::Int64(3), Pk::Int64(2)]
        );
        assert_eq!(c.count_partition("_pk_10").unwrap(), 2);
        assert_eq!(
            c.prepare_partition_get(
                &[Pk::Int64(3), Pk::Int64(2), Pk::Int64(2)],
                &["id".into()],
                &["_pk_10".into()]
            )
            .unwrap()
            .read()
            .rows
            .pks,
            vec![Pk::Int64(2), Pk::Int64(2)]
        );
        let old = c.read_named_snapshot(&["_pk_10".into()]).unwrap();
        c.delete_from_db(&[Pk::Int64(1)]).unwrap();
        assert_eq!(c.count_partition("_pk_10").unwrap(), 1);
        assert_eq!(old.count_filtered("").unwrap(), 2);
        assert_eq!(
            c.read_named_snapshot(&["_default".into()])
                .unwrap()
                .count_filtered("")
                .unwrap(),
            0
        );
    }

    #[test]
    fn collision_guard_is_global_auto_only_and_preserves_sequence() {
        let root = tempfile::tempdir().unwrap();
        let db = db(root.path());
        db.create_collection("", "c", schema(true)).unwrap();
        // 旧目录允许 id 大于其 seq；不能从 max(id) 擅自提高恢复编号。
        db.insert("", "c", batch(&[1, 1], Some(&[3, 4]))).unwrap();
        let before = files(root.path());
        let error = db.insert("", "c", batch(&[0, 0], None)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "storage error: auto-generated primary key 3 would overwrite an existing row"
        );
        assert_eq!(files(root.path()), before);
        let handle = db.collection("", "c").unwrap();
        assert_eq!(handle.read().next_seq, 3);
        assert_eq!(handle.read().count(), 2);
        db.delete("", "c", &[Pk::Int64(3), Pk::Int64(4)]).unwrap();
        assert_eq!(
            db.insert("", "c", batch(&[0], None)).unwrap(),
            vec![Pk::Int64(4)]
        );
        // 显式覆盖不受 B3 的保护限制。
        db.insert("", "c", batch(&[0, 1], Some(&[4, 4]))).unwrap();
        assert_eq!(handle.read().count(), 1);
        db.close();
    }

    #[test]
    fn implicit_buckets_are_registered_before_wal_recovery_without_rewriting_manifest() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("collections/c");
        std::fs::create_dir_all(dir.join("wal")).unwrap();
        let s = schema(false);
        std::fs::write(
            dir.join("schema.json"),
            crate::storage::schema_file::SchemaFile::from_core("c", &s)
                .to_bytes()
                .unwrap(),
        )
        .unwrap();
        let columns = vec![
            ColumnData::Int64(vec![1]),
            ColumnData::Int64(vec![0]),
            ColumnData::FloatVector {
                dim: 2,
                values: vec![1., 1.],
            },
            crate::data::array::convert(
                &s.fields[3],
                &InputValues::Array(vec![Some(InputValues::Int(vec![1]))]),
            )
            .unwrap(),
        ];
        let arrow = crate::storage::arrow_data::wal_data_batch(
            &s,
            &[1],
            "_pk_10",
            &columns,
            Some(&[Map::new()]),
        )
        .unwrap();
        let mut writer = crate::storage::wal::WalWriter::create(
            &dir.join("wal"),
            crate::storage::wal::WalKind::Data,
            1,
            &arrow.schema(),
        )
        .unwrap();
        writer.append(&arrow).unwrap();
        drop(writer);
        let db = db(root.path());
        let handle = db.collection("", "c").unwrap();
        assert_eq!(handle.read().count(), 1);
        assert_eq!(handle.read().count_partition("_pk_10").unwrap(), 1);
        assert!(!dir.join("manifest.json").exists());
        assert_eq!(handle.read().partition_count(), 17);
        db.close();
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn later_bucket_partial_wal_failure_keeps_prefix_and_does_not_apply_tail() {
        for bytes in [0, 1, 64, usize::MAX] {
            let root = tempfile::tempdir().unwrap();
            let db = db(root.path());
            db.create_collection("", "c", schema(true)).unwrap();
            crate::storage::wal::inject_storage_full_on_append(1, bytes);
            assert!(db.insert("", "c", batch(&[0, 1, 0], None)).is_err());
            let handle = db.collection("", "c").unwrap();
            let c = handle.read();
            assert_eq!(c.count(), 2);
            assert_eq!(c.next_seq, 4);
            assert!(c.pk_index.get(&Pk::Int64(3)).is_none());
            assert!(c.pk_index.get(&Pk::Int64(2)).is_some());
            drop(c);
            // 未完成批的全部 seq 不回退；重试冻结代后继续下一代写入。
            assert_eq!(
                db.insert("", "c", batch(&[1], None)).unwrap(),
                vec![Pk::Int64(4)]
            );
            db.close();
            drop(handle);
            drop(db);
            let reopened = self::db(root.path());
            let c = reopened.collection("", "c").unwrap();
            assert_eq!(c.read().count(), 3);
            assert!(c.read().pk_index.get(&Pk::Int64(2)).is_some());
            reopened.close();
        }
    }
    #[cfg(feature = "fault-injection")]
    #[test]
    fn crash_boundaries_recover_only_actual_wal_prefix() {
        const ROOT: &str = "MLR_KEY_CRASH_ROOT";
        const MODE: &str = "MLR_KEY_CRASH_MODE";
        if let Ok(root) = std::env::var(ROOT) {
            let mode = std::env::var(MODE).unwrap();
            let db = db(Path::new(&root));
            let handle = db.collection("", "c").unwrap();
            assert_eq!(handle.read().count(), 1);
            if mode == "allocated" {
                EXIT_AFTER_ALLOCATION.with(|v| v.set(true));
            }
            if mode == "applied-first" {
                EXIT_AFTER_APPLIED_BUCKET.with(|v| v.set(Some(0)));
            }
            if mode.starts_with("partial-") || mode == "error-return" {
                let (skip, bytes) = match mode.as_str() {
                    "partial-first" => (0, 1),
                    "partial-second-zero" => (1, 0),
                    "partial-second-tail" => (1, usize::MAX),
                    _ => (1, 64),
                };
                crate::storage::wal::inject_storage_full_on_append(skip, bytes);
            }
            // WAL 暂停恢复信号的发送端须存活到子进程以 exit(88) 结束；在块内 drop 会让暂停在写入后立即断连并 panic，
            // 与监视线程的 exit 竞争，结果随调度而变。
            let (_resume, wait) = std::sync::mpsc::channel::<()>();
            if mode.starts_with("full-") || mode.starts_with("partial-") {
                let skip = usize::from(mode.contains("second"));
                let (entered, reached) = std::sync::mpsc::channel();
                crate::storage::wal::pause_append_after_io_after(skip, entered, wait);
                std::thread::spawn(move || {
                    reached
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .unwrap();
                    std::process::exit(88);
                });
            }
            let result = db.insert("", "c", batch(&[0, 1, 0], None));
            if mode == "error-return" {
                assert!(result.is_err());
                assert_eq!(handle.read().count(), 3);
                std::process::exit(88);
            }
            if mode == "returned" {
                assert_eq!(
                    result.unwrap(),
                    vec![Pk::Int64(2), Pk::Int64(4), Pk::Int64(3)]
                );
                std::process::exit(88);
            }
            panic!("requested crash boundary did not fire: {mode}: {result:?}");
        }
        for (mode, expected) in [
            ("allocated", vec![90]),
            ("partial-first", vec![90]),
            ("full-first", vec![2, 3, 90]),
            ("applied-first", vec![2, 3, 90]),
            ("partial-second-zero", vec![2, 3, 90]),
            ("partial-second-mid", vec![2, 3, 90]),
            ("partial-second-tail", vec![2, 3, 90]),
            ("error-return", vec![2, 3, 90]),
            ("full-second", vec![2, 3, 4, 90]),
            ("returned", vec![2, 3, 4, 90]),
        ] {
            let root = tempfile::tempdir().unwrap();
            let db = db(root.path());
            db.create_collection("", "c", schema(true)).unwrap();
            db.insert("", "c", batch(&[1], Some(&[90]))).unwrap();
            db.flush_collection("", "c").unwrap();
            db.close();
            drop(db);
            let status=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","collection::partition_key::tests::crash_boundaries_recover_only_actual_wal_prefix","--nocapture"])
                .env(ROOT,root.path()).env(MODE,mode).output().unwrap();
            assert_eq!(status.status.code(), Some(88), "{mode}: {:?}", status);
            let db = self::db(root.path());
            let handle = db.collection("", "c").unwrap();
            let c = handle.read();
            assert_eq!(c.count(), expected.len(), "{mode}");
            for pk in &expected {
                assert!(c.pk_index.get(&Pk::Int64(*pk)).is_some(), "{mode}: {pk}");
            }
            let next = c.next_seq;
            drop(c);
            assert_eq!(
                db.insert("", "c", batch(&[0], None)).unwrap(),
                vec![Pk::Int64(next as i64)]
            );
            assert_eq!(handle.read().count(), expected.len() + 1);
            db.close();
        }
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn old_snapshot_survives_bucket_write_while_close_and_new_reads_wait_for_permission() {
        use std::sync::mpsc::channel;
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        let db = db(root.path());
        db.create_collection("", "c", schema(true)).unwrap();
        db.insert("", "c", batch(&[0], None)).unwrap();
        let handle = db.collection("", "c").unwrap();
        let snapshot = handle.read().read_snapshot();
        let (entered, reached) = channel();
        let (resume, wait) = channel();
        let writer_db = Arc::clone(&db);
        let writer = std::thread::spawn(move || {
            crate::storage::wal::pause_append_after_io_after(1, entered, wait);
            writer_db.insert("", "c", batch(&[0, 1, 0], None))
        });
        reached.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!handle.available());
        assert_eq!(snapshot.count_filtered("id >= 0").unwrap(), 1);
        assert!(DataDir::open(root.path()).is_err());
        let (closed, done) = channel();
        let closing = Arc::clone(&db);
        let close = std::thread::spawn(move || {
            closing.close();
            closed.send(()).unwrap();
        });
        assert!(done.try_recv().is_err());
        resume.send(()).unwrap();
        assert_eq!(
            writer.join().unwrap().unwrap(),
            vec![Pk::Int64(2), Pk::Int64(4), Pk::Int64(3)]
        );
        done.recv_timeout(Duration::from_secs(10)).unwrap();
        close.join().unwrap();
        assert_eq!(snapshot.count_filtered("id >= 0").unwrap(), 1);
        drop(handle);
        drop(db);
        assert!(DataDir::open(root.path()).is_ok());
        let reopened = self::db(root.path());
        assert_eq!(reopened.collection("", "c").unwrap().read().count(), 4);
        reopened.close();
    }
    #[test]
    fn sequence_and_auto_id_overflow_are_preflight_errors_and_empty_batch_ignores_columns() {
        let mut c = Collection::new("c", schema(true)).unwrap();
        for next in [u64::MAX, i64::MAX as u64] {
            c.next_seq = next;
            assert!(c.insert(batch(&[0, 1, 0], None)).is_err());
            assert_eq!(c.next_seq, next);
            assert_eq!(c.count(), 0);
        }
        assert!(
            c.insert(InsertBatch {
                num_rows: 0,
                columns: vec![("broken".into(), InputValues::Int(vec![1]))]
            })
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn recovered_key_collection_can_append_after_every_open_without_memory_shadow_state() {
        let root = tempfile::tempdir().unwrap();
        for round in 0..3 {
            let db = db(root.path());
            if round == 0 {
                db.create_collection("", "c", schema(true)).unwrap();
            }
            let handle = db.collection("", "c").unwrap();
            assert!(handle.read().memory_key.is_none());
            db.insert("", "c", batch(&[0, 1, 0], None)).unwrap();
            assert_eq!(handle.read().count(), (round + 1) * 3);
            db.close();
        }
    }

    #[test]
    fn unknown_bucket_wal_is_rejected_instead_of_accepting_the_key_prefix() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("collections/c");
        std::fs::create_dir_all(dir.join("wal")).unwrap();
        let s = schema(false);
        std::fs::write(
            dir.join("schema.json"),
            crate::storage::schema_file::SchemaFile::from_core("c", &s)
                .to_bytes()
                .unwrap(),
        )
        .unwrap();
        let columns = vec![
            ColumnData::Int64(vec![1]),
            ColumnData::Int64(vec![0]),
            ColumnData::FloatVector {
                dim: 2,
                values: vec![1., 1.],
            },
            crate::data::array::convert(
                &s.fields[3],
                &InputValues::Array(vec![Some(InputValues::Int(vec![1]))]),
            )
            .unwrap(),
        ];
        let arrow = crate::storage::arrow_data::wal_data_batch(
            &s,
            &[1],
            "_pk_16",
            &columns,
            Some(&[Map::new()]),
        )
        .unwrap();
        let mut writer = crate::storage::wal::WalWriter::create(
            &dir.join("wal"),
            crate::storage::wal::WalKind::Data,
            1,
            &arrow.schema(),
        )
        .unwrap();
        writer.append(&arrow).unwrap();
        drop(writer);
        let before = files(&dir);
        let db = db(root.path());
        assert!(
            db.collection("", "c")
                .unwrap_err()
                .to_string()
                .contains("not registered")
        );
        assert_eq!(files(&dir), before);
        db.close();
    }
    #[test]
    fn implicit_bucket_materialization_rechecks_aliases_even_after_inherited_sync() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("collections/c");
        std::fs::create_dir_all(&dir).unwrap();
        let s = schema(false);
        std::fs::write(
            dir.join("schema.json"),
            crate::storage::schema_file::SchemaFile::from_core("c", &s)
                .to_bytes()
                .unwrap(),
        )
        .unwrap();
        let mut manifest = crate::storage::manifest::Manifest::default();
        manifest
            .partitions
            .insert("_PK_10".into(), Default::default());
        manifest.partition_order.push("_PK_10".into());
        std::fs::write(dir.join("manifest.json"), manifest.to_bytes().unwrap()).unwrap();
        let probe = root.path().join("probe");
        std::fs::create_dir(&probe).unwrap();
        std::fs::create_dir(probe.join("lower")).unwrap();
        let alias = probe.join("LOWER").exists();
        let db = db(root.path());
        let handle = db.collection("", "c").unwrap();
        db.create_index(
            "",
            "c",
            "v",
            IndexRequest {
                index_type: "FLAT".into(),
                metric_type: Some("L2".into()),
                params: Map::new(),
                search_params: Map::new(),
            },
        )
        .unwrap();
        assert!(handle.read().persistent().unwrap().inherited_synced());
        let before = std::fs::read(dir.join("manifest.json")).unwrap();
        db.insert("", "c", batch(&[0], Some(&[1]))).unwrap();
        let result = db.flush_collection("", "c");
        if alias {
            assert!(result.unwrap_err().to_string().contains("same directory"));
            assert_eq!(std::fs::read(dir.join("manifest.json")).unwrap(), before);
            assert!(!dir.join("partitions/_pk_10").exists());
            assert!(!dir.join("partitions/_PK_10").exists());
            // 仅移除空的、未被使用的额外逻辑别名后，原 WAL 的重试应能正常完成。
            {
                let mut c = handle.write();
                let p = c.persistent_mut().unwrap();
                p.manifest.partitions.remove("_PK_10");
                p.manifest.partition_order.retain(|n| n != "_PK_10");
            }
            db.flush_collection("", "c").unwrap();
        } else {
            result.unwrap();
        }
        assert_eq!(handle.read().count(), 1);
        db.close();
        drop(handle);
        drop(db);
        let db = self::db(root.path());
        assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
        db.close();
    }
}
