//! HybridSearchRequest 的解析与执行，与 3.x `servicer.HybridSearch` 一致（0026 第 3、6、8、9 节）：
//! `rank_params` 解析 → `function_score` 解码 → 逐路依次完整执行（子请求解析、分区、未加载、引擎检索）→
//! 构造合并链（校验）→ 合并、排序或分组 → 响应编码（请求时区的第二个校验点）。
//! 各路在 RPC 阻塞线程上顺序执行，每一路内部照常在计算池上并行（0005）。

use std::collections::HashMap;

use milvus_lite_core::rerank::boost::BoostSpec;
use milvus_lite_core::rerank::chain::{L2Function, Tail};
use milvus_lite_core::rerank::window;
use milvus_lite_core::timezone::TimezoneSource;
use milvus_lite_core::{
    CollectionHandle, Compute, EngineError, HybridMerge, ReadSnapshot, RouteFilter, RouteRanking,
    SearchRequest, hybrid_merge,
};
use serde_json::{Map, Value};

use crate::output::TimeFields;
use crate::pb::common::KeyValuePair;
use crate::pb::service as svc;
use crate::schema::unsupported;
use crate::{function_score, pyint, search};

/// 3.x `parse_rank_params` 的结果，另加 `timezone` / `time_fields`（3.x `_extract_timezone`）。
struct RankParams {
    strategy: Value,
    params: Value,
    limit: i64,
    offset: i64,
    group_by_field: Option<String>,
    group_size: Option<i64>,
    timezone: Option<String>,
    time_fields: Option<String>,
}

fn decode_value(text: &str) -> Value {
    milvus_lite_core::json_text::value_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

/// `int()` 依次作用在 `limit`、`offset`、`group_size` 上（3.x 字典字面量的求值顺序），失败都是 code 1。
/// `round_decimal`、`strict_group_size`、`rank_group_scorer` 不解析（3.x 忽略）。
fn parse_rank_params(pairs: &[KeyValuePair]) -> Result<RankParams, EngineError> {
    let raw: HashMap<&str, Value> = pairs
        .iter()
        .map(|kv| (kv.key.as_str(), decode_value(&kv.value)))
        .collect();
    let int =
        |v: &Value| pyint::int(v).map_err(|e| EngineError::InvalidArgument(e.message().to_owned()));
    let limit = raw.get("limit").map_or(Ok(10), int)?;
    let offset = raw.get("offset").map_or(Ok(0), int)?;
    let group_size = match raw.get("group_size") {
        None | Some(Value::Null) => None,
        Some(v) => Some(int(v)?),
    };
    // 3.x 取第一个同名键（与 search_params 的“后者为准”不同）。
    let first_string = |key: &str| {
        pairs
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| match decode_value(&kv.value) {
                Value::String(s) if !s.is_empty() => Some(s),
                _ => None,
            })
    };
    Ok(RankParams {
        strategy: raw
            .get("strategy")
            .cloned()
            .unwrap_or_else(|| Value::String("rrf".to_owned())),
        params: raw
            .get("params")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new())),
        limit,
        offset,
        group_by_field: match raw.get("group_by_field") {
            Some(Value::String(field)) if !field.is_empty() => Some(field.clone()),
            _ => None,
        },
        group_size,
        timezone: first_string("timezone"),
        time_fields: first_string("time_fields"),
    })
}

/// 3.x `merge_boost_rankers(route, top)`：函数按“路由在前、顶层在后”，两个 mode 在顶层有 boost 时取顶层的。
fn merge_boost(route: Option<BoostSpec>, top: Option<&BoostSpec>) -> Option<BoostSpec> {
    match (route, top) {
        (route, None) => route,
        (None, Some(top)) => Some(top.clone()),
        (Some(mut route), Some(top)) => {
            route.functions.extend(top.functions.iter().cloned());
            route.boost_mode = top.boost_mode;
            route.function_mode = top.function_mode;
            Some(route)
        }
    }
}

