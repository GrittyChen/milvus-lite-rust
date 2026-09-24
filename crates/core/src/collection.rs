//! 单个 collection 的内存引擎：写入、按主键读取、计数、暴力检索、索引定义与 load 状态。
//! 语义以 3.x 的 memtable 路径为准；与 3.x 不同之处见 0004「决策」与「阶段性差异」。

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::data::{ColumnData, InputValues, InsertBatch, META_FIELD, Pk, Rows, field_dim};
use crate::error::{EngineError, Result, py_float_repr, py_repr, py_repr_list};
use crate::flat;
use crate::index::{IndexRequest, IndexSpec, Metric, build_spec};
use crate::projection::{ApiKind, Projection};
use crate::schema::{CollectionSchema, DataType, FieldSchema};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadState {
    Loaded,
    Released,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchRequest {
    /// 为空时取第一个 FLOAT_VECTOR 字段。
    pub anns_field: Option<String>,
    pub vectors: Vec<Vec<f32>>,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowRef {
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
        })
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

    /// 保存索引定义，不改变 load 状态（3.x 的 Milvus 语义）。
    pub fn create_index(&mut self, field_name: &str, req: IndexRequest) -> Result<()> {
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
        self.indexes.push(spec);
        Ok(())
    }

    /// 写入一批数据，逐行返回主键（批内重复的主键原样重复）。
    ///
    /// 校验全部通过才写入；校验顺序与错误信息与 3.x 的 `validate_record` 一致，
    /// 畸形的 JSON 与动态字段按 0004「决策」明确报错。
    pub fn insert(&mut self, batch: InsertBatch) -> Result<Vec<Pk>> {
        let n = batch.num_rows;
        // 3.x：num_rows 为 0 时不看列内容，直接返回空结果。
        if n == 0 {
            return Ok(Vec::new());
        }
        for (name, values) in &batch.columns {
            check_column_len(name, values, n)?;
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
        let seqs: Vec<u64> = (0..n as u64).map(|i| first_seq + i).collect();
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
        self.next_seq += n as u64;

        let meta = self.schema.enable_dynamic_field.then_some(dynamic.maps);
        let chunk_idx = self.chunks.len();
        let mut chunk = Chunk {
            columns,
            meta,
            live: vec![true; n],
        };
        let mut ids = Vec::with_capacity(n);
        for row in 0..n {
            let pk = pk_at(&chunk.columns[self.pk_pos], row);
            let new_ref = RowRef {
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

    pub fn search(&self, req: SearchRequest) -> Result<SearchResult> {
        let projection = Projection::new(&self.schema, &req.output_fields, ApiKind::Search);
        // 3.x：没有查询向量时在一切检查之前返回空结果。
        if req.vectors.is_empty() {
            return Ok(SearchResult {
                projection,
                hits: Vec::new(),
            });
        }
        self.require_loaded()?;
        let field_pos = self.resolve_anns_field(req.anns_field.as_deref())?;
        let field = &self.schema.fields[field_pos];
        let metric = match &req.metric_type {
            Some(m) => Metric::parse(m)?,
            None => self.default_metric(&field.name)?,
        };
        let dim = field_dim(field);
        // 3.x 不校验，维度不符时报 numpy 的原始错误，空 collection 上反而成功（0004「决策」）。
        for v in &req.vectors {
            if v.len() != dim {
                return Err(EngineError::invalid(format!(
                    "query vector has dim {}, but field {} expects dim {dim}",
                    v.len(),
                    py_repr(&field.name)
                )));
            }
        }
        let take = req.limit.saturating_add(req.offset);
        let hits = req
            .vectors
            .iter()
            .map(|v| {
                let query = flat::Query::new(metric, v);
                let mut scored: Vec<(f32, RowRef)> = Vec::new();
                for (ci, chunk) in self.chunks.iter().enumerate() {
                    let (dim, values) = vector_values(&chunk.columns[field_pos]);
                    for (row, live) in chunk.live.iter().enumerate() {
                        if *live {
                            let score = query.score(&values[row * dim..(row + 1) * dim]);
                            scored.push((score, RowRef { chunk: ci, row }));
                        }
                    }
                }
                // 稳定排序：分数相同时保持扫描顺序。
                scored.sort_by(|a, b| query.rank(a.0, b.0));
                scored.truncate(take);
                let kept = scored.get(req.offset..).unwrap_or(&[]);
                let refs: Vec<RowRef> = kept.iter().map(|(_, r)| *r).collect();
                Hits {
                    scores: kept.iter().map(|(s, _)| *s).collect(),
                    rows: self.gather(&refs, &projection),
                }
            })
            .collect();
        Ok(SearchResult { projection, hits })
    }

    fn require_loaded(&self) -> Result<()> {
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
        let Value::Object(obj) = parse_json(META_FIELD, &texts[row])? else {
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
    /// 每行合并后的动态字段；serde_json 的 Map 按 key 排序，与 3.x `json.dumps(sort_keys=True)` 一致。
    maps: Vec<Map<String, Value>>,
    /// 每行解码动态字段时遇到的第一个错误。
    errors: Vec<Option<String>>,
}

/// 3.x 在解码 FieldData 时的行数检查（`translators/records.py`）。
fn check_column_len(name: &str, values: &InputValues, n: usize) -> Result<()> {
    if let InputValues::FloatVector { dim, values } = values {
        let expected = n.saturating_mul(*dim);
        if *dim == 0 || values.len() != expected {
            return Err(EngineError::schema(format!(
                "FieldData {} float_vector data has {} elements, expected num_rows({n}) * dim({dim}) = {expected}",
                py_repr(name),
                values.len()
            )));
        }
        return Ok(());
    }
    if values.len() != n {
        return Err(EngineError::schema(format!(
            "FieldData {} has {} rows, expected {n}",
            py_repr(name),
            values.len()
        )));
    }
    Ok(())
}

fn vector_values(column: &ColumnData) -> (usize, &[f32]) {
    match column {
        ColumnData::FloatVector { dim, values } => (*dim, values),
        // resolve_anns_field 只返回 FLOAT_VECTOR 字段的下标。
        _ => unreachable!("anns field column must be FLOAT_VECTOR"),
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
        InputValues::Json(v) => parse_json(name, &v[row])?,
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
            Ok(value) => json_py_repr(&value),
            Err(_) => py_repr(&String::from_utf8_lossy(&v[row])),
        },
    }
}

fn json_py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_owned(),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => i.to_string(),
            (None, Some(u), _) => u.to_string(),
            (None, None, Some(f)) => py_float_repr(f),
            (None, None, None) => n.to_string(),
        },
        Value::String(s) => py_repr(s),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(json_py_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(obj) => {
            let parts: Vec<String> = obj
                .iter()
                .map(|(k, v)| format!("{}: {}", py_repr(k), json_py_repr(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}
