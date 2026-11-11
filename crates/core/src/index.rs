//! 索引定义与度量。M1 只做暴力检索（FLAT），索引定义只决定默认度量与 load 语义。

use serde_json::{Map, Value};

use crate::error::{EngineError, Result, py_repr};
use crate::schema::{DataType, FieldSchema};

/// 检索度量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    L2,
    Ip,
    Cosine,
    /// BM25 function 输出字段的度量（0031）。3.x 的内部距离就是 `-score`，对外也原样返回，
    /// 所以下面三个换算都是恒等。只在稀疏分支按字段直接取得，[`Self::parse`] 不接纳。
    Bm25,
}

impl Metric {
    /// 大写的稠密度量名；大小写规范化与报错由调用方按 3.x 的 `resolve_search_metric` 完成。
    /// 3.x 对稠密字段的 BM25 报 `unsupported dense metric`，所以这里不接纳 BM25。
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "L2" => Some(Self::L2),
            "IP" => Some(Self::Ip),
            "COSINE" => Some(Self::Cosine),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::L2 => "L2",
            Self::Ip => "IP",
            Self::Cosine => "COSINE",
            Self::Bm25 => "BM25",
        }
    }

    /// 3.x 在 float64 中得到的对外分数，`round_decimal` 作用在这个值上。3.x 的内部距离是 float32：
    /// L2 为开方后的距离，COSINE 为 `max(1 - sim, 0)`，IP 为点积的相反数；换算为对外分数时才转为
    /// float64（`collection.py` 的 `_internal_distance_to_milvus_distance`）。`score` 是本实现的对外分数。
    pub fn public_f64(self, score: f32) -> f64 {
        match self {
            Self::L2 => {
                let d = f64::from(score.sqrt());
                d * d
            }
            Self::Cosine => 1.0 - f64::from(1.0 - score),
            Self::Ip | Self::Bm25 => f64::from(score),
        }
    }

    /// 对外分数 → 内部 float32 距离（越小越好）；boost 只在这个空间里计算（0026）。
    /// 与引擎打分时的内部距离逐位相同（`flat` 的分数只是它的另一种写法，单测逐位核对），所以不必在
    /// 扫描路径上多带一列。与 3.x 的差别只来自 float32 的求和顺序，见 0004「决策」。
    pub(crate) fn internal(self, score: f32) -> f32 {
        match self {
            Self::L2 => score.sqrt(),
            Self::Cosine => 1.0 - score,
            Self::Ip => -score,
            Self::Bm25 => score,
        }
    }

    /// 内部距离 → 对外分数（3.x `_internal_distance_to_milvus_distance`）。只用于 boost 之后的 float64 距离；
    /// 普通检索的 `score` 已是对外分数，走 [`Self::public_f64`]。
    pub(crate) fn public_from_internal(self, distance: f64) -> f64 {
        match self {
            Self::L2 => distance * distance,
            Self::Cosine => 1.0 - distance,
            Self::Ip => -distance,
            Self::Bm25 => distance,
        }
    }

    /// 对外分数是否越大越相似（COSINE / IP 返回相似度，L2 返回平方距离，BM25 返回 `-score`）。
    /// 只服务排序与 range；rerank 链的「正相关」是另一个概念，BM25 在那里是正相关（`rerank::chain`）。
    pub fn higher_is_better(self) -> bool {
        matches!(self, Self::Ip | Self::Cosine)
    }
}

/// CreateIndex 的请求参数（gRPC 层已从 extra_params 中取出）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IndexRequest {
    pub index_type: String,
    pub metric_type: Option<String>,
    pub params: Map<String, Value>,
    pub search_params: Map<String, Value>,
}

/// 已保存的索引定义，对应 3.x 的 `IndexSpec`。
#[derive(Debug, Clone, PartialEq)]
pub struct IndexSpec {
    pub field_name: String,
    /// 已转为大写。
    pub index_type: String,
    /// 原样保存（3.x 不转大小写）；标量索引为 `NONE`。
    pub metric_type: String,
    pub params: Map<String, Value>,
    pub search_params: Map<String, Value>,
}

#[derive(Debug, Clone)]
pub(crate) enum IndexChange {
    Create(IndexSpec),
    Drop(Vec<IndexSpec>),
}

