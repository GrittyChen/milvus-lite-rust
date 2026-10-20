//! 单个 collection 的内存引擎：写入、按主键读取、计数、暴力检索、索引定义与 load 状态。
//! 语义以 3.x 的 memtable 路径为准；与 3.x 不同之处见 0004「决策」与「阶段性差异」。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::{Map, Value};

mod partition_key;

use crate::compute::Compute;
use crate::data::{
    ColumnData, InputValues, InsertBatch, META_FIELD, Pk, PkMap, Rows, check_column_len, field_dim,
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
use crate::storage::visibility::{SourceBits, SourceId};

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

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SearchRange {
    pub radius: Option<f64>,
    pub range_filter: Option<f64>,
}

impl SearchRange {
    fn contains(self, metric: Metric, score: f32) -> bool {
        // 3.x 先将内部 float32 距离转换为公共 float64，再比较范围；响应舍入在此之后。
        let value = metric.public_f64(score);
        if metric.higher_is_better() {
            self.radius.is_none_or(|r| value > r) && self.range_filter.is_none_or(|r| value <= r)
        } else {
            self.radius.is_none_or(|r| value < r) && self.range_filter.is_none_or(|r| value >= r)
        }
    }
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

/// 锁内取得的不可变读取视图，不包含全量主键索引或可变持久化状态。
#[derive(Debug)]
pub struct ReadSnapshot {
    name: String,
    schema: CollectionSchema,
    pk_pos: usize,
    vector_pos: usize,
    chunks: Vec<Chunk>,
    indexes: Vec<IndexSpec>,
    load_state: LoadState,
    deleted: bool,
    unusable: bool,
}

/// 主键在锁内定位，投影与结果复制在放锁后完成。
#[derive(Debug)]
pub struct GetSnapshot {
    // chunks 仅为命中块的子集，refs 的块下标已重映射；此私有视图不可用于全量检索。
    snapshot: ReadSnapshot,
    refs: Vec<RowRef>,
    projection: Projection,
}

impl GetSnapshot {
    pub fn name(&self) -> &str {
        self.snapshot.name()
    }
    pub fn schema(&self) -> &CollectionSchema {
        self.snapshot.schema()
    }
    pub fn read(&self) -> QueryResult {
        QueryResult {
            projection: self.projection.clone(),
            rows: self.snapshot.gather(&self.refs, &self.projection),
        }
    }
}

impl ReadSnapshot {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn schema(&self) -> &CollectionSchema {
        &self.schema
    }
    /// 在 `compute` 上并行计算（[0005](../../../docs/decisions/0005-search-thread-model.md)），
    /// 结果与串行计算逐位相同。
    pub fn search(&self, req: SearchRequest, compute: &Compute) -> Result<SearchResult> {
        let rows_per_block = (BLOCK_ELEMS / field_dim(&self.schema.fields[self.vector_pos])).max(1);
        self.search_in_blocks(req, compute, rows_per_block)
    }

    pub fn search_filtered(
        &self,
        req: SearchRequest,
        compute: &Compute,
        expr: &str,
    ) -> Result<SearchResult> {
        self.search_filtered_with_range(req, compute, expr, None)
    }

    pub fn search_filtered_with_range(
        &self,
        req: SearchRequest,
        compute: &Compute,
        expr: &str,
        range: Option<SearchRange>,
    ) -> Result<SearchResult> {
        let rows_per_block = (BLOCK_ELEMS / field_dim(&self.schema.fields[self.vector_pos])).max(1);
        self.search_filtered_in_blocks(req, compute, rows_per_block, expr, range)
    }

    fn search_in_blocks(
        &self,
        req: SearchRequest,
        compute: &Compute,
        rows_per_block: usize,
    ) -> Result<SearchResult> {
        self.search_filtered_in_blocks(req, compute, rows_per_block, "", None)
    }

    fn search_filtered_in_blocks(
        &self,
        req: SearchRequest,
        compute: &Compute,
        rows_per_block: usize,
        expr: &str,
        range: Option<SearchRange>,
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
        let predicate = if expr.is_empty() {
            None
        } else {
            Some(crate::filter::Predicate::compile(expr, &self.schema)?)
        };
        let range = range.filter(|r| r.radius.is_some() || r.range_filter.is_some());
        let end = req.limit.saturating_add(req.offset);
        // 3.x 只在有限近邻窗口内做范围过滤，不能在扫描时提前筛掉范围外候选。
        let take = if range.is_some() {
            end.saturating_mul(5)
        } else {
            end
        };
        // 3.x 编译过滤后才处理 topk<=0，但此时不会进入数据求值或数值转换。
        let chunks = if take > 0 {
            self.filtered_chunks_compiled(predicate.as_ref())?
        } else {
            std::borrow::Cow::Borrowed(self.chunks.as_slice())
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
        let queries: Vec<flat::Query> = vectors
            .iter()
            .map(|v| flat::Query::new(metric, v))
            .collect();
        let mut tops: Vec<TopK> = queries.iter().map(|_| TopK::new(metric, take)).collect();
        let blocks = row_blocks(&chunks, rows_per_block);
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
                    scan(&chunks, leaf.block, field_pos, queries, &mut leaf.tops);
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
                let mut ranked = top.finish();
                if let Some(range) = range {
                    ranked.retain(|(score, _)| range.contains(metric, *score));
                    ranked.truncate(end);
                }
                let kept = ranked.get(req.offset..).unwrap_or(&[]);
                let refs: Vec<RowRef> = kept.iter().map(|(_, r)| r.at).collect();
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

    fn filtered_chunks_parsed(
        &self,
        parsed: &crate::filter::ParsedFilter,
    ) -> Result<std::borrow::Cow<'_, [Chunk]>> {
        let predicate = crate::filter::Predicate::from_parsed(parsed, &self.schema)?;
        // 只有空表达式可直接借用块表；通用布尔树也可能没有旧 Term。
        if parsed.is_empty() {
            return Ok(std::borrow::Cow::Borrowed(&self.chunks));
        }
        self.filtered_chunks_compiled(Some(&predicate))
    }

    fn filtered_chunks_compiled(
        &self,
        predicate: Option<&crate::filter::Predicate>,
    ) -> Result<std::borrow::Cow<'_, [Chunk]>> {
        let Some(predicate) = predicate else {
            return Ok(std::borrow::Cow::Borrowed(&self.chunks));
        };
        let mut chunks = self.chunks.clone();
        if predicate.requires_source_rows() {
            let mut start = 0;
            while start < chunks.len() {
                let source = chunks[start].source;
                let end = start
                    + chunks[start..]
                        .iter()
                        .take_while(|c| c.source == source)
                        .count();
                let (locations, matching) = {
                    let mut locations: Vec<_> = (start..end)
                        .flat_map(|chunk| {
                            chunks[chunk].live.live_rows().map(move |row| (chunk, row))
                        })
                        .collect();
                    // 参照按整个 segment/memtable 推断类型；活跃代的报错顺序沿用主键位次。
                    locations.sort_unstable_by_key(|&(i, row)| {
                        if chunks[i].generation == 0 {
                            (chunks[i].row_start + row) as u64
                        } else {
                            chunks[i].orders[row]
                        }
                    });
                    let rows: Vec<_> = locations
                        .iter()
                        .map(|&(i, row)| crate::filter::DynamicRow {
                            columns: &chunks[i].columns,
                            meta: chunks[i].meta.as_ref().map(|m| &m[row]),
                            row,
                            sequence: Some(chunks[i].seqs[row]),
                        })
                        .collect();
                    let matching = predicate.evaluate_source_rows(&rows)?;
                    (locations, matching)
                };
                for (index, (chunk, row)) in locations.into_iter().enumerate() {
                    if !matching.get(index) {
                        chunks[chunk].live.set(row, false);
                    }
                }
                start = end;
            }
        } else {
            for chunk in &mut chunks {
                chunk.live = predicate.evaluate(&chunk.columns, &chunk.live)?;
            }
        }
        Ok(std::borrow::Cow::Owned(chunks))
    }

    pub fn count_filtered(&self, expr: &str) -> Result<usize> {
        self.require_loaded()?;
        self.count_filtered_parsed(&crate::filter::ParsedFilter::parse(expr)?)
    }

    pub fn count_filtered_parsed(&self, parsed: &crate::filter::ParsedFilter) -> Result<usize> {
        self.require_loaded()?;
        let chunks = self.filtered_chunks_parsed(parsed)?;
        Ok(chunks.iter().map(|chunk| chunk.live.count_live()).sum())
    }

    pub fn query(
        &self,
        expr: &str,
        output_fields: &[String],
        limit: Option<i64>,
        offset: i64,
    ) -> Result<QueryResult> {
        self.require_loaded()?;
        self.query_parsed(
            &crate::filter::ParsedFilter::parse(expr)?,
            output_fields,
            limit,
            offset,
        )
    }

    pub fn query_parsed(
        &self,
        parsed: &crate::filter::ParsedFilter,
        output_fields: &[String],
        limit: Option<i64>,
        offset: i64,
    ) -> Result<QueryResult> {
        self.require_loaded()?;
        self.query_loaded(parsed, output_fields, limit, offset, false)
    }

    /// iterator 每次请求在自己的读快照上按主键排序；跨请求续读条件由客户端提供。
    pub fn query_ordered_parsed(
        &self,
        parsed: &crate::filter::ParsedFilter,
        output_fields: &[String],
        limit: Option<i64>,
        offset: i64,
    ) -> Result<QueryResult> {
        self.require_loaded()?;
        self.query_loaded(parsed, output_fields, limit, offset, true)
    }

    /// Delete 不要求 load；只在自己的不可变视图上绕过 load 检查，不临时改动共享状态。
    pub(crate) fn query_for_delete(&self, parsed: &crate::filter::ParsedFilter) -> Result<Vec<Pk>> {
        if self.deleted {
            return Err(EngineError::CollectionNotFound(self.name.clone()));
        }
        if self.unusable {
            return Err(EngineError::Storage(
                "collection publication is poisoned".into(),
            ));
        }
        let pk = self.schema.fields[self.pk_pos].name.clone();
        Ok(self.query_loaded(parsed, &[pk], None, 0, false)?.rows.pks)
    }

    fn query_loaded(
        &self,
        parsed: &crate::filter::ParsedFilter,
        output_fields: &[String],
        limit: Option<i64>,
        offset: i64,
        order_by_pk: bool,
    ) -> Result<QueryResult> {
        let chunks = self.filtered_chunks_parsed(parsed)?;
        let projection = Projection::new(&self.schema, output_fields, ApiKind::Query);
        let mut refs = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            for row in chunk.live.live_rows() {
                refs.push(RowRef {
                    chunk: index,
                    row,
                    order: if chunk.generation == 0 {
                        (chunk.row_start + row) as u64
                    } else {
                        chunk.orders[row]
                    },
                });
            }
        }
        if order_by_pk {
            refs.sort_unstable_by(|a, b| {
                match (
                    &chunks[a.chunk].columns[self.pk_pos],
                    &chunks[b.chunk].columns[self.pk_pos],
                ) {
                    (ColumnData::Int64(x), ColumnData::Int64(y)) => x[a.row].cmp(&y[b.row]),
                    (ColumnData::VarChar(x), ColumnData::VarChar(y)) => x[a.row].cmp(&y[b.row]),
                    // schema 与加载校验保证同一集合的主键为同一种非空 INT64 / VARCHAR。
                    _ => unreachable!("primary key columns must have the same supported type"),
                }
            });
        } else {
            refs.sort_by_key(|r| (chunks[r.chunk].source_rank, r.order));
        }
        // 3.x 在追加第一条后才判断 effective_limit，故 0 / 负 limit 也可能保留一条。
        if let Some(limit) = limit {
            let end = (i128::from(offset) + i128::from(limit)).max(1);
            refs.truncate(usize::try_from(end).unwrap_or(usize::MAX));
        }
        let start = if offset < 0 {
            usize::try_from((refs.len() as i128 + i128::from(offset)).max(0)).unwrap_or(0)
        } else {
            usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(refs.len())
        };
        let rows = self.gather(&refs[start..], &projection);
        Ok(QueryResult { projection, rows })
    }

    /// 3.x `_require_loaded`：检索与按主键读取之前的 load 状态检查。
    pub fn require_loaded(&self) -> Result<()> {
        check_loaded(&self.name, self.load_state, self.deleted, self.unusable)
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
        match self
            .indexes
            .iter()
            .find(|spec| spec.field_name == field_name)
        {
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
}

/// 数据与有效位分离，使覆盖和删除无需复制向量、JSON 与动态字段。
#[derive(Debug, Clone)]
pub(crate) struct Chunk {
    pub(crate) data: Arc<ChunkData>,
    live: SourceBits,
    generation: u64,
    source: SourceId,
    pub(crate) row_start: usize,
    source_rank: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ChunkData {
    pub(crate) columns: Vec<ColumnData>,
    pub(crate) meta: Option<Vec<Map<String, Value>>>,
    pub(crate) norms: Vec<f32>,
    pub(crate) seqs: Vec<u64>,
    orders: Vec<u64>,
}

impl std::ops::Deref for Chunk {
    type Target = ChunkData;
    fn deref(&self) -> &ChunkData {
        &self.data
    }
}

/// 块 ID 不因其他块的合并或发布移动；位次保留，检索的源序另由快照派生。
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
    chunks: BTreeMap<usize, Chunk>,
    next_chunk: usize,
    pk_index: PkMap<RowRef>,
    /// 下一个 `_seq`。auto_id 取所在行的 `_seq`（0004「决策」），因此 auto_id 不超过 `_seq`（0002 约束 4）。
    next_seq: u64,
    /// 按创建顺序。
    indexes: Vec<IndexSpec>,
    load_state: LoadState,
    next_order: u64,
    persistent: Option<PersistentCollection>,
    memory_key: Option<Box<partition_key::MemoryKey>>,
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
        let memory_key = schema
            .partition_key()
            .map(|_| partition_key::MemoryKey::new(&schema).map(Box::new))
            .transpose()?;
        Ok(Self {
            name: name.into(),
            schema,
            pk_pos,
            vector_pos,
            chunks: BTreeMap::new(),
            next_chunk: 0,
            pk_index: PkMap::default(),
            next_seq: 1,
            indexes: Vec::new(),
            // 3.x：新建的 collection 没有索引定义，初始即为 loaded。
            load_state: LoadState::Loaded,
            next_order: 0,
            persistent: None,
            memory_key,
            deleted: false,
        })
    }

    pub(crate) fn attach_persistence(&mut self, persistent: PersistentCollection) {
        self.memory_key = None;
        self.persistent = Some(persistent);
    }

    pub(crate) fn persistent(&self) -> Option<&PersistentCollection> {
        self.persistent.as_ref()
    }

    pub(crate) fn persistent_mut(&mut self) -> Option<&mut PersistentCollection> {
        self.persistent.as_mut()
    }

    /// 首次启用分区时从既有列重建位置索引；状态锁内失败会封锁实例，磁盘格式不变。
    pub fn enable_partition_visibility(&mut self) -> Result<()> {
        use crate::storage::visibility::partitions::{PartitionRow, SourceKind};
        let chunks = &self.chunks;
        let pk_pos = self.pk_pos;
        let pk_type = self.schema.fields[pk_pos].data_type;
        let count = self.pk_index.len();
        let p = self.persistent.as_mut().ok_or_else(|| {
            EngineError::Storage("partition visibility requires persistence".into())
        })?;
        if p.visibility.is_partitioned() {
            return Ok(());
        }
        if p.manifest.partitions.len() != 1 || !p.manifest.partitions.contains_key("_default") {
            return Err(EngineError::Storage(
                "partition activation requires default-only source metadata".into(),
            ));
        }
        p.rebuild_partition_visibility(|p, mut visibility| {
            let frozen: HashMap<_, _> = p
                .pending_generations()
                .map(|generation| (generation.table.source(), &generation.table))
                .collect();
            visibility.reserve(pk_type, count);
            for &source in p.visibility.source_order() {
                let len = p
                    .visibility
                    .source_len(source)
                    .ok_or("source length missing at partition activation")?;
                let kind = if source == p.active.source {
                    SourceKind::Active
                } else if frozen.contains_key(&source) {
                    SourceKind::Frozen
                } else {
                    SourceKind::Immutable(0)
                };
                visibility
                    .register_source(source, len, kind)
                    .map_err(|error| error.to_string())?;
            }
            for chunk in chunks.values() {
                for row in 0..chunk.live.len {
                    let pk = pk_at(&chunk.columns[pk_pos], row);
                    let physical = chunk.row_start + row;
                    let eligible = if chunk.source == p.active.source {
                        p.active.position(&pk, physical).is_some()
                    } else if let Some(table) = frozen.get(&chunk.source) {
                        table.position(&pk, physical).is_some()
                    } else {
                        true
                    };
                    if eligible {
                        visibility
                            .load_candidate(
                                pk,
                                PartitionRow {
                                    partition: 0,
                                    location: crate::storage::visibility::RowLocation {
                                        source: chunk.source,
                                        row: physical,
                                        seq: chunk.seqs[row],
                                    },
                                },
                            )
                            .map_err(|error| error.to_string())?;
                    }
                }
            }
            for (pk, &seq) in &p.committed_deletes {
                visibility
                    .committed_delete(pk.clone(), seq)
                    .map_err(|error| error.to_string())?;
            }
            for (pk, seq) in p.active.delete_entries() {
                visibility
                    .load_pending_delete(p.active.source, pk.clone(), seq)
                    .map_err(|error| error.to_string())?;
            }
            for (&source, table) in &frozen {
                for (pk, seq) in table.delete_entries() {
                    visibility
                        .load_pending_delete(source, pk.clone(), seq)
                        .map_err(|error| error.to_string())?;
                }
            }
            let snapshot = visibility.snapshot(None);
            if snapshot.live_count != count
                || chunks.values().any(|chunk| {
                    (0..chunk.live.len).any(|row| {
                        snapshot.is_live(chunk.source, chunk.row_start + row)
                            != Some(chunk.live.get(row))
                    })
                })
            {
                return Err("partition activation changes global visibility".into());
            }
            Ok(visibility)
        })
        .map_err(EngineError::Storage)
    }

    pub(crate) fn mark_deleted(&mut self, deleted: bool) {
        self.deleted = deleted;
    }

    pub(crate) fn is_deleted(&self) -> bool {
        self.deleted
    }

    pub(crate) fn from_recovered(
        name: &str,
        mut recovered: RecoveredCollection,
        dir: std::path::PathBuf,
    ) -> Result<Self> {
        let mut collection = Self::new(name, recovered.schema.clone())?;
        collection.next_seq = recovered.next_seq;
        let snapshot = recovered.visibility.snapshot();
        let data_sources = recovered
            .segments
            .iter()
            .map(|s| ((s.partition.clone(), s.relative_path.clone()), s.source))
            .collect();
        // 活行数已由恢复确定；一次预留避免建表时同时保留扩容前后的两张哈希表。
        collection.pk_index.reserve(
            collection.schema.fields[collection.pk_pos].data_type,
            snapshot.live_count,
        );
        // 已提交批只供读块使用；在登记前移交独占列，避免恢复时同时保留两份全量数据。
        for segment in std::mem::take(&mut recovered.segments) {
            let mut row = 0;
            for batch in segment.batches {
                let rows = batch.seqs.len();
                let live = (0..rows)
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
                collection.append_recovered(batch, 0, segment.source, row, live, None)?;
                row += rows;
            }
        }
        let active = &recovered.active_memtable;
        let base_order = collection.next_order;
        let mut row_start = 0;
        for batch in active.read_batches() {
            let live = (0..batch.seqs.len())
                .map(|row| {
                    snapshot
                        .is_live(active.source, row_start + row)
                        .ok_or_else(|| {
                            EngineError::Storage(
                                "recovered WAL visibility is incomplete".to_owned(),
                            )
                        })
                })
                .collect::<Result<Vec<_>>>()?;
            let orders = (0..batch.seqs.len())
                .map(|row| {
                    let pk = pk_at(&batch.columns[collection.pk_pos], row);
                    match active.position(&pk, row_start + row) {
                        Some(position) => base_order.checked_add(position).ok_or_else(|| {
                            EngineError::Storage("row order space exhausted".to_owned())
                        }),
                        None if !live[row] => Ok(0),
                        None => Err(EngineError::Storage(format!(
                            "live recovered WAL row {} has no memtable position",
                            row_start + row
                        ))),
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            collection.append_recovered(
                // 活跃 WAL 的原批还用于冻结与 flush，不移走 memtable 的列。
                batch.as_ref().clone(),
                recovered.next_wal_number,
                active.source,
                row_start,
                live,
                Some(orders),
            )?;
            row_start += batch.seqs.len();
        }
        collection.next_order = base_order
            .checked_add(active.next_position())
            .ok_or_else(|| EngineError::Storage("row order space exhausted".to_owned()))?;
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
        collection.memory_key = None;
        collection.persistent = Some(PersistentCollection::recovered(
            dir,
            recovered,
            data_sources,
        ));
        Ok(collection)
    }

    fn append_recovered(
        &mut self,
        decoded: DecodedData,
        generation: u64,
        source: SourceId,
        row_start: usize,
        live: Vec<bool>,
        orders: Option<Vec<u64>>,
    ) -> Result<()> {
        let n = decoded.seqs.len();
        let (dim, vectors) = vector_values(&decoded.columns[self.vector_pos]);
        let norms = vectors.chunks_exact(dim).map(flat::norm).collect();
        let orders = if let Some(orders) = orders {
            orders
        } else {
            let start = self.next_order;
            self.next_order = start
                .checked_add(n as u64)
                .ok_or_else(|| EngineError::Storage("row order space exhausted".to_owned()))?;
            (start..self.next_order).collect()
        };
        let chunk_index = self.next_chunk;
        self.next_chunk += 1;
        let chunk = Chunk {
            data: Arc::new(ChunkData {
                columns: decoded.columns,
                meta: decoded.meta,
                norms,
                seqs: decoded.seqs,
                orders,
            }),
            live: SourceBits::from_live(&live),
            generation,
            source,
            row_start,
            source_rank: 0,
        };
        for row in 0..n {
            if chunk.live.get(row) {
                let pk = pk_at(&chunk.columns[self.pk_pos], row);
                self.pk_index.insert(
                    pk,
                    RowRef {
                        order: chunk.orders[row],
                        chunk: chunk_index,
                        row,
                    },
                );
            }
        }
        self.chunks.insert(chunk_index, chunk);
        if generation != 0 {
            self.merge_tail();
        }
        Ok(())
    }

    fn clear_live(&mut self, at: RowRef) {
        match self.chunks.get_mut(&at.chunk) {
            Some(chunk) => chunk.live.set(at.row, false),
            // 主键只从仍在表中的块建立，合并与发布在同一次写锁内重映射它。
            None => unreachable!("live primary key refers to an existing block"),
        }
    }

    fn merge_tail(&mut self) {
        let limit = (BLOCK_ELEMS / field_dim(&self.schema.fields[self.vector_pos])).max(1);
        loop {
            let mut tail = self.chunks.iter().rev();
            let Some((&right_id, right)) = tail.next() else {
                break;
            };
            let Some((&left_id, left)) = tail.next() else {
                break;
            };
            let left_len = left.live.len;
            if left.generation == 0
                || left.source != right.source
                || left.generation != right.generation
                || left.row_start + left_len != right.row_start
                || !crate::storage::memtable::mergeable(left_len, right.live.len, limit)
            {
                break;
            }
            let mut data = (*left.data).clone();
            for (dst, src) in data.columns.iter_mut().zip(&right.columns) {
                dst.append_column(src);
            }
            if let (Some(dst), Some(src)) = (&mut data.meta, &right.meta) {
                dst.extend_from_slice(src);
            }
            data.norms.extend_from_slice(&right.norms);
            data.seqs.extend_from_slice(&right.seqs);
            data.orders.extend_from_slice(&right.orders);
            let live: Vec<_> = (0..left_len)
                .map(|row| left.live.get(row))
                .chain((0..right.live.len).map(|row| right.live.get(row)))
                .collect();
            let merged = Chunk {
                data: Arc::new(data),
                live: SourceBits::from_live(&live),
                generation: left.generation,
                source: left.source,
                row_start: left.row_start,
                source_rank: 0,
            };
            // 稳定块 ID 不搬动后续数据源；只重映射参与拼接的主键引用。
            for row in 0..merged.live.len {
                let pk = pk_at(&merged.columns[self.pk_pos], row);
                if let Some(at) = self.pk_index.get_mut(&pk)
                    && at.chunk == right_id
                {
                    at.chunk = left_id;
                    at.row += left_len;
                }
            }
            self.chunks.remove(&right_id);
            self.chunks.insert(left_id, merged);
        }
    }

    /// 同一次状态写锁内转换读取映射与持久化位图；列与范数在锁外已构建完成。
    pub(crate) fn publish_generation(
        &mut self,
        generation: &Arc<crate::storage::runtime::FrozenGeneration>,
        manifest: crate::storage::manifest::Manifest,
        mapping: &[Option<usize>],
        decoded: Option<DecodedData>,
        norms: Vec<f32>,
        retry_manifest: bool,
    ) -> Result<()> {
        let old_source = generation.table.source();
        let ids: Vec<_> = self
            .chunks
            .iter()
            .filter(|(_, c)| c.source == old_source)
            .map(|(&id, _)| id)
            .collect();
        let rows = decoded.as_ref().map_or(0, |data| data.seqs.len());
        let prepared = (|| -> Result<_> {
            if decoded.is_some() && rows == 0 {
                return Err(EngineError::Storage("published data batch is empty".into()));
            }
            if norms.len() != rows {
                return Err(EngineError::Storage(
                    "published vector norms are incomplete".into(),
                ));
            }
            let mut orders = vec![0; rows];
            let mut live = vec![false; rows];
            let mut seen = vec![false; rows];
            let mut physical_rows = 0;
            let mut refs = Vec::new();
            let mut source_rows = Vec::with_capacity(mapping.len());
            for &id in &ids {
                let chunk = &self.chunks[&id];
                if chunk.row_start != physical_rows {
                    return Err(EngineError::Storage(
                        "frozen read blocks are not contiguous".into(),
                    ));
                }
                for row in 0..chunk.live.len {
                    let global_row = chunk.row_start + row;
                    source_rows.push((pk_at(&chunk.columns[self.pk_pos], row), global_row));
                    let target = mapping.get(global_row).ok_or_else(|| {
                        EngineError::Storage("frozen read mapping is incomplete".into())
                    })?;
                    if let Some(target) = *target {
                        if target >= rows || seen[target] {
                            return Err(EngineError::Storage(
                                "frozen read mapping has invalid targets".into(),
                            ));
                        }
                        let data = decoded.as_ref().ok_or_else(|| {
                            EngineError::Storage("published data is missing".into())
                        })?;
                        let pk = pk_at(&chunk.columns[self.pk_pos], row);
                        if chunk.seqs[row] != data.seqs[target]
                            || pk != pk_at(&data.columns[self.pk_pos], target)
                        {
                            return Err(EngineError::Storage(
                                "published data does not match frozen rows".into(),
                            ));
                        }
                        seen[target] = true;
                        orders[target] = chunk.orders[row];
                        live[target] = chunk.live.get(row);
                        if live[target] {
                            let at = self.pk_index.get(&pk).ok_or_else(|| {
                                EngineError::Storage(
                                    "live frozen row has no primary reference".into(),
                                )
                            })?;
                            if at.chunk != id || at.row != row {
                                return Err(EngineError::Storage(
                                    "live frozen primary reference does not match".into(),
                                ));
                            }
                            refs.push((
                                pk,
                                RowRef {
                                    order: orders[target],
                                    chunk: ids[0],
                                    row: target,
                                },
                            ));
                        }
                    } else if chunk.live.get(row) {
                        return Err(EngineError::Storage(
                            "published mapping omits a live frozen row".into(),
                        ));
                    }
                }
                physical_rows += chunk.live.len;
            }
            if mapping.len() != physical_rows || seen.iter().any(|value| !value) {
                return Err(EngineError::Storage(
                    "published mapping does not cover frozen physical rows".into(),
                ));
            }
            Ok((orders, live, refs, source_rows))
        })();
        let (orders, live, refs, source_rows) = match prepared {
            Ok(value) => value,
            Err(error) => {
                if let Some(persistent) = self.persistent.as_mut() {
                    persistent.poison();
                }
                return Err(error);
            }
        };
        let persistent = self
            .persistent
            .as_mut()
            .ok_or_else(|| EngineError::Storage("persistent state missing".into()))?;
        let source = match persistent.publish(
            generation,
            manifest,
            mapping,
            rows,
            &source_rows,
            retry_manifest,
        ) {
            Ok(source) => source,
            Err(reason) => {
                persistent.poison();
                return Err(EngineError::Storage(reason));
            }
        };
        for &id in &ids {
            self.chunks.remove(&id);
        }
        if let Some(decoded) = decoded {
            let chunk = Chunk {
                data: Arc::new(ChunkData {
                    columns: decoded.columns,
                    meta: decoded.meta,
                    seqs: decoded.seqs,
                    norms,
                    orders,
                }),
                live: SourceBits::from_live(&live),
                source,
                row_start: 0,
                source_rank: 0,
                generation: 0,
            };
            self.chunks.insert(ids[0], chunk);
            for (pk, at) in refs {
                self.pk_index.insert(pk, at);
            }
        }
        Ok(())
    }

    pub(crate) fn publish_partitioned_generation(
        &mut self,
        generation: &Arc<crate::storage::runtime::FrozenGeneration>,
        manifest: crate::storage::manifest::Manifest,
        mut publication: crate::storage::runtime::PartitionFlushOutcome,
        retry: bool,
    ) -> Result<()> {
        let old = generation.table.source();
        let ids: Vec<_> = self
            .chunks
            .iter()
            .filter(|(_, chunk)| chunk.source == old)
            .map(|(&id, _)| id)
            .collect();
        let p = self
            .persistent
            .as_ref()
            .ok_or_else(|| EngineError::Storage("persistent state missing".into()))?;
        let ordered: Vec<_> = manifest
            .partition_order
            .iter()
            .filter_map(|name| p.partitions.id(name))
            .collect();
        publication
            .data
            .sort_by_key(|data| ordered.iter().position(|id| *id == data.id));
        let prepared = (|| -> Result<_> {
            if publication
                .data
                .iter()
                .any(|data| data.data.seqs.is_empty())
            {
                return Err(EngineError::Storage(
                    "published partition data is empty".into(),
                ));
            }
            if publication
                .data
                .iter()
                .any(|data| !ordered.contains(&data.id))
            {
                return Err(EngineError::Storage(
                    "published partition is not registered".into(),
                ));
            }
            let mut seen: Vec<_> = publication
                .data
                .iter()
                .map(|data| vec![false; data.data.seqs.len()])
                .collect();
            let mut orders: Vec<_> = publication
                .data
                .iter()
                .map(|data| vec![0; data.data.seqs.len()])
                .collect();
            let mut rows = Vec::with_capacity(publication.mapping.len());
            let mut offset = 0;
            for &id in &ids {
                let chunk = &self.chunks[&id];
                if chunk.row_start != offset {
                    return Err(EngineError::Storage(
                        "frozen partition blocks are not contiguous".into(),
                    ));
                }
                for row in 0..chunk.live.len {
                    let physical = offset + row;
                    let pk = pk_at(&chunk.columns[self.pk_pos], row);
                    rows.push((pk.clone(), physical, chunk.seqs[row]));
                    let target = publication.mapping.get(physical).ok_or_else(|| {
                        EngineError::Storage("frozen partition mapping missing".into())
                    })?;
                    if let Some((part, target)) = target {
                        let index = publication
                            .data
                            .iter()
                            .position(|data| data.id == *part)
                            .ok_or_else(|| {
                                EngineError::Storage("published partition data missing".into())
                            })?;
                        let data = &publication.data[index];
                        if *target >= data.data.seqs.len()
                            || seen[index][*target]
                            || data.norms.len() != data.data.seqs.len()
                            || data.data.seqs[*target] != chunk.seqs[row]
                            || pk_at(&data.data.columns[self.pk_pos], *target) != pk
                        {
                            return Err(EngineError::Storage(
                                "published partition data differs from frozen row".into(),
                            ));
                        }
                        seen[index][*target] = true;
                        orders[index][*target] = chunk.orders[row];
                        if chunk.live.get(row)
                            && self
                                .pk_index
                                .get(&pk)
                                .is_none_or(|at| at.chunk != id || at.row != row)
                        {
                            return Err(EngineError::Storage(
                                "frozen partition primary reference differs".into(),
                            ));
                        }
                    } else if chunk.live.get(row) {
                        return Err(EngineError::Storage(
                            "partition publication omits a global live row".into(),
                        ));
                    }
                }
                offset += chunk.live.len;
            }
            if offset != publication.mapping.len()
                || seen.iter().any(|bits| bits.iter().any(|live| !*live))
            {
                return Err(EngineError::Storage(
                    "partition publication mapping is incomplete".into(),
                ));
            }
            self.next_chunk
                .checked_add(publication.data.len())
                .ok_or_else(|| EngineError::Storage("read chunk ID space exhausted".into()))?;
            Ok((rows, orders))
        })();
        let (rows, orders) = match prepared {
            Ok(value) => value,
            Err(error) => {
                if let Some(p) = self.persistent.as_mut() {
                    p.poison();
                }
                return Err(error);
            }
        };
        let p = self
            .persistent
            .as_mut()
            .ok_or_else(|| EngineError::Storage("persistent state missing".into()))?;
        let targets = match p.publish_partitioned(generation, manifest, &publication, &rows, retry)
        {
            Ok(value) => value,
            Err(error) => {
                p.poison();
                return Err(EngineError::Storage(error));
            }
        };
        let snapshot = p.visibility.snapshot();
        let built = (|| -> Result<_> {
            let mut built = Vec::new();
            for (offset, (data, orders)) in publication.data.into_iter().zip(orders).enumerate() {
                let source = targets
                    .iter()
                    .find(|(id, _)| *id == data.id)
                    .map(|(_, source)| *source)
                    .ok_or_else(|| {
                        EngineError::Storage("published partition source missing".into())
                    })?;
                let live = snapshot.source_bits(source).cloned().ok_or_else(|| {
                    EngineError::Storage("published partition bitmap missing".into())
                })?;
                let id = self.next_chunk + offset;
                built.push((
                    id,
                    Chunk {
                        data: Arc::new(ChunkData {
                            columns: data.data.columns,
                            meta: data.data.meta,
                            seqs: data.data.seqs,
                            norms: data.norms,
                            orders,
                        }),
                        live,
                        source,
                        row_start: 0,
                        source_rank: 0,
                        generation: 0,
                    },
                ));
            }
            Ok(built)
        })();
        let built = match built {
            Ok(value) => value,
            Err(error) => {
                if let Some(p) = self.persistent.as_mut() {
                    p.poison();
                }
                return Err(error);
            }
        };
        for id in ids {
            self.chunks.remove(&id);
        }
        self.next_chunk += built.len();
        for (id, chunk) in built {
            for row in chunk.live.live_rows() {
                self.pk_index.insert(
                    pk_at(&chunk.columns[self.pk_pos], row),
                    RowRef {
                        chunk: id,
                        row,
                        order: chunk.orders[row],
                    },
                );
            }
            self.chunks.insert(id, chunk);
        }
        Ok(())
    }

    pub(crate) fn publish_partition_drop(&mut self, name: &str, partition: u32) -> Result<()> {
        use crate::storage::visibility::partitions::SourceReplacement;
        let p = self
            .persistent
            .as_mut()
            .ok_or_else(|| EngineError::Storage("persistent state missing".into()))?;
        let sources: std::collections::HashSet<_> = p
            .data_sources
            .iter()
            .filter(|((part, _), _)| part == name)
            .map(|(_, source)| *source)
            .collect();
        let mut inputs = Vec::new();
        for &source in &sources {
            let mut rows = Vec::new();
            for chunk in self.chunks.values().filter(|c| c.source == source) {
                if chunk.row_start != rows.len() {
                    return Err(EngineError::Storage(
                        "drop source blocks are not contiguous".into(),
                    ));
                }
                for row in 0..chunk.seqs.len() {
                    rows.push((
                        crate::storage::compaction::primary(chunk, self.pk_pos, row)?,
                        chunk.row_start + row,
                        None,
                    ));
                }
            }
            inputs.push(SourceReplacement { source, rows });
        }
        p.visibility
            .drop_partition_sources(partition, &inputs)
            .map_err(EngineError::Storage)?;
        p.data_sources.retain(|(part, _), _| part != name);
        p.partition_cleanup = true;
        self.chunks
            .retain(|_, chunk| !sources.contains(&chunk.source));
        let visible = p.visibility.snapshot();
        self.pk_index.clear();
        for (&id, chunk) in &mut self.chunks {
            let bits = visible
                .source_bits(chunk.source)
                .ok_or_else(|| EngineError::Storage("drop source bitmap missing".into()))?;
            chunk.live = bits
                .slice(chunk.row_start, chunk.seqs.len())
                .ok_or_else(|| EngineError::Storage("drop source rows missing".into()))?;
            for row in 0..chunk.seqs.len() {
                if chunk.live.get(row) {
                    let pk = crate::storage::compaction::primary(chunk, self.pk_pos, row)?;
                    self.pk_index.insert(
                        pk,
                        RowRef {
                            chunk: id,
                            row,
                            order: chunk.orders[row],
                        },
                    );
                }
            }
        }
        Ok(())
    }

    pub(crate) fn compaction_view(&self) -> Result<Option<crate::storage::compaction::View>> {
        use crate::storage::compaction::{Input, View};
        let p = self
            .persistent
            .as_ref()
            .ok_or_else(|| EngineError::Storage("persistent state missing".into()))?;
        if self.deleted || !p.can_compact().map_err(EngineError::Storage)? {
            return Ok(None);
        }
        let mut inputs = Vec::new();
        for (partition, files) in &p.manifest.partitions {
            for relative in &files.data_files {
                let source = *p
                    .data_sources
                    .get(&(partition.clone(), relative.clone()))
                    .ok_or_else(|| EngineError::Storage("manifest read source missing".into()))?;
                let rank = p
                    .visibility
                    .source_order()
                    .iter()
                    .position(|id| *id == source)
                    .ok_or_else(|| EngineError::Storage("manifest source rank missing".into()))?;
                let chunks = self
                    .chunks
                    .values()
                    .filter(|c| c.source == source)
                    .cloned()
                    .collect();
                inputs.push(Input {
                    partition: partition.clone(),
                    relative: relative.clone(),
                    source,
                    rank,
                    chunks,
                });
            }
        }
        Ok(Some(View {
            dir: p.dir.clone(),
            schema: self.schema.clone(),
            manifest: p.manifest.clone(),
            pins: p.pinned_files.clone(),
            inputs,
            sync_inherited: !p.inherited_synced(),
        }))
    }

    pub(crate) fn compaction_deletes(
        &self,
        inputs: &[crate::storage::compaction::Input],
    ) -> Result<HashMap<Pk, u64>> {
        let p = self
            .persistent
            .as_ref()
            .ok_or_else(|| EngineError::Storage("persistent state missing".into()))?;
        let mut deletes = HashMap::new();
        for input in inputs {
            for chunk in &input.chunks {
                for row in 0..chunk.seqs.len() {
                    let pk = crate::storage::compaction::primary(chunk, self.pk_pos, row)?;
                    if let Some(&seq) = p.committed_deletes.get(&pk) {
                        deletes.insert(pk, seq);
                    }
                }
            }
        }
        Ok(deletes)
    }

    pub(crate) fn validate_compaction(
        &self,
        prepared: &crate::storage::compaction::Prepared,
    ) -> Result<()> {
        let mut seen: Vec<_> = prepared
            .outputs
            .iter()
            .map(|o| vec![false; o.data.seqs.len()])
            .collect();
        for (i, input) in prepared.inputs.iter().enumerate() {
            let current: Vec<_> = self
                .chunks
                .iter()
                .filter(|(_, c)| c.source == input.source)
                .collect();
            let mut physical = 0;
            for (&id, chunk) in current {
                if chunk.row_start != physical {
                    return Err(EngineError::Storage(
                        "compaction source blocks changed".into(),
                    ));
                }
                for row in 0..chunk.seqs.len() {
                    let pk = crate::storage::compaction::primary(chunk, self.pk_pos, row)?;
                    let target = prepared
                        .mapping
                        .get(i)
                        .and_then(|v| v.get(physical + row))
                        .ok_or_else(|| EngineError::Storage("compaction mapping missing".into()))?;
                    if let Some(target) = target {
                        let output = prepared.outputs.get(target.output).ok_or_else(|| {
                            EngineError::Storage("compaction output missing".into())
                        })?;
                        let marked = seen
                            .get_mut(target.output)
                            .and_then(|s| s.get_mut(target.row))
                            .ok_or_else(|| {
                                EngineError::Storage("compaction target out of range".into())
                            })?;
                        let output_pk = match output.data.columns.get(self.pk_pos) {
                            Some(ColumnData::Int64(v)) => v.get(target.row).copied().map(Pk::Int64),
                            Some(ColumnData::VarChar(v)) => {
                                v.get(target.row).cloned().map(Pk::VarChar)
                            }
                            _ => None,
                        };
                        if *marked
                            || output.data.seqs.get(target.row) != Some(&chunk.seqs[row])
                            || output_pk.as_ref() != Some(&pk)
                        {
                            return Err(EngineError::Storage(
                                "compaction data mapping differs from input".into(),
                            ));
                        }
                        *marked = true;
                        if chunk.live.get(row)
                            && self
                                .pk_index
                                .get(&pk)
                                .is_none_or(|at| at.chunk != id || at.row != row)
                        {
                            return Err(EngineError::Storage(
                                "compaction live primary reference missing".into(),
                            ));
                        }
                    } else if chunk.live.get(row) {
                        return Err(EngineError::Storage("compaction drops a live row".into()));
                    }
                }
                physical += chunk.seqs.len();
            }
            if physical != prepared.mapping[i].len() {
                return Err(EngineError::Storage(
                    "compaction source row count changed".into(),
                ));
            }
        }
        if seen.iter().flatten().any(|v| !v) {
            return Err(EngineError::Storage(
                "compaction output not fully mapped".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn publish_compaction(
        &mut self,
        prepared: &mut crate::storage::compaction::Prepared,
        manifest: crate::storage::manifest::Manifest,
        threshold: Option<u64>,
        durability_error: Option<String>,
    ) -> Result<()> {
        use crate::storage::visibility::CompactionRow;
        self.validate_compaction(prepared)?;
        let p = self
            .persistent
            .as_ref()
            .ok_or_else(|| EngineError::Storage("persistent state missing".into()))?;
        if p.poisoned() {
            return Err(EngineError::Storage(
                "collection was poisoned during compaction".into(),
            ));
        }
        let ids = p
            .compaction_sources(prepared.outputs.len())
            .map_err(EngineError::Storage)?;
        let end = self
            .next_chunk
            .checked_add(ids.len())
            .ok_or_else(|| EngineError::Storage("read block ID exhausted".into()))?;
        let mut live: Vec<_> = prepared
            .outputs
            .iter()
            .map(|o| vec![false; o.data.seqs.len()])
            .collect();
        let mut refs = Vec::new();
        let mut physical_rows = Vec::new();
        for (i, input) in prepared.inputs.iter().enumerate() {
            let mut rows = Vec::with_capacity(prepared.mapping[i].len());
            for chunk in self.chunks.values().filter(|c| c.source == input.source) {
                for row in 0..chunk.seqs.len() {
                    let pk = crate::storage::compaction::primary(chunk, self.pk_pos, row)?;
                    let target = prepared.mapping[i][chunk.row_start + row];
                    if let Some(t) = target
                        && chunk.live.get(row)
                    {
                        live[t.output][t.row] = true;
                        refs.push((
                            pk.clone(),
                            RowRef {
                                chunk: self.next_chunk + t.output,
                                row: t.row,
                                order: t.row as u64,
                            },
                        ));
                    }
                    rows.push(CompactionRow {
                        pk,
                        seq: chunk.seqs[row],
                        target: target.map(|t| (ids[t.output], t.row)),
                    });
                }
            }
            physical_rows.push((input.source, rows));
        }
        self.persistent
            .as_mut()
            .ok_or_else(|| EngineError::Storage("persistent state missing".into()))?
            .publish_compaction(
                prepared,
                manifest,
                &ids,
                &physical_rows,
                threshold,
                durability_error,
            )
            .map_err(EngineError::Storage)?;
        self.chunks
            .retain(|_, c| !prepared.inputs.iter().any(|i| i.source == c.source));
        for (i, output) in prepared.outputs.drain(..).enumerate() {
            let orders = (0..output.data.seqs.len() as u64).collect();
            self.chunks.insert(
                self.next_chunk + i,
                Chunk {
                    data: Arc::new(ChunkData {
                        columns: output.data.columns,
                        meta: output.data.meta,
                        seqs: output.data.seqs,
                        norms: output.norms,
                        orders,
                    }),
                    live: SourceBits::from_live(&live[i]),
                    generation: 0,
                    source: ids[i],
                    row_start: 0,
                    source_rank: 0,
                },
            );
        }
        self.next_chunk = end;
        for (pk, at) in refs {
            self.pk_index.insert(pk, at);
        }
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

    /// 3.x：已 loaded 时直接返回；否则为有行的已提交 segment 建索引，建不成就保持 released。
    /// 只有 WAL 的数据不触发，load 之后新提交的 segment 要等 release 后再次 load 才判定。
    pub fn load(&mut self) -> Result<()> {
        // 懒打开返回句柄后可能被并发 drop 退役；不得再读取旧目录的侧车元数据。
        if self.deleted {
            return Err(EngineError::CollectionNotFound(self.name.clone()));
        }
        if self.load_state == LoadState::Loaded {
            return Ok(());
        }
        for spec in &self.indexes {
            let Some(field) = self
                .schema
                .fields
                .iter()
                .find(|f| f.name == spec.field_name)
            else {
                continue;
            };
            if let Some(error) = crate::index::unbuildable(spec, field)
                && self.has_committed_rows()?
            {
                return Err(error);
            }
            if let Some(error) = crate::index::unbuildable_metric(spec, field)
                && self.has_committed_rows_without_sidecar(spec)?
            {
                return Err(error);
            }
        }
        self.load_state = LoadState::Loaded;
        Ok(())
    }

    /// 按物理行数：被删除隐藏的行仍在 segment 中，3.x 照样为它建索引。
    fn has_committed_rows(&self) -> Result<bool> {
        let Some(p) = &self.persistent else {
            return Ok(false);
        };
        for source in p.data_sources.values() {
            let rows = p
                .visibility
                .source_len(*source)
                .ok_or_else(|| EngineError::Storage("manifest read source missing".into()))?;
            if rows > 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn has_committed_rows_without_sidecar(&self, spec: &IndexSpec) -> Result<bool> {
        let Some(p) = &self.persistent else {
            return Ok(false);
        };
        for ((partition, file), source) in &p.data_sources {
            let dir = p.dir.join("partitions").join(partition).join("indexes");
            let rows = p
                .visibility
                .source_len(*source)
                .ok_or_else(|| EngineError::Storage("manifest read source missing".into()))?;
            if rows == 0 {
                continue;
            }
            let stem = std::path::Path::new(file)
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| {
                    EngineError::Storage("manifest data filename has no source stem".into())
                })?;
            let path = dir.join(format!(
                "{stem}.{}.{}.idx",
                spec.field_name,
                spec.index_type.to_lowercase()
            ));
            // os.path.exists 跟随链接；只看存在性，不读取或校验 3.x 的索引内容。
            match std::fs::metadata(&path) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
                Err(e) => {
                    return Err(EngineError::Storage(format!(
                        "index sidecar metadata at {path:?}: {e}"
                    )));
                }
            }
        }
        Ok(false)
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

    fn create_index_inner(&mut self, field_name: &str, req: IndexRequest) -> Result<()> {
        let spec = self.prepare_create_index(field_name, req)?;
        self.indexes.push(spec);
        Ok(())
    }

    pub(crate) fn prepare_create_index(
        &self,
        field_name: &str,
        req: IndexRequest,
    ) -> Result<IndexSpec> {
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
        build_spec(field, req)
    }

    pub(crate) fn prepare_drop_index(
        &self,
        field_name: &str,
        index_name: &str,
    ) -> Result<Vec<IndexSpec>> {
        if self.deleted {
            return Err(EngineError::CollectionNotFound(self.name.clone()));
        }
        if self.indexes.is_empty() {
            return Err(EngineError::IndexNotFound("no index to drop".to_owned()));
        }
        let field = if !field_name.is_empty() {
            Some(field_name)
        } else {
            self.indexes
                .iter()
                .find(|spec| format!("{}_idx", spec.field_name) == index_name)
                .map(|spec| spec.field_name.as_str())
        };
        if let Some(field) = field
            && self.index(field).is_none()
        {
            return Err(self.drop_field_error(field));
        }
        if self.load_state == LoadState::Loaded {
            return Err(EngineError::schema(
                "index cannot be dropped on loaded collection; call release() first",
            ));
        }
        Ok(self
            .indexes
            .iter()
            .filter(|spec| field.is_none_or(|f| f == spec.field_name))
            .cloned()
            .collect())
    }

    fn drop_field_error(&self, field: &str) -> EngineError {
        EngineError::IndexNotFound(format!(
            "no index on field {}; indexed fields: [{}]",
            py_repr(field),
            self.indexes
                .iter()
                .map(|s| py_repr(&s.field_name))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    pub(crate) fn validate_index_change(&self, change: &crate::index::IndexChange) -> Result<()> {
        if self.deleted {
            return Err(EngineError::CollectionNotFound(self.name.clone()));
        }
        match change {
            crate::index::IndexChange::Create(spec) => {
                if self.index(&spec.field_name).is_some() {
                    return Err(EngineError::IndexAlreadyExists(spec.field_name.clone()));
                }
            }
            crate::index::IndexChange::Drop(specs) => {
                if self.indexes.is_empty() {
                    return Err(EngineError::IndexNotFound("no index to drop".to_owned()));
                }
                for spec in specs {
                    if self.index(&spec.field_name).is_none() {
                        return Err(self.drop_field_error(&spec.field_name));
                    }
                }
                if self.load_state == LoadState::Loaded {
                    return Err(EngineError::schema(
                        "index cannot be dropped on loaded collection; call release() first",
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn publish_index_change(&mut self, change: crate::index::IndexChange) {
        match change {
            crate::index::IndexChange::Create(spec) => self.indexes.push(spec),
            crate::index::IndexChange::Drop(specs) => {
                self.indexes
                    .retain(|old| !specs.iter().any(|spec| spec.field_name == old.field_name));
                if self.indexes.is_empty() {
                    self.load_state = LoadState::Loaded;
                }
            }
        }
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

    pub(crate) fn insert_partition_from_db(
        &mut self,
        batch: InsertBatch,
        partition: &str,
    ) -> Result<Vec<Pk>> {
        self.insert_partition_inner(batch, partition)
    }

    pub(crate) fn upsert_partition_from_db(
        &mut self,
        batch: InsertBatch,
        partition: &str,
    ) -> Result<Vec<Pk>> {
        if self.schema.partition_key().is_some() {
            return self.insert_key_batch(batch, true);
        }
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
        let mut json_checks: HashMap<&str, Vec<&InputValues>> = HashMap::new();
        for (name, values) in &batch.columns {
            if name != META_FIELD
                && self.schema.field(name).is_some()
                && let Some(previous) = supplied.insert(name, values)
            {
                json_checks.entry(name.as_str()).or_default().push(previous);
            }
        }
        retain_masked_json(&supplied, &mut json_checks);
        let pk_field = &self.schema.fields[self.pk_pos];
        let mut fields: Vec<ColumnData> =
            self.schema.fields.iter().map(ColumnData::empty).collect();
        let mut array_inputs: HashMap<usize, Vec<Option<InputValues>>> = HashMap::new();
        let mut metas = Vec::with_capacity(n);
        for row in 0..n {
            let candidate = supplied
                .get(pk_field.name.as_str())
                .and_then(|values| input_pk(values, row));
            let current = candidate.as_ref().and_then(|pk| self.pk_index.get(pk));
            let mut values = if let Some(old) = current {
                self.chunks[&old.chunk]
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
            let mut present = vec![current.is_some(); values.len()];
            for (i, field) in self.schema.fields.iter().enumerate() {
                if let Some(input) = supplied.get(field.name.as_str()) {
                    values[i] = input_row(input, row);
                    present[i] = true;
                }
            }
            if pk_field.auto_id && (!present[self.pk_pos] || input_is_null(&values[self.pk_pos], 0))
            {
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
            let mut meta = if let Some(old) = current {
                self.chunks[&old.chunk]
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
            self.validate_row(&row_fields, &row_dynamic, 0, &json_checks, row)?;
            for (i, field) in self.schema.fields.iter().enumerate() {
                if field.data_type == DataType::Array {
                    let (input, index) =
                        resolved_input(field, present[i].then_some(&values[i]), 0)?
                            .ok_or_else(|| EngineError::schema("validated ARRAY lost field"))?;
                    let list = crate::data::array::row_input(&input, index)?
                        .map(std::borrow::Cow::into_owned);
                    array_inputs.entry(i).or_default().push(list);
                    continue;
                }
                let converted = convert_optional(field, present[i].then_some(&values[i]), 1)?;
                fields[i].push_row(&converted, 0);
            }
            metas.push(meta);
        }
        // 与参照一样，全部行的业务检查完成后才做 ARRAY 元素的 Arrow 转换。
        self.resolve_write_partition(partition)?;
        for (i, rows) in array_inputs {
            fields[i] =
                convert_optional(&self.schema.fields[i], Some(&InputValues::Array(rows)), n)?;
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
        self.insert_partition_inner(
            InsertBatch {
                num_rows: n,
                columns,
            },
            partition,
        )
    }

    pub(crate) fn delete_from_db(&mut self, pks: &[Pk]) -> Result<usize> {
        self.delete_partition_from_db(pks, "")
    }

    pub(crate) fn delete_partition_from_db(
        &mut self,
        pks: &[Pk],
        partition: &str,
    ) -> Result<usize> {
        if self.deleted {
            return Err(EngineError::CollectionNotFound(self.name.clone()));
        }
        if pks.is_empty() {
            return Ok(0);
        }
        let target = self.resolve_delete_partition(partition)?;
        let seq = self.next_seq;
        self.next_seq = seq
            .checked_add(1)
            .ok_or_else(|| EngineError::invalid("_seq space exhausted"))?;
        self.delete_at_seq(pks, seq, target)
    }

    pub(crate) fn delete_partition_literals_from_db(
        &mut self,
        literals: &[PkLiteral],
        partition: &str,
    ) -> Result<Vec<Pk>> {
        if self.deleted {
            return Err(EngineError::CollectionNotFound(self.name.clone()));
        }
        if literals.is_empty() {
            return Ok(Vec::new());
        }
        let target = self.resolve_delete_partition(partition)?;
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
        self.delete_at_seq(&pks, seq, target)?;
        Ok(pks)
    }

    fn delete_at_seq(&mut self, pks: &[Pk], seq: u64, target: Option<u32>) -> Result<usize> {
        if let Some(persistent) = self.persistent.as_mut() {
            persistent
                .append_partition_delete(&self.schema, pks, seq, target)
                .map_err(EngineError::Storage)?;
        }
        if let Some(memory) = self.memory_key.as_mut() {
            for pk in pks {
                memory
                    .visibility
                    .apply_local_delete(SourceId(1), pk.clone(), seq)
                    .map_err(|e| EngineError::Storage(e.to_string()))?;
            }
        }
        for pk in pks {
            if let Some(old) = self.pk_index.remove(pk) {
                self.clear_live(old);
            }
        }
        Ok(pks.len())
    }

    fn insert_inner(&mut self, batch: InsertBatch) -> Result<Vec<Pk>> {
        self.insert_partition_inner(batch, "_default")
    }

    fn resolve_delete_partition(&self, name: &str) -> Result<Option<u32>> {
        if name.is_empty() {
            Ok(None)
        } else {
            self.resolve_write_partition(name).map(Some)
        }
    }

    fn resolve_write_partition(&self, name: &str) -> Result<u32> {
        let name = if name.is_empty() { "_default" } else { name };
        if let Some(p) = &self.persistent {
            if !p.manifest.partitions.contains_key(name) {
                return Err(EngineError::PartitionNotFound(name.to_owned()));
            }
            return p
                .partitions
                .id(name)
                .ok_or_else(|| EngineError::Storage("partition identifier missing".into()));
        }
        if let Some(memory) = &self.memory_key {
            return memory
                .registry
                .id(name)
                .ok_or_else(|| EngineError::PartitionNotFound(name.into()));
        }
        if name == "_default" {
            Ok(0)
        } else {
            Err(EngineError::PartitionNotFound(name.to_owned()))
        }
    }

    fn insert_partition_inner(&mut self, batch: InsertBatch, partition: &str) -> Result<Vec<Pk>> {
        if self.schema.partition_key().is_some() {
            return self.insert_key_batch(batch, false);
        }
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
        if !pk.auto_id && pk.default_value.is_none() && !has_column(&pk.name) {
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
        let mut json_checks: HashMap<&str, Vec<&InputValues>> = HashMap::new();
        for (name, values) in &batch.columns {
            if name != META_FIELD
                && self.schema.field(name).is_some()
                && let Some(previous) = by_name.insert(name.as_str(), values)
            {
                json_checks.entry(name.as_str()).or_default().push(previous);
            }
        }
        retain_masked_json(&by_name, &mut json_checks);
        for row in 0..n {
            self.validate_row(&by_name, &dynamic, row, &json_checks, row)?;
        }
        let partition = self.resolve_write_partition(partition)?;

        let pk_field = &self.schema.fields[self.pk_pos];
        let auto_pk = pk_field.auto_id && !by_name.contains_key(pk_field.name.as_str());
        let first_seq = self.next_seq;
        let next_seq = first_seq
            .checked_add(n as u64)
            .ok_or_else(|| EngineError::invalid("_seq space exhausted"))?;
        let seqs: Vec<u64> = (first_seq..next_seq).collect();
        let mut columns = Vec::with_capacity(self.schema.fields.len());
        for (i, field) in self.schema.fields.iter().enumerate() {
            let null_auto_pk = if i == self.pk_pos && pk_field.auto_id && !auto_pk {
                let mut found = false;
                if let Some(input) = by_name.get(pk_field.name.as_str()) {
                    for row in 0..n {
                        found |= input_is_null(input, row);
                    }
                }
                found
            } else {
                false
            };
            let column = if i == self.pk_pos && (auto_pk || null_auto_pk) {
                ColumnData::Int64(
                    seqs.iter()
                        .enumerate()
                        .map(|(row, s)| {
                            if let Some(input) = by_name.get(pk_field.name.as_str())
                                && !input.is_null(row)
                                && let Some(Pk::Int64(id)) = input_pk(input, row)
                            {
                                return Ok(id);
                            }
                            i64::try_from(*s)
                        })
                        .collect::<Result<_, _>>()
                        .map_err(|_| EngineError::invalid("auto_id space exhausted"))?,
                )
            } else {
                convert_optional(field, by_name.get(field.name.as_str()).copied(), n)?
            };
            columns.push(column);
        }
        let mut normalized = None;
        let column_values = if self.persistent.is_some() {
            normalized
                .insert(
                    crate::storage::arrow_data::normalize_data_json(&self.schema, &mut columns)
                        .map_err(|error| match error {
                            crate::storage::arrow_data::ArrowDataError::NonFiniteJsonNumber {
                                ..
                            } => EngineError::InvalidArgument(error.to_string()),
                            _ => EngineError::Storage(error.to_string()),
                        })?,
                )
                .columns()
        } else {
            &columns
        };
        // schema 校验保证 dim > 0。
        let (dim, values) = vector_values(&column_values[self.vector_pos]);
        let norms = values.chunks_exact(dim).map(flat::norm).collect();
        let meta = self.schema.enable_dynamic_field.then_some(dynamic.maps);
        let (source, row_start) = self.persistent.as_ref().map_or_else(
            || {
                (
                    SourceId(1),
                    self.chunks
                        .last_key_value()
                        .map_or(0, |(_, chunk)| chunk.row_start + chunk.live.len),
                )
            },
            |p| (p.active.source, p.active.physical_rows()),
        );
        self.next_seq = next_seq;
        if let Some(persistent) = self.persistent.as_mut() {
            persistent
                .append_normalized_partition(
                    &seqs,
                    normalized
                        .ok_or_else(|| EngineError::Storage("normalized columns missing".into()))?,
                    meta.as_deref(),
                    partition,
                )
                .map_err(EngineError::Storage)?;
        }
        Ok(self.publish_insert(columns, meta, norms, seqs, source, row_start))
    }

    fn publish_insert(
        &mut self,
        columns: Vec<ColumnData>,
        meta: Option<Vec<Map<String, Value>>>,
        norms: Vec<f32>,
        seqs: Vec<u64>,
        source: SourceId,
        row_start: usize,
    ) -> Vec<Pk> {
        let n = seqs.len();
        // WAL 已成功追加；后续内存更新必须保持不可失败。
        let chunk_idx = self.next_chunk;
        self.next_chunk += 1;
        let generation = self.persistent.as_ref().map_or(1, |p| p.generation);
        let mut chunk = Chunk {
            data: Arc::new(ChunkData {
                columns,
                meta,
                norms,
                seqs: seqs.clone(),
                orders: Vec::with_capacity(n),
            }),
            live: SourceBits::from_live(&vec![true; n]),
            generation,
            source,
            row_start,
            source_rank: 0,
        };
        let mut ids = Vec::with_capacity(n);
        for row in 0..n {
            let pk = pk_at(&chunk.columns[self.pk_pos], row);
            let previous = self.pk_index.get(&pk).copied();
            let order = match previous {
                Some(old) if old.chunk == chunk_idx => old.order,
                Some(old) if self.chunks[&old.chunk].generation == generation => old.order,
                _ => {
                    let order = self.next_order;
                    self.next_order += 1;
                    order
                }
            };
            // 尚未发布的局部块独占此 Arc，不会复制列数据；发布后不得补写位次。
            Arc::make_mut(&mut chunk.data).orders.push(order);
            let new_ref = RowRef {
                order,
                chunk: chunk_idx,
                row,
            };
            // _seq 严格递增，因此新行总是该主键的最新版本（不变量 1–2）。
            if let Some(old) = self.pk_index.insert(pk.clone(), new_ref) {
                if old.chunk == chunk_idx {
                    chunk.live.set(old.row, false);
                } else {
                    self.clear_live(old);
                }
            }
            ids.push(pk);
        }
        self.chunks.insert(chunk_idx, chunk);
        self.merge_tail();
        ids
    }

    /// 只钉住命中的数据块，主键索引留在状态锁内，避免按总行数复制索引。
    pub fn prepare_get(&self, pks: &[Pk], output_fields: &[String]) -> Result<GetSnapshot> {
        self.prepare_partition_get(pks, output_fields, &[])
    }

    pub fn prepare_partition_get(
        &self,
        pks: &[Pk],
        output_fields: &[String],
        names: &[String],
    ) -> Result<GetSnapshot> {
        self.require_loaded()?;
        let selected = self.resolve_partitions(names)?;
        let mut snapshot = self.read_metadata();
        let mut mapping = HashMap::new();
        let mut locations = HashMap::<_, std::collections::BTreeMap<usize, (usize, &Chunk)>>::new();
        if selected.is_some() {
            for (&id, chunk) in &self.chunks {
                locations
                    .entry(chunk.source)
                    .or_default()
                    .insert(chunk.row_start, (id, chunk));
            }
        }
        let refs = pks
            .iter()
            .filter_map(|pk| {
                let mut at = if let Some(selected) = &selected {
                    if let Some(visibility) = self.partition_visibility() {
                        let at = visibility.visible_data(pk, Some(selected))?;
                        let (id, chunk) =
                            locations.get(&at.source)?.range(..=at.row).next_back()?.1;
                        if at.row - chunk.row_start >= chunk.seqs.len() {
                            return None;
                        }
                        RowRef {
                            chunk: *id,
                            row: at.row - chunk.row_start,
                            order: chunk.orders[at.row - chunk.row_start],
                        }
                    } else {
                        if !selected.contains(&0) {
                            return None;
                        }
                        *self.pk_index.get(pk)?
                    }
                } else {
                    *self.pk_index.get(pk)?
                };
                at.chunk = *mapping.entry(at.chunk).or_insert_with(|| {
                    let index = snapshot.chunks.len();
                    snapshot.chunks.push(self.chunks[&at.chunk].clone());
                    index
                });
                Some(at)
            })
            .collect();
        let projection = Projection::new(&self.schema, output_fields, ApiKind::Query);
        Ok(GetSnapshot {
            snapshot,
            refs,
            projection,
        })
    }

    pub fn get(&self, pks: &[Pk], output_fields: &[String]) -> Result<QueryResult> {
        Ok(self.prepare_get(pks, output_fields)?.read())
    }

    /// 类型不符的字面量查不到，与 3.x 的主键快路径相同。
    pub fn prepare_get_literals(
        &self,
        literals: &[PkLiteral],
        output_fields: &[String],
    ) -> Result<GetSnapshot> {
        self.prepare_partition_get_literals(literals, output_fields, &[])
    }

    pub fn prepare_partition_get_literals(
        &self,
        literals: &[PkLiteral],
        output_fields: &[String],
        names: &[String],
    ) -> Result<GetSnapshot> {
        let int_pk = self.schema.fields[self.pk_pos].data_type == DataType::Int64;
        let pks: Vec<Pk> = literals
            .iter()
            .filter_map(|lit| match lit {
                PkLiteral::Int(v) if int_pk => Some(Pk::Int64(*v)),
                PkLiteral::Str(v) if !int_pk => Some(Pk::VarChar(v.clone())),
                _ => None,
            })
            .collect();
        self.prepare_partition_get(&pks, output_fields, names)
    }

    pub fn get_literals(
        &self,
        literals: &[PkLiteral],
        output_fields: &[String],
    ) -> Result<QueryResult> {
        Ok(self.prepare_get_literals(literals, output_fields)?.read())
    }

    /// 数据与位图分别共享，后续覆盖只复制位图，不复制向量或动态字段。
    fn read_metadata(&self) -> ReadSnapshot {
        ReadSnapshot {
            name: self.name.clone(),
            schema: self.schema.clone(),
            pk_pos: self.pk_pos,
            vector_pos: self.vector_pos,
            chunks: Vec::new(),
            indexes: self.indexes.clone(),
            load_state: self.load_state,
            deleted: self.deleted,
            unusable: self.persistent.as_ref().is_some_and(|p| p.poisoned()),
        }
    }

    #[allow(clippy::expect_used)] // 持久化块与 visibility 来源在同一次状态写锁内登记/移除。
    pub fn read_snapshot(&self) -> ReadSnapshot {
        let mut snapshot = self.read_metadata();
        if let Some(persistent) = &self.persistent {
            let ranks: HashMap<_, _> = persistent
                .visibility
                .source_order()
                .iter()
                .enumerate()
                .map(|(rank, id)| (*id, rank))
                .collect();
            snapshot.chunks = self
                .chunks
                .values()
                .cloned()
                .map(|mut chunk| {
                    chunk.source_rank = *ranks
                        .get(&chunk.source)
                        .expect("persistent read source is registered");
                    chunk
                })
                .collect();
            snapshot
                .chunks
                .sort_by_key(|chunk| (chunk.source_rank, chunk.row_start));
            return snapshot;
        }
        let mut previous = None;
        let mut rank = 0;
        snapshot.chunks = self
            .chunks
            .values()
            .map(|chunk| {
                if previous.is_some_and(|source| source != chunk.source) {
                    rank += 1;
                }
                previous = Some(chunk.source);
                let mut chunk = chunk.clone();
                chunk.source_rank = rank;
                chunk
            })
            .collect();
        snapshot
    }

    /// 分区有效位复用同一列式读取管线；只重建位图，不复制列、向量或逐行判版本。
    pub fn read_snapshot_with_visibility(
        &self,
        visibility: &crate::storage::visibility::VisibilitySnapshot,
    ) -> Result<ReadSnapshot> {
        let ranks: HashMap<_, _> = visibility
            .source_order
            .iter()
            .enumerate()
            .map(|(rank, &id)| (id, rank))
            .collect();
        let mut snapshot = self.read_metadata();
        for chunk in self.chunks.values() {
            let rank = *ranks
                .get(&chunk.source)
                .ok_or_else(|| EngineError::Storage("selected visibility source missing".into()))?;
            let Some(bits) = visibility.source_bits(chunk.source) else {
                continue;
            };
            let mut chunk = chunk.clone();
            chunk.live = bits.slice(chunk.row_start, chunk.live.len).ok_or_else(|| {
                EngineError::Storage("selected visibility rows are incomplete".into())
            })?;
            chunk.source_rank = rank;
            snapshot.chunks.push(chunk);
        }
        snapshot
            .chunks
            .sort_by_key(|chunk| (chunk.source_rank, chunk.row_start));
        Ok(snapshot)
    }

    pub fn read_named_snapshot(&self, names: &[String]) -> Result<ReadSnapshot> {
        match self.resolve_partitions(names)? {
            None => Ok(self.read_snapshot()),
            Some(ids) if self.partition_visibility().is_some() => {
                self.read_partition_snapshot(Some(&ids))
            }
            Some(ids) if ids.contains(&0) => Ok(self.read_snapshot()),
            Some(_) => Ok(self.read_metadata()),
        }
    }

    pub fn partition_count(&self) -> usize {
        self.persistent
            .as_ref()
            .map_or(if self.memory_key.is_some() { 17 } else { 1 }, |p| {
                p.manifest.partitions.len()
            })
    }

    pub fn count_partition(&self, name: &str) -> Result<usize> {
        let exists = self.persistent.as_ref().map_or_else(
            || {
                self.memory_key
                    .as_ref()
                    .map_or(name == "_default", |m| m.registry.id(name).is_some())
            },
            |p| p.manifest.partitions.contains_key(name),
        );
        if !exists {
            return Err(EngineError::PartitionNotFound(name.into()));
        }
        let ids = self.resolve_partitions(&[name.into()])?;
        if let Some(visibility) = self.partition_visibility() {
            Ok(visibility.selected_snapshot(ids.as_deref()).live_count)
        } else {
            Ok(self.count())
        }
    }

    fn partition_visibility(&self) -> Option<&crate::storage::visibility::Visibility> {
        self.persistent
            .as_ref()
            .map(|p| &p.visibility)
            .or_else(|| self.memory_key.as_ref().map(|m| &m.visibility))
    }

    pub fn read_partition_snapshot(&self, selected: Option<&[u32]>) -> Result<ReadSnapshot> {
        let visibility = self.partition_visibility().ok_or_else(|| {
            EngineError::Storage("partition snapshot requires partition state".into())
        })?;
        self.read_snapshot_with_visibility(&visibility.selected_snapshot(selected))
    }

    pub fn resolve_partitions(&self, names: &[String]) -> Result<Option<Vec<u32>>> {
        if names.is_empty() {
            return Ok(None);
        }
        let mut ids = Vec::new();
        if let Some(p) = &self.persistent {
            for name in names {
                if p.manifest.partitions.contains_key(name) {
                    ids.push(p.partitions.id(name).ok_or_else(|| {
                        EngineError::Storage("partition identifier missing".into())
                    })?);
                }
            }
        } else if let Some(memory) = &self.memory_key {
            ids.extend(names.iter().filter_map(|name| memory.registry.id(name)));
        } else if names.iter().any(|name| name == "_default") {
            ids.push(0);
        }
        ids.sort_unstable();
        ids.dedup();
        Ok(Some(ids))
    }

    pub fn search(&self, req: SearchRequest, compute: &Compute) -> Result<SearchResult> {
        self.read_snapshot().search(req, compute)
    }

    #[cfg(test)]
    fn search_in_blocks(
        &self,
        req: SearchRequest,
        compute: &Compute,
        rows_per_block: usize,
    ) -> Result<SearchResult> {
        self.read_snapshot()
            .search_in_blocks(req, compute, rows_per_block)
    }

    pub fn require_loaded(&self) -> Result<()> {
        check_loaded(
            &self.name,
            self.load_state,
            self.deleted,
            self.persistent.as_ref().is_some_and(|p| p.poisoned()),
        )
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
        let InputValues::Json(texts) = values.values() else {
            return Err(EngineError::schema(format!(
                "dynamic field {} must carry JSON data",
                py_repr(META_FIELD)
            )));
        };
        if values.is_null(row) {
            return Err(EngineError::schema(format!(
                "dynamic field {} value must be a JSON object",
                py_repr(META_FIELD)
            )));
        }
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
        json_checks: &HashMap<&str, Vec<&InputValues>>,
        source_row: usize,
    ) -> Result<()> {
        let pk = &self.schema.fields[self.pk_pos];
        let primary = resolved_input(pk, by_name.get(pk.name.as_str()).copied(), row)?;
        if !pk.auto_id
            && primary
                .as_ref()
                .is_none_or(|(value, index)| value.is_null(*index))
        {
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
            Some(other) if input_is_null(other, row) => {
                return Err(EngineError::schema(format!(
                    "vector field {} missing or None",
                    py_repr(&vector.name)
                )));
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
            // 0004 的坏 JSON 拒绝也适用于被覆盖、掩码或默认值替换的值，但不能抢在本行
            // 主键、向量及前序字段检查之前。Upsert 的合并行只有一行，原始列仍按 source_row 取值。
            if let Some(previous) = json_checks.get(field.name.as_str()) {
                for values in previous {
                    if let InputValues::Json(raw) = values.values() {
                        parse_json(&field.name, &raw[source_row])?;
                    }
                }
            }
            if field.data_type.is_vector() {
                continue;
            }
            if i == self.pk_pos
                && pk.auto_id
                && let Some(values) = by_name.get(field.name.as_str())
                && input_is_null(values, row)
            {
                continue;
            }
            let Some(values) = by_name.get(field.name.as_str()) else {
                if i == self.pk_pos && pk.auto_id {
                    continue;
                }
                if let Some((value, index)) = resolved_input(field, None, row)? {
                    check_value(field, &value, index)?;
                    continue;
                }
                return Err(EngineError::schema(format!(
                    "{} {} missing and not nullable / no default",
                    if field.data_type == DataType::Array {
                        "ARRAY field"
                    } else {
                        "field"
                    },
                    py_repr(&field.name)
                )));
            };
            let (value, index) = resolved_input(field, Some(values), row)?
                .ok_or_else(|| EngineError::schema("field resolution lost a supplied value"))?;
            if i == self.pk_pos && pk.auto_id && value.is_null(index) {
                continue;
            }
            check_value(field, &value, index)?;
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
pub(crate) const BLOCK_ELEMS: usize = 1 << 20;

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
        while start < c.live.len {
            let end = c.live.len.min(start + rows_per_block - filled);
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
                .filter(|row| chunk.live.get(*row))
                .map(|row| (row, &values[row * dim..(row + 1) * dim], chunk.norms[row]))
        };
        let at = |row| RankedRow {
            source_rank: chunk.source_rank,
            at: RowRef {
                order: if chunk.generation == 0 {
                    (chunk.row_start + row) as u64
                } else {
                    chunk.orders[row]
                },
                chunk: seg.chunk,
                row,
            },
        };
        if let Ok(batch) = <&[flat::Query; flat::BATCH]>::try_from(queries) {
            for (row, candidate, norm) in live_rows() {
                let scores = flat::score_batch(batch, candidate, norm);
                for (top, score) in tops.iter_mut().zip(scores) {
                    top.push(score, || at(row));
                }
            }
        } else {
            for (query, top) in queries.iter().zip(tops.iter_mut()) {
                for (row, candidate, norm) in live_rows() {
                    top.push(query.score(candidate, norm), || at(row));
                }
            }
        }
    }
}

/// 数据源序号只存在于本次检索候选中，不扩大存活主键索引的行引用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RankedRow {
    source_rank: usize,
    at: RowRef,
}

/// 一条查询的前 `take` 个结果，按 (分数的名次, 行的位置) 这一全序排列。
///
/// 扫描物理行时位次不一定递增：同代覆盖会复用旧位次。
/// 所有叶子与归并都使用同一全序，因而结果与按该全序串行排序一致，且不依赖任务完成顺序。
/// 只保留可能进入前 `take` 个的候选，不收集全部分数。
struct TopK {
    metric: Metric,
    take: usize,
    candidates: Vec<(f32, RankedRow)>,
    /// 已确定的第 `take` 名；不排在它前面的候选不可能进入结果。
    bound: Option<(f32, RankedRow)>,
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

    #[inline(always)]
    fn push(&mut self, score: f32, pos: impl FnOnce() -> RankedRow) {
        if self.take == 0 {
            return;
        }
        // 严格更差的分数无需读取源内位次；并列（含 NaN 与 ±0）仍必须按完整位置判定。
        let rank = self.bound.map(|b| flat::rank(self.metric, score, b.0));
        if rank.is_some_and(|rank| rank.is_gt()) {
            return;
        }
        let pos = pos();
        if rank.is_some_and(|rank| rank.is_eq()) && self.bound.is_some_and(|b| pos >= b.1) {
            return;
        }
        self.candidates.push((score, pos));
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
            self.push(score, || pos);
        }
    }

    fn finish(mut self) -> Vec<(f32, RankedRow)> {
        self.shrink();
        let metric = self.metric;
        // 位置各不相同，全序下没有相等的元素，不稳定排序即可。
        self.candidates.sort_unstable_by(|a, b| order(metric, a, b));
        self.candidates
    }
}

fn order(metric: Metric, a: &(f32, RankedRow), b: &(f32, RankedRow)) -> std::cmp::Ordering {
    flat::rank(metric, a.0, b.0).then_with(|| a.1.cmp(&b.1))
}

fn vector_values(column: &ColumnData) -> (usize, &[f32]) {
    match column {
        ColumnData::FloatVector { dim, values } => (*dim, values),
        // resolve_anns_field 只返回 FLOAT_VECTOR 字段的下标。
        _ => unreachable!("anns field column must be FLOAT_VECTOR"),
    }
}

fn input_pk(values: &InputValues, row: usize) -> Option<Pk> {
    if values.is_null(row) {
        return None;
    }
    match values.values() {
        InputValues::Int(v) => Some(Pk::Int64(v[row])),
        InputValues::Str(v) => Some(Pk::VarChar(v[row].clone())),
        InputValues::JsonValues(v) => v[row]
            .as_i64()
            .map(Pk::Int64)
            .or_else(|| v[row].as_str().map(|v| Pk::VarChar(v.to_owned()))),
        _ => None,
    }
}

fn input_row(values: &InputValues, row: usize) -> InputValues {
    match values {
        InputValues::Array(rows) => InputValues::Array(vec![rows[row].clone()]),
        InputValues::Null(_) => InputValues::Null(1),
        InputValues::JsonValues(v) => InputValues::JsonValues(vec![v[row].clone()]),
        InputValues::Nullable { values, validity } => {
            if validity.get(row) {
                input_row(values, row)
            } else {
                InputValues::Null(1)
            }
        }
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

pub(crate) fn input_from_column(column: &ColumnData, row: usize) -> InputValues {
    match column {
        ColumnData::Array(array) => InputValues::Array(vec![Some(array.input(row))]),
        ColumnData::Nullable { values, validity } => {
            if validity.get(row) {
                input_from_column(values, row)
            } else {
                InputValues::Null(1)
            }
        }
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

pub(crate) fn input_from_column_all(column: &ColumnData) -> InputValues {
    match column {
        ColumnData::Array(array) => {
            InputValues::Array((0..array.len()).map(|row| Some(array.input(row))).collect())
        }
        ColumnData::Nullable { values, validity } => InputValues::Nullable {
            values: Box::new(input_from_column_all(values)),
            validity: validity.clone(),
        },
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
        InputValues::Array(rows) => match &rows[row] {
            None => Value::Null,
            Some(items) => Value::Array(
                (0..items.len())
                    .map(|i| to_json(name, items, i))
                    .collect::<Result<_>>()?,
            ),
        },
        InputValues::Null(_) => Value::Null,
        InputValues::JsonValues(v) => v[row].clone(),
        InputValues::Nullable { values, validity } => {
            if validity.get(row) {
                to_json(name, values, row)?
            } else {
                if matches!(values.values(), InputValues::Json(_)) {
                    to_json(name, values, row)?;
                }
                Value::Null
            }
        }
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
    if field.data_type == DataType::Array {
        if values.is_null(row) {
            return if field.nullable {
                Ok(())
            } else {
                Err(EngineError::schema(format!(
                    "ARRAY field {} missing and not nullable / no default",
                    py_repr(&field.name)
                )))
            };
        }
        let len = crate::data::array::row_len(values, row).ok_or_else(|| {
            EngineError::schema(format!(
                "ARRAY field {} must be list/tuple, got {}",
                py_repr(&field.name),
                py_type_name(values, row)
            ))
        })?;
        if let Some(max) = field.max_capacity
            && (len as i128) > i128::from(max)
        {
            return Err(EngineError::schema(format!(
                "ARRAY field {} has {len} elements, exceeds max_capacity={max}",
                py_repr(&field.name)
            )));
        }
        return Ok(());
    }
    if values.is_null(row) {
        return if field.nullable {
            Ok(())
        } else {
            Err(EngineError::schema(format!(
                "field {} is None but not nullable",
                py_repr(&field.name)
            )))
        };
    }
    if let InputValues::JsonValues(values) = values {
        let value = &values[row];
        let integer = value
            .as_number()
            .filter(|n| !n.as_str().contains(['.', 'e', 'E']));
        let ok = match field.data_type {
            DataType::Bool => value.is_boolean(),
            DataType::Int64 => {
                integer.is_some_and(|n| !field.is_partition_key || n.as_i64().is_some())
            }
            DataType::Int8 => integer
                .and_then(|n| n.as_i64())
                .is_some_and(|v| i8::try_from(v).is_ok()),
            DataType::Int16 => integer
                .and_then(|n| n.as_i64())
                .is_some_and(|v| i16::try_from(v).is_ok()),
            DataType::Int32 => integer
                .and_then(|n| n.as_i64())
                .is_some_and(|v| i32::try_from(v).is_ok()),
            DataType::Float | DataType::Double => value.is_number(),
            DataType::VarChar => {
                if let Some(text) = value.as_str() {
                    if let Some(max) = field.max_length
                        && text.chars().count() as i64 > max
                    {
                        return Err(EngineError::schema(format!(
                            "VARCHAR field {} value length {} exceeds max_length={max}",
                            py_repr(&field.name),
                            text.chars().count()
                        )));
                    }
                    true
                } else {
                    false
                }
            }
            DataType::Json => true,
            DataType::Array => unreachable!("ARRAY has separate outer validation"),
            DataType::FloatVector => false,
        };
        return if ok {
            Ok(())
        } else {
            // key 的宽整数在 P1 拒绝，诊断仍保留 Python int 的十进制文本。
            // 只处理本片新增 key；非 key 的既有默认值诊断不在这里改动。
            let value_repr = if field.is_partition_key {
                integer
                    .map(|n| {
                        n.as_i64()
                            .map_or_else(|| n.as_str().to_owned(), |n| n.to_string())
                    })
                    .unwrap_or_else(|| py_json_repr(value))
            } else {
                py_json_repr(value)
            };
            Err(EngineError::schema(format!(
                "field {} value {} does not match dtype {}",
                py_repr(&field.name),
                value_repr,
                field.data_type.qualified()
            )))
        };
    }
    let values = values.values();
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
            if parse_json(&field.name, &v[row])?.is_null() && !field.nullable {
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

/// 保存可能在 Upsert/default 处理时被替换的原始 JSON，在字段校验阶段再报告错误。
fn retain_masked_json<'a>(
    supplied: &HashMap<&'a str, &'a InputValues>,
    checks: &mut HashMap<&'a str, Vec<&'a InputValues>>,
) {
    for (&name, &values) in supplied {
        if matches!(values, InputValues::Nullable { values, .. } if matches!(values.as_ref(), InputValues::Json(_)))
        {
            checks.entry(name).or_default().push(values);
        }
    }
}

/// JSON null 只允许首尾的四种 JSON 空白；不解析普通 JSON，也不把畸形文本视为空值。
fn json_text_is_null(text: &[u8]) -> bool {
    let whitespace = |byte: &u8| matches!(byte, b' ' | b'\t' | b'\n' | b'\r');
    let start = text
        .iter()
        .position(|byte| !whitespace(byte))
        .unwrap_or(text.len());
    text[start..]
        .strip_prefix(b"null")
        .is_some_and(|suffix| suffix.iter().all(whitespace))
}

fn input_is_null(values: &InputValues, row: usize) -> bool {
    let semantic = match values.values() {
        // 存在性探测只识别 JSON null；畸形文本视为非空，由后续字段类型/JSON 校验拒绝。
        // 这样主键/向量存在性检查不会提前报告 JSON 错误；被掩码替换的值另由 json_checks 检查。
        InputValues::Json(v) => json_text_is_null(&v[row]),
        InputValues::JsonValues(v) => v[row].is_null(),
        InputValues::Null(_) => true,
        _ => false,
    };
    semantic || values.is_null(row)
}

fn resolved_input<'a>(
    field: &FieldSchema,
    values: Option<&'a InputValues>,
    row: usize,
) -> Result<Option<(std::borrow::Cow<'a, InputValues>, usize)>> {
    if let Some(values) = values
        && !input_is_null(values, row)
    {
        return Ok(Some((std::borrow::Cow::Borrowed(values.values()), row)));
    }
    if let Some(default) = field.default_value.as_ref().filter(|v| !v.is_null()) {
        return Ok(Some((
            std::borrow::Cow::Owned(InputValues::JsonValues(vec![default.clone()])),
            0,
        )));
    }
    if values.is_some() || field.nullable {
        return Ok(Some((std::borrow::Cow::Owned(InputValues::Null(1)), 0)));
    }
    Ok(None)
}

fn convert_optional(
    field: &FieldSchema,
    values: Option<&InputValues>,
    n: usize,
) -> Result<ColumnData> {
    if let Some(values) = values
        && !matches!(
            values,
            InputValues::Nullable { .. } | InputValues::Null(_) | InputValues::JsonValues(_)
        )
    {
        let contains_null = if let InputValues::Array(rows) = values {
            rows.iter().any(Option::is_none)
        } else if let InputValues::Json(raw) = values {
            raw.iter().any(|text| json_text_is_null(text))
        } else {
            false
        };
        if !contains_null {
            return convert_nonnull(field, values);
        }
    }
    let mut column = ColumnData::empty(field);
    for row in 0..n {
        let (input, index) = resolved_input(field, values, row)?.ok_or_else(|| {
            EngineError::schema(format!(
                "field {} missing and not nullable / no default",
                py_repr(&field.name)
            ))
        })?;
        let converted = if input.is_null(index) {
            if !field.nullable {
                return Err(EngineError::schema(format!(
                    "field {} is None but not nullable",
                    py_repr(&field.name)
                )));
            }
            ColumnData::nulls(field, 1)
        } else if field.data_type == DataType::Array {
            crate::data::array::convert(field, &input_row(&input, index))?
        } else if let InputValues::JsonValues(values) = input.as_ref() {
            let value = &values[index];
            let concrete = if field.data_type == DataType::Json {
                // 默认值沿用 0004 已登记的 JSON 顶层标量/字符串规则。
                InputValues::Json(vec![crate::pyjson::dumps_unicode(value).into_bytes()])
            } else {
                match value {
                    Value::Bool(v) => InputValues::Bool(vec![*v]),
                    Value::String(v) => InputValues::Str(vec![v.clone()]),
                    Value::Number(v) if !v.as_str().contains(['.', 'e', 'E']) => {
                        InputValues::Int(vec![v.as_i64().ok_or_else(|| {
                            EngineError::invalid("Python int too large to convert to C long")
                        })?])
                    }
                    Value::Number(v) => InputValues::Float(vec![v.as_f64().ok_or_else(|| {
                        EngineError::invalid("float value cannot be represented")
                    })?]),
                    _ => return Err(EngineError::schema("invalid default value type")),
                }
            };
            convert_nonnull(field, &concrete)?
        } else {
            convert_nonnull(field, &input_row(&input, index))?
        };
        column.push_row(&converted, 0);
    }
    Ok(column)
}

/// 校验通过后按字段类型转换为存储列。
fn convert_nonnull(field: &FieldSchema, values: &InputValues) -> Result<ColumnData> {
    if field.data_type == DataType::Array {
        return crate::data::array::convert(field, values);
    }
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
pub(crate) fn exact_int(x: i64, limit: i64, precision: &str) -> Result<i64> {
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
pub(crate) fn py_type_name(values: &InputValues, row: usize) -> &'static str {
    match values {
        InputValues::Array(rows) => {
            if rows[row].is_some() {
                "list"
            } else {
                "NoneType"
            }
        }
        InputValues::Null(_) => "NoneType",
        InputValues::Nullable { values, validity } => {
            if validity.get(row) {
                py_type_name(values, row)
            } else {
                "NoneType"
            }
        }
        InputValues::JsonValues(v) => match &v[row] {
            Value::Null => "NoneType",
            Value::Bool(_) => "bool",
            Value::String(_) => "str",
            Value::Array(_) => "list",
            Value::Object(_) => "dict",
            Value::Number(n) => {
                if n.as_str().contains(['.', 'e', 'E']) {
                    "float"
                } else {
                    "int"
                }
            }
        },
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
            Ok(Value::Null) => "NoneType",
            // 参照的 JSON 解码失败后保留原文本；存在性探测已把它与 null 区分。
            Err(_) => "str",
        },
    }
}

/// Python `repr(value)`，用于 3.x 的 `value {value!r} does not match dtype` 信息。
pub(crate) fn py_value_repr(values: &InputValues, row: usize) -> String {
    match values {
        InputValues::Array(rows) => match &rows[row] {
            None => "None".into(),
            Some(items) => format!(
                "[{}]",
                (0..items.len())
                    .map(|i| py_value_repr(items, i))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
        InputValues::Null(_) => "None".into(),
        InputValues::Nullable { values, validity } => {
            if validity.get(row) {
                py_value_repr(values, row)
            } else {
                "None".into()
            }
        }
        InputValues::JsonValues(v) => py_json_repr(&v[row]),
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

fn check_loaded(name: &str, state: LoadState, deleted: bool, unusable: bool) -> Result<()> {
    if deleted {
        return Err(EngineError::CollectionNotFound(name.to_owned()));
    }
    if unusable {
        return Err(EngineError::Storage(
            "collection state is unusable after a storage failure".to_owned(),
        ));
    }
    match state {
        LoadState::Loaded => Ok(()),
        LoadState::Released => Err(EngineError::CollectionNotLoaded(name.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_json_default_keeps_normalized_wal_bytes() {
        use crate::{Compute, DataDir, Db};
        for key in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let db = Db::new(
                DataDir::open(root.path()).unwrap(),
                Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
            )
            .unwrap();
            db.create_collection(
                "",
                "c",
                CollectionSchema {
                    fields: vec![
                        FieldSchema {
                            is_primary: true,
                            ..FieldSchema::new("id", DataType::Int64)
                        },
                        FieldSchema {
                            is_partition_key: key,
                            ..FieldSchema::new("k", DataType::Int64)
                        },
                        FieldSchema {
                            dim: Some(2),
                            ..FieldSchema::new("v", DataType::FloatVector)
                        },
                        FieldSchema {
                            default_value: Some(
                                serde_json::from_str(r#"{"z":1E2,"a":"中文"}"#).unwrap(),
                            ),
                            ..FieldSchema::new("j", DataType::Json)
                        },
                    ],
                    ..Default::default()
                },
            )
            .unwrap();
            crate::storage::arrow_data::take_json_encoding_count();
            db.insert(
                "",
                "c",
                InsertBatch {
                    num_rows: 2,
                    columns: vec![
                        ("id".into(), InputValues::Int(vec![1, 2])),
                        ("k".into(), InputValues::Int(vec![0, 1])),
                        (
                            "v".into(),
                            InputValues::FloatVector {
                                dim: 2,
                                values: vec![1.; 4],
                            },
                        ),
                    ],
                },
            )
            .unwrap();
            assert_eq!(crate::storage::arrow_data::take_json_encoding_count(), 2);
            let wal = crate::storage::wal::read_prefix(
                &root.path().join("collections/c/wal/wal_data_000001.arrow"),
            )
            .unwrap();
            assert_eq!(
                wal.batches
                    .iter()
                    .map(arrow_array::RecordBatch::num_rows)
                    .sum::<usize>(),
                2
            );
            for batch in wal.batches {
                let column = batch
                    .column(5)
                    .as_any()
                    .downcast_ref::<arrow_array::StringArray>()
                    .unwrap();
                for row in 0..batch.num_rows() {
                    assert_eq!(
                        column.value(row).as_bytes(),
                        r#"{"z": 100.0, "a": "中文"}"#.as_bytes()
                    );
                }
            }
            db.close();
        }
    }

    #[test]
    fn persistent_json_write_normalizes_once_for_insert_and_upsert_in_every_bucket() {
        use crate::storage::arrow_data::take_json_encoding_count;
        use crate::{Compute, DataDir, Db};
        for key in [false, true] {
            for nullable in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let db = Db::new(
                    DataDir::open(root.path()).unwrap(),
                    Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
                )
                .unwrap();
                let schema = CollectionSchema {
                    fields: vec![
                        FieldSchema {
                            is_primary: true,
                            ..FieldSchema::new("id", DataType::Int64)
                        },
                        FieldSchema {
                            is_partition_key: key,
                            ..FieldSchema::new("k", DataType::Int64)
                        },
                        FieldSchema {
                            dim: Some(2),
                            ..FieldSchema::new("v", DataType::FloatVector)
                        },
                        FieldSchema {
                            nullable,
                            ..FieldSchema::new("j", DataType::Json)
                        },
                    ],
                    ..Default::default()
                };
                db.create_collection("", "c", schema).unwrap();
                let mut json = InputValues::Json(vec![
                    br#" {"z":1E2,"a":2} "#.to_vec(),
                    br#"{"k":[1,2.5]}"#.to_vec(),
                    br#"{"last":true}"#.to_vec(),
                ]);
                if nullable {
                    json = InputValues::Nullable {
                        values: Box::new(json),
                        validity: crate::data::Validity::from_bools(&[true, false, true]),
                    };
                }
                let batch = InsertBatch {
                    num_rows: 3,
                    columns: vec![
                        ("id".into(), InputValues::Int(vec![1, 2, 1])),
                        ("k".into(), InputValues::Int(vec![0, 1, 11])),
                        (
                            "v".into(),
                            InputValues::FloatVector {
                                dim: 2,
                                values: vec![1.; 6],
                            },
                        ),
                        ("j".into(), json),
                    ],
                };
                take_json_encoding_count();
                db.insert("", "c", batch.clone()).unwrap();
                assert_eq!(
                    take_json_encoding_count(),
                    3,
                    "insert key={key} nullable={nullable}"
                );
                db.upsert_with_pk_type("", "c", batch.clone()).unwrap();
                assert_eq!(
                    take_json_encoding_count(),
                    3,
                    "upsert key={key} nullable={nullable}"
                );
                let handle = db.collection("", "c").unwrap();
                let next_seq = handle.read().next_seq;
                let wal_dir = root.path().join("collections/c/wal");
                let files = || {
                    std::fs::read_dir(&wal_dir)
                        .unwrap()
                        .map(|entry| {
                            let path = entry.unwrap().path();
                            (
                                path.file_name().unwrap().to_owned(),
                                std::fs::read(path).unwrap(),
                            )
                        })
                        .collect::<std::collections::BTreeMap<_, _>>()
                };
                let before = files();
                let mut bad = batch;
                bad.columns[3].1 = InputValues::Json(vec![
                    b"{}".to_vec(),
                    br#"{"bad":1e999}"#.to_vec(),
                    b"{}".to_vec(),
                ]);
                assert!(db.insert("", "c", bad.clone()).is_err());
                assert_eq!(handle.read().next_seq, next_seq);
                assert_eq!(files(), before);
                assert!(db.upsert_with_pk_type("", "c", bad).is_err());
                assert_eq!(handle.read().next_seq, next_seq);
                assert_eq!(files(), before);
                db.close();
            }
        }
    }

    #[test]
    fn selected_visibility_reuses_columns_and_old_vector_snapshot_after_mask_update() {
        use crate::storage::visibility::partitions::{
            PartitionRow, PartitionVisibility, SourceKind,
        };
        use crate::storage::visibility::{RowLocation, SourceId};
        let mut id = FieldSchema::new("id", DataType::Int64);
        id.is_primary = true;
        let mut vector = FieldSchema::new("v", DataType::FloatVector);
        vector.dim = Some(2);
        let mut collection = Collection::new(
            "c",
            CollectionSchema {
                fields: vec![id, vector],
                enable_dynamic_field: false,
                properties: vec![],
            },
        )
        .unwrap();
        collection
            .insert(InsertBatch {
                num_rows: 2,
                columns: vec![
                    ("id".into(), InputValues::Int(vec![1, 2])),
                    (
                        "v".into(),
                        InputValues::FloatVector {
                            dim: 2,
                            values: vec![1., 0., 2., 0.],
                        },
                    ),
                ],
            })
            .unwrap();
        let mut visibility = PartitionVisibility::default();
        visibility
            .register_source(SourceId(1), 2, SourceKind::Active)
            .unwrap();
        for row in 0..2 {
            visibility
                .insert(
                    Pk::Int64(row + 1),
                    PartitionRow {
                        partition: row as u32,
                        location: RowLocation {
                            source: SourceId(1),
                            row: row as usize,
                            seq: row as u64 + 1,
                        },
                    },
                )
                .unwrap();
        }
        let snapshot = collection
            .read_snapshot_with_visibility(&visibility.snapshot(Some(&[0])))
            .unwrap();
        assert!(Arc::ptr_eq(
            &snapshot.chunks[0].data,
            &collection.chunks.values().next().unwrap().data
        ));
        visibility.committed_delete(Pk::Int64(1), 3).unwrap();
        assert_eq!(
            snapshot
                .query("", &["v".into()], None, 0)
                .unwrap()
                .rows
                .fields[0]
                .1,
            ColumnData::FloatVector {
                dim: 2,
                values: vec![1., 0.]
            }
        );
        assert_eq!(
            collection
                .read_snapshot_with_visibility(&visibility.snapshot(Some(&[0])))
                .unwrap()
                .count_filtered("")
                .unwrap(),
            0
        );
        let mut wrong = PartitionVisibility::default();
        wrong
            .register_source(SourceId(1), 1, SourceKind::Active)
            .unwrap();
        assert!(
            collection
                .read_snapshot_with_visibility(&wrong.snapshot(None))
                .is_err()
        );
        assert_eq!(collection.count(), 2);
    }

    #[test]
    fn active_range_rejects_nan_scores_and_nan_bounds() {
        for metric in [Metric::L2, Metric::Ip, Metric::Cosine] {
            for range in [
                SearchRange {
                    radius: Some(10.),
                    range_filter: None,
                },
                SearchRange {
                    radius: None,
                    range_filter: Some(10.),
                },
            ] {
                assert!(!range.contains(metric, f32::NAN));
            }
            assert!(
                !SearchRange {
                    radius: Some(f64::NAN),
                    range_filter: None
                }
                .contains(metric, 1.)
            );
            assert!(
                !SearchRange {
                    radius: None,
                    range_filter: Some(f64::NAN)
                }
                .contains(metric, 1.)
            );
        }
    }

    #[test]
    fn json_null_probe_matches_parser_and_rejects_non_json_whitespace() {
        let check = |text: &[u8]| {
            let expected = serde_json::from_slice::<Value>(text).is_ok_and(|value| value.is_null());
            assert_eq!(json_text_is_null(text), expected, "{text:?}");
        };
        // 覆盖全部字节边界，防止 trim_ascii 扩大 JSON 空白集合或 UTF-8 解码改变语义。
        for prefix in 0..=u8::MAX {
            for suffix in 0..=u8::MAX {
                check(&[prefix, b'n', b'u', b'l', b'l', suffix]);
            }
        }
        for text in [
            &b"null"[..],
            b" \t\n\rnull\r\n\t ",
            b"",
            b" \t\r\n",
            b"n ull",
            b"nu\nll",
            b"null null",
            b"null0",
            b"\x0cnull",
            b"null\x0b",
            b"\xef\xbb\xbfnull",
            b"\xc2\xa0null",
            b"null\0",
            b"\"null\"",
            b"[null]",
            b"{\"x\":null}",
            b"false",
            b"0",
            b"{}",
            b"[]",
            b"NULL",
            b"nul",
            b"null\xff",
        ] {
            check(text);
        }
    }

    #[test]
    fn zero_row_source_skips_sidecar_metadata() {
        use crate::{DataDir, Db};
        let root = tempfile::tempdir().unwrap();
        let mut id = FieldSchema::new("id", DataType::Int64);
        id.is_primary = true;
        let mut v = FieldSchema::new("v", DataType::FloatVector);
        v.dim = Some(2);
        let db = Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap(),
        )
        .unwrap();
        db.create_collection(
            "",
            "c",
            CollectionSchema {
                fields: vec![id, v],
                enable_dynamic_field: false,
                properties: vec![],
            },
        )
        .unwrap();
        let handle = db.collection("", "c").unwrap();
        {
            let mut c = handle.write();
            c.indexes.push(IndexSpec {
                field_name: "v".into(),
                index_type: "HNSW".into(),
                metric_type: "NONE".into(),
                params: Map::new(),
                search_params: Map::new(),
            });
            c.release();
            let p = c.persistent_mut().unwrap();
            // 零行数据文件在恢复入口已拒绝；直接验证 load 原语对零行来源的跳过边界。
            p.data_sources.insert(
                ("_default".into(), "data/zero.parquet".into()),
                p.active.source,
            );
            let parent = p.dir.join("partitions/_default");
            std::fs::create_dir_all(&parent).unwrap();
            std::fs::write(parent.join("indexes"), b"stat would fail").unwrap();
            c.load().unwrap();
            assert_eq!(c.load_state(), LoadState::Loaded);
        }
        db.close();
    }

    #[test]
    fn recovered_segments_move_buffers_into_read_blocks_while_wal_keeps_its_batch() {
        use crate::storage::recovery::load_committed;
        use crate::{DataDir, Db};
        let root = tempfile::tempdir().unwrap();
        let mut id = FieldSchema::new("id", DataType::Int64);
        id.is_primary = true;
        let mut vector = FieldSchema::new("v", DataType::FloatVector);
        vector.dim = Some(2);
        let mut tag = FieldSchema::new("tag", DataType::VarChar);
        tag.max_length = Some(100);
        let schema = CollectionSchema {
            fields: vec![id, vector, tag, FieldSchema::new("json", DataType::Json)],
            enable_dynamic_field: true,
            properties: vec![],
        };
        let db = Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap(),
        )
        .unwrap();
        db.create_collection("", "c", schema).unwrap();
        for ids in [vec![1, 2], vec![3, 4]] {
            db.insert(
                "",
                "c",
                InsertBatch {
                    num_rows: 2,
                    columns: vec![
                        ("id".into(), InputValues::Int(ids)),
                        (
                            "v".into(),
                            InputValues::FloatVector {
                                dim: 2,
                                values: vec![1.0, 0.0, 2.0, 0.0],
                            },
                        ),
                        (
                            "tag".into(),
                            InputValues::Str(vec!["中文".into(), "é".into()]),
                        ),
                        (
                            "json".into(),
                            InputValues::Json(vec![br#"{"wide":9007199254740993}"#.to_vec(); 2]),
                        ),
                        (
                            "$meta".into(),
                            InputValues::Json(vec![br#"{"x":"dynamic"}"#.to_vec(); 2]),
                        ),
                    ],
                },
            )
            .unwrap();
            db.flush_collection("", "c").unwrap();
        }
        db.close();
        let dir = root.path().join("collections/c");
        let mut recovered = load_committed(&dir).unwrap();
        fn addresses(
            columns: &[ColumnData],
            seqs: &[u64],
            meta: &Option<Vec<Map<String, Value>>>,
        ) -> [usize; 5] {
            let ColumnData::FloatVector { values, .. } = &columns[1] else {
                panic!("vector")
            };
            let ColumnData::VarChar(tags) = &columns[2] else {
                panic!("tag")
            };
            let ColumnData::Json(json) = &columns[3] else {
                panic!("json")
            };
            [
                values.as_ptr() as usize,
                tags[0].as_ptr() as usize,
                json[0].as_ptr() as usize,
                seqs.as_ptr() as usize,
                meta.as_ref().unwrap().as_ptr() as usize,
            ]
        }
        let original = recovered
            .segments
            .iter()
            .flat_map(|s| &s.batches)
            .map(|b| addresses(&b.columns, &b.seqs, &b.meta))
            .collect::<Vec<_>>();
        assert_eq!(original.len(), 2);
        let mut wal = recovered.segments[0].batches[0].clone();
        wal.seqs = vec![5, 6];
        wal.columns[0] = ColumnData::Int64(vec![5, 6]);
        recovered
            .active_memtable
            .apply_data(&recovered.schema, wal, &mut recovered.visibility)
            .unwrap();
        recovered.next_seq = 7;
        let active = Arc::clone(&recovered.active_memtable.read_batches()[0]);
        let active_addresses = addresses(&active.columns, &active.seqs, &active.meta);
        let c = Collection::from_recovered("c", recovered, dir).unwrap();
        for (chunk, expected) in c
            .chunks
            .values()
            .filter(|c| c.generation == 0)
            .zip(&original)
        {
            assert_eq!(
                addresses(&chunk.columns, &chunk.seqs, &chunk.meta),
                *expected
            );
            assert_eq!(
                chunk.norms,
                vec![flat::norm(&[1.0, 0.0]), flat::norm(&[2.0, 0.0])]
            );
        }
        assert_eq!(
            c.chunks.values().filter(|c| c.generation == 0).count(),
            original.len()
        );
        let retained = &c.persistent().unwrap().active.read_batches()[0];
        assert!(Arc::ptr_eq(&active, retained));
        assert_eq!(
            addresses(&retained.columns, &retained.seqs, &retained.meta),
            active_addresses
        );
        let wal_chunk = c.chunks.values().find(|c| c.generation != 0).unwrap();
        assert_ne!(
            addresses(&wal_chunk.columns, &wal_chunk.seqs, &wal_chunk.meta)[0],
            active_addresses[0]
        );
        assert_eq!(c.count(), 6);
        assert_eq!(
            c.get(&[Pk::Int64(1), Pk::Int64(5)], &["tag".into()])
                .unwrap()
                .rows
                .pks,
            vec![Pk::Int64(1), Pk::Int64(5)]
        );
    }

    #[test]
    fn recovery_requires_a_position_for_live_rows_but_allows_dead_row_placeholders() {
        use crate::storage::recovery::load_committed;
        use crate::{DataDir, Db};
        for force_live in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mut rng = Rng(3);
            let db = Db::new(
                DataDir::open(root.path()).unwrap(),
                Compute::for_host().unwrap(),
            )
            .unwrap();
            db.create_collection("", "c", collection(&mut rng, 2).schema.clone())
                .unwrap();
            db.close();
            let dir = root.path().join("collections/c");
            let mut recovered = load_committed(&dir).unwrap();
            let source = recovered.active_memtable.source;
            recovered.visibility.apply_delete(Pk::Int64(1), 5).unwrap();
            recovered
                .active_memtable
                .apply_data(
                    &recovered.schema,
                    DecodedData {
                        seqs: vec![1],
                        columns: vec![
                            ColumnData::Int64(vec![1]),
                            ColumnData::FloatVector {
                                dim: 2,
                                values: vec![1.0, 0.0],
                            },
                        ],
                        meta: None,
                    },
                    &mut recovered.visibility,
                )
                .unwrap();
            // 构造不一致的恢复对象：位图声称有效，但 memtable 未保留这行的位次。
            if force_live {
                recovered
                    .visibility
                    .apply_row(Pk::Int64(1), 6, source, 0)
                    .unwrap();
            }
            let result = Collection::from_recovered("c", recovered, dir);
            if force_live {
                assert!(
                    matches!(result, Err(EngineError::Storage(message)) if message.contains("has no memtable position"))
                );
            } else {
                assert_eq!(result.unwrap().count(), 0);
            }
        }
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn publication_poison_after_committed_manifest_recovers_without_replaying_retained_wal() {
        use crate::storage::{fs_commit, runtime};
        use crate::{DataDir, Db};
        let root = tempfile::tempdir().unwrap();
        let mut rng = Rng(9);
        let db = Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap(),
        )
        .unwrap();
        db.create_collection("", "c", collection(&mut rng, 2).schema.clone())
            .unwrap();
        db.insert(
            "",
            "c",
            InsertBatch {
                num_rows: 2,
                columns: vec![
                    ("id".into(), InputValues::Int(vec![1, 2])),
                    (
                        "v".into(),
                        InputValues::FloatVector {
                            dim: 2,
                            values: vec![1.0, 0.0, 2.0, 0.0],
                        },
                    ),
                ],
            },
        )
        .unwrap();
        let handle = db.collection("", "c").unwrap();
        let mut c = handle.write();
        c.persistent_mut().unwrap().freeze_active(false).unwrap();
        let persistent = c.persistent().unwrap();
        let work = persistent.front().unwrap();
        let dir = persistent.dir.clone();
        let manifest = persistent.manifest.clone();
        let schema = c.schema().clone();
        drop(c);
        fs_commit::inject_manifest_dir_sync_failure_once();
        let outcome = runtime::flush_one(&dir, &manifest, &work, &schema, true).unwrap();
        assert!(matches!(
            outcome.post_commit_error,
            Some(runtime::PostCommitError::Durability(_))
        ));
        assert_eq!(std::fs::read_dir(dir.join("wal")).unwrap().count(), 1);
        let mut mapping = outcome.mapping;
        mapping[0] = None;
        assert!(
            handle
                .write()
                .publish_generation(
                    &work,
                    outcome.manifest,
                    &mapping,
                    outcome.data,
                    outcome.norms,
                    true
                )
                .is_err()
        );
        assert!(matches!(
            db.collection("", "c"),
            Err(EngineError::Storage(_))
        ));
        db.close();
        let reopened = Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap(),
        )
        .unwrap();
        let recovered = reopened.collection("", "c").unwrap();
        assert_storage_mapping(&recovered.read());
        assert_eq!(recovered.read().count(), 2);
        assert_eq!(
            recovered
                .read()
                .get(&[Pk::Int64(1), Pk::Int64(2)], &[])
                .unwrap()
                .rows
                .pks,
            [Pk::Int64(1), Pk::Int64(2)]
        );
        reopened.close();
    }

    #[test]
    fn committed_ties_use_source_rank_then_original_parquet_row() {
        use crate::{DataDir, Db};
        let root = tempfile::tempdir().unwrap();
        let mut rng = Rng(6);
        let db = Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap(),
        )
        .unwrap();
        db.create_collection("", "c", collection(&mut rng, 2).schema.clone())
            .unwrap();
        let rows = |ids: &[i64]| InsertBatch {
            num_rows: ids.len(),
            columns: vec![
                ("id".into(), InputValues::Int(ids.to_vec())),
                (
                    "v".into(),
                    InputValues::FloatVector {
                        dim: 2,
                        values: [1.0, 0.0].repeat(ids.len()),
                    },
                ),
            ],
        };
        db.insert("", "c", rows(&[1, 2, 3])).unwrap();
        db.flush_collection("", "c").unwrap();
        db.delete("", "c", &[Pk::Int64(1), Pk::Int64(2)]).unwrap();
        db.insert("", "c", rows(&[4, 5])).unwrap();
        db.flush_collection("", "c").unwrap();
        let handle = db.collection("", "c").unwrap();
        let c = handle.read();
        assert_storage_mapping(&c);
        let first = c.chunks.first_key_value().unwrap().1;
        assert_eq!(first.live.len, 3);
        assert_eq!(
            (first.live.get(0), first.live.get(1), first.live.get(2)),
            (false, false, true)
        );
        let req = SearchRequest {
            anns_field: None,
            vectors: QueryVectors::Dense(vec![vec![1.0, 0.0]]),
            metric_type: Some("L2".into()),
            limit: 10,
            offset: 0,
            output_fields: vec![],
        };
        assert_eq!(
            c.search(req, db.compute()).unwrap().hits[0].rows.pks,
            [Pk::Int64(3), Pk::Int64(4), Pk::Int64(5)]
        );
        drop(c);
        db.close();
    }

    fn assert_storage_mapping(c: &Collection) {
        let persistent = c.persistent().unwrap();
        let visibility = persistent.visibility.snapshot();
        let mut physical_rows = HashMap::<SourceId, usize>::new();
        let mut live_count = 0;
        for (&id, chunk) in &c.chunks {
            let cursor = physical_rows.entry(chunk.source).or_default();
            assert_eq!(chunk.row_start, *cursor);
            *cursor += chunk.live.len;
            assert_eq!(chunk.seqs.len(), chunk.live.len);
            assert_eq!(chunk.orders.len(), chunk.live.len);
            for row in 0..chunk.live.len {
                assert_eq!(
                    visibility.is_live(chunk.source, chunk.row_start + row),
                    Some(chunk.live.get(row))
                );
                if chunk.live.get(row) {
                    live_count += 1;
                    let pk = pk_at(&chunk.columns[c.pk_pos], row);
                    let at = *c.pk_index.get(&pk).unwrap();
                    assert_eq!((at.chunk, at.row), (id, row));
                    let stored = persistent.visibility.latest_data(&pk).unwrap();
                    assert_eq!(
                        (stored.source, stored.row, stored.seq),
                        (chunk.source, chunk.row_start + row, chunk.seqs[row])
                    );
                }
            }
        }
        assert_eq!(live_count, c.count());
        assert_eq!(live_count, visibility.live_count);
        let committed_sources = c
            .chunks
            .values()
            .filter(|c| c.generation == 0)
            .map(|c| c.source)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            committed_sources.len(),
            persistent.manifest.partitions["_default"].data_files.len()
        );
    }

    #[test]
    fn row_model_matches_blocks_primary_references_and_storage_after_each_publication() {
        use crate::{DataDir, Db};
        let root = tempfile::tempdir().unwrap();
        let mut rng = Rng(19);
        let template = collection(&mut rng, 2);
        let db = Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap(),
        )
        .unwrap();
        db.create_collection("", "c", template.schema.clone())
            .unwrap();
        let handle = db.collection("", "c").unwrap();
        let mut model = HashMap::new();
        let mut previous_snapshots = Vec::new();
        let req = SearchRequest {
            anns_field: None,
            vectors: QueryVectors::Dense(vec![vec![0.0, 0.0]]),
            metric_type: Some("L2".into()),
            limit: 100,
            offset: 0,
            output_fields: vec!["*".into()],
        };
        for step in 0..180 {
            let pk = rng.below(13) as i64;
            if step % 7 == 0 {
                db.delete("", "c", &[Pk::Int64(pk)]).unwrap();
                model.remove(&pk);
            } else {
                let value = step as f32;
                let batch = InsertBatch {
                    num_rows: 1,
                    columns: vec![
                        ("id".into(), InputValues::Int(vec![pk])),
                        (
                            "v".into(),
                            InputValues::FloatVector {
                                dim: 2,
                                values: vec![value, 0.0],
                            },
                        ),
                    ],
                };
                if step % 3 == 0 {
                    db.upsert_with_pk_type("", "c", batch).unwrap();
                } else {
                    db.insert("", "c", batch).unwrap();
                }
                model.insert(pk, value);
            }
            if step % 11 == 0 {
                let snapshot = handle.read().read_snapshot();
                let expected = snapshot.search(req.clone(), db.compute()).unwrap();
                previous_snapshots.push((snapshot, expected));
                db.flush_collection("", "c").unwrap();
            }
            let c = handle.read();
            assert_storage_mapping(&c);
            assert_eq!(c.count(), model.len());
            let mut ids: Vec<_> = model.keys().copied().collect();
            ids.sort();
            let pks: Vec<_> = ids.iter().map(|id| Pk::Int64(*id)).collect();
            let actual = c.get(&pks, &["v".into()]).unwrap();
            assert_eq!(actual.rows.pks, pks);
            assert_eq!(
                actual.rows.fields[0].1,
                ColumnData::FloatVector {
                    dim: 2,
                    values: ids.iter().flat_map(|id| [model[id], 0.0]).collect(),
                }
            );
            drop(c);
            for (snapshot, expected) in &previous_snapshots {
                assert_eq!(
                    &snapshot.search(req.clone(), db.compute()).unwrap(),
                    expected
                );
            }
        }
        db.close();
        let reopened = Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap(),
        )
        .unwrap();
        assert_storage_mapping(&reopened.collection("", "c").unwrap().read());
        reopened.close();
    }

    #[test]
    fn bounded_tail_merges_keep_old_blocks_and_do_not_filter_dead_rows() {
        let dim = 32768;
        let mut rng = Rng(11);
        let schema = collection(&mut rng, 2).schema.clone();
        let mut schema = schema;
        schema.fields[1].dim = Some(dim as i64);
        let mut c = Collection::new("c", schema).unwrap();
        let mut snapshots = Vec::new();
        for step in 0..80 {
            c.insert(InsertBatch {
                num_rows: 1,
                columns: vec![
                    ("id".into(), InputValues::Int(vec![step % 3])),
                    (
                        "v".into(),
                        InputValues::FloatVector {
                            dim,
                            values: vec![step as f32; dim],
                        },
                    ),
                ],
            })
            .unwrap();
            let snapshot = c.read_snapshot();
            assert!(snapshot.chunks.iter().all(|chunk| chunk.live.len <= 32));
            assert_eq!(
                snapshot
                    .chunks
                    .iter()
                    .map(|chunk| chunk.live.len)
                    .sum::<usize>(),
                step as usize + 1
            );
            assert!(snapshot.chunks.len() <= (step as usize + 1).div_ceil(32) + 5);
            snapshots.push(snapshot);
        }
        for (step, snapshot) in snapshots.iter().enumerate() {
            assert_eq!(
                snapshot
                    .chunks
                    .iter()
                    .map(|chunk| chunk.live.len)
                    .sum::<usize>(),
                step + 1
            );
            assert_eq!(
                snapshot
                    .chunks
                    .iter()
                    .map(|chunk| (0..chunk.live.len)
                        .filter(|&row| chunk.live.get(row))
                        .count())
                    .sum::<usize>(),
                (step + 1).min(3)
            );
        }
    }

    #[test]
    fn invalid_publication_mapping_rejects_cached_reads_and_writes_before_changing_sources() {
        use crate::{DataDir, Db};
        for mode in 0..6 {
            let root = tempfile::tempdir().unwrap();
            let mut rng = Rng(5);
            let db = Db::new(
                DataDir::open(root.path()).unwrap(),
                Compute::for_host().unwrap(),
            )
            .unwrap();
            db.create_collection("", "c", collection(&mut rng, 2).schema.clone())
                .unwrap();
            db.insert(
                "",
                "c",
                InsertBatch {
                    num_rows: 2,
                    columns: vec![
                        ("id".into(), InputValues::Int(vec![1, 2])),
                        (
                            "v".into(),
                            InputValues::FloatVector {
                                dim: 2,
                                values: vec![1.0, 0.0, 2.0, 0.0],
                            },
                        ),
                    ],
                },
            )
            .unwrap();
            let handle = db.collection("", "c").unwrap();
            let mut c = handle.write();
            c.persistent_mut().unwrap().freeze_active(false).unwrap();
            let work = c.persistent().unwrap().front().unwrap();
            let materialized = work.table.materialize(c.schema()).unwrap();
            let mut decoded = crate::storage::arrow_data::decode_data_batch(
                c.schema(),
                materialized.data.as_ref().unwrap(),
            )
            .unwrap();
            let mut mapping = materialized.mapping;
            let mut norms = vec![1.0, 2.0];
            match mode {
                0 => mapping.swap(0, 1),
                1 => mapping[0] = Some(8),
                2 => mapping[0] = None,
                3 => {
                    mapping.pop();
                }
                4 => norms.clear(),
                _ => decoded.seqs[0] += 1,
            }
            let manifest = c.persistent().unwrap().manifest.clone();
            let sources: Vec<_> = c.chunks.values().map(|c| c.source).collect();
            let seq = c.next_seq;
            assert!(
                c.publish_generation(
                    &work,
                    manifest.clone(),
                    &mapping,
                    Some(decoded),
                    norms,
                    false
                )
                .is_err()
            );
            assert_eq!(c.persistent().unwrap().manifest, manifest);
            assert_eq!(
                c.chunks.values().map(|c| c.source).collect::<Vec<_>>(),
                sources
            );
            assert!(matches!(c.require_loaded(), Err(EngineError::Storage(_))));
            drop(c);
            assert!(matches!(
                db.collection("", "c"),
                Err(EngineError::Storage(_))
            ));
            assert!(matches!(
                db.delete("", "c", &[Pk::Int64(1)]),
                Err(EngineError::Storage(_))
            ));
            assert_eq!(handle.read().next_seq, seq);
            db.close();
        }
    }

    #[test]
    fn snapshots_share_columns_and_only_pin_get_matches() {
        let mut rng = Rng(7);
        let mut c = collection(&mut rng, 2);
        let snapshot = c.read_snapshot();
        assert_eq!(snapshot.chunks.len(), c.chunks.len());
        for (old, current) in snapshot.chunks.iter().zip(c.chunks.values()) {
            assert!(Arc::ptr_eq(&old.data, &current.data));
        }
        let at = *c.pk_index.get(&Pk::Int64(1)).unwrap();
        let get = c
            .prepare_get(&[Pk::Int64(1), Pk::Int64(1), Pk::Int64(-1)], &[])
            .unwrap();
        assert_eq!(get.snapshot.chunks.len(), 1);
        assert_eq!(get.refs.len(), 2);
        assert!(Arc::ptr_eq(
            &get.snapshot.chunks[0].data,
            &c.chunks[&at.chunk].data
        ));
        c.delete_at_seq(&[Pk::Int64(1)], c.next_seq, None).unwrap();
        assert!(!c.chunks[&at.chunk].live.get(at.row));
        assert!(snapshot.chunks[at.chunk].live.get(at.row));
        assert!(Arc::ptr_eq(
            &snapshot.chunks[at.chunk].data,
            &c.chunks[&at.chunk].data
        ));
        assert_eq!(get.read().rows.pks, [Pk::Int64(1), Pk::Int64(1)]);
        assert!(
            c.prepare_get(&[Pk::Int64(-1)], &[])
                .unwrap()
                .snapshot
                .chunks
                .is_empty()
        );
    }

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
            // 取值很少的分量制造大量并列，偶尔插入 NaN、±inf 与 ±0 分量。
            let values = (0..pks.len() * dim)
                .map(|_| special_or(rng, 40, |r| (r % 4) as f32 - 1.5))
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

    /// 每个分量只取一次随机数：前 5 个取值依次给 NaN（带 payload）、±inf、±0，其余由 `finite`
    /// 给出有限值。随机数的消耗与只生成有限值时相同，共用本辅助函数的用例的数据布局不变。
    fn special_or(rng: &mut Rng, range: usize, finite: impl Fn(usize) -> f32) -> f32 {
        match rng.below(range) {
            0 => f32::from_bits(0x7fc0_0123),
            1 => f32::INFINITY,
            2 => f32::NEG_INFINITY,
            3 => -0.0,
            4 => 0.0,
            r => finite(r),
        }
    }

    /// 与独立参照比较时的分数键：NaN 的符号与 payload 不受保证（见 flat 模块说明），
    /// 只要求仍是 NaN；其余分数逐位比较。
    fn nan_equivalent(score: f32) -> u32 {
        if score.is_nan() {
            f32::NAN.to_bits()
        } else {
            score.to_bits()
        }
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
                for chunk in c.chunks.values() {
                    let (dim, values) = vector_values(&chunk.columns[c.vector_pos]);
                    for row in 0..chunk.live.len {
                        if chunk.live.get(row) {
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
                    .map(|(s, _, pk)| (nan_equivalent(s), pk))
                    .collect()
            })
            .collect()
    }

    /// 0005「验证」：行块边界落在 chunk 内部、行块中没有存活行、各种查询条数与 limit / offset、
    /// 并列、±0、±inf 与 NaN 下，各行块划分与线程数的结果与串行逐位相同（含 NaN 位模式，
    /// 它们走同一扫描代码）。独立参照逐条打分，不按产品的查询分组走批量核，因此只按
    /// [`nan_equivalent`] 比较。
    #[test]
    fn parallel_search_matches_serial_reference() {
        let mut rng = Rng(0x1234_5678);
        let pools = [1, 3].map(|n| Compute::new(NonZeroUsize::new(n).unwrap()).unwrap());
        for case in 0..400 {
            // 33 维有两段完整的 16 路累加，单条核与批量核的 NaN 位模式可在此分叉。
            let dim = [1, 3, 17, 33][case % 4];
            let c = collection(&mut rng, dim);
            let metric = [Metric::L2, Metric::Ip, Metric::Cosine][rng.below(3)];
            let nq = [1, 3, 4, 5, 9][rng.below(5)];
            let (limit, offset) =
                [(0, 0), (1, 0), (5, 2), (10, 0), (3, 40), (500, 0), (7, 500)][rng.below(7)];
            let vectors = (0..nq)
                .map(|_| {
                    (0..dim)
                        .map(|_| special_or(&mut rng, 20, |r| (r % 4) as f32 - 1.5))
                        .collect()
                })
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
            let run = |pool, rows_per_block| -> Vec<Vec<(u32, Pk)>> {
                c.search_in_blocks(req.clone(), pool, rows_per_block)
                    .unwrap()
                    .hits
                    .iter()
                    .map(|h| {
                        h.scores
                            .iter()
                            .map(|s| s.to_bits())
                            .zip(h.rows.pks.iter().cloned())
                            .collect()
                    })
                    .collect()
            };
            // 单线程、单个行块即串行计算。
            let serial = run(&pools[0], usize::MAX);
            let comparable = |hits: &[Vec<(u32, Pk)>]| -> Vec<Vec<(u32, Pk)>> {
                hits.iter()
                    .map(|h| {
                        h.iter()
                            .map(|(bits, pk)| (nan_equivalent(f32::from_bits(*bits)), pk.clone()))
                            .collect()
                    })
                    .collect()
            };
            assert_eq!(comparable(&serial), expected, "case {case}, serial");
            for rows_per_block in [1, 2, 7, 64, 10_000] {
                for pool in &pools {
                    let got = run(pool, rows_per_block);
                    assert_eq!(got, serial, "case {case}, block {rows_per_block}");
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
            let scored: Vec<(f32, RankedRow)> = (0..n)
                .map(|i| {
                    let r = next();
                    let score = if r % 4 == 0 {
                        palette[(r / 4) as usize % palette.len()]
                    } else {
                        (r % 17) as f32 / 4.0
                    };
                    (
                        score,
                        RankedRow {
                            source_rank: 0,
                            at: RowRef {
                                order: i as u64,
                                chunk: i / 50,
                                row: i % 50,
                            },
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
                top.push(*s, || *pos);
            }
            let got = top.finish();
            let bits = |v: &[(f32, RankedRow)]| {
                v.iter().map(|(s, p)| (s.to_bits(), *p)).collect::<Vec<_>>()
            };
            assert_eq!(bits(&got), bits(&expected), "case {case}");
        }
    }

    #[test]
    fn top_k_reads_position_only_when_score_can_reach_the_bound() {
        let position = |source_rank, order| RankedRow {
            source_rank,
            at: RowRef {
                order,
                chunk: 1,
                row: 2,
            },
        };
        let bound_pos = position(3, 9);
        let earlier = position(2, 99);
        let later = position(4, 0);
        for metric in [Metric::L2, Metric::Ip, Metric::Cosine] {
            let (better, worse) = if metric.higher_is_better() {
                (2.0, 0.0)
            } else {
                (0.0, 2.0)
            };
            for (bound, score, pos, reads, accepted) in [
                (1.0, worse, earlier, false, false),
                (1.0, better, later, true, true),
                (1.0, 1.0, earlier, true, true),
                (1.0, 1.0, bound_pos, true, false),
                (1.0, 1.0, later, true, false),
                (0.0, -0.0, earlier, true, true),
                (-0.0, 0.0, later, true, false),
                (1.0, f32::NAN, earlier, false, false),
                (f32::NAN, 1.0, later, true, true),
                (f32::NAN, -f32::NAN, earlier, true, true),
                (f32::NAN, f32::NAN, later, true, false),
                (f32::INFINITY, f32::INFINITY, earlier, true, true),
            ] {
                let mut top = TopK::new(metric, 1);
                top.candidates.push((bound, bound_pos));
                top.bound = Some((bound, bound_pos));
                let called = std::cell::Cell::new(false);
                top.push(score, || {
                    called.set(true);
                    pos
                });
                assert_eq!(called.get(), reads, "{metric:?}: {score} / {bound}");
                assert_eq!(top.candidates.len(), 1 + usize::from(accepted));
            }
            TopK::new(metric, 0).push(1.0, || panic!("zero take must not read a position"));
            let mut unbounded = TopK::new(metric, 1);
            unbounded.push(f32::NAN, || earlier);
            assert_eq!(unbounded.candidates[0].1, earlier);
        }
    }

    #[test]
    fn top_k_keeps_full_source_and_position_order_with_lazy_candidates() {
        let mut rng = Rng(0x8765_4321);
        for metric in [Metric::L2, Metric::Ip, Metric::Cosine] {
            for take in [0, 1, 10, 65, 500] {
                let palette = [
                    0.0,
                    -0.0,
                    1.0,
                    -1.0,
                    f32::INFINITY,
                    f32::NEG_INFINITY,
                    f32::NAN,
                    -f32::NAN,
                ];
                let scored: Vec<_> = (0..300)
                    .map(|row| {
                        (
                            palette[rng.below(palette.len())],
                            RankedRow {
                                source_rank: rng.below(5),
                                at: RowRef {
                                    order: rng.below(20) as u64,
                                    chunk: rng.below(4),
                                    row,
                                },
                            },
                        )
                    })
                    .collect();
                let mut expected = scored.clone();
                // 保留原先的完整比较，避免模型复用被优化的推入或懒比较。
                expected.sort_by(|a, b| flat::rank(metric, a.0, b.0).then(a.1.cmp(&b.1)));
                expected.truncate(take);
                let mut top = TopK::new(metric, take);
                for (score, pos) in scored {
                    top.push(score, || pos);
                }
                let bits = |rows: &[(f32, RankedRow)]| {
                    rows.iter()
                        .map(|(s, p)| (s.to_bits(), *p))
                        .collect::<Vec<_>>()
                };
                assert_eq!(
                    bits(&top.finish()),
                    bits(&expected),
                    "{metric:?}, take={take}"
                );
            }
        }
    }
}
