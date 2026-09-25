//! SearchRequest 的解析与 SearchResults 的编码，与 3.x `translators/search.py`、`servicer.Search` 与
//! `translators/result.py` 一致。解析错误按 3.x 的先后报告：placeholder、topk、radius、range_filter、
//! group_size、offset、round_decimal。

use std::collections::HashMap;

use milvus_lite_core::error::py_json_repr;
use milvus_lite_core::{Collection, Compute, EngineError, Metric, QueryVectors, SearchRequest};
use prost::Message;
use serde_json::Value;

use crate::output;
use crate::pb::common::PlaceholderGroup;
use crate::pb::schema as pb;
use crate::pb::service as svc;
use crate::pyint::{self, PyError};
use crate::schema::unsupported;
use crate::{insert, status};

const PLACEHOLDER_FLOAT_VECTOR: i32 = 101;
const PLACEHOLDER_SPARSE_FLOAT_VECTOR: i32 = 104;
const PLACEHOLDER_VARCHAR: i32 = 21;

/// 解析出的请求。M1 不支持的参数只记录是否出现，在 load 状态检查之后才报告（0004「决策」）。
pub(crate) struct Parsed {
    vectors: QueryVectors,
    top_k: i64,
    metric_type: Option<String>,
    anns_field: Option<String>,
    offset: i64,
    round_decimal: i64,
    unsupported: Option<&'static str>,
}

/// `parsed` 由 [`parse`] 在取 collection 读锁之前得到。
pub(crate) fn search(
    c: &Collection,
    compute: &Compute,
    parsed: Parsed,
    output_fields: Vec<String>,
) -> Result<svc::SearchResults, EngineError> {
    c.require_loaded()?;
    if let Some(feature) = parsed.unsupported {
        return Err(unsupported(feature));
    }
    // 3.x 在没有 group_by / range / rerank 的路径上取前 topk + offset 个结果，offset 为正时再跳过 offset 个；
    // 二者之和不为正时结果为空。
    let end = parsed.top_k.saturating_add(parsed.offset).max(0);
    let skip = parsed.offset.max(0);
    let limit = usize::try_from(end.saturating_sub(skip).max(0)).unwrap_or(usize::MAX);
    let result = c.search(
        SearchRequest {
            anns_field: parsed.anns_field,
            vectors: parsed.vectors,
            metric_type: parsed.metric_type,
            limit,
            offset: usize::try_from(skip).unwrap_or(usize::MAX),
            output_fields,
        },
        compute,
    )?;

    let schema = c.schema();
    let pk = schema.fields.iter().find(|f| f.is_primary).ok_or_else(|| {
        EngineError::SchemaValidation("schema has no primary key field".to_owned())
    })?;
    let varchar = pk.data_type == milvus_lite_core::DataType::VarChar;
    let mut scores = Vec::new();
    let mut pks = Vec::new();
    let mut topks = Vec::with_capacity(result.hits.len());
    for hits in &result.hits {
        topks.push(i64::try_from(hits.scores.len()).unwrap_or(i64::MAX));
        pks.extend(hits.rows.pks.iter().cloned());
        match result.metric {
            Some(metric) if parsed.round_decimal >= 0 => {
                scores.extend(
                    hits.scores
                        .iter()
                        .map(|s| round_score(metric, *s, parsed.round_decimal)),
                );
            }
            _ => scores.extend_from_slice(&hits.scores),
        }
    }
    let rows: Vec<_> = result.hits.iter().map(|h| &h.rows).collect();
    Ok(svc::SearchResults {
        status: Some(status::success()),
        results: Some(pb::SearchResultData {
            num_queries: i64::try_from(result.hits.len()).unwrap_or(i64::MAX),
            top_k: parsed.top_k,
            fields_data: output::fields_data(schema, &result.projection, &rows),
            scores,
            ids: Some(insert::pb_ids(&pks, varchar)),
            topks,
            output_fields: result.projection.output_fields(schema, false),
            primary_field_name: pk.name.clone(),
            ..Default::default()
        }),
        collection_name: c.name().to_owned(),
        ..Default::default()
    })
}

