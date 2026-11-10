//! SearchRequest 的解析与 SearchResults 的编码，与 3.x `translators/search.py`、`servicer.Search` 与
//! `translators/result.py` 一致。解析错误按 3.x 的先后报告：placeholder、topk、metric_type 的类型、radius、range_filter、
//! group_size、offset、round_decimal。

use std::collections::HashMap;

use milvus_lite_core::error::{py_json_repr, py_repr};
use milvus_lite_core::rerank::boost::{BoostPlan, BoostSpec};
use milvus_lite_core::rerank::chain::L2Function;
use milvus_lite_core::rerank::pyval::PyVal;
use milvus_lite_core::timezone::TimezoneSource;
use milvus_lite_core::{
    BoostedFilter, Compute, EngineError, GroupBy, Metric, Pk, QueryVectors, ReadSnapshot, Rerank,
    SearchRange, SearchRequest,
};
use prost::Message;
use serde_json::Value;

use crate::output::{self, TimeFields};
use crate::pb::common::PlaceholderGroup;
use crate::pb::schema as pb;
use crate::pb::service as svc;
use crate::pyint::{self, PyError};
use crate::schema::{truthy, unsupported};
use crate::{function_score, insert, status};

const PLACEHOLDER_FLOAT_VECTOR: i32 = 101;
const PLACEHOLDER_SPARSE_FLOAT_VECTOR: i32 = 104;
const PLACEHOLDER_VARCHAR: i32 = 21;

/// 解析出的请求。M1 不支持的参数只记录是否出现，在 load 状态检查之后才报告（0004「决策」）。
pub(crate) struct Parsed {
    pub(crate) vectors: QueryVectors,
    pub(crate) top_k: i64,
    pub(crate) metric_type: Option<String>,
    pub(crate) anns_field: Option<String>,
    offset: i64,
    round_decimal: i64,
    range: Option<SearchRange>,
    group: Option<GroupBy>,
    pub(crate) unsupported: Option<&'static str>,
    function_chains_conflict: bool,
    pub(crate) expr: String,
    pub(crate) boost: Option<BoostSpec>,
    l2: Option<L2Function>,
    /// 请求里的 `timezone` / `time_fields`（空串与非字符串视为没有，同 3.x `translators/search.py`）。
    pub(crate) timezone: Option<String>,
    pub(crate) time_fields: Option<String>,
}

impl Parsed {
    /// 3.x 在 `prepare_search_function_chain` 中检查，晚于度量解析、早于 load。
    pub(crate) fn check_function_chains(&self) -> Result<(), EngineError> {
        if self.function_chains_conflict {
            return Err(EngineError::SchemaValidation(
                "function_score and function_chains cannot be used together".to_owned(),
            ));
        }
        Ok(())
    }
}

/// `parsed` 由 [`parse`] 在取 collection 读锁之前得到。
/// `database_timezone` 读取所在数据库的 `timezone` 属性，只在需要时才会被调用；调用者不得持有集合锁。
pub(crate) fn search(
    c: &ReadSnapshot,
    compute: &Compute,
    database_timezone: &dyn Fn() -> Result<Option<String>, EngineError>,
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
    let mut skip = parsed.offset.max(0);
    // 负 topk 加正 offset 时结果必为空，但范围与 boost 的候选窗仍须按真实 end 决定是否求值。
    if (parsed.range.is_some() || parsed.boost.is_some()) && end < skip {
        skip = end;
    }
    let limit = usize::try_from(end.saturating_sub(skip).max(0)).unwrap_or(usize::MAX);
    let group_field = parsed.group.as_ref().map(|g| g.field.clone());
    let timezone = TimezoneSource::new(
        parsed.timezone.clone(),
        c.schema().timezone().map(str::to_owned),
        database_timezone,
    );
    let request = SearchRequest {
        anns_field: parsed.anns_field,
        vectors: parsed.vectors,
        metric_type: parsed.metric_type,
        limit,
        offset: usize::try_from(skip).unwrap_or(usize::MAX),
        output_fields,
    };
    let filter = BoostedFilter {
        expr: &parsed.expr,
        range: parsed.range,
        group: parsed.group,
        timezone: &timezone,
    };
    let result = match (&parsed.l2, parsed.boost) {
        // 3.x 的 Search + L2：候选窗口、分组与 offset 都交给 rerank 链（0026 第 6、7 节）。
        (Some(function), boost) => c.search_reranked_in(
            request,
            compute,
            filter,
            boost.as_ref(),
            Rerank {
                function,
                limit: parsed.top_k,
                offset: parsed.offset,
            },
        )?,
        (None, Some(spec)) => c.search_boosted_in(
            request,
            compute,
            filter,
            // 3.x：offset 为正时 `hits[offset : offset + top_k]`，否则 `hits[:top_k]`。
            &BoostPlan {
                spec,
                keep: if parsed.offset > 0 {
                    limit
                } else {
                    usize::try_from(parsed.top_k.max(0)).unwrap_or(usize::MAX)
                },
            },
        )?,
        (None, None) => c.search_grouped_in(
            request,
            compute,
            filter.expr,
            filter.range,
            filter.group,
            filter.timezone,
        )?,
    };
    // 响应编码处再校验一次请求时区（3.x 的第二个校验点）：空表达式、空结果也会在这里报错。
    timezone.next_phase();
    timezone.validate_request()?;
    let time_fields = TimeFields::new(parsed.time_fields.as_deref(), &timezone);

    encode(
        c.schema(),
        c.name(),
        result,
        parsed.top_k,
        parsed.round_decimal,
        group_field.as_deref(),
        time_fields.as_ref(),
    )
}

