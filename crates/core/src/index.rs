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
/// FLAT 阶段在稠密向量字段上原生支持的类型（0002「结论」）；AUTOINDEX 按 FLAT 执行。
const FLAT_STAGE_VECTOR_TYPES: [&str; 3] = ["FLAT", "BRUTE_FORCE", "AUTOINDEX"];

/// 按 3.x `Collection.create_index` 的顺序校验，再按 FLAT 阶段的规则收窄。
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
        req.metric_type.unwrap_or_else(|| "NONE".to_owned())
    };
    if !SPEC_METRICS.contains(&metric_type.as_str()) {
        return Err(EngineError::invalid(format!(
            "metric_type must be one of COSINE/L2/IP/BM25/NONE, got {}",
            py_repr(&metric_type)
        )));
    }

    // 以上都是 3.x 会接受的定义；下面是 FLAT 阶段的限制（0002「结论」、0004「范围」）。
    if field.data_type.is_vector() {
        if !FLAT_STAGE_VECTOR_TYPES.contains(&index_type.as_str()) {
            return Err(EngineError::unsupported(
                "vector index type",
                format!(
                    "index_type {} is not supported in this build of milvus-lite-rust yet; \
                     supported: FLAT, BRUTE_FORCE, AUTOINDEX (executed as FLAT)",
                    py_repr(&index_type)
                ),
            ));
        }
    } else {
        return Err(EngineError::unsupported(
            "scalar index",
            format!(
                "index_type {} on scalar fields is not supported in this build of milvus-lite-rust yet",
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
    fn flat_stage_vector_types_are_accepted_and_uppercased() {
        for t in ["FLAT", "flat", "BRUTE_FORCE", "AUTOINDEX"] {
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
    fn non_flat_vector_types_are_unsupported() {
        for t in ["HNSW", "IVF_FLAT", "FOO"] {
            assert!(matches!(
                err(&vector(), req(t, Some("L2"))),
                EngineError::Unsupported { .. }
            ));
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
        assert!(matches!(
            err(&t, req("INVERTED", None)),
            EngineError::Unsupported { .. }
        ));
    }
}