impl IndexChange {
    pub(crate) fn specs(&self) -> &[IndexSpec] {
        match self {
            Self::Create(spec) => std::slice::from_ref(spec),
            Self::Drop(specs) => specs,
        }
    }
    pub(crate) fn operation(&self) -> &'static str {
        match self {
            Self::Create(_) => "create_index",
            Self::Drop(_) => "drop_index",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SidecarName<'a> {
    pub source_stem: &'a str,
    pub field_name: &'a str,
    pub index_type: &'a str,
    pub extension: &'a str,
}

pub(crate) fn parse_sidecar_name(name: &str) -> Option<SidecarName<'_>> {
    let extension = if name.ends_with(".idx") {
        ".idx"
    } else if name.ends_with(".sidx") {
        ".sidx"
    } else {
        return None;
    };
    let base = name.strip_suffix(extension)?;
    let (stem_field, index_type) = base.rsplit_once('.')?;
    let (source_stem, field_name) = stem_field.rsplit_once('.')?;
    if source_stem.is_empty() || field_name.is_empty() || index_type.is_empty() {
        return None;
    }
    Some(SidecarName {
        source_stem,
        field_name,
        index_type,
        extension,
    })
}

/// 3.x 识别的向量索引类型（`index/factory.py`），用于拒绝在标量字段上建向量索引。
const KNOWN_VECTOR_INDEX_TYPES: [&str; 9] = [
    "AUTOINDEX",
    "BRUTE_FORCE",
    "FLAT",
    "SPARSE_INVERTED_INDEX",
    "HNSW",
    "HNSW_SQ",
    "IVF_FLAT",
    "IVF_SQ8",
    "IVF_PQ",
];
const KNOWN_SCALAR_INDEX_TYPES: [&str; 5] = ["INVERTED", "BITMAP", "STL_SORT", "TRIE", "NGRAM"];
const SPEC_METRICS: [&str; 5] = ["COSINE", "L2", "IP", "BM25", "NONE"];
const FAISS_DENSE_TYPES: [&str; 5] = ["AUTOINDEX", "HNSW", "HNSW_SQ", "IVF_FLAT", "IVF_SQ8"];
/// 接受 3.x 可构建的稠密向量定义；实际检索保持 FLAT（0014）。稠密字段上的 SPARSE_INVERTED_INDEX
/// 在 3.x 中按字段类型走稠密检索、load 时不建任何索引，同样接受（0030）。
const ACCEPTED_DENSE_INDEX_TYPES: [&str; 8] = [
    "FLAT",
    "BRUTE_FORCE",
    "AUTOINDEX",
    "HNSW",
    "HNSW_SQ",
    "IVF_FLAT",
    "IVF_SQ8",
    SPARSE_INDEX,
];
pub(crate) const SPARSE_INDEX: &str = "SPARSE_INVERTED_INDEX";

/// 3.x `_validate_sparse_index_config` 与 `resolve_search_metric` 的 `expected`：BM25 function 的输出字段要求
/// BM25，其余稀疏字段要求 IP。本实现只接受 BM25 这一种函数，稀疏的函数输出字段一定由它产出（0031）。
pub(crate) fn sparse_metric(field: &FieldSchema) -> &'static str {
    if field.is_function_output {
        "BM25"
    } else {
        "IP"
    }
}

/// 3.x `_validate_sparse_index_config`：稀疏字段只接受 SPARSE_INVERTED_INDEX + 期望的度量，再校验
/// build 与 search 两组参数里的调参项。create_index、load 与每次检索都用它校验同一份定义。
pub(crate) fn check_sparse_config(
    field: &FieldSchema,
    index_type: &str,
    metric_type: &str,
    params: &Map<String, Value>,
    search_params: &Map<String, Value>,
) -> Result<()> {
    let expected = sparse_metric(field);
    if index_type != SPARSE_INDEX || metric_type != expected {
        return Err(EngineError::schema(format!(
            "sparse field {} requires SPARSE_INVERTED_INDEX with metric {expected}; got {}/{}. \
             For an existing index, release, drop, and recreate it with the allowed metric.",
            py_repr(&field.name),
            py_repr(index_type),
            py_repr(metric_type)
        )));
    }
    check_sparse_tuning(&field.name, metric_type, params)?;
    check_sparse_tuning(&field.name, metric_type, search_params)
}