/// `function_chains` 的拒绝与 collection 是否存在由调用者先行处理（3.x 的先后）。
pub(crate) fn hybrid_search(
    handle: &CollectionHandle,
    compute: &Compute,
    database_timezone: &dyn Fn() -> Result<Option<String>, EngineError>,
    req: &svc::HybridSearchRequest,
) -> Result<svc::SearchResults, EngineError> {
    let rank = parse_rank_params(&req.rank_params)?;
    let top = function_score::decode(req.function_score.as_ref())?;
    let (schema, name) = {
        let guard = handle.read();
        (guard.schema().clone(), guard.name().to_owned())
    };
    // 3.x：请求了输出字段时并上分组字段与 L2 的输入字段；没有请求时引擎本来就取全部字段。
    let mut route_fields = req.output_fields.clone();
    if !route_fields.is_empty() {
        let inputs: &[String] = match &top.l2 {
            Some(L2Function::Decay { inputs, .. } | L2Function::Model { inputs, .. }) => inputs,
            _ => &[],
        };
        for field in rank.group_by_field.iter().chain(inputs) {
            if !route_fields.contains(field) {
                route_fields.push(field.clone());
            }
        }
    }
    let group_size = rank
        .group_by_field
        .as_ref()
        .map(|_| rank.group_size.filter(|size| *size != 0).unwrap_or(1));
    let mut output_timezone = rank.timezone.clone();
    let mut output_time_fields = rank.time_fields.clone();
    let mut routes: Vec<(ReadSnapshot, RouteRanking)> = Vec::with_capacity(req.requests.len());
    for sub in &req.requests {
        let parsed = search::parse_route(sub)?;
        // 与 Search 相同，逐路在该路的分区、load 与过滤之前确定度量；前一路已完整执行。
        handle
            .read()
            .resolve_search_metric(parsed.anns_field.as_deref(), parsed.metric_type.as_deref())?;
        // 过滤与 boost 过滤用 `路由的 || rank_params 的`；输出取 rank_params 的，没有则取第一个带它的路由。
        let route_timezone = parsed.timezone.clone().or_else(|| rank.timezone.clone());
        if output_timezone.is_none() {
            output_timezone = parsed.timezone.clone();
        }
        if output_time_fields.is_none() {
            output_time_fields = parsed.time_fields.clone();
        }
        let boost = merge_boost(parsed.boost, top.boost.as_ref());
        let window = window::hybrid_route(parsed.top_k, rank.limit, group_size, boost.is_some());
        let partitions = if sub.partition_names.is_empty() {
            &req.partition_names
        } else {
            &sub.partition_names
        };
        let snapshot = handle.read().read_named_snapshot(partitions)?;
        snapshot.require_loaded()?;
        if let Some(feature) = parsed.unsupported {
            return Err(unsupported(feature));
        }
        let timezone = TimezoneSource::new(
            route_timezone,
            snapshot.schema().timezone().map(str::to_owned),
            database_timezone,
        );
        let ranking = snapshot.rank_route(
            SearchRequest {
                anns_field: parsed.anns_field,
                vectors: parsed.vectors,
                metric_type: parsed.metric_type,
                limit: usize::try_from(window.max(0)).unwrap_or(usize::MAX),
                offset: 0,
                output_fields: route_fields.clone(),
            },
            compute,
            RouteFilter {
                expr: &parsed.expr,
                timezone: &timezone,
                boost: boost.as_ref(),
            },
        )?;
        routes.push((snapshot, ranking));
    }
    // 顶层 function_score 有 L2 时忽略 rank_params 的 strategy / params（设计第 8 节）。
    let merge = match &top.l2 {
        Some(function) => HybridMerge::Function(function),
        None => HybridMerge::RankParams {
            strategy: &rank.strategy,
            params: &rank.params,
        },
    };
    let pairs: Vec<(&ReadSnapshot, &RouteRanking)> = routes.iter().map(|(s, r)| (s, r)).collect();
    let result = hybrid_merge(
        &schema,
        &pairs,
        merge,
        Tail {
            limit: rank.limit,
            offset: rank.offset,
            group: rank.group_by_field.clone().zip(group_size),
        },
        &req.output_fields,
    )?;
    let timezone = TimezoneSource::new(
        output_timezone,
        schema.timezone().map(str::to_owned),
        database_timezone,
    );
    timezone.validate_request()?;
    let time_fields = TimeFields::new(output_time_fields.as_deref(), &timezone);
    // Hybrid 的 `round_decimal` 在 rank_params 里，服务端忽略（pymilvus 在客户端取整）。
    search::encode(
        &schema,
        &name,
        result,
        rank.limit,
        -1,
        rank.group_by_field.as_deref(),
        time_fields.as_ref(),
    )
}
