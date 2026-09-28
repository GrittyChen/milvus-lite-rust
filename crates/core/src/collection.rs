//! 单个 collection 的内存引擎：写入、按主键读取、计数、暴力检索、索引定义与 load 状态。
//! 语义以 3.x 的 memtable 路径为准；与 3.x 不同之处见 0004「决策」与「阶段性差异」。

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::compute::Compute;
use crate::data::{
    ColumnData, InputValues, InsertBatch, META_FIELD, Pk, Rows, check_column_len, field_dim,
};
use crate::error::{EngineError, Result, py_float_repr, py_json_repr, py_repr, py_repr_list};
use crate::filter::PkLiteral;
use crate::flat;
use crate::index::{IndexRequest, IndexSpec, Metric, build_spec};
use crate::projection::{ApiKind, Projection};
use crate::schema::{CollectionSchema, DataType, FieldSchema};
use crate::storage::arrow_data::DecodedData;
use crate::storage::recovery::RecoveredCollection;
use crate::storage::runtime::PersistentCollection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadState {
    Loaded,
    Released,
}

/// 查询向量。M1 的 collection 只有稠密向量字段，稀疏与文本查询在 3.x 中同样报错，
/// 这里保留它们的种类，以便在与 3.x 相同的位置报出相同的错误。
#[derive(Debug, Clone, PartialEq)]
pub enum QueryVectors {
    Dense(Vec<Vec<f32>>),
    Sparse,
    Text,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchRequest {
    /// 为空时取第一个 FLOAT_VECTOR 字段。
    pub anns_field: Option<String>,
    pub vectors: QueryVectors,
    /// 为空时用向量字段索引的度量；没有索引时用 COSINE（与 3.x 相同）。
    pub metric_type: Option<String>,
    pub limit: usize,
    pub offset: usize,
    pub output_fields: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hits {
    /// 与 `rows` 逐行对应的对外分数。
    pub scores: Vec<f32>,
    pub rows: Rows,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub projection: Projection,
    /// 实际使用的度量；没有查询向量时为 None。
    pub metric: Option<Metric>,
    /// 每个查询向量一组结果。
    pub hits: Vec<Hits>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub projection: Projection,
    pub rows: Rows,
}

/// 一次 insert 追加的列式数据块。
#[derive(Debug)]
struct Chunk {
    /// 按 schema 字段顺序。
    columns: Vec<ColumnData>,
    /// 开启动态字段时每行的动态字段。
    meta: Option<Vec<Map<String, Value>>>,
    /// 该行是否仍是其主键的最新版本。被覆盖时增量清除，查询时无需逐行去重（0001 约束）。
    live: Vec<bool>,
    /// 向量字段每行的范数，写入时算好供 COSINE 使用（与检索时现算逐位相同），不落盘。
    norms: Vec<f32>,
    generation: u64,
    orders: Vec<u64>,
}

/// 行的位置。派生的全序即扫描顺序：先按 chunk、再按行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RowRef {
    order: u64,
    chunk: usize,
    row: usize,
}

#[derive(Debug)]
pub struct Collection {
    name: String,
    schema: CollectionSchema,
    pk_pos: usize,
    vector_pos: usize,
    chunks: Vec<Chunk>,
    pk_index: HashMap<Pk, RowRef>,
    /// 下一个 `_seq`。auto_id 取所在行的 `_seq`（0004「决策」），因此 auto_id 不超过 `_seq`（0002 约束 4）。
    next_seq: u64,
    /// 按创建顺序。
    indexes: Vec<IndexSpec>,
    load_state: LoadState,
    next_order: u64,
    persistent: Option<PersistentCollection>,
    deleted: bool,
}

impl Collection {
    pub fn new(name: impl Into<String>, schema: CollectionSchema) -> Result<Self> {
        schema.validate()?;
        let pk_pos = schema
            .primary_position()
            .ok_or_else(|| EngineError::schema("schema has no primary key field"))?;
        let vector_pos = schema
            .fields
            .iter()
            .position(|f| f.data_type.is_vector())
            .ok_or_else(|| {
                EngineError::schema(
                    "schema has no vector field (FLOAT_VECTOR or SPARSE_FLOAT_VECTOR)",
                )
            })?;
        Ok(Self {
            name: name.into(),
            schema,
            pk_pos,
            vector_pos,
            chunks: Vec::new(),
            pk_index: HashMap::new(),
            next_seq: 1,
            indexes: Vec::new(),
            // 3.x：新建的 collection 没有索引定义，初始即为 loaded。
            load_state: LoadState::Loaded,
            next_order: 0,
            persistent: None,
            deleted: false,
        })
    }

    pub(crate) fn attach_persistence(&mut self, persistent: PersistentCollection) {
        self.persistent = Some(persistent);
    }

    pub(crate) fn persistent(&self) -> Option<&PersistentCollection> {
        self.persistent.as_ref()
    }

    pub(crate) fn persistent_mut(&mut self) -> Option<&mut PersistentCollection> {
        self.persistent.as_mut()
    }

    pub(crate) fn mark_deleted(&mut self, deleted: bool) {
        self.deleted = deleted;
    }

    pub(crate) fn is_deleted(&self) -> bool {
        self.deleted
    }

    pub(crate) fn from_recovered(
        name: &str,
        recovered: RecoveredCollection,
        dir: std::path::PathBuf,
    ) -> Result<Self> {
        let mut collection = Self::new(name, recovered.schema.clone())?;
        collection.next_seq = recovered.next_seq;
        let snapshot = recovered.visibility.snapshot();
        for segment in &recovered.segments {
            let mut row = 0;
            for batch in &segment.batches {
                let live = (0..batch.seqs.len())
                    .map(|offset| {
                        snapshot
                            .is_live(segment.source, row + offset)
                            .ok_or_else(|| {
                                EngineError::Storage(
                                    "recovered segment visibility is incomplete".to_owned(),
                                )
                            })
                    })
                    .collect::<Result<Vec<_>>>()?;
                collection.append_recovered(batch, 0, live)?;
                row += batch.seqs.len();
            }
        }
        let active_source = recovered.active_memtable.source;
        let materialized = recovered
            .active_memtable
            .materialize(&recovered.schema, &recovered.visibility)
            .map_err(|error| EngineError::Storage(error.to_string()))?;
        if let Some(batch) = materialized.data {
            let decoded = crate::storage::arrow_data::decode_data_batch(&collection.schema, &batch)
                .map_err(|error| EngineError::Storage(error.to_string()))?;
            let mut live = vec![false; decoded.seqs.len()];
            for (old, mapped) in materialized.mapping.iter().enumerate() {
                if let Some(new_row) = mapped {
                    live[*new_row] = snapshot.is_live(active_source, old).ok_or_else(|| {
                        EngineError::Storage("recovered WAL visibility is incomplete".to_owned())
                    })?;
                }
            }
            collection.append_recovered(&decoded, recovered.next_wal_number, live)?;
        }
        for spec in recovered.manifest.index_specs.values() {
            collection.indexes.push(crate::index::IndexSpec {
                field_name: spec.field_name.clone(),
                index_type: spec.index_type.clone(),
                metric_type: spec.metric_type.clone(),
                params: spec.build_params.clone(),
                search_params: spec.search_params.clone(),
            });
        }
        if !collection.indexes.is_empty() {
            collection.load_state = LoadState::Released;
        }
        collection.persistent = Some(PersistentCollection::recovered(dir, recovered));
        Ok(collection)
    }