/// 3.x `_validate_sparse_tuning`：数值按 Python `float()` 转换（`"0"`、`False` 可以，`True`、`None` 不行）。
/// `metric` 是期望的度量（`IP` / `BM25`）。溢出的数（`1e999`、超长整数）与 3.x 一样在这里被拒绝，
/// 不会进入持久化；`NaN` / `Infinity` 字面量不是严格 JSON，已在 gRPC 解码时返回 UNIMPLEMENTED。
pub(crate) fn check_sparse_tuning(
    field: &str,
    metric: &str,
    params: &Map<String, Value>,
) -> Result<()> {
    use crate::rerank::pyval::PyVal;
    let float = |value: &Value| PyVal::from_json(value).to_float().ok();
    for name in ["bm25_k1", "bm25_b"] {
        let Some(value) = params.get(name) else {
            continue;
        };
        if metric != "BM25" {
            return Err(EngineError::schema(format!(
                "IP field {} does not accept {name}",
                py_repr(field)
            )));
        }
        let valid = !value.is_boolean()
            && float(value).is_some_and(|v| {
                v.is_finite()
                    && if name == "bm25_k1" {
                        v > 0.0
                    } else {
                        (0.0..=1.0).contains(&v)
                    }
            });
        if !valid {
            return Err(EngineError::schema(format!(
                "field {}: invalid {name}",
                py_repr(field)
            )));
        }
    }
    for name in ["drop_ratio_build", "drop_ratio_search"] {
        if let Some(value) = params.get(name)
            && float(value) != Some(0.0)
        {
            return Err(EngineError::schema(format!(
                "field {}: only {name}=0 is supported",
                py_repr(field)
            )));
        }
    }
    match params.get("inverted_index_algo") {
        None => Ok(()),
        Some(Value::String(algo)) if algo.to_uppercase() == "TAAT_NAIVE" => Ok(()),
        Some(other) => Err(EngineError::schema(format!(
            "field {}: unsupported sparse algorithm {}; use TAAT_NAIVE",
            py_repr(field),
            crate::error::py_json_repr(other)
        ))),
    }
}

/// 按 3.x `Collection.create_index` 的顺序校验，再拒绝本阶段无法写出的类型。
/// 调用方已检查「索引已存在」与「字段不存在」。
pub(crate) fn build_spec(field: &FieldSchema, req: IndexRequest) -> Result<IndexSpec> {
    let index_type = req.index_type.to_uppercase();
    let requested_metric = req.metric_type.map(|m| m.to_uppercase());
    let metric_type = if field.data_type.is_any_vector() {
        let metric = requested_metric.ok_or_else(|| {
            EngineError::schema("create_index missing required 'metric_type' parameter")
        })?;
        if field.data_type.is_sparse_vector() {
            check_sparse_config(field, &index_type, &metric, &req.params, &req.search_params)?;
        }
        metric
    } else {
        if KNOWN_VECTOR_INDEX_TYPES.contains(&index_type.as_str()) {
            return Err(EngineError::schema(format!(
                "field {} has type {}; create_index with index_type {} requires a vector field",
                py_repr(&field.name),
                field.data_type.name(),
                py_repr(&index_type)
            )));
        }
        validate_scalar_request(field.data_type, &index_type)?;
        // 3.x：`metric_type or "NONE"`，空串同样视为没有。
        requested_metric
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "NONE".to_owned())
    };
    if !SPEC_METRICS.contains(&metric_type.as_str()) {
        return Err(EngineError::invalid(format!(
            "metric_type must be one of COSINE/L2/IP/BM25/NONE, got {}",
            py_repr(&metric_type)
        )));
    }

    // 3.x 的必装 FAISS 后端（含 AUTOINDEX → HNSW）只构建这三种度量。
    // 先保留 IndexSpec 的原有校验，再拒绝能解析、但切回 3.x 无法 load 的组合。
    if field.data_type.is_dense_vector()
        && FAISS_DENSE_TYPES.contains(&index_type.as_str())
        && !matches!(metric_type.as_str(), "COSINE" | "L2" | "IP")
    {
        return Err(EngineError::unsupported(
            "vector index metric",
            format!(
                "index_type {} requires COSINE / L2 / IP for compatibility with 3.x; got {}",
                py_repr(&index_type),
                py_repr(&metric_type)
            ),
        ));
    }

    // 3.x 会保存无法构建的定义；本实现只写它能构建的类型（0002 约束 1、0014）。
    if field.data_type.is_dense_vector()
        && !ACCEPTED_DENSE_INDEX_TYPES.contains(&index_type.as_str())
    {
        return Err(EngineError::unsupported(
            "vector index type",
            format!(
                "index_type {} is not supported in this build of milvus-lite-rust yet; \
                     supported: FLAT, BRUTE_FORCE, AUTOINDEX, HNSW, HNSW_SQ, IVF_FLAT, IVF_SQ8, \
                     SPARSE_INVERTED_INDEX (executed as FLAT)",
                py_repr(&index_type)
            ),
        ));
    }

    // 3.x 把溢出的浮点文本（如 1e999）读成 inf，并以非标准 JSON 的 `Infinity` 写进 manifest；
    // 与 gRPC 层拒绝的 `NaN` / `Infinity` 字面量是同一类值，新定义不接受。存量定义的写出见
    // `pyjson::dump_file`。
    if [&req.params, &req.search_params]
        .into_iter()
        .any(|params| params.values().any(non_finite))
    {
        return Err(EngineError::unsupported(
            "index params",
            "numbers outside the finite float64 range cannot be persisted",
        ));
    }

    Ok(IndexSpec {
        field_name: field.name.clone(),
        index_type,
        metric_type,
        params: req.params,
        search_params: req.search_params,
    })
}

