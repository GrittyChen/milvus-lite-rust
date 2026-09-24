//! CreateIndex 与 DescribeIndex 的翻译，与 3.x `translators/index.py` 及 `servicer.DescribeIndex` 一致。

use milvus_lite_core::{Collection, EngineError, IndexRequest, IndexSpec, pyjson};
use serde_json::{Map, Value};

use crate::pb::common::{self, IndexState};
use crate::pb::service as svc;
use crate::pyint;
use crate::schema::{kv, unsupported};

/// extra_params → 引擎的索引请求。3.x 只读取 index_type、metric_type、params、search_params 四个键；
/// pymilvus 把用户参数平铺成单独的键，3.x 因此不会保存它们。
pub(crate) fn request(extra_params: &[common::KeyValuePair]) -> Result<IndexRequest, EngineError> {
    // 同名 key 以后出现者为准（3.x 构造 dict）。
    let get = |key: &str| {
        extra_params
            .iter()
            .rev()
            .find(|p| p.key == key)
            .map(|p| p.value.as_str())
    };
    let index_type = get("index_type").filter(|t| !t.is_empty()).ok_or_else(|| {
        EngineError::SchemaValidation(
            "create_index missing required 'index_type' parameter".to_owned(),
        )
    })?;
    Ok(IndexRequest {
        index_type: index_type.to_owned(),
        metric_type: get("metric_type").map(str::to_owned),
        params: json_object(get("params"))?,
        search_params: json_object(get("search_params"))?,
    })
}

/// 3.x：值为空、不是合法 JSON 或不是对象时当作 `{}`。
fn json_object(text: Option<&str>) -> Result<Map<String, Value>, EngineError> {
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return Ok(Map::new());
    };
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => Ok(m),
        Err(_) if pyint::maybe_python_json_container(text) => {
            Err(unsupported("index params that are not strict JSON"))
        }
        Ok(_) | Err(_) => Ok(Map::new()),
    }
}

pub(crate) fn describe(
    c: &Collection,
    req: &svc::DescribeIndexRequest,
) -> Result<svc::DescribeIndexResponse, EngineError> {
    // 3.x 先看 field_name；为空时 index_name 可以是字段名，或 `<字段名>_idx`。都不匹配时返回全部索引。
    let field = if !req.field_name.is_empty() {
        Some(req.field_name.as_str())
    } else if c.index(&req.index_name).is_some() {
        Some(req.index_name.as_str())
    } else {
        req.index_name
            .strip_suffix("_idx")
            .filter(|f| c.index(f).is_some())
    };
    let rows = i64::try_from(c.count()).unwrap_or(i64::MAX);
    let index_descriptions = c
        .describe_indexes(field)?
        .into_iter()
        .map(|spec| svc::IndexDescription {
            index_name: spec.field_name.clone(),
            field_name: spec.field_name.clone(),
            params: params(spec),
            state: IndexState::Finished as i32,
            indexed_rows: rows,
            total_rows: rows,
            ..Default::default()
        })
        .collect();
    Ok(svc::DescribeIndexResponse {
        status: Some(crate::status::success()),
        index_descriptions,
    })
}

/// 3.x `index_spec_to_kv_pairs`：params 与 search_params 非空时以 `json.dumps(sort_keys=True)` 输出。
fn params(spec: &IndexSpec) -> Vec<common::KeyValuePair> {
    let mut out = vec![
        kv("index_type", spec.index_type.clone()),
        kv("metric_type", spec.metric_type.clone()),
    ];
    for (key, map) in [
        ("params", &spec.params),
        ("search_params", &spec.search_params),
    ] {
        if !map.is_empty() {
            out.push(kv(key, pyjson::dumps(&Value::Object(map.clone()))));
        }
    }
    out
}