/// 3.x 对 float64 对外分数做 Python `round(x, ndigits)`（按十进制正确舍入、同距取偶），再存为 float32。
fn round_score(metric: Metric, score: f32, ndigits: i64) -> f32 {
    py_round(metric.public_f64(score), ndigits) as f32
}

/// Python `float.__round__`：位数超过 323 时原样返回（float64 在十进制下的最大有效位置）。
fn py_round(x: f64, ndigits: i64) -> f64 {
    if !x.is_finite() || ndigits > 323 {
        return x;
    }
    let digits = usize::try_from(ndigits).unwrap_or(0);
    // Rust 的定点格式化按精确的二进制值做正确舍入、同距取偶，与 Python 的 round 相同。
    format!("{x:.digits$}").parse().unwrap_or(x)
}

pub(crate) fn parse(req: &svc::SearchRequest) -> Result<Parsed, EngineError> {
    let vectors = placeholders(req)?;
    // 每个值先按 JSON 解析，失败时保留原字符串；同名 key 以后出现者为准。
    let raw: HashMap<&str, Value> = req
        .search_params
        .iter()
        .map(|kv| {
            let v =
                serde_json::from_str(&kv.value).unwrap_or_else(|_| Value::String(kv.value.clone()));
            (kv.key.as_str(), v)
        })
        .collect();

    let top_k = match raw.get("topk") {
        None | Some(Value::Null) => {
            return Err(EngineError::SchemaValidation(
                "SearchRequest.search_params is missing required 'topk'".to_owned(),
            ));
        }
        Some(v) => match pyint::int(v) {
            Ok(k) => k,
            Err(PyError::Overflow(m)) => return Err(EngineError::InvalidArgument(m)),
            Err(_) => {
                return Err(EngineError::SchemaValidation(format!(
                    "SearchRequest.search_params['topk'] must be int, got {}",
                    py_json_repr(v)
                )));
            }
        },
    };
    let metric_type = raw.get("metric_type").map(pyint::py_str);
    let anns_field = match raw.get("anns_field") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };

    let mut unsupported_feature = None;
    let mut note = |feature: &'static str| {
        unsupported_feature.get_or_insert(feature);
    };
    if let Some(kv) = req.search_params.iter().rev().find(|kv| kv.key == "params")
        && matches!(raw.get("params"), Some(Value::String(_)))
        && pyint::maybe_python_json_container(&kv.value)
    {
        note("search params that are not strict JSON");
    }
    // range search 的参数可以放在 params 对象里，也可以在顶层；为 null 时视同没有。
    let inner = match raw.get("params") {
        Some(Value::Object(m)) => Some(m),
        _ => None,
    };
    for key in ["radius", "range_filter"] {
        let value = inner
            .and_then(|m| m.get(key))
            .filter(|v| !v.is_null())
            .or_else(|| raw.get(key).filter(|v| !v.is_null()));
        if let Some(v) = value {
            pyint::float(v).map_err(|e| EngineError::InvalidArgument(e.message().to_owned()))?;
            note("range search");
        }
    }
    if let Some(Value::String(s)) = raw.get("group_by_field")
        && !s.is_empty()
    {
        note("grouping search");
    }
    if let Some(v) = raw.get("group_size").filter(|v| !v.is_null()) {
        pyint::int(v).map_err(|e| EngineError::InvalidArgument(e.message().to_owned()))?;
    }
    if let Some(Value::String(s)) = raw.get("timezone")
        && !s.is_empty()
    {
        note("timezone");
    }
    let offset = match raw.get("offset") {
        None => 0,
        Some(v) => {
            pyint::int(v).map_err(|e| EngineError::InvalidArgument(e.message().to_owned()))?
        }
    };
    let round_decimal = match raw.get("round_decimal") {
        None => -1,
        Some(v) => {
            pyint::int(v).map_err(|e| EngineError::InvalidArgument(e.message().to_owned()))?
        }
    };

    if !req.dsl.is_empty() {
        note("filter expressions");
    }
    if req.partition_names.iter().any(|p| p != "_default") {
        note("partitions other than '_default'");
    }
    if req
        .function_score
        .as_ref()
        .is_some_and(|f| !f.functions.is_empty())
        || !req.function_chains.is_empty()
    {
        note("rerank and function chains");
    }
    Ok(Parsed {
        vectors,
        top_k,
        metric_type,
        anns_field,
        offset,
        round_decimal,
        unsupported: unsupported_feature,
    })
}