fn non_finite(value: &Value) -> bool {
    match value {
        Value::Number(n) => {
            crate::pyjson::json_float_text(&n.to_string()).is_some_and(|f| !f.is_finite())
        }
        Value::Array(items) => items.iter().any(non_finite),
        Value::Object(map) => map.values().any(non_finite),
        _ => false,
    }
}

/// 3.x `load()` 为每个有行的已提交 segment 建索引：INVERTED 不论字段类型都走标量倒排，其余类型走
/// `index/factory.py` 的 `build_index_from_spec`（SPARSE_INVERTED_INDEX 不逐 segment 建）。本实现不建
/// 索引，但 3.x 自身建不成的存量定义要在同一时机以同样的错误拒绝（0002「结论」）。只判定向量字段：
/// 3.x 的 `create_index` 在标量字段上只接受 INVERTED，写不出别的定义。
pub(crate) fn unbuildable(spec: &IndexSpec, field: &FieldSchema) -> Option<EngineError> {
    if field.data_type == DataType::Array && spec.index_type == "INVERTED" {
        return Some(EngineError::invalid(
            "scalar index INVERTED does not support field type ARRAY",
        ));
    }
    if !field.data_type.is_dense_vector() {
        return None;
    }
    let message = match spec.index_type.as_str() {
        "FLAT"
        | "BRUTE_FORCE"
        | "AUTOINDEX"
        | "HNSW"
        | "HNSW_SQ"
        | "IVF_FLAT"
        | "IVF_SQ8"
        | "SPARSE_INVERTED_INDEX" => return None,
        "INVERTED" => format!(
            "scalar index INVERTED does not support field type {}",
            field.data_type.name()
        ),
        "IVF_PQ" => format!(
            "index_type={} is reserved for a future phase; \
             supported: HNSW, HNSW_SQ, IVF_FLAT, IVF_SQ8, BRUTE_FORCE",
            py_repr(&spec.index_type)
        ),
        other => format!("unknown index_type: {}", py_repr(other)),
    };
    Some(EngineError::invalid(message))
}

/// 已有侧车的 load 不校验度量；调用方仅在至少一个非空 segment 缺少侧车时使用此错误。
pub(crate) fn unbuildable_metric(spec: &IndexSpec, field: &FieldSchema) -> Option<EngineError> {
    (field.data_type.is_dense_vector()
        && FAISS_DENSE_TYPES.contains(&spec.index_type.as_str())
        && matches!(spec.metric_type.as_str(), "BM25" | "NONE"))
    .then(|| {
        EngineError::invalid(format!(
            "unsupported metric {}; expected COSINE / L2 / IP",
            py_repr(&spec.metric_type)
        ))
    })
}

