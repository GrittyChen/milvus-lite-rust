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
}

impl Metric {
    /// 与 3.x `search/distance.py` 相同：区分大小写，只接受这三种。
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "L2" => Ok(Self::L2),
            "IP" => Ok(Self::Ip),
            "COSINE" => Ok(Self::Cosine),
            _ => Err(EngineError::invalid(format!(
                "unsupported metric_type {}; expected one of ('COSINE', 'L2', 'IP')",
                py_repr(s)
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::L2 => "L2",
            Self::Ip => "IP",
            Self::Cosine => "COSINE",
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
            Self::Ip => f64::from(score),
        }
    }

    /// 对外分数是否越大越相似（COSINE / IP 返回相似度，L2 返回平方距离）。
    pub fn higher_is_better(self) -> bool {
        !matches!(self, Self::L2)
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
/// 接受 3.x 可构建的稠密向量定义；实际检索保持 FLAT（0014）。
const ACCEPTED_DENSE_INDEX_TYPES: [&str; 7] = [
    "FLAT",
    "BRUTE_FORCE",
    "AUTOINDEX",
    "HNSW",
    "HNSW_SQ",
    "IVF_FLAT",
    "IVF_SQ8",
];

/// 按 3.x `Collection.create_index` 的顺序校验，再拒绝本阶段无法写出的类型。
/// 调用方已检查「索引已存在」与「字段不存在」。
pub(crate) fn build_spec(field: &FieldSchema, req: IndexRequest) -> Result<IndexSpec> {
    let index_type = req.index_type.to_uppercase();
    let metric_type = if field.data_type.is_vector() {
        req.metric_type.ok_or_else(|| {
            EngineError::schema("create_index missing required 'metric_type' parameter")
        })?
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
        req.metric_type
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
    if field.data_type.is_vector()
        && matches!(
            index_type.as_str(),
            "AUTOINDEX" | "HNSW" | "HNSW_SQ" | "IVF_FLAT" | "IVF_SQ8"
        )
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
    if field.data_type.is_vector() && !ACCEPTED_DENSE_INDEX_TYPES.contains(&index_type.as_str()) {
        return Err(EngineError::unsupported(
            "vector index type",
            format!(
                "index_type {} is not supported in this build of milvus-lite-rust yet; \
                     supported: FLAT, BRUTE_FORCE, AUTOINDEX, HNSW, HNSW_SQ, IVF_FLAT, IVF_SQ8 (executed as FLAT)",
                py_repr(&index_type)
            ),
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

/// 3.x `load()` 为每个有行的已提交 segment 建索引：INVERTED 不论字段类型都走标量倒排，其余类型走
/// `index/factory.py` 的 `build_index_from_spec`（SPARSE_INVERTED_INDEX 不逐 segment 建）。本实现不建
/// 索引，但 3.x 自身建不成的存量定义要在同一时机以同样的错误拒绝（0002「结论」）。只判定向量字段：
/// 3.x 的 `create_index` 在标量字段上只接受 INVERTED，写不出别的定义。
pub(crate) fn unbuildable(spec: &IndexSpec, field: &FieldSchema) -> Option<EngineError> {
    if !field.data_type.is_vector() {
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
    // INVERTED 支持除 JSON 以外的全部 M1 标量类型（3.x 另支持 TIMESTAMPTZ，M1 没有这个类型）。
    if data_type == DataType::Json {
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
        let e = err(&vector(), req("FLAT", Some("l2")));
        assert!(matches!(e, EngineError::InvalidArgument(_)));
        assert_eq!(
            e.to_string(),
            "metric_type must be one of COSINE/L2/IP/BM25/NONE, got 'l2'"
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