    fn append_recovered(
        &mut self,
        decoded: &DecodedData,
        generation: u64,
        live: Vec<bool>,
    ) -> Result<()> {
        let n = decoded.seqs.len();
        let (dim, vectors) = vector_values(&decoded.columns[self.vector_pos]);
        let chunk_index = self.chunks.len();
        let mut chunk = Chunk {
            columns: decoded.columns.clone(),
            meta: decoded.meta.clone(),
            live,
            norms: vectors.chunks_exact(dim).map(flat::norm).collect(),
            generation,
            orders: Vec::with_capacity(n),
        };
        for row in 0..n {
            let order = self.next_order;
            self.next_order = self
                .next_order
                .checked_add(1)
                .ok_or_else(|| EngineError::Storage("row order space exhausted".to_owned()))?;
            chunk.orders.push(order);
            if chunk.live[row] {
                let pk = pk_at(&chunk.columns[self.pk_pos], row);
                self.pk_index.insert(
                    pk,
                    RowRef {
                        order,
                        chunk: chunk_index,
                        row,
                    },
                );
            }
        }
        self.chunks.push(chunk);
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn schema(&self) -> &CollectionSchema {
        &self.schema
    }

    /// 去重后的存活行数。不检查 load 状态（3.x 的 `num_entities`）。
    pub fn count(&self) -> usize {
        self.pk_index.len()
    }

    pub fn load_state(&self) -> LoadState {
        self.load_state
    }

    pub fn load(&mut self) {
        self.load_state = LoadState::Loaded;
    }

    /// 3.x：没有任何索引定义时 release 不生效，collection 仍可检索。
    pub fn release(&mut self) {
        if !self.indexes.is_empty() {
            self.load_state = LoadState::Released;
        }
    }

    pub fn indexes(&self) -> &[IndexSpec] {
        &self.indexes
    }

    pub fn index(&self, field_name: &str) -> Option<&IndexSpec> {
        self.indexes.iter().find(|s| s.field_name == field_name)
    }

    /// 3.x DescribeIndex：collection 没有任何索引时先报错；指定字段时只返回该字段的索引。
    pub fn describe_indexes(&self, field_name: Option<&str>) -> Result<Vec<&IndexSpec>> {
        if self.indexes.is_empty() {
            return Err(EngineError::IndexNotFound(format!(
                "no index on collection {}",
                py_repr(&self.name)
            )));
        }
        match field_name {
            None => Ok(self.indexes.iter().collect()),
            Some(f) => self.index(f).map(|spec| vec![spec]).ok_or_else(|| {
                EngineError::IndexNotFound(format!(
                    "no index on field {} of collection {}",
                    py_repr(f),
                    py_repr(&self.name)
                ))
            }),
        }
    }

    /// 保存索引定义，不改变 load 状态（3.x 的 Milvus 语义）。
    pub fn create_index(&mut self, field_name: &str, req: IndexRequest) -> Result<()> {
        if self.deleted {
            return Err(EngineError::CollectionNotFound(self.name.clone()));
        }
        if self.persistent.is_some() {
            return Err(EngineError::Storage(
                "persisted index changes must enter through Db".to_owned(),
            ));
        }
        self.create_index_inner(field_name, req)
    }

    pub(crate) fn create_index_from_db(
        &mut self,
        field_name: &str,
        req: IndexRequest,
    ) -> Result<()> {
        self.create_index_inner(field_name, req)
    }

    fn create_index_inner(&mut self, field_name: &str, req: IndexRequest) -> Result<()> {
        if self.deleted {
            return Err(EngineError::CollectionNotFound(self.name.clone()));
        }
        if self.index(field_name).is_some() {
            return Err(EngineError::IndexAlreadyExists(field_name.to_owned()));
        }
        let field = self.schema.field(field_name).ok_or_else(|| {
            EngineError::schema(format!(
                "unknown field {} for create_index",
                py_repr(field_name)
            ))
        })?;
        let spec = build_spec(field, req)?;
        if let Some(persistent) = self.persistent.as_mut() {
            persistent
                .commit_index(&spec)
                .map_err(EngineError::Storage)?;
        }
        self.indexes.push(spec);
        Ok(())
    }

    /// 写入一批数据，逐行返回主键（批内重复的主键原样重复）。
    ///
    /// 校验全部通过才写入；校验顺序与错误信息与 3.x 的 `validate_record` 一致，
    /// 畸形的 JSON 与动态字段按 0004「决策」明确报错。
    pub fn insert(&mut self, batch: InsertBatch) -> Result<Vec<Pk>> {
        if self.deleted {
            return Err(EngineError::CollectionNotFound(self.name.clone()));
        }
        if self.persistent.is_some() {
            return Err(EngineError::Storage(
                "persisted writes must enter through Db".to_owned(),
            ));
        }
        self.insert_inner(batch)
    }

    pub(crate) fn insert_from_db(&mut self, batch: InsertBatch) -> Result<Vec<Pk>> {
        self.insert_inner(batch)
    }

    pub(crate) fn upsert_from_db(&mut self, batch: InsertBatch) -> Result<Vec<Pk>> {
        if self.deleted {
            return Err(EngineError::CollectionNotFound(self.name.clone()));
        }
        let n = batch.num_rows;
        if n == 0 {
            return Ok(Vec::new());
        }
        for (name, values) in &batch.columns {
            check_column_len(name, values, n)?;
        }
        let dynamic = self.decode_dynamic(&batch);
        let mut supplied: HashMap<&str, &InputValues> = HashMap::new();
        for (name, values) in &batch.columns {
            if name != META_FIELD && self.schema.field(name).is_some() {
                supplied.insert(name, values);
            }
        }
        let pk_field = &self.schema.fields[self.pk_pos];
        let mut pending: HashMap<Pk, (Vec<InputValues>, Map<String, Value>)> = HashMap::new();
        let mut fields: Vec<ColumnData> =
            self.schema.fields.iter().map(ColumnData::empty).collect();
        let mut metas = Vec::with_capacity(n);
        for row in 0..n {
            let candidate = supplied
                .get(pk_field.name.as_str())
                .and_then(|values| input_pk(values, row));
            let previous = candidate.as_ref().and_then(|pk| pending.get(pk));
            let current = candidate.as_ref().and_then(|pk| self.pk_index.get(pk));
            let mut values = if let Some((old, _)) = previous {
                old.clone()
            } else if let Some(old) = current {
                self.chunks[old.chunk]
                    .columns
                    .iter()
                    .map(|column| input_from_column(column, old.row))
                    .collect()
            } else {
                Vec::new()
            };
            if values.is_empty() {
                values = self
                    .schema
                    .fields
                    .iter()
                    .map(|_| InputValues::Int(Vec::new()))
                    .collect();
            }
            let mut present = vec![previous.is_some() || current.is_some(); values.len()];
            for (i, field) in self.schema.fields.iter().enumerate() {
                if let Some(input) = supplied.get(field.name.as_str()) {
                    values[i] = input_row(input, row);
                    present[i] = true;
                }
            }
            if !present[self.pk_pos] && pk_field.auto_id {
                let seq = self
                    .next_seq
                    .checked_add(row as u64)
                    .ok_or_else(|| EngineError::invalid("auto_id space exhausted"))?;
                values[self.pk_pos] = InputValues::Int(vec![
                    i64::try_from(seq)
                        .map_err(|_| EngineError::invalid("auto_id space exhausted"))?,
                ]);
                present[self.pk_pos] = true;
            }
            let mut meta = if let Some((_, old)) = previous {
                old.clone()
            } else if let Some(old) = current {
                self.chunks[old.chunk]
                    .meta
                    .as_ref()
                    .map_or_else(Map::new, |m| m[old.row].clone())
            } else {
                Map::new()
            };
            meta.extend(dynamic.maps[row].clone());
            let row_fields: HashMap<&str, &InputValues> = self
                .schema
                .fields
                .iter()
                .enumerate()
                .filter(|(i, _)| present[*i])
                .map(|(i, field)| (field.name.as_str(), &values[i]))
                .collect();
            let row_dynamic = Dynamic {
                maps: vec![meta.clone()],
                errors: vec![dynamic.errors[row].clone()],
            };
            self.validate_row(&row_fields, &row_dynamic, 0)?;
            for (i, field) in self.schema.fields.iter().enumerate() {
                let converted = convert(field, &values[i])?;
                fields[i].push_row(&converted, 0);
                values[i] = input_from_column(&converted, 0);
            }
            let pk = pk_at(&fields[self.pk_pos], row);
            pending.insert(pk, (values, meta.clone()));
            metas.push(meta);
        }
        let mut columns: Vec<(String, InputValues)> = self
            .schema
            .fields
            .iter()
            .zip(fields.iter())
            .map(|(field, column)| (field.name.clone(), input_from_column_all(column)))
            .collect();
        if self.schema.enable_dynamic_field {
            columns.push((
                META_FIELD.to_owned(),
                InputValues::Json(
                    metas
                        .iter()
                        .map(serde_json::to_vec)
                        .collect::<std::result::Result<_, _>>()
                        .map_err(|error| EngineError::schema(error.to_string()))?,
                ),
            ));
        }
        self.insert_inner(InsertBatch {
            num_rows: n,
            columns,
        })
    }

    pub(crate) fn delete_from_db(&mut self, pks: &[Pk]) -> Result<usize> {
        if self.deleted {
            return Err(EngineError::CollectionNotFound(self.name.clone()));
        }
        if pks.is_empty() {
            return Ok(0);
        }
        let seq = self.next_seq;
        self.next_seq = seq
            .checked_add(1)
            .ok_or_else(|| EngineError::invalid("_seq space exhausted"))?;
        self.delete_at_seq(pks, seq)
    }

    pub(crate) fn delete_literals_from_db(&mut self, literals: &[PkLiteral]) -> Result<Vec<Pk>> {
        if self.deleted {
            return Err(EngineError::CollectionNotFound(self.name.clone()));
        }
        if literals.is_empty() {
            return Ok(Vec::new());
        }
        let seq = self.next_seq;
        self.next_seq = seq
            .checked_add(1)
            .ok_or_else(|| EngineError::invalid("_seq space exhausted"))?;
        let varchar = self.schema.fields[self.pk_pos].data_type == DataType::VarChar;
        let pks = literals
            .iter()
            .map(|literal| match (varchar, literal) {
                (false, PkLiteral::Int(value)) => Ok(Pk::Int64(*value)),
                (true, PkLiteral::Str(value)) => Ok(Pk::VarChar(value.clone())),
                (_, PkLiteral::IntOutOfRange) => Err(EngineError::invalid(
                    "Python int too large to convert to C long",
                )),
                (false, PkLiteral::Str(value)) => Err(EngineError::invalid(format!(
                    "Could not convert {} with type str: tried to convert to int64",
                    py_repr(value)
                ))),
                (true, PkLiteral::Int(_)) => {
                    Err(EngineError::invalid("Expected bytes, got a 'int' object"))
                }
            })
            .collect::<Result<Vec<_>>>()?;
        self.delete_at_seq(&pks, seq)?;
        Ok(pks)
    }

    fn delete_at_seq(&mut self, pks: &[Pk], seq: u64) -> Result<usize> {
        if let Some(persistent) = self.persistent.as_mut() {
            persistent
                .append_delete(&self.schema, pks, seq)
                .map_err(EngineError::Storage)?;
        }
        for pk in pks {
            if let Some(old) = self.pk_index.remove(pk) {
                self.chunks[old.chunk].live[old.row] = false;
            }
        }
        Ok(pks.len())
    }

    fn insert_inner(&mut self, batch: InsertBatch) -> Result<Vec<Pk>> {
        if self.deleted {
            return Err(EngineError::CollectionNotFound(self.name.clone()));
        }
        let n = batch.num_rows;
        // 3.x：num_rows 为 0 时不看列内容，直接返回空结果。
        if n == 0 {
            return Ok(Vec::new());
        }
        for (name, values) in &batch.columns {
            check_column_len(name, values, n)?;
        }

        // 与 validate_row 对第 0 行最先做的两项检查相同。提前做，避免在缺列时仍按 num_rows
        // 为动态字段分配内存（num_rows 由请求给出，没有列时不受列长度检查的约束）。
        let has_column = |name: &str| batch.columns.iter().any(|(n, _)| n == name);
        let pk = &self.schema.fields[self.pk_pos];
        if !pk.auto_id && !has_column(&pk.name) {
            return Err(EngineError::schema(format!(
                "primary key {} missing or None",
                py_repr(&pk.name)
            )));
        }
        let vector = &self.schema.fields[self.vector_pos];
        if !has_column(&vector.name) {
            return Err(EngineError::schema(format!(
                "vector field {} missing or None",
                py_repr(&vector.name)
            )));
        }

        let dynamic = self.decode_dynamic(&batch);
        // 同名列以后出现者为准（3.x 逐列写入同一个 dict）。
        let mut by_name: HashMap<&str, &InputValues> = HashMap::new();
        for (name, values) in &batch.columns {
            if name != META_FIELD && self.schema.field(name).is_some() {
                by_name.insert(name.as_str(), values);
            }
        }
        for row in 0..n {
            self.validate_row(&by_name, &dynamic, row)?;
        }

        let pk_field = &self.schema.fields[self.pk_pos];
        let auto_pk = pk_field.auto_id && !by_name.contains_key(pk_field.name.as_str());
        let first_seq = self.next_seq;
        let next_seq = first_seq
            .checked_add(n as u64)
            .ok_or_else(|| EngineError::invalid("_seq space exhausted"))?;
        let seqs: Vec<u64> = (first_seq..next_seq).collect();
        let mut columns = Vec::with_capacity(self.schema.fields.len());
        for (i, field) in self.schema.fields.iter().enumerate() {
            let column = if i == self.pk_pos && auto_pk {
                ColumnData::Int64(
                    seqs.iter()
                        .map(|s| i64::try_from(*s))
                        .collect::<Result<_, _>>()
                        .map_err(|_| EngineError::invalid("auto_id space exhausted"))?,
                )
            } else {
                let values = by_name.get(field.name.as_str()).ok_or_else(|| {
                    EngineError::schema(format!(
                        "field {} missing and not nullable / no default",
                        py_repr(&field.name)
                    ))
                })?;
                convert(field, values)?
            };
            columns.push(column);
        }
        // schema 校验保证 dim > 0。
        let (dim, values) = vector_values(&columns[self.vector_pos]);
        let norms = values.chunks_exact(dim).map(flat::norm).collect();
        let meta = self.schema.enable_dynamic_field.then_some(dynamic.maps);
        self.next_seq = next_seq;
        if let Some(persistent) = self.persistent.as_mut() {
            persistent
                .append(&self.schema, &seqs, &columns, meta.as_deref())
                .map_err(EngineError::Storage)?;
        }
        // WAL 已成功追加；后续内存更新必须保持不可失败。
        let chunk_idx = self.chunks.len();
        let generation = self.persistent.as_ref().map_or(1, |p| p.generation);
        let mut chunk = Chunk {
            columns,
            meta,
            live: vec![true; n],
            norms,
            generation,
            orders: Vec::with_capacity(n),
        };
        let mut ids = Vec::with_capacity(n);
        for row in 0..n {
            let pk = pk_at(&chunk.columns[self.pk_pos], row);
            let previous = self.pk_index.get(&pk).copied();
            let order = match previous {
                Some(old) if old.chunk == chunk_idx => old.order,
                Some(old) if self.chunks[old.chunk].generation == generation => old.order,
                _ => {
                    let order = self.next_order;
                    self.next_order += 1;
                    order
                }
            };
            chunk.orders.push(order);
            let new_ref = RowRef {
                order,
                chunk: chunk_idx,
                row,
            };
            // _seq 严格递增，因此新行总是该主键的最新版本（不变量 1–2）。
            if let Some(old) = self.pk_index.insert(pk.clone(), new_ref) {
                if old.chunk == chunk_idx {
                    chunk.live[old.row] = false;
                } else {
                    self.chunks[old.chunk].live[old.row] = false;
                }
            }
            ids.push(pk);
        }
        self.chunks.push(chunk);
        Ok(ids)
    }

    /// 按主键读取。结果按输入顺序，重复的主键重复输出，不存在的跳过。
    pub fn get(&self, pks: &[Pk], output_fields: &[String]) -> Result<QueryResult> {
        self.require_loaded()?;
        let projection = Projection::new(&self.schema, output_fields, ApiKind::Query);
        let refs: Vec<RowRef> = pks
            .iter()
            .filter_map(|pk| self.pk_index.get(pk).copied())
            .collect();
        let rows = self.gather(&refs, &projection);
        Ok(QueryResult { projection, rows })
    }

    /// 按 `<主键> in [...]` 中的字面量读取。与主键类型不符或超出 int64 的字面量查不到，不报错（3.x 相同）。
    pub fn get_literals(
        &self,
        literals: &[PkLiteral],
        output_fields: &[String],
    ) -> Result<QueryResult> {
        let int_pk = self.schema.fields[self.pk_pos].data_type == DataType::Int64;
        let pks: Vec<Pk> = literals
            .iter()
            .filter_map(|lit| match lit {
                PkLiteral::Int(v) if int_pk => Some(Pk::Int64(*v)),
                PkLiteral::Str(v) if !int_pk => Some(Pk::VarChar(v.clone())),
                _ => None,
            })
            .collect();
        self.get(&pks, output_fields)
    }

    /// 在 `compute` 上并行计算（[0005](../../../docs/decisions/0005-search-thread-model.md)），
    /// 结果与串行计算逐位相同。
    pub fn search(&self, req: SearchRequest, compute: &Compute) -> Result<SearchResult> {
        let rows_per_block = (BLOCK_ELEMS / field_dim(&self.schema.fields[self.vector_pos])).max(1);
        self.search_in_blocks(req, compute, rows_per_block)
    }

    fn search_in_blocks(
        &self,
        req: SearchRequest,
        compute: &Compute,
        rows_per_block: usize,
    ) -> Result<SearchResult> {
        let projection = Projection::new(&self.schema, &req.output_fields, ApiKind::Search);
        // 3.x：没有查询向量时在一切检查之前返回空结果。
        if matches!(&req.vectors, QueryVectors::Dense(v) if v.is_empty()) {
            return Ok(SearchResult {
                projection,
                metric: None,
                hits: Vec::new(),
            });
        }
        self.require_loaded()?;
        let field_pos = self.resolve_anns_field(req.anns_field.as_deref())?;
        let field = &self.schema.fields[field_pos];
        let vectors = match &req.vectors {
            QueryVectors::Dense(v) => v,
            // 3.x `_maybe_embed_queries`：字段上没有 TEXT_EMBEDDING function（M1 不支持 function）。
            QueryVectors::Text => {
                return Err(EngineError::schema(format!(
                    "Text query on field {} requires a TEXT_EMBEDDING function targeting that field",
                    py_repr(&field.name)
                )));
            }
            // 3.x 把稀疏向量（dict）交给 numpy 转成稠密数组时的 TypeError。
            QueryVectors::Sparse => {
                return Err(EngineError::invalid(
                    "float() argument must be a string or a real number, not 'dict'",
                ));
            }
        };
        let metric = match &req.metric_type {
            Some(m) => Metric::parse(m)?,
            None => self.default_metric(&field.name)?,
        };
        let dim = field_dim(field);
        // 3.x 不校验，维度不符时报 numpy 的原始错误，空 collection 上反而成功（0004「决策」）。
        for v in vectors {
            if v.len() != dim {
                return Err(EngineError::invalid(format!(
                    "query vector has dim {}, but field {} expects dim {dim}",
                    v.len(),
                    py_repr(&field.name)
                )));
            }
        }
        // M1 只允许一个 FLOAT_VECTOR 字段，范数只为它计算。
        debug_assert_eq!(field_pos, self.vector_pos);
        let take = req.limit.saturating_add(req.offset);
        let queries: Vec<flat::Query> = vectors
            .iter()
            .map(|v| flat::Query::new(metric, v))
            .collect();
        let mut tops: Vec<TopK> = queries.iter().map(|_| TopK::new(metric, take)).collect();
        let blocks = row_blocks(&self.chunks, rows_per_block);
        if take > 0 && !blocks.is_empty() {
            let groups: Vec<&[flat::Query]> = queries.chunks(flat::BATCH).collect();
            let per_wave = groups_per_wave(compute.threads(), blocks.len());
            for wave in (0..groups.len()).step_by(per_wave) {
                let mut leaves: Vec<Leaf> = (wave..groups.len().min(wave + per_wave))
                    .flat_map(|group| {
                        blocks.iter().map(move |block| Leaf {
                            group,
                            block,
                            tops: Vec::new(),
                        })
                    })
                    .collect();
                compute.for_each(&mut leaves, |leaf| {
                    let queries = groups[leaf.group];
                    leaf.tops = queries.iter().map(|_| TopK::new(metric, take)).collect();
                    scan(&self.chunks, leaf.block, field_pos, queries, &mut leaf.tops);
                    leaf.tops.iter_mut().for_each(TopK::shrink);
                });
                for leaf in leaves {
                    for (i, top) in leaf.tops.into_iter().enumerate() {
                        tops[leaf.group * flat::BATCH + i].absorb(top);
                    }
                }
            }
        }
        let hits = tops
            .into_iter()
            .map(|top| {
                let ranked = top.finish();
                let kept = ranked.get(req.offset..).unwrap_or(&[]);
                let refs: Vec<RowRef> = kept.iter().map(|(_, r)| *r).collect();
                Hits {
                    scores: kept.iter().map(|(s, _)| *s).collect(),
                    rows: self.gather(&refs, &projection),
                }
            })
            .collect();
        Ok(SearchResult {
            projection,
            metric: Some(metric),
            hits,
        })
    }

    /// 3.x `_require_loaded`：检索与按主键读取之前的 load 状态检查。
    pub fn require_loaded(&self) -> Result<()> {
        if self.deleted {
            return Err(EngineError::CollectionNotFound(self.name.clone()));
        }
        match self.load_state {
            LoadState::Loaded => Ok(()),
            LoadState::Released => Err(EngineError::CollectionNotLoaded(self.name.clone())),
        }
    }

    fn resolve_anns_field(&self, anns_field: Option<&str>) -> Result<usize> {
        let Some(name) = anns_field else {
            return Ok(self.vector_pos);
        };
        let pos = self
            .schema
            .fields
            .iter()
            .position(|f| f.name == name)
            .ok_or_else(|| {
                EngineError::schema(format!("anns_field {} not found in schema", py_repr(name)))
            })?;
        let field = &self.schema.fields[pos];
        if !field.data_type.is_vector() {
            return Err(EngineError::schema(format!(
                "anns_field {} is not a vector field (dtype={})",
                py_repr(name),
                field.data_type.name()
            )));
        }
        Ok(pos)
    }

    /// 3.x：向量字段有索引且度量不是 NONE 时用索引的度量，否则 COSINE。
    fn default_metric(&self, field_name: &str) -> Result<Metric> {
        match self.index(field_name) {
            Some(spec) if spec.metric_type != "NONE" => Metric::parse(&spec.metric_type),
            _ => Ok(Metric::Cosine),
        }
    }

    fn gather(&self, refs: &[RowRef], projection: &Projection) -> Rows {
        let pks = refs
            .iter()
            .map(|r| pk_at(&self.chunks[r.chunk].columns[self.pk_pos], r.row))
            .collect();
        let fields = projection
            .schema_fields
            .iter()
            .map(|&i| {
                let field = &self.schema.fields[i];
                let mut column = ColumnData::empty(field);
                for r in refs {
                    column.push_row(&self.chunks[r.chunk].columns[i], r.row);
                }
                (field.name.clone(), column)
            })
            .collect();
        let meta = self.schema.enable_dynamic_field.then(|| {
            refs.iter()
                .map(|r| {
                    let full = self.chunks[r.chunk]
                        .meta
                        .as_ref()
                        .and_then(|m| m.get(r.row));
                    match full {
                        Some(full) if projection.all_dynamic => full.clone(),
                        Some(full) => projection
                            .dynamic_keys
                            .iter()
                            .filter_map(|k| full.get(k).map(|v| (k.clone(), v.clone())))
                            .collect(),
                        None => Map::new(),
                    }
                })
                .collect()
        });
        Rows { pks, fields, meta }
    }

    /// 解出每行的动态字段：`$meta` 列（JSON 对象）与不在 schema 中的列，按列顺序合并。
    ///
    /// 3.x 在解码阶段不会因动态字段报错；本实现新增的错误（0004「决策」）按行记下，
    /// 在该行其余 3.x 检查之后才报告，使同时含两类错误的批次报出与 3.x 相同的错误。
    fn decode_dynamic(&self, batch: &InsertBatch) -> Dynamic {
        let n = batch.num_rows;
        let mut maps = vec![Map::new(); n];
        let mut errors: Vec<Option<String>> = vec![None; n];
        for (name, values) in &batch.columns {
            let is_meta = name == META_FIELD;
            if !is_meta && self.schema.field(name).is_some() {
                continue;
            }
            for row in 0..n {
                if errors[row].is_some() {
                    continue;
                }
                let decoded = if is_meta {
                    self.decode_meta(values, row)
                } else if !self.schema.enable_dynamic_field {
                    // 未开启动态字段时 3.x 只报多余字段的名字，不看它的值。
                    Ok(vec![(name.clone(), Value::Null)])
                } else {
                    to_json(name, values, row).map(|v| vec![(name.clone(), v)])
                };
                match decoded {
                    Ok(entries) => maps[row].extend(entries),
                    Err(e) => errors[row] = Some(e.to_string()),
                }
            }
        }
        Dynamic { maps, errors }
    }

    fn decode_meta(&self, values: &InputValues, row: usize) -> Result<Vec<(String, Value)>> {
        let InputValues::Json(texts) = values else {
            return Err(EngineError::schema(format!(
                "dynamic field {} must carry JSON data",
                py_repr(META_FIELD)
            )));
        };
        let Value::Object(obj) = parse_dynamic_json(META_FIELD, &texts[row])? else {
            return Err(EngineError::schema(format!(
                "dynamic field {} value must be a JSON object",
                py_repr(META_FIELD)
            )));
        };
        if let Some(key) = obj.keys().find(|k| self.schema.field(k).is_some()) {
            return Err(EngineError::schema(format!(
                "dynamic field key {} conflicts with schema field {}",
                py_repr(key),
                py_repr(key)
            )));
        }
        Ok(obj.into_iter().collect())
    }

    /// 与 3.x `validate_record` 的检查顺序一致：主键、向量、其余字段（schema 顺序）、多余字段。
    fn validate_row(
        &self,
        by_name: &HashMap<&str, &InputValues>,
        dynamic: &Dynamic,
        row: usize,
    ) -> Result<()> {
        let pk = &self.schema.fields[self.pk_pos];
        if !pk.auto_id && !by_name.contains_key(pk.name.as_str()) {
            return Err(EngineError::schema(format!(
                "primary key {} missing or None",
                py_repr(&pk.name)
            )));
        }
        let vector = &self.schema.fields[self.vector_pos];
        match by_name.get(vector.name.as_str()) {
            None => {
                return Err(EngineError::schema(format!(
                    "vector field {} missing or None",
                    py_repr(&vector.name)
                )));
            }
            Some(InputValues::FloatVector { dim, .. }) => {
                if *dim != field_dim(vector) {
                    return Err(EngineError::schema(format!(
                        "vector field {} expected dim {}, got {dim}",
                        py_repr(&vector.name),
                        field_dim(vector)
                    )));
                }
            }
            Some(other) => {
                return Err(EngineError::schema(format!(
                    "vector field {} must be list/tuple, got {}",
                    py_repr(&vector.name),
                    py_type_name(other, row)
                )));
            }
        }
        for (i, field) in self.schema.fields.iter().enumerate() {
            if field.data_type.is_vector() {
                continue;
            }
            let Some(values) = by_name.get(field.name.as_str()) else {
                if i == self.pk_pos && pk.auto_id {
                    continue;
                }
                return Err(EngineError::schema(format!(
                    "field {} missing and not nullable / no default",
                    py_repr(&field.name)
                )));
            };
            check_value(field, values, row)?;
        }
        if let Some(msg) = &dynamic.errors[row] {
            return Err(EngineError::schema(msg.clone()));
        }
        if !self.schema.enable_dynamic_field && !dynamic.maps[row].is_empty() {
            return Err(EngineError::schema(format!(
                "record has fields {} not in schema (enable_dynamic_field is False)",
                py_repr_list(dynamic.maps[row].keys().map(String::as_str))
            )));
        }
        Ok(())
    }
}

struct Dynamic {
    /// 每行合并后的动态字段；落盘时由 pyjson 显式按 key 排序。
    maps: Vec<Map<String, Value>>,
    /// 每行解码动态字段时遇到的第一个错误。
    errors: Vec<Option<String>>,
}

/// 每个行块的计算量约为这么多个浮点数（0005，初值，由基准调整）。
const BLOCK_ELEMS: usize = 1 << 20;

/// 扫描顺序上连续的一段行。
struct Segment {
    chunk: usize,
    rows: std::ops::Range<usize>,
}

/// 按扫描顺序把全部行切成每块 `rows_per_block` 行（最后一块可以更少）；块可以跨 chunk，
/// 也可以落在 chunk 内部，一次大批量写入的 chunk 同样能并行。
fn row_blocks(chunks: &[Chunk], rows_per_block: usize) -> Vec<Vec<Segment>> {
    let mut blocks = Vec::new();
    let mut current = Vec::new();
    let mut filled = 0;
    for (chunk, c) in chunks.iter().enumerate() {
        let mut start = 0;
        while start < c.live.len() {
            let end = c.live.len().min(start + rows_per_block - filled);
            current.push(Segment {
                chunk,
                rows: start..end,
            });
            filled += end - start;
            start = end;
            if filled == rows_per_block {
                blocks.push(std::mem::take(&mut current));
                filled = 0;
            }
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

/// 每波提交的查询组数：刚好填满计算池（0005「查询组分波提交」）。同时存活的候选数因此与查询条数无关。
fn groups_per_wave(threads: usize, blocks: usize) -> usize {
    threads.div_ceil(blocks.max(1)).max(1)
}

/// 一个叶子任务：一个查询组在一个行块上的前 k 名。
struct Leaf<'a> {
    group: usize,
    block: &'a [Segment],
    tops: Vec<TopK>,
}

/// 为一个查询组（至多 [`flat::BATCH`] 条）扫描一个行块中的存活行，候选交给各自的 [`TopK`]。
/// 组满 [`flat::BATCH`] 条时一起打分，共用读入的候选向量。
fn scan(
    chunks: &[Chunk],
    block: &[Segment],
    field_pos: usize,
    queries: &[flat::Query],
    tops: &mut [TopK],
) {
    for seg in block {
        let chunk = &chunks[seg.chunk];
        let (dim, values) = vector_values(&chunk.columns[field_pos]);
        let live_rows = || {
            seg.rows
                .clone()
                .filter(|row| chunk.live[*row])
                .map(|row| (row, &values[row * dim..(row + 1) * dim], chunk.norms[row]))
        };
        let at = |row| RowRef {
            order: chunk.orders[row],
            chunk: seg.chunk,
            row,
        };
        if let Ok(batch) = <&[flat::Query; flat::BATCH]>::try_from(queries) {
            for (row, candidate, norm) in live_rows() {
                let scores = flat::score_batch(batch, candidate, norm);
                for (top, score) in tops.iter_mut().zip(scores) {
                    top.push(score, at(row));
                }
            }
        } else {
            for (query, top) in queries.iter().zip(tops.iter_mut()) {
                for (row, candidate, norm) in live_rows() {
                    top.push(query.score(candidate, norm), at(row));
                }
            }
        }
    }
}

/// 一条查询的前 `take` 个结果，按 (分数的名次, 行的位置) 这一全序排列。
///
/// 与对全部分数按名次做稳定排序的结果完全相同：扫描按位置递增进行，稳定排序在名次相同时保持扫描顺序，
/// 即按位置排序。只保留可能进入前 `take` 个的候选，不收集全部分数。
struct TopK {
    metric: Metric,
    take: usize,
    candidates: Vec<(f32, RowRef)>,
    /// 已确定的第 `take` 名；不排在它前面的候选不可能进入结果。
    bound: Option<(f32, RowRef)>,
}

impl TopK {
    fn new(metric: Metric, take: usize) -> Self {
        Self {
            metric,
            take,
            candidates: Vec::new(),
            bound: None,
        }
    }

    fn push(&mut self, score: f32, pos: RowRef) {
        let candidate = (score, pos);
        if self.take == 0
            || self
                .bound
                .is_some_and(|b| order(self.metric, &candidate, &b).is_ge())
        {
            return;
        }
        self.candidates.push(candidate);
        // 攒够一批再筛，均摊每个候选的筛选代价。
        if self.candidates.len() >= self.take.saturating_mul(2).max(64) {
            self.shrink();
        }
    }

    /// 只留前 `take` 个，并记下第 `take` 名。
    fn shrink(&mut self) {
        let k = self.take;
        if k == 0 {
            self.candidates.clear();
        } else if self.candidates.len() > k {
            let metric = self.metric;
            self.candidates
                .select_nth_unstable_by(k - 1, |a, b| order(metric, a, b));
            self.candidates.truncate(k);
            self.bound = Some(self.candidates[k - 1]);
        }
    }

    /// 并入另一部分行的前 `take` 名。结果与推入的先后无关：被跳过的候选前面总有至少 `take` 个更好的。
    fn absorb(&mut self, other: TopK) {
        for (score, pos) in other.candidates {
            self.push(score, pos);
        }
    }

    fn finish(mut self) -> Vec<(f32, RowRef)> {
        self.shrink();
        let metric = self.metric;
        // 位置各不相同，全序下没有相等的元素，不稳定排序即可。
        self.candidates.sort_unstable_by(|a, b| order(metric, a, b));
        self.candidates
    }
}

fn order(metric: Metric, a: &(f32, RowRef), b: &(f32, RowRef)) -> std::cmp::Ordering {
    flat::rank(metric, a.0, b.0).then(a.1.cmp(&b.1))
}

fn vector_values(column: &ColumnData) -> (usize, &[f32]) {
    match column {
        ColumnData::FloatVector { dim, values } => (*dim, values),
        // resolve_anns_field 只返回 FLOAT_VECTOR 字段的下标。
        _ => unreachable!("anns field column must be FLOAT_VECTOR"),
    }
}

fn input_pk(values: &InputValues, row: usize) -> Option<Pk> {
    match values {
        InputValues::Int(v) => Some(Pk::Int64(v[row])),
        InputValues::Str(v) => Some(Pk::VarChar(v[row].clone())),
        _ => None,
    }
}

fn input_row(values: &InputValues, row: usize) -> InputValues {
    match values {
        InputValues::Bool(v) => InputValues::Bool(vec![v[row]]),
        InputValues::Int(v) => InputValues::Int(vec![v[row]]),
        InputValues::Float(v) => InputValues::Float(vec![v[row]]),
        InputValues::Str(v) => InputValues::Str(vec![v[row].clone()]),
        InputValues::Json(v) => InputValues::Json(vec![v[row].clone()]),
        InputValues::FloatVector { dim, values } => InputValues::FloatVector {
            dim: *dim,
            values: values[row * dim..(row + 1) * dim].to_vec(),
        },
    }
}

fn input_from_column(column: &ColumnData, row: usize) -> InputValues {
    match column {
        ColumnData::Bool(v) => InputValues::Bool(vec![v[row]]),
        ColumnData::Int8(v) => InputValues::Int(vec![i64::from(v[row])]),
        ColumnData::Int16(v) => InputValues::Int(vec![i64::from(v[row])]),
        ColumnData::Int32(v) => InputValues::Int(vec![i64::from(v[row])]),
        ColumnData::Int64(v) => InputValues::Int(vec![v[row]]),
        ColumnData::Float(v) => InputValues::Float(vec![f64::from(v[row])]),
        ColumnData::Double(v) => InputValues::Float(vec![v[row]]),
        ColumnData::VarChar(v) => InputValues::Str(vec![v[row].clone()]),
        ColumnData::Json(v) => InputValues::Json(vec![v[row].clone()]),
        ColumnData::FloatVector { dim, values } => InputValues::FloatVector {
            dim: *dim,
            values: values[row * dim..(row + 1) * dim].to_vec(),
        },
    }
}

fn input_from_column_all(column: &ColumnData) -> InputValues {
    match column {
        ColumnData::Bool(v) => InputValues::Bool(v.clone()),
        ColumnData::Int8(v) => InputValues::Int(v.iter().map(|x| i64::from(*x)).collect()),
        ColumnData::Int16(v) => InputValues::Int(v.iter().map(|x| i64::from(*x)).collect()),
        ColumnData::Int32(v) => InputValues::Int(v.iter().map(|x| i64::from(*x)).collect()),
        ColumnData::Int64(v) => InputValues::Int(v.clone()),
        ColumnData::Float(v) => InputValues::Float(v.iter().map(|x| f64::from(*x)).collect()),
        ColumnData::Double(v) => InputValues::Float(v.clone()),
        ColumnData::VarChar(v) => InputValues::Str(v.clone()),
        ColumnData::Json(v) => InputValues::Json(v.clone()),
        ColumnData::FloatVector { dim, values } => InputValues::FloatVector {
            dim: *dim,
            values: values.clone(),
        },
    }
}

fn pk_at(column: &ColumnData, row: usize) -> Pk {
    match column {
        ColumnData::Int64(v) => Pk::Int64(v[row]),
        ColumnData::VarChar(v) => Pk::VarChar(v[row].clone()),
        // schema 校验保证主键列只能是 INT64 或 VARCHAR。
        _ => unreachable!("primary key column must be INT64 or VARCHAR"),
    }
}

fn parse_dynamic_json(field: &str, text: &[u8]) -> Result<Value> {
    parse_json(field, text)
}

fn parse_json(field: &str, text: &[u8]) -> Result<Value> {
    serde_json::from_slice(text).map_err(|e| {
        EngineError::schema(format!(
            "field {} value is not valid JSON: {e}",
            py_repr(field)
        ))
    })
}

/// 不在 schema 中的列作为动态字段时的 JSON 值。
fn to_json(name: &str, values: &InputValues, row: usize) -> Result<Value> {
    Ok(match values {
        InputValues::Bool(v) => Value::Bool(v[row]),
        InputValues::Int(v) => Value::from(v[row]),
        InputValues::Float(v) => json_number(name, v[row])?,
        InputValues::Str(v) => Value::String(v[row].clone()),
        InputValues::Json(v) => parse_dynamic_json(name, &v[row])?,
        InputValues::FloatVector { dim, values } => Value::Array(
            values[row * dim..(row + 1) * dim]
                .iter()
                .map(|x| json_number(name, f64::from(*x)))
                .collect::<Result<_>>()?,
        ),
    })
}

fn json_number(name: &str, v: f64) -> Result<Value> {
    serde_json::Number::from_f64(v)
        .map(Value::Number)
        .ok_or_else(|| {
            EngineError::schema(format!(
                "field {} value {} is not representable in JSON",
                py_repr(name),
                py_float_repr(v)
            ))
        })
}

/// 3.x `_DTYPE_PYTHON_CHECK` 与 VARCHAR 长度检查。
fn check_value(field: &FieldSchema, values: &InputValues, row: usize) -> Result<()> {
    let ok = match (field.data_type, values) {
        (DataType::Bool, InputValues::Bool(_)) => true,
        (DataType::Int8, InputValues::Int(v)) => i8::try_from(v[row]).is_ok(),
        (DataType::Int16, InputValues::Int(v)) => i16::try_from(v[row]).is_ok(),
        (DataType::Int32, InputValues::Int(v)) => i32::try_from(v[row]).is_ok(),
        (DataType::Int64, InputValues::Int(_)) => true,
        (DataType::Float | DataType::Double, InputValues::Int(_) | InputValues::Float(_)) => true,
        (DataType::VarChar, InputValues::Str(v)) => {
            if let Some(max) = field.max_length {
                let len = v[row].chars().count();
                if i64::try_from(len).is_ok_and(|l| l > max) {
                    return Err(EngineError::schema(format!(
                        "VARCHAR field {} value length {len} exceeds max_length={max}",
                        py_repr(&field.name)
                    )));
                }
            }
            true
        }
        (DataType::Json, InputValues::Json(v)) => {
            // JSON 的 null 在 3.x 中即 None。
            if parse_json(&field.name, &v[row])?.is_null() {
                return Err(EngineError::schema(format!(
                    "field {} is None but not nullable",
                    py_repr(&field.name)
                )));
            }
            true
        }
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(EngineError::schema(format!(
            "field {} value {} does not match dtype {}",
            py_repr(&field.name),
            py_value_repr(values, row),
            field.data_type.qualified()
        )))
    }
}

/// 校验通过后按字段类型转换为存储列。
fn convert(field: &FieldSchema, values: &InputValues) -> Result<ColumnData> {
    let mismatch = || {
        EngineError::schema(format!(
            "field {} values do not match dtype {}",
            py_repr(&field.name),
            field.data_type.qualified()
        ))
    };
    Ok(match (field.data_type, values) {
        (DataType::Bool, InputValues::Bool(v)) => ColumnData::Bool(v.clone()),
        (DataType::Int8, InputValues::Int(v)) => ColumnData::Int8(narrow(v).ok_or_else(mismatch)?),
        (DataType::Int16, InputValues::Int(v)) => {
            ColumnData::Int16(narrow(v).ok_or_else(mismatch)?)
        }
        (DataType::Int32, InputValues::Int(v)) => {
            ColumnData::Int32(narrow(v).ok_or_else(mismatch)?)
        }
        (DataType::Int64, InputValues::Int(v)) => ColumnData::Int64(v.clone()),
        // 3.x 以 float32 存 FLOAT、float64 存 DOUBLE；整数由 pyarrow 转换，不能精确表示时报错。
        (DataType::Float, InputValues::Int(v)) => ColumnData::Float(
            v.iter()
                .map(|x| exact_int(*x, 1 << 24, "single").map(|x| x as f32))
                .collect::<Result<_>>()?,
        ),
        (DataType::Float, InputValues::Float(v)) => {
            ColumnData::Float(v.iter().map(|x| *x as f32).collect())
        }
        (DataType::Double, InputValues::Int(v)) => ColumnData::Double(
            v.iter()
                .map(|x| exact_int(*x, 1 << 53, "double").map(|x| x as f64))
                .collect::<Result<_>>()?,
        ),
        (DataType::Double, InputValues::Float(v)) => ColumnData::Double(v.clone()),
        (DataType::VarChar, InputValues::Str(v)) => ColumnData::VarChar(v.clone()),
        (DataType::Json, InputValues::Json(v)) => ColumnData::Json(v.clone()),
        (DataType::FloatVector, InputValues::FloatVector { dim, values }) => {
            ColumnData::FloatVector {
                dim: *dim,
                values: values.clone(),
            }
        }
        _ => return Err(mismatch()),
    })
}

/// pyarrow 把整数转成浮点时要求能精确表示（|x| ≤ 2^24 或 2^53），否则报 ArrowInvalid（3.x 中 code 1）。
fn exact_int(x: i64, limit: i64, precision: &str) -> Result<i64> {
    if x.unsigned_abs() > limit.unsigned_abs() {
        return Err(EngineError::invalid(format!(
            "Integer value {x} is outside of the range exactly representable by a IEEE 754 {precision} precision value"
        )));
    }
    Ok(x)
}

fn narrow<T: TryFrom<i64>>(v: &[i64]) -> Option<Vec<T>> {
    v.iter().map(|x| T::try_from(*x).ok()).collect()
}

/// Python `type(value).__name__`。
fn py_type_name(values: &InputValues, row: usize) -> &'static str {
    match values {
        InputValues::Bool(_) => "bool",
        InputValues::Int(_) => "int",
        InputValues::Float(_) => "float",
        InputValues::Str(_) => "str",
        InputValues::FloatVector { .. } => "list",
        InputValues::Json(v) => match serde_json::from_slice::<Value>(&v[row]) {
            Ok(Value::Object(_)) => "dict",
            Ok(Value::Array(_)) => "list",
            Ok(Value::String(_)) => "str",
            Ok(Value::Bool(_)) => "bool",
            Ok(Value::Number(n)) if n.is_f64() => "float",
            Ok(Value::Number(_)) => "int",
            Ok(Value::Null) | Err(_) => "NoneType",
        },
    }
}

/// Python `repr(value)`，用于 3.x 的 `value {value!r} does not match dtype` 信息。
fn py_value_repr(values: &InputValues, row: usize) -> String {
    match values {
        InputValues::Bool(v) => if v[row] { "True" } else { "False" }.to_owned(),
        InputValues::Int(v) => v[row].to_string(),
        InputValues::Float(v) => py_float_repr(v[row]),
        InputValues::Str(v) => py_repr(&v[row]),
        InputValues::FloatVector { dim, values } => {
            let items: Vec<String> = values[row * dim..(row + 1) * dim]
                .iter()
                .map(|x| py_float_repr(f64::from(*x)))
                .collect();
            format!("[{}]", items.join(", "))
        }
        InputValues::Json(v) => match serde_json::from_slice::<Value>(&v[row]) {
            Ok(value) => py_json_repr(&value),
            Err(_) => py_repr(&String::from_utf8_lossy(&v[row])),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::num::NonZeroUsize;

    use crate::data::{InputValues, InsertBatch};
    use crate::schema::FieldSchema;

    struct Rng(u32);

    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            self.next() as usize % n
        }
    }

    fn collection(rng: &mut Rng, dim: usize) -> Collection {
        let schema = CollectionSchema {
            fields: vec![
                FieldSchema {
                    is_primary: true,
                    ..FieldSchema::new("id", DataType::Int64)
                },
                FieldSchema {
                    dim: Some(dim as i64),
                    ..FieldSchema::new("v", DataType::FloatVector)
                },
            ],
            enable_dynamic_field: false,
            properties: Vec::new(),
        };
        let mut c = Collection::new("c", schema).unwrap();
        let insert = |c: &mut Collection, rng: &mut Rng, pks: Vec<i64>| {
            // 取值很少的分量制造大量并列，偶尔插入 NaN 行。
            let values = (0..pks.len() * dim)
                .map(|_| match rng.below(40) {
                    0 => f32::NAN,
                    r => (r % 4) as f32 - 1.5,
                })
                .collect();
            c.insert(InsertBatch {
                num_rows: pks.len(),
                columns: vec![
                    ("id".to_owned(), InputValues::Int(pks)),
                    ("v".to_owned(), InputValues::FloatVector { dim, values }),
                ],
            })
            .unwrap();
        };
        // 第一批随后被整批覆盖：行块中可能没有任何存活行。
        insert(&mut c, rng, (0..20).collect());
        insert(&mut c, rng, (0..20).collect());
        for _ in 0..rng.below(8) {
            let n = rng.below(40);
            let pks = (0..n).map(|_| rng.below(200) as i64).collect::<Vec<_>>();
            let mut seen = std::collections::HashSet::new();
            let pks = pks.into_iter().filter(|p| seen.insert(*p)).collect();
            insert(&mut c, rng, pks);
        }
        c
    }

    /// 独立的参照：每个存活行逐个打分，按名次稳定排序（扫描顺序即位置顺序）。
    fn reference(c: &Collection, req: &SearchRequest, metric: Metric) -> Vec<Vec<(u32, Pk)>> {
        let QueryVectors::Dense(vectors) = &req.vectors else {
            unreachable!()
        };
        vectors
            .iter()
            .map(|v| {
                let q = flat::Query::new(metric, v);
                let mut scored = Vec::new();
                for chunk in &c.chunks {
                    let (dim, values) = vector_values(&chunk.columns[c.vector_pos]);
                    for row in 0..chunk.live.len() {
                        if chunk.live[row] {
                            let cand = &values[row * dim..(row + 1) * dim];
                            let pk = pk_at(&chunk.columns[c.pk_pos], row);
                            scored.push((q.score(cand, flat::norm(cand)), chunk.orders[row], pk));
                        }
                    }
                }
                scored.sort_by(|a, b| flat::rank(metric, a.0, b.0).then(a.1.cmp(&b.1)));
                scored
                    .into_iter()
                    .take(req.limit.saturating_add(req.offset))
                    .skip(req.offset)
                    .map(|(s, _, pk)| (s.to_bits(), pk))
                    .collect()
            })
            .collect()
    }

    /// 0005「验证」：行块边界落在 chunk 内部、行块中没有存活行、各种查询条数与 limit / offset、
    /// 并列与 NaN 下，并行结果与串行参照逐位相同。
    #[test]
    fn parallel_search_matches_serial_reference() {
        let mut rng = Rng(0x1234_5678);
        let pools = [1, 3].map(|n| Compute::new(NonZeroUsize::new(n).unwrap()).unwrap());
        for case in 0..400 {
            let dim = [1, 3, 17][case % 3];
            let c = collection(&mut rng, dim);
            let metric = [Metric::L2, Metric::Ip, Metric::Cosine][rng.below(3)];
            let nq = [1, 3, 4, 5, 9][rng.below(5)];
            let (limit, offset) =
                [(0, 0), (1, 0), (5, 2), (10, 0), (3, 40), (500, 0), (7, 500)][rng.below(7)];
            let vectors = (0..nq)
                .map(|_| (0..dim).map(|_| (rng.below(4) as f32) - 1.5).collect())
                .collect();
            let req = SearchRequest {
                anns_field: None,
                vectors: QueryVectors::Dense(vectors),
                metric_type: Some(metric.as_str().to_owned()),
                limit,
                offset,
                output_fields: Vec::new(),
            };
            let expected = reference(&c, &req, metric);
            for rows_per_block in [1, 2, 7, 64, 10_000] {
                for pool in &pools {
                    let got = c
                        .search_in_blocks(req.clone(), pool, rows_per_block)
                        .unwrap();
                    let got: Vec<Vec<(u32, Pk)>> = got
                        .hits
                        .iter()
                        .map(|h| {
                            h.scores
                                .iter()
                                .map(|s| s.to_bits())
                                .zip(h.rows.pks.iter().cloned())
                                .collect()
                        })
                        .collect();
                    assert_eq!(got, expected, "case {case}, block {rows_per_block}");
                }
            }
        }
    }

    /// 0005「查询组分波提交」：每波的查询组数只由线程数与块数决定，同时存活的候选数与查询条数无关。
    #[test]
    fn groups_per_wave_fills_the_pool_without_depending_on_nq() {
        assert_eq!(groups_per_wave(10, 123), 1);
        assert_eq!(groups_per_wave(10, 13), 1);
        assert_eq!(groups_per_wave(10, 3), 4);
        assert_eq!(groups_per_wave(10, 1), 10);
        assert_eq!(groups_per_wave(1, 0), 1);
    }

    /// TopK 与「对全部分数按名次稳定排序、取前 take 个」逐位相同：并列、±0、±inf、NaN（含负 NaN）都覆盖。
    #[test]
    fn top_k_matches_stable_sort() {
        let mut x = 0x9e37_79b9_u32;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let palette = [
            0.0,
            -0.0,
            1.0,
            1.0,
            2.5,
            -3.0,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            -f32::NAN,
        ];
        for case in 0..3000 {
            let metric = [Metric::L2, Metric::Ip, Metric::Cosine][case % 3];
            let n = (next() % 300) as usize;
            let scored: Vec<(f32, RowRef)> = (0..n)
                .map(|i| {
                    let r = next();
                    let score = if r % 4 == 0 {
                        palette[(r / 4) as usize % palette.len()]
                    } else {
                        (r % 17) as f32 / 4.0
                    };
                    (
                        score,
                        RowRef {
                            order: i as u64,
                            chunk: i / 50,
                            row: i % 50,
                        },
                    )
                })
                .collect();
            let take = match case % 5 {
                0 => 0,
                1 => n,
                2 => n + 3,
                _ => (next() % 80) as usize,
            };
            let mut expected = scored.clone();
            expected.sort_by(|a, b| flat::rank(metric, a.0, b.0));
            expected.truncate(take);
            let mut top = TopK::new(metric, take);
            for (s, pos) in &scored {
                top.push(*s, *pos);
            }
            let got = top.finish();
            let bits =
                |v: &[(f32, RowRef)]| v.iter().map(|(s, p)| (s.to_bits(), *p)).collect::<Vec<_>>();
            assert_eq!(bits(&got), bits(&expected), "case {case}");
        }
    }
}