fn validate_scalar_request(data_type: DataType, index_type: &str) -> Result<()> {
    if !KNOWN_SCALAR_INDEX_TYPES.contains(&index_type) {
        return Err(EngineError::schema(format!(
            "unknown scalar index_type {}; supported scalar index_type: INVERTED",
            py_repr(index_type)
        )));
    }
    if index_type != "INVERTED" {
        return Err(EngineError::schema(format!(
            "scalar index_type {} is not implemented; supported: INVERTED",
            py_repr(index_type)
        )));
    }
    // 参照的 INVERTED 支持这八种标量类型，JSON / ARRAY 不在其能力表中。
    if matches!(data_type, DataType::Json | DataType::Array) {
        return Err(EngineError::schema(format!(
            "field type {} does not support scalar index",
            data_type.name()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_array_inverted_build_error_is_not_a_schema_validation_error() {
        let field = FieldSchema::new("x", DataType::Array);
        let spec = IndexSpec {
            field_name: "x".into(),
            index_type: "INVERTED".into(),
            metric_type: "NONE".into(),
            params: Map::new(),
            search_params: Map::new(),
        };
        assert!(
            matches!(unbuildable(&spec, &field), Some(EngineError::InvalidArgument(message)) if message == "scalar index INVERTED does not support field type ARRAY")
        );
    }

    fn vector() -> FieldSchema {
        FieldSchema {
            dim: Some(4),
            ..FieldSchema::new("v", DataType::FloatVector)
        }
    }

    fn req(index_type: &str, metric: Option<&str>) -> IndexRequest {
        IndexRequest {
            index_type: index_type.to_owned(),
            metric_type: metric.map(str::to_owned),
            ..Default::default()
        }
    }

    fn err(field: &FieldSchema, r: IndexRequest) -> EngineError {
        build_spec(field, r).unwrap_err()
    }

    fn params(json: serde_json::Value) -> Map<String, Value> {
        json.as_object().unwrap().clone()
    }

    /// 3.x `create_index`：先要求度量，再按 `_validate_sparse_index_config` 只接受 SPARSE_INVERTED_INDEX + IP。
    #[test]
    fn sparse_fields_accept_only_sparse_inverted_index_with_ip() {
        let field = FieldSchema::new("sp", DataType::SparseFloatVector);
        for index_type in ["INVERTED", "SPARSE_INVERTED_INDEX", "FLAT"] {
            assert_eq!(
                build_spec(&field, req(index_type, None))
                    .unwrap_err()
                    .to_string(),
                "create_index missing required 'metric_type' parameter"
            );
        }
        for (index_type, metric) in [
            ("INVERTED", "IP"),
            ("FLAT", "IP"),
            ("AUTOINDEX", "IP"),
            ("SPARSE_INVERTED_INDEX", "BM25"),
            ("SPARSE_INVERTED_INDEX", "L2"),
            ("SPARSE_INVERTED_INDEX", "FOO"),
        ] {
            let error = build_spec(&field, req(index_type, Some(metric))).unwrap_err();
            assert!(
                matches!(error, EngineError::SchemaValidation(_)),
                "{error:?}"
            );
            assert_eq!(
                error.to_string(),
                format!(
                    "sparse field 'sp' requires SPARSE_INVERTED_INDEX with metric IP; got '{index_type}'/'{metric}'. \
                     For an existing index, release, drop, and recreate it with the allowed metric."
                )
            );
        }
        let spec = build_spec(&field, req("sparse_inverted_index", Some("ip"))).unwrap();
        assert_eq!(
            (spec.index_type.as_str(), spec.metric_type.as_str()),
            ("SPARSE_INVERTED_INDEX", "IP")
        );
    }

    /// 3.x `_validate_sparse_tuning` 的 Python `float()` 规则；build 与 search 两组参数都校验。
    #[test]
    fn sparse_tuning_follows_python_float_rules() {
        let check = |json| check_sparse_tuning("sp", "IP", &params(json));
        for ok in [
            serde_json::json!({}),
            serde_json::json!({"drop_ratio_build": 0}),
            serde_json::json!({"drop_ratio_build": "0", "drop_ratio_search": -0.0}),
            serde_json::json!({"drop_ratio_search": false}),
            serde_json::json!({"drop_ratio_build": " 0e5 "}),
            serde_json::json!({"inverted_index_algo": "taat_naive"}),
            serde_json::json!({"unrelated": [1]}),
        ] {
            assert!(check(ok.clone()).is_ok(), "{ok}");
        }
        for (bad, message) in [
            (
                serde_json::json!({"drop_ratio_build": true}),
                "field 'sp': only drop_ratio_build=0 is supported",
            ),
            (
                serde_json::json!({"drop_ratio_build": null}),
                "field 'sp': only drop_ratio_build=0 is supported",
            ),
            (
                serde_json::json!({"drop_ratio_search": "0.1"}),
                "field 'sp': only drop_ratio_search=0 is supported",
            ),
            (
                serde_json::json!({"drop_ratio_search": [0]}),
                "field 'sp': only drop_ratio_search=0 is supported",
            ),
            (
                serde_json::json!({"bm25_k1": 1.2}),
                "IP field 'sp' does not accept bm25_k1",
            ),
            (
                serde_json::json!({"drop_ratio_build": 0.5, "bm25_b": 0.5}),
                "IP field 'sp' does not accept bm25_b",
            ),
            (
                serde_json::json!({"inverted_index_algo": "DAAT"}),
                "field 'sp': unsupported sparse algorithm 'DAAT'; use TAAT_NAIVE",
            ),
            (
                serde_json::json!({"inverted_index_algo": null}),
                "field 'sp': unsupported sparse algorithm None; use TAAT_NAIVE",
            ),
            (
                serde_json::json!({"inverted_index_algo": 1}),
                "field 'sp': unsupported sparse algorithm 1; use TAAT_NAIVE",
            ),
        ] {
            assert_eq!(
                check(bad.clone()).unwrap_err().to_string(),
                message,
                "{bad}"
            );
        }
        let mut request = req("SPARSE_INVERTED_INDEX", Some("IP"));
        request.search_params = params(serde_json::json!({"drop_ratio_search": 0.2}));
        assert_eq!(
            build_spec(
                &FieldSchema::new("sp", DataType::SparseFloatVector),
                request
            )
            .unwrap_err()
            .to_string(),
            "field 'sp': only drop_ratio_search=0 is supported"
        );
    }

    /// 3.x `_validate_sparse_tuning` 的 BM25 分支（期望值逐条取自 `1320d26` 的该函数）：`float()` 之后有限、
    /// 不是 bool，k1 > 0、b 在 [0, 1]；依次校验 k1、b、drop ratio、算法名。溢出的数同样报 invalid。
    #[test]
    fn bm25_tuning_follows_3x() {
        let check = |json: &str| {
            check_sparse_tuning(
                "sp",
                "BM25",
                &crate::json_text::value_str(json)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .map_err(|e| e.to_string())
        };
        let huge = format!("1{}", "0".repeat(400));
        for ok in [
            r#"{"bm25_k1": 1.2}"#,
            r#"{"bm25_k1": "2"}"#,
            r#"{"bm25_k1": 1e308}"#,
            r#"{"bm25_k1": " 3 "}"#,
            r#"{"bm25_k1": "1_0"}"#,
            r#"{"bm25_k1": 1}"#,
            r#"{"bm25_b": 0}"#,
            r#"{"bm25_b": 1}"#,
            r#"{"bm25_b": "0.5"}"#,
            r#"{"bm25_b": -0.0}"#,
            r#"{"bm25_b": 0.5, "drop_ratio_search": "0.0", "inverted_index_algo": "Taat_Naive"}"#,
        ] {
            assert_eq!(check(ok), Ok(()), "{ok}");
        }
        let k1 = "field 'sp': invalid bm25_k1";
        let b = "field 'sp': invalid bm25_b";
        let mut bad: Vec<(String, &str)> = [
            "0",
            "-1",
            "true",
            "false",
            "null",
            "[1]",
            "\"x\"",
            "\"inf\"",
            "\"nan\"",
            "{}",
            "\"Infinity\"",
            "\"1e999\"",
            "1e999",
            "-1e999",
        ]
        .into_iter()
        .map(|v| (format!(r#"{{"bm25_k1": {v}}}"#), k1))
        .collect();
        bad.push((format!(r#"{{"bm25_k1": {huge}}}"#), k1));
        bad.extend(
            [
                "false",
                "true",
                "null",
                "1.0000001",
                "\"nan\"",
                "-1e-300",
                "1e999",
            ]
            .into_iter()
            .map(|v| (format!(r#"{{"bm25_b": {v}}}"#), b)),
        );
        bad.push((r#"{"bm25_k1": 1.2, "bm25_b": 2}"#.into(), b));
        bad.push((r#"{"bm25_k1": 0, "drop_ratio_build": 1}"#.into(), k1));
        bad.push((
            r#"{"drop_ratio_build": 1e999}"#.into(),
            "field 'sp': only drop_ratio_build=0 is supported",
        ));
        bad.push((
            r#"{"inverted_index_algo": 1e999}"#.into(),
            "field 'sp': unsupported sparse algorithm inf; use TAAT_NAIVE",
        ));
        for (json, message) in bad {
            assert_eq!(check(&json), Err(message.to_owned()), "{json}");
        }
    }

    /// BM25 输出字段只接受 SPARSE_INVERTED_INDEX + BM25，两组参数都校验（3.x `_validate_sparse_index_config`）。
    #[test]
    fn bm25_output_fields_require_bm25_metric() {
        let field = FieldSchema {
            is_function_output: true,
            ..FieldSchema::new("sp", DataType::SparseFloatVector)
        };
        for (index_type, metric) in [
            ("SPARSE_INVERTED_INDEX", "IP"),
            ("AUTOINDEX", "BM25"),
            ("INVERTED", "BM25"),
        ] {
            assert_eq!(
                build_spec(&field, req(index_type, Some(metric)))
                    .unwrap_err()
                    .to_string(),
                format!(
                    "sparse field 'sp' requires SPARSE_INVERTED_INDEX with metric BM25; got '{index_type}'/'{metric}'. \
                     For an existing index, release, drop, and recreate it with the allowed metric."
                )
            );
        }
        let mut request = req("sparse_inverted_index", Some("bm25"));
        request.params = params(serde_json::json!({"bm25_k1": "2", "bm25_b": 0}));
        let spec = build_spec(&field, request).unwrap();
        assert_eq!(
            (spec.index_type.as_str(), spec.metric_type.as_str()),
            ("SPARSE_INVERTED_INDEX", "BM25")
        );
        let mut request = req("SPARSE_INVERTED_INDEX", Some("BM25"));
        request.search_params = params(serde_json::json!({"bm25_b": 2}));
        assert_eq!(
            build_spec(&field, request).unwrap_err().to_string(),
            "field 'sp': invalid bm25_b"
        );
        // 普通稀疏字段仍是 IP，BM25 度量与 BM25 调参项都拒绝。
        let plain = FieldSchema::new("sp", DataType::SparseFloatVector);
        assert!(
            build_spec(&plain, req("SPARSE_INVERTED_INDEX", Some("BM25")))
                .unwrap_err()
                .to_string()
                .contains("requires SPARSE_INVERTED_INDEX with metric IP")
        );
        assert_eq!(Metric::parse("BM25"), None);
    }

    /// 稠密字段上的 SPARSE_INVERTED_INDEX：五种度量都能建，调参项不校验，load 时不建任何东西（3.x 实测）。
    #[test]
    fn dense_fields_keep_sparse_inverted_index_definitions_unvalidated() {
        for metric in ["IP", "COSINE", "L2", "BM25", "NONE"] {
            let mut request = req("SPARSE_INVERTED_INDEX", Some(metric));
            request.params = params(serde_json::json!({
                "drop_ratio_build": 0.5, "bm25_k1": 1, "inverted_index_algo": "DAAT"
            }));
            let spec = build_spec(&vector(), request).unwrap();
            assert_eq!(spec.metric_type, metric);
            assert!(unbuildable(&spec, &vector()).is_none());
            assert!(unbuildable_metric(&spec, &vector()).is_none());
        }
    }

    #[test]
    fn public_f64_follows_3x_float64_conversion() {
        // 3.x 实测：L2 平方距离 1.25（向量 [1, 0.5] 到原点）在 float64 中是 1.250000036705515。
        assert_eq!(Metric::L2.public_f64(1.25), 1.250_000_036_705_515);
        assert_eq!(Metric::Ip.public_f64(0.1), f64::from(0.1f32));
        // COSINE：3.x 的内部距离 float32(1 - sim)，对外 1.0 - 距离在 float64 中计算。
        let sim = 0.982_710_54_f32;
        assert_eq!(Metric::Cosine.public_f64(sim), 1.0 - f64::from(1.0 - sim));
    }

    #[test]
    fn flat_stage_vector_types_are_accepted_and_uppercased() {
        for t in [
            "FLAT",
            "flat",
            "BRUTE_FORCE",
            "AUTOINDEX",
            "HNSW",
            "HNSW_SQ",
            "IVF_FLAT",
            "IVF_SQ8",
        ] {
            let spec = build_spec(&vector(), req(t, Some("L2"))).unwrap();
            assert_eq!(spec.index_type, t.to_uppercase());
            assert_eq!(spec.metric_type, "L2");
        }
    }

    #[test]
    fn vector_messages_match_3x() {
        assert_eq!(
            err(&vector(), req("FLAT", None)).to_string(),
            "create_index missing required 'metric_type' parameter"
        );
        let e = err(&vector(), req("FLAT", Some("bad")));
        assert!(matches!(e, EngineError::InvalidArgument(_)));
        assert_eq!(
            e.to_string(),
            "metric_type must be one of COSINE/L2/IP/BM25/NONE, got 'BAD'"
        );
    }

    #[test]
    fn requested_metric_is_uppercased_before_validation_and_storage() {
        let spec = build_spec(&vector(), req("FLAT", Some("cosine"))).unwrap();
        assert_eq!(spec.metric_type, "COSINE");
        // 向量字段上空串不等于缺失，继续由度量校验拒绝。
        let e = err(&vector(), req("FLAT", Some("")));
        assert_eq!(
            e.to_string(),
            "metric_type must be one of COSINE/L2/IP/BM25/NONE, got ''"
        );
    }

    #[test]
    fn faiss_definitions_only_accept_buildable_metrics() {
        for kind in ["AUTOINDEX", "HNSW", "HNSW_SQ", "IVF_FLAT", "IVF_SQ8"] {
            for metric in ["BM25", "NONE"] {
                assert!(
                    matches!(
                        build_spec(&vector(), req(&kind.to_lowercase(), Some(metric))),
                        Err(EngineError::Unsupported { .. })
                    ),
                    "{kind}/{metric}"
                );
            }
            for metric in ["COSINE", "L2", "IP"] {
                let spec = build_spec(&vector(), req(kind, Some(metric))).unwrap();
                assert_eq!(spec.metric_type, metric);
            }
            assert_eq!(
                err(&vector(), req(kind, None)).to_string(),
                "create_index missing required 'metric_type' parameter"
            );
            assert!(matches!(
                err(&vector(), req(kind, Some("bad"))),
                EngineError::InvalidArgument(_)
            ));
        }
        for kind in ["FLAT", "BRUTE_FORCE"] {
            for metric in SPEC_METRICS {
                assert_eq!(
                    build_spec(&vector(), req(kind, Some(metric)))
                        .unwrap()
                        .metric_type,
                    metric
                );
            }
        }
    }

    #[test]
    fn unbuildable_vector_types_remain_unsupported() {
        for t in ["IVF_PQ", "DISKANN", "INVERTED", "FOO"] {
            assert!(matches!(
                err(&vector(), req(t, Some("L2"))),
                EngineError::Unsupported { .. }
            ));
        }
    }

    #[test]
    fn inverted_accepts_all_supported_scalar_types_without_changing_parameters() {
        for data_type in [
            DataType::Int8,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::Float,
            DataType::Double,
            DataType::VarChar,
            DataType::Bool,
            DataType::Timestamptz,
        ] {
            let mut request = req("inverted", Some(""));
            request.params.insert("value".into(), 1.into());
            let spec = build_spec(&FieldSchema::new("s", data_type), request).unwrap();
            assert_eq!(spec.index_type, "INVERTED");
            assert_eq!(spec.metric_type, "NONE");
            assert_eq!(spec.params["value"], 1);
        }
    }

    #[test]
    fn scalar_messages_match_3x() {
        let t = FieldSchema::new("t", DataType::VarChar);
        assert_eq!(
            err(&t, req("FLAT", None)).to_string(),
            "field 't' has type VARCHAR; create_index with index_type 'FLAT' requires a vector field"
        );
        assert_eq!(
            err(&t, req("BITMAP", None)).to_string(),
            "scalar index_type 'BITMAP' is not implemented; supported: INVERTED"
        );
        assert_eq!(
            err(&t, req("FOO", None)).to_string(),
            "unknown scalar index_type 'FOO'; supported scalar index_type: INVERTED"
        );
        let j = FieldSchema::new("j", DataType::Json);
        assert_eq!(
            err(&j, req("INVERTED", None)).to_string(),
            "field type JSON does not support scalar index"
        );
        assert_eq!(
            build_spec(&t, req("INVERTED", None)).unwrap().metric_type,
            "NONE"
        );
    }
}