/// 3.x `_decode_placeholder_group`：只看第一个 PlaceholderValue。
fn placeholders(req: &svc::SearchRequest) -> Result<QueryVectors, EngineError> {
    let schema_err = |m: String| EngineError::SchemaValidation(m);
    let bytes = match &req.search_input {
        Some(svc::search_request::SearchInput::PlaceholderGroup(b)) if !b.is_empty() => b,
        _ => {
            return Err(schema_err(
                "SearchRequest is missing placeholder_group".to_owned(),
            ));
        }
    };
    let group = PlaceholderGroup::decode(bytes.as_slice()).map_err(|_| {
        EngineError::InvalidArgument(
            "Error parsing message with type 'milvus.proto.common.PlaceholderGroup': Wire format was corrupt"
                .to_owned(),
        )
    })?;
    let Some(first) = group.placeholders.first() else {
        return Err(schema_err("PlaceholderGroup is empty".to_owned()));
    };
    let no_vectors = || schema_err("PlaceholderGroup has no query vectors".to_owned());
    match first.r#type {
        PLACEHOLDER_FLOAT_VECTOR => {
            let mut out = Vec::with_capacity(first.values.len());
            for blob in &first.values {
                if blob.is_empty() {
                    return Err(schema_err(
                        "PlaceholderValue contains an empty FloatVector blob".to_owned(),
                    ));
                }
                if blob.len() % 4 != 0 {
                    return Err(schema_err(format!(
                        "PlaceholderValue blob is not a multiple of 4 bytes ({})",
                        blob.len()
                    )));
                }
                out.push(
                    blob.as_chunks::<4>()
                        .0
                        .iter()
                        .map(|b| f32::from_le_bytes(*b))
                        .collect(),
                );
            }
            if out.is_empty() {
                return Err(no_vectors());
            }
            Ok(QueryVectors::Dense(out))
        }
        PLACEHOLDER_SPARSE_FLOAT_VECTOR | PLACEHOLDER_VARCHAR if first.values.is_empty() => {
            Err(no_vectors())
        }
        PLACEHOLDER_SPARSE_FLOAT_VECTOR => {
            // 3.x `bytes_to_sparse`：每个非零项是 (u32 下标, f32 值) 共 8 字节。
            if let Some(blob) = first.values.iter().find(|b| b.len() % 8 != 0) {
                return Err(EngineError::InvalidArgument(format!(
                    "sparse vector bytes length {} is not a multiple of 8",
                    blob.len()
                )));
            }
            Ok(QueryVectors::Sparse)
        }
        PLACEHOLDER_VARCHAR => Ok(QueryVectors::Text),
        t => Err(schema_err(format!(
            "PlaceholderValue type {t} not supported \
             (supported: FloatVector=101, SparseFloatVector=104, VarChar=21)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn py_round_matches_python() {
        // 期望值取自 Python 3.12 的 round。
        assert_eq!(py_round(1.250_000_036_705_515, 1), 1.3);
        assert_eq!(py_round(1.25, 1), 1.2);
        assert_eq!(py_round(0.125, 2), 0.12);
        assert_eq!(py_round(0.375, 2), 0.38);
        assert_eq!(py_round(2.5, 0), 2.0);
        assert_eq!(py_round(-0.5, 0), -0.0);
        assert_eq!(py_round(123.456_789, 4), 123.4568);
        assert_eq!(py_round(1e-300, 400), 1e-300);
        assert!(py_round(f64::NAN, 2).is_nan());
    }

    #[test]
    fn rounded_scores_use_the_3x_float64_value() {
        // 3.x 实测：[1, 0.5] 到原点的 L2 距离，round_decimal=1 时为 1.3（float32 为 1.2999999523162842）。
        assert_eq!(round_score(Metric::L2, 1.25, 1), 1.3_f32);
    }
}