/// 3.x `build_search_result_data`：Search 与 HybridSearch 共用。`round_decimal` 小于 0 时不取整。
pub(crate) fn encode(
    schema: &milvus_lite_core::CollectionSchema,
    collection_name: &str,
    mut result: milvus_lite_core::SearchResult,
    top_k: i64,
    round_decimal: i64,
    group_field: Option<&str>,
    time_fields: Option<&TimeFields>,
) -> Result<svc::SearchResults, EngineError> {
    let pk = schema.fields.iter().find(|f| f.is_primary).ok_or_else(|| {
        EngineError::SchemaValidation("schema has no primary key field".to_owned())
    })?;
    let varchar = pk.data_type == milvus_lite_core::DataType::VarChar;
    let mut scores = Vec::new();
    let mut pks = Vec::new();
    let mut topks = Vec::with_capacity(result.hits.len());
    let mut null_pks = Vec::new();
    for hits in &mut result.hits {
        // 3.x 在响应编码时才对命中的 id 做 `int()` / `str()`；响应的主键列用同一个值。
        match hits.id_overrides.take() {
            Some(ids) => {
                hits.rows.pks = ids
                    .iter()
                    .map(|id| pk_from_override(id, varchar))
                    .collect::<Result<_, _>>()?;
                null_pks.extend(ids.iter().map(|id| *id == PyVal::None));
            }
            None => null_pks.resize(null_pks.len() + hits.rows.pks.len(), false),
        }
        topks.push(i64::try_from(hits.scores.len()).unwrap_or(i64::MAX));
        pks.extend(hits.rows.pks.iter().cloned());
        match (&hits.public_scores, result.metric) {
            // rerank 的分数已经是 float64 对外分数，不能再经过 `round_score` 的换算。
            (Some(public), _) if round_decimal >= 0 => {
                scores.extend(public.iter().map(|s| py_round(*s, round_decimal) as f32));
            }
            (None, Some(metric)) if round_decimal >= 0 => {
                scores.extend(
                    hits.scores
                        .iter()
                        .map(|s| round_score(metric, *s, round_decimal)),
                );
            }
            _ => scores.extend_from_slice(&hits.scores),
        }
    }
    let rows: Vec<_> = result.hits.iter().map(|h| &h.rows).collect();
    let mut fields_data = output::fields_data(schema, &result.projection, &rows, time_fields)?;
    if null_pks.contains(&true) {
        blank_null_pks(&mut fields_data, &pk.name, &null_pks);
    }
    Ok(svc::SearchResults {
        status: Some(status::success()),
        results: Some(pb::SearchResultData {
            num_queries: i64::try_from(result.hits.len()).unwrap_or(i64::MAX),
            top_k,
            fields_data,
            scores,
            ids: Some(insert::pb_ids(&pks, varchar)),
            topks,
            output_fields: result.projection.output_fields(schema, true),
            primary_field_name: pk.name.clone(),
            group_by_field_value: match group_field {
                Some(name) => output::group_by_field_data(
                    schema,
                    name,
                    &result
                        .hits
                        .iter()
                        .filter_map(|h| h.group_values.as_ref())
                        .collect::<Vec<_>>(),
                )?,
                None => None,
            },
            ..Default::default()
        }),
        collection_name: collection_name.to_owned(),
        ..Default::default()
    })
}

/// 3.x `build_search_result_data` 的 `int(p)` / `str(p)`，`p` 是盖掉 `$id` 的同名字段值。
fn pk_from_override(id: &PyVal, varchar: bool) -> Result<Pk, EngineError> {
    if varchar {
        return Ok(Pk::VarChar(id.py_str()));
    }
    let wrong = |e: PyError| EngineError::InvalidArgument(e.message().to_owned());
    Ok(Pk::Int64(match id {
        PyVal::Float(f) if f.is_nan() => {
            return Err(EngineError::InvalidArgument(
                "cannot convert float NaN to integer".to_owned(),
            ));
        }
        PyVal::Float(f) if f.is_infinite() => {
            return Err(EngineError::InvalidArgument(
                "cannot convert float infinity to integer".to_owned(),
            ));
        }
        PyVal::Float(f) => {
            let number = serde_json::Number::from_f64(*f)
                .ok_or_else(|| EngineError::Storage("finite float lost".to_owned()))?;
            pyint::proto_integer(&Value::Number(number)).map_err(wrong)?
        }
        PyVal::Str(s) => pyint::proto_integer(&Value::String(s.clone())).map_err(wrong)?,
        PyVal::Bool(b) => i64::from(*b),
        PyVal::Int(i) => *i,
        // Python 的 int() 成功，写进 protobuf 的 int64 时才越界。
        PyVal::BigInt(text) => {
            return Err(EngineError::InvalidArgument(format!(
                "Value out of range: {text}"
            )));
        }
        PyVal::None | PyVal::List(_) | PyVal::Dict(_) | PyVal::DateTime(_) => {
            return Err(EngineError::InvalidArgument(format!(
                "int() argument must be a string, a bytes-like object or a real number, not {}",
                py_repr(id.type_name())
            )));
        }
    }))
}

/// 3.x 的主键列直接编码命中的值，所以 `$id` 盖成 `None` 时这一格是空值，而 ids 里是 `str(None)`。
/// 主键列的类型决定它不会有别的空值来源，这里只处理 VARCHAR 主键（整数主键在 `int(None)` 已报错）。
fn blank_null_pks(fields: &mut [pb::FieldData], pk: &str, nulls: &[bool]) {
    let Some(field) = fields.iter_mut().find(|f| f.field_name == pk) else {
        return;
    };
    if let Some(pb::field_data::Field::Scalars(pb::ScalarField {
        data: Some(pb::scalar_field::Data::StringData(strings)),
    })) = &mut field.field
    {
        for (value, null) in strings.data.iter_mut().zip(nulls) {
            if *null {
                value.clear();
            }
        }
        field.valid_data = nulls.iter().map(|null| !null).collect();
    }
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
    parse_request(req, false)
}

/// HybridSearch 的子请求：与 Search 相同的 3.x `parse_search_request`，但子请求的 `function_chains`
/// 被忽略（3.x 只在 servicer.Search 里检查它）。
pub(crate) fn parse_route(req: &svc::SearchRequest) -> Result<Parsed, EngineError> {
    parse_request(req, true)
}

fn parse_request(req: &svc::SearchRequest, route: bool) -> Result<Parsed, EngineError> {
    let vectors = placeholders(req)?;
    // 每个值先按 JSON 解析，失败时保留原字符串；同名 key 以后出现者为准。
    let raw: HashMap<&str, Value> = req
        .search_params
        .iter()
        .map(|kv| {
            let v = milvus_lite_core::json_text::value_str(&kv.value)
                .unwrap_or_else(|_| Value::String(kv.value.clone()));
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
    // `json.loads` 接受而 serde_json 回退成字符串的 NaN / Infinity 在 3.x 是 float。
    let nonfinite_metric = req
        .search_params
        .iter()
        .rev()
        .find(|kv| kv.key == "metric_type")
        .is_some_and(|kv| pyint::is_python_json_nonfinite(&kv.value));
    let metric_type = match raw.get("metric_type") {
        None | Some(Value::Null) => None,
        Some(Value::String(metric)) if !nonfinite_metric => Some(metric.clone()),
        Some(_) => {
            return Err(EngineError::SchemaValidation(
                "metric_type must be a string".to_owned(),
            ));
        }
    };
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
    let mut range = SearchRange::default();
    for (key, target) in [
        ("radius", &mut range.radius),
        ("range_filter", &mut range.range_filter),
    ] {
        let value = inner
            .and_then(|m| m.get(key))
            .filter(|v| !v.is_null())
            .or_else(|| raw.get(key).filter(|v| !v.is_null()));
        if let Some(v) = value {
            *target = Some(
                pyint::float(v)
                    .map_err(|e| EngineError::InvalidArgument(e.message().to_owned()))?,
            );
        }
    }
    // 3.x 无论是否分组都解析 group_size；`group_size or 1` 使 0 变为 1，负数则让每组都留不下命中。
    let mut group_size = 1;
    if let Some(v) = raw.get("group_size").filter(|v| !v.is_null()) {
        group_size =
            pyint::int(v).map_err(|e| EngineError::InvalidArgument(e.message().to_owned()))?;
        if group_size == 0 {
            group_size = 1;
        }
    }
    let group = match raw.get("group_by_field") {
        Some(Value::String(field)) if !field.is_empty() => Some(GroupBy {
            field: field.clone(),
            size: group_size,
            strict: raw.get("strict_group_size").is_some_and(truthy),
        }),
        _ => None,
    };
    // 3.x 在 `int(offset)` 与 `int(round_decimal)` 之前解码 function_score。
    let decoded = function_score::decode(req.function_score.as_ref())?;
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

    let function_chains_conflict =
        !route && !req.function_chains.is_empty() && req.function_score.is_some();
    if !route && !req.function_chains.is_empty() && !function_chains_conflict {
        note("function chains");
    }
    Ok(Parsed {
        vectors,
        top_k,
        metric_type,
        anns_field,
        offset,
        round_decimal,
        range: (range.radius.is_some() || range.range_filter.is_some()).then_some(range),
        function_chains_conflict,
        group,
        unsupported: unsupported_feature,
        expr: req.dsl.clone(),
        boost: decoded.boost,
        l2: decoded.l2,
        timezone: non_empty_string(raw.get("timezone")),
        time_fields: non_empty_string(raw.get("time_fields")),
    })
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
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
