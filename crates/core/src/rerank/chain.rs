//! L2 rerank 链（0026 第 4、7 节；3.x `build_hybrid_function_score_chain`）：合并（rrf / weighted /
//! max / sum / avg）→ decay → 分组或排序与切片。Search + L2 与 HybridSearch 共用。
//!
//! 候选行是扁平的名字到值的映射：`$id` 与 `$score` 先写，entity（不含主键）再覆盖，所以同名字段能盖掉
//! 虚拟值；`_decay_score`、`$group_score` 由链写入。各阶段都可能抛 Python 异常，阶段之间的先后要与
//! 3.x 相同，所以每个阶段先对全部查询做完再进入下一个阶段。

use std::cmp::Ordering;
use std::collections::HashMap;

use serde_json::{Map, Value};

use super::boost::{ID_FIELD, python_sum};
use super::pyval::PyVal;
use super::try_sort_by;
use crate::error::{EngineError, Result, py_repr};
use crate::index::Metric;
use crate::schema::{CollectionSchema, DataType};

pub(crate) const SCORE_FIELD: &str = "$score";
pub(crate) const DECAY_SCORE_FIELD: &str = "_decay_score";
pub(crate) const GROUP_SCORE_FIELD: &str = "$group_score";

/// `function_score` 里的 L2 函数（解码期的规整与校验已在 gRPC 层做完，见设计第 2 节）。
#[derive(Debug, Clone, PartialEq)]
pub enum L2Function {
    /// `k` 已经过 `float()`；没写时为 60。
    Rrf { k: f64 },
    /// `weights` 的元素已经过 `float()`，没写与空数组都是空；`normalize` 已规整为布尔，缺省为假。
    Weighted { weights: Vec<f64>, normalize: bool },
    /// decay 的参数在解码期不规整，原样保留（JSON 能解析按 JSON，否则是字符串）。
    Decay {
        inputs: Vec<String>,
        params: Map<String, Value>,
    },
    Model {
        inputs: Vec<String>,
        params: Map<String, Value>,
    },
}

/// 链尾的分组与切片参数。`group` 的大小已是 `group_size or 1`。
#[derive(Debug, Clone, PartialEq)]
pub struct Tail {
    pub limit: i64,
    pub offset: i64,
    pub group: Option<(String, i64)>,
}

fn invalid(message: impl Into<String>) -> EngineError {
    EngineError::invalid(message.into())
}

/// 一个候选行。`fields` 是链需要的非虚拟名字（分组字段、decay 输入字段）在 entity 里的值，
/// 外层 None 表示 entity 里没有这个名字。
#[derive(Debug, Clone)]
pub(crate) struct ChainRow {
    pub(crate) id: PyVal,
    pub(crate) score: PyVal,
    pub(crate) decay: Option<PyVal>,
    pub(crate) group_score: Option<PyVal>,
    fields: Vec<Option<PyVal>>,
    /// 调用者给的候选标识，链不解释它。
    pub(crate) source: usize,
}

/// 每一路的分数归一化（3.x `merge_op._normalize_func` / `_direction_convert_func`）。
#[derive(Debug, Clone, Copy, PartialEq)]
enum Norm {
    Cosine,
    /// IP 与其他正相关度量。
    Positive,
    /// L2 类：`1 − 2·atan(d)/π`。
    Distance,
}

impl Norm {
    fn apply(self, value: &PyVal) -> Result<f64> {
        let x = value.to_float()?;
        Ok(match self {
            Self::Cosine => (1.0 + x) * 0.5,
            Self::Positive => 0.5 + x.atan() / std::f64::consts::PI,
            Self::Distance => 1.0 - 2.0 * x.atan() / std::f64::consts::PI,
        })
    }
}

fn positively_related(metric: Metric) -> bool {
    matches!(metric, Metric::Cosine | Metric::Ip)
}

/// 3.x `_resolve_merge_behavior`：返回排序方向（降序为真）与每一路的归一化。
fn resolve(
    normalize: bool,
    force_descending: bool,
    metrics: &[Metric],
) -> (bool, Vec<Option<Norm>>) {
    if metrics.is_empty() {
        return (true, Vec::new());
    }
    if normalize {
        let norms = metrics
            .iter()
            .map(|m| {
                Some(match m {
                    Metric::Cosine => Norm::Cosine,
                    Metric::Ip => Norm::Positive,
                    Metric::L2 => Norm::Distance,
                })
            })
            .collect();
        return (true, norms);
    }
    let positive = metrics.iter().filter(|m| positively_related(**m)).count();
    let mixed = positive > 0 && positive < metrics.len();
    if mixed || force_descending {
        let norms = metrics
            .iter()
            .map(|m| (!positively_related(*m)).then_some(Norm::Distance))
            .collect();
        return (true, norms);
    }
    (positive == metrics.len(), vec![None; metrics.len()])
}

#[derive(Debug)]
enum Merge {
    Rrf(f64),
    Weighted(Vec<f64>),
    /// decay 的 `score_mode`：校验时取小写，合并时用原值（`"MAX"` 通过校验、合并时才报错）。
    Simple(PyVal),
}

/// 3.x `DecayReranker` 构造好的曲线。
#[derive(Debug)]
struct Curve {
    function: Function,
    origin: f64,
    scale: f64,
    offset: f64,
    ln_decay: f64,
    /// gauss 的 `scale² / ln(decay)`。
    sigma_sq: f64,
    /// linear 的 `(1 − decay) / scale`。
    slope: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Function {
    Gauss,
    Exp,
    Linear,
}

impl Curve {
    fn factor(&self, value: f64) -> Result<f64> {
        // Python 的 max(0.0, nan) 返回 0.0：NaN 的比较为假，保留第一个参数。
        let raw = (value - self.origin).abs() - self.offset;
        let d = if raw > 0.0 { raw } else { 0.0 };
        if d == 0.0 {
            return Ok(1.0);
        }
        Ok(match self.function {
            Function::Gauss => {
                if self.sigma_sq == 0.0 {
                    return Err(invalid("float division by zero"));
                }
                (d * d / self.sigma_sq).exp()
            }
            Function::Exp => (self.ln_decay * d / self.scale).exp(),
            Function::Linear => {
                let v = 1.0 - self.slope * d;
                if v > 0.0 { v } else { 0.0 }
            }
        })
    }
}

/// 构造好的 L2 链。
#[derive(Debug)]
pub(crate) struct Chain {
    merge: Merge,
    norms: Vec<Option<Norm>>,
    descending: bool,
    /// decay 的输入字段与曲线。
    decay: Option<(String, Curve)>,
    tail: Tail,
    /// 候选行需要从 entity 取值的非虚拟名字。
    names: Vec<String>,
}

fn is_virtual(name: &str) -> bool {
    matches!(
        name,
        ID_FIELD | SCORE_FIELD | DECAY_SCORE_FIELD | GROUP_SCORE_FIELD
    )
}

/// Python 的 `a < b` 等比较里，一侧是整数 0。`zero_left` 时 0 在左边。NaN 的比较总为假，由调用方先排除。
fn compare_zero(value: &PyVal, op: &str, zero_left: bool) -> Result<Ordering> {
    match value {
        PyVal::Bool(_) | PyVal::Int(_) | PyVal::BigInt(_) | PyVal::Float(_) => {
            value.py_order(&PyVal::Int(0))
        }
        _ => {
            let (left, right) = if zero_left {
                ("int", value.type_name())
            } else {
                (value.type_name(), "int")
            };
            Err(invalid(format!(
                "{} not supported between instances of {} and {}",
                py_repr(op),
                py_repr(left),
                py_repr(right)
            )))
        }
    }
}

fn is_nan(value: &PyVal) -> bool {
    matches!(value, PyVal::Float(f) if f.is_nan())
}

/// Python 的真值。
fn truthy(value: &PyVal) -> bool {
    match value {
        PyVal::None => false,
        PyVal::Bool(b) => *b,
        PyVal::Int(i) => *i != 0,
        PyVal::BigInt(_) | PyVal::DateTime(_) => true,
        PyVal::Float(f) => *f != 0.0,
        PyVal::Str(s) => !s.is_empty(),
        PyVal::List(items) => !items.is_empty(),
        PyVal::Dict(items) => !items.is_empty(),
    }
}

fn param(params: &Map<String, Value>, key: &str) -> Option<PyVal> {
    params.get(key).map(PyVal::from_json)
}

/// 3.x `DecayReranker.__init__`：先校验 function、scale、offset、decay，再把 origin 等转成浮点。
fn curve(params: &Map<String, Value>) -> Result<Curve> {
    let required = |key: &str| param(params, key).ok_or_else(|| invalid(py_repr(key)));
    let function = required("function")?;
    let origin = required("origin")?;
    let scale = required("scale")?;
    let offset = param(params, "offset").unwrap_or(PyVal::Float(0.0));
    let decay = param(params, "decay").unwrap_or(PyVal::Float(0.5));
    let function = match &function {
        PyVal::Str(s) if s == "gauss" => Function::Gauss,
        PyVal::Str(s) if s == "exp" => Function::Exp,
        PyVal::Str(s) if s == "linear" => Function::Linear,
        PyVal::List(_) | PyVal::Dict(_) => {
            return Err(invalid(format!(
                "unhashable type: {}",
                py_repr(function.type_name())
            )));
        }
        other => {
            return Err(invalid(format!(
                "decay: invalid function {}, must be one of ['exp', 'gauss', 'linear']",
                other.repr()
            )));
        }
    };
    // `scale <= 0`：NaN 的比较为假，通过。
    if !is_nan(&scale) && compare_zero(&scale, "<=", false)? != Ordering::Greater {
        return Err(invalid(format!(
            "decay: scale must be > 0, got {}",
            scale.py_str()
        )));
    }
    if !is_nan(&offset) && compare_zero(&offset, "<", false)? == Ordering::Less {
        return Err(invalid(format!(
            "decay: offset must be >= 0, got {}",
            offset.py_str()
        )));
    }
    // `not (0 < decay < 1)`：先比较 `0 < decay`，类型错误的文案里 0 在左边。
    let above_zero = !is_nan(&decay) && compare_zero(&decay, "<", true)? == Ordering::Greater;
    let below_one = above_zero && decay.to_float()? < 1.0;
    if !below_one {
        return Err(invalid(format!(
            "decay: decay must be 0 < decay < 1, got {}",
            decay.py_str()
        )));
    }
    let decay = decay.to_float()?;
    let scale_value = scale.to_float()?;
    let ln_decay = decay.ln();
    let sigma_sq = scale_value * scale_value / ln_decay;
    let slope = (1.0 - decay) / scale_value;
    Ok(Curve {
        function,
        origin: origin.to_float()?,
        scale: scale_value,
        offset: offset.to_float()?,
        ln_decay,
        sigma_sq,
        slope,
    })
}

const NUMERIC_TYPES: [DataType; 6] = [
    DataType::Int8,
    DataType::Int16,
    DataType::Int32,
    DataType::Int64,
    DataType::Float,
    DataType::Double,
];

fn model_unsupported() -> EngineError {
    EngineError::Unsupported {
        feature: "rerank model providers",
        reason: "not supported in this build of milvus-lite-rust yet".to_owned(),
    }
}

/// 3.x `rerank/factory.py` 的 provider 校验；合法的 provider（cohere）在这之后才会调用外部服务。
fn check_provider(params: &Map<String, Value>) -> Result<()> {
    let supported = "'cohere'";
    let provider = match param(params, "provider") {
        Some(PyVal::Str(s)) if !s.is_empty() => s,
        _ => {
            return Err(invalid(format!(
                "RERANK function requires 'provider' in params. Supported: {supported}"
            )));
        }
    };
    let normalized = provider.to_lowercase();
    if normalized != "cohere" {
        return Err(invalid(format!(
            "Unknown rerank provider: {}. Supported: {supported}",
            py_repr(&normalized)
        )));
    }
    if param(params, "model_name").is_some_and(|v| !matches!(v, PyVal::Str(_))) {
        return Err(invalid(
            "Cohere rerank parameter 'model_name' must be a string",
        ));
    }
    for key in ["api_key", "base_url"] {
        if param(params, key).is_some_and(|v| !matches!(v, PyVal::Str(_) | PyVal::None)) {
            return Err(invalid(format!(
                "Cohere rerank parameter {} must be a string",
                py_repr(key)
            )));
        }
    }
    Ok(())
}

impl Chain {
    /// 3.x `_validate_rerank_func` 再加 `_build_rerank_head`：校验与曲线构造都在检索之后、执行之前。
    /// `metrics` 是每一路的度量（Search 只有一路）。
    pub(crate) fn build(
        function: &L2Function,
        schema: &CollectionSchema,
        metrics: &[Metric],
        tail: Tail,
    ) -> Result<Self> {
        let (merge, normalize, force_descending, decay) = match function {
            L2Function::Rrf { k } => {
                if *k <= 0.0 || *k >= 16384.0 {
                    return Err(invalid("The rank params k should be in range (0, 16384)"));
                }
                (Merge::Rrf(*k), false, false, None)
            }
            L2Function::Weighted { weights, normalize } => {
                if weights.is_empty() {
                    return Err(invalid("weighted reranker requires weights parameter"));
                }
                if weights.len() != metrics.len() {
                    return Err(invalid(
                        "the length of weights param mismatch with ann search requests",
                    ));
                }
                if weights.iter().any(|w| *w < 0.0 || *w > 1.0) {
                    return Err(invalid("rank param weight should be in range [0, 1]"));
                }
                (Merge::Weighted(weights.clone()), *normalize, false, None)
            }
            L2Function::Decay { inputs, params } => {
                if inputs.len() != 1 {
                    return Err(invalid("decay reranker requires exactly 1 input field"));
                }
                let mode = param(params, "score_mode").unwrap_or(PyVal::Str("max".to_owned()));
                if !matches!(mode.py_str().to_lowercase().as_str(), "max" | "sum" | "avg") {
                    return Err(invalid(
                        "unsupported score_mode: only supports [max, sum, avg]",
                    ));
                }
                let input = &inputs[0];
                let Some(field) = schema.field(input) else {
                    return Err(invalid(format!(
                        "input field {input} not found in collection schema"
                    )));
                };
                if !NUMERIC_TYPES.contains(&field.data_type) {
                    return Err(invalid(format!(
                        "decay input field {} must be numeric, got {}",
                        field.name,
                        field.data_type.name()
                    )));
                }
                let normalize = param(params, "norm_score").is_some_and(|v| truthy(&v));
                let curve = curve(params)?;
                (
                    Merge::Simple(mode),
                    normalize,
                    true,
                    Some((input.clone(), curve)),
                )
            }
            L2Function::Model { inputs, params } => {
                if inputs.len() != 1 {
                    return Err(invalid("model reranker requires exactly 1 input field"));
                }
                let input = &inputs[0];
                let Some(field) = schema.field(input) else {
                    return Err(invalid(format!(
                        "input field {input} not found in collection schema"
                    )));
                };
                if field.data_type != DataType::VarChar {
                    return Err(invalid(format!(
                        "model input field {} must be VARCHAR, got {}",
                        field.name,
                        field.data_type.name()
                    )));
                }
                check_provider(params)?;
                return Err(model_unsupported());
            }
        };
        // 3.x 构造 rrf 的 MergeOp 时不传度量，因此总是降序、不归一化。
        let (descending, norms) = match merge {
            Merge::Rrf(_) => (true, Vec::new()),
            _ => resolve(normalize, force_descending, metrics),
        };
        let mut names = Vec::new();
        for name in tail
            .group
            .iter()
            .map(|(field, _)| field.as_str())
            .chain(decay.iter().map(|(input, _)| input.as_str()))
        {
            if !is_virtual(name) && !names.iter().any(|n| n == name) {
                names.push(name.to_owned());
            }
        }
        Ok(Self {
            merge,
            norms,
            descending,
            decay,
            tail,
            names,
        })
    }

    /// 一个候选行：`entity(name)` 是投影内同名字段或动态键的值（没有则为 None）。
    pub(crate) fn row(
        &self,
        source: usize,
        id: PyVal,
        public_score: f64,
        mut entity: impl FnMut(&str) -> Result<Option<PyVal>>,
    ) -> Result<ChainRow> {
        let mut fields = Vec::with_capacity(self.names.len());
        for name in &self.names {
            fields.push(entity(name)?);
        }
        Ok(ChainRow {
            id: entity(ID_FIELD)?.unwrap_or(id),
            score: entity(SCORE_FIELD)?.unwrap_or(PyVal::Float(public_score)),
            decay: entity(DECAY_SCORE_FIELD)?,
            group_score: entity(GROUP_SCORE_FIELD)?,
            fields,
            source,
        })
    }

    /// 是否有 decay（响应要把 `_decay_score` 写成因子）。
    pub(crate) fn has_decay(&self) -> bool {
        self.decay.is_some()
    }

    /// `row.get(name)`：没有这个名字时是 None。
    pub(crate) fn get(&self, row: &ChainRow, name: &str) -> PyVal {
        match name {
            ID_FIELD => row.id.clone(),
            SCORE_FIELD => row.score.clone(),
            DECAY_SCORE_FIELD => row.decay.clone().unwrap_or(PyVal::None),
            GROUP_SCORE_FIELD => row.group_score.clone().unwrap_or(PyVal::None),
            _ => self
                .names
                .iter()
                .position(|n| n == name)
                .and_then(|i| row.fields[i].clone())
                .unwrap_or(PyVal::None),
        }
    }

    /// 执行整条链。`queries[q][r]` 是第 q 个查询在第 r 路的候选（按该路的名次）。
    pub(crate) fn execute(&self, queries: Vec<Vec<Vec<ChainRow>>>) -> Result<Vec<Vec<ChainRow>>> {
        let mut merged = Vec::with_capacity(queries.len());
        for routes in queries {
            merged.push(self.merge(routes)?);
        }
        if let Some((input, curve)) = &self.decay {
            let mut factors = Vec::with_capacity(merged.len());
            for rows in &merged {
                let mut column = Vec::with_capacity(rows.len());
                for row in rows {
                    column.push(match self.get(row, input) {
                        PyVal::None => 0.0,
                        value => curve.factor(value.to_float()?)?,
                    });
                }
                factors.push(column);
            }
            for (rows, column) in merged.iter_mut().zip(factors) {
                for (row, factor) in rows.iter_mut().zip(column) {
                    row.decay = Some(PyVal::Float(factor));
                    row.score = PyVal::Float(combine(&row.score, factor)?);
                }
            }
        }
        let mut out = Vec::with_capacity(merged.len());
        for rows in merged {
            out.push(match &self.tail.group {
                Some((field, size)) => self.group(rows, field, *size)?,
                None => self.sort_and_limit(rows)?,
            });
        }
        Ok(out)
    }

    fn merge(&self, routes: Vec<Vec<ChainRow>>) -> Result<Vec<ChainRow>> {
        let mut order: Vec<ChainRow> = Vec::new();
        let mut index: HashMap<Key, usize> = HashMap::new();
        let mut nan = 0usize;
        let mut slot = |row: &ChainRow, order: &mut Vec<ChainRow>| -> Result<(usize, bool)> {
            let key = Key::of(&row.id, &mut nan)?;
            if let Some(&i) = index.get(&key) {
                return Ok((i, false));
            }
            index.insert(key, order.len());
            order.push(row.clone());
            Ok((order.len() - 1, true))
        };
        match &self.merge {
            Merge::Rrf(k) => {
                let mut scores: Vec<f64> = Vec::new();
                for route in &routes {
                    for (rank, row) in route.iter().enumerate() {
                        let (i, new) = slot(row, &mut order)?;
                        let denominator = k + rank as f64 + 1.0;
                        if denominator == 0.0 {
                            return Err(invalid("float division by zero"));
                        }
                        let rrf = 1.0 / denominator;
                        if new {
                            scores.push(0.0 + rrf);
                        } else {
                            scores[i] += rrf;
                        }
                    }
                }
                for (row, score) in order.iter_mut().zip(scores) {
                    row.score = PyVal::Float(score);
                }
            }
            Merge::Weighted(weights) => {
                let mut route_scores: Vec<Vec<Option<PyVal>>> = Vec::new();
                for (r, route) in routes.iter().enumerate() {
                    for row in route {
                        let (i, new) = slot(row, &mut order)?;
                        if new {
                            route_scores.push(vec![None; routes.len()]);
                        }
                        route_scores[i][r] = Some(row.score.clone());
                    }
                }
                for (row, scores) in order.iter_mut().zip(route_scores) {
                    let mut total = 0.0;
                    for (r, score) in scores.iter().enumerate() {
                        // 3.x 用 `is not None` 判断这一路有没有分数：盖成 `None` 的 `$score` 当作缺席。
                        let Some(score) = score.as_ref().filter(|s| **s != PyVal::None) else {
                            continue;
                        };
                        let value = match self.norms.get(r).copied().flatten() {
                            Some(norm) => weights[r] * norm.apply(score)?,
                            None => multiply(weights[r], score)?,
                        };
                        total += value;
                    }
                    row.score = PyVal::Float(total);
                }
            }
            Merge::Simple(strategy) => {
                let mut lists: Vec<Vec<PyVal>> = Vec::new();
                for (r, route) in routes.iter().enumerate() {
                    for row in route {
                        let score = match self.norms.get(r).copied().flatten() {
                            Some(norm) => PyVal::Float(norm.apply(&row.score)?),
                            None => row.score.clone(),
                        };
                        let (i, new) = slot(row, &mut order)?;
                        if new {
                            lists.push(Vec::new());
                        }
                        lists[i].push(score);
                    }
                }
                for (row, scores) in order.iter_mut().zip(lists) {
                    row.score = match strategy {
                        PyVal::Str(s) if s == "max" => py_max(scores)?,
                        PyVal::Str(s) if s == "sum" => py_sum(&scores)?,
                        PyVal::Str(s) if s == "avg" => {
                            let n = scores.len();
                            py_divide(&py_sum(&scores)?, n)?
                        }
                        other => {
                            return Err(invalid(format!(
                                "Unknown merge strategy: {}",
                                other.py_str()
                            )));
                        }
                    };
                }
            }
        }
        Ok(order)
    }

    fn score_of(row: &ChainRow) -> f64 {
        match row.score {
            PyVal::Float(f) => f,
            // 合并与 decay 之后分数总是浮点。
            _ => f64::NAN,
        }
    }

    /// 3.x `SortOp`（平局键 `$id`）+ `LimitOp`。
    fn sort_and_limit(&self, rows: Vec<ChainRow>) -> Result<Vec<ChainRow>> {
        let descending = self.descending;
        let mut rows = try_sort_by(rows, &mut |a, b| {
            let (x, y) = (Self::score_of(a), Self::score_of(b));
            match (x.is_nan(), y.is_nan()) {
                (true, true) => tie(a, b),
                (true, false) => Ok(Ordering::Greater),
                (false, true) => Ok(Ordering::Less),
                (false, false) if x == y => tie(a, b),
                (false, false) => Ok(if descending {
                    y.total_cmp(&x)
                } else {
                    x.total_cmp(&y)
                }),
            }
        })?;
        if self.tail.limit > 0 {
            let len = rows.len() as i128;
            let start = i128::from(self.tail.offset).min(len);
            let end = (start + i128::from(self.tail.limit)).min(len);
            let range = py_slice(rows.len(), start, end);
            rows = rows.drain(range).collect();
        }
        Ok(rows)
    }

    /// 3.x `GroupByOp`：组键是分组前的 `row.get(field)`；组内稳定排序取 `hits[:group_size]`，
    /// 组得分取 `max`，组按组得分稳定排序后取 `[offset : offset + limit]`。
    fn group(&self, rows: Vec<ChainRow>, field: &str, size: i64) -> Result<Vec<ChainRow>> {
        let mut groups: Vec<Vec<ChainRow>> = Vec::new();
        let mut index: HashMap<Key, usize> = HashMap::new();
        let mut nan = 0usize;
        for row in rows {
            let key = Key::of(&self.get(&row, field), &mut nan)?;
            match index.get(&key) {
                Some(&i) => groups[i].push(row),
                None => {
                    index.insert(key, groups.len());
                    groups.push(vec![row]);
                }
            }
        }
        let descending = self.descending;
        // NaN 的位置依赖 Timsort 的比较序列，不逐位复现，排在末尾（设计“登记的差异”）。
        let by_score = |x: f64, y: f64| match (x.is_nan(), y.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            // `-0.0 == 0.0`：相等时保持原有顺序，不能用 total_cmp。
            (false, false) if descending => y.partial_cmp(&x).unwrap_or(Ordering::Equal),
            (false, false) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        };
        let mut scored: Vec<(f64, Vec<ChainRow>)> = Vec::with_capacity(groups.len());
        for mut hits in groups {
            hits.sort_by(|a, b| by_score(Self::score_of(a), Self::score_of(b)));
            let range = py_slice(hits.len(), 0, i128::from(size));
            let top: Vec<ChainRow> = hits.drain(range).collect();
            // Python 的 `max`：后面的值严格大于当前值才替换，NaN 的比较为假。
            let mut best: Option<f64> = None;
            for hit in &top {
                let score = Self::score_of(hit);
                best = Some(match best {
                    Some(current) if score.partial_cmp(&current) != Some(Ordering::Greater) => {
                        current
                    }
                    _ => score,
                });
            }
            scored.push((best.unwrap_or(0.0), top));
        }
        scored.sort_by(|a, b| by_score(a.0, b.0));
        let start = i128::from(self.tail.offset);
        let range = py_slice(scored.len(), start, start + i128::from(self.tail.limit));
        let mut out = Vec::new();
        for (score, hits) in scored.drain(range) {
            for mut hit in hits {
                hit.group_score = Some(PyVal::Float(score));
                out.push(hit);
            }
        }
        Ok(out)
    }
}

fn tie(a: &ChainRow, b: &ChainRow) -> Result<Ordering> {
    match (a.id.is_missing(), b.id.is_missing()) {
        (true, true) => Ok(Ordering::Equal),
        (true, false) => Ok(Ordering::Greater),
        (false, true) => Ok(Ordering::Less),
        (false, false) => a.id.py_order(&b.id),
    }
}

/// Python 切片 `seq[start:stop]` 的下标范围（步长 1）。
pub(crate) fn py_slice(len: usize, start: i128, stop: i128) -> std::ops::Range<usize> {
    let len_i = len as i128;
    let clamp = |i: i128| -> usize {
        let i = if i < 0 {
            (i + len_i).max(0)
        } else {
            i.min(len_i)
        };
        i as usize
    };
    let (s, e) = (clamp(start), clamp(stop));
    s..e.max(s)
}

/// 3.x `NumCombineExpr("multiply")` 的一行：`[分数, 因子]`。
fn combine(score: &PyVal, factor: f64) -> Result<f64> {
    let not_numeric = || invalid("num_combine row values must be finite numeric non-bool values");
    let value = match score {
        PyVal::None => return Ok(0.0),
        PyVal::Int(i) => *i as f64,
        // Python 整数是 `Real`；`math.isfinite` 把它转成浮点，超出 float64 时是 OverflowError，按非有限处理。
        PyVal::BigInt(_) => match score.to_float() {
            Ok(f) if f.is_finite() => f,
            _ => return Err(not_numeric()),
        },
        PyVal::Float(f) if f.is_finite() => *f,
        _ => return Err(not_numeric()),
    };
    let result = 1.0 * value * factor;
    if !result.is_finite() {
        return Err(invalid("num_combine produced a non-finite result"));
    }
    Ok(result)
}

/// Python `float * value`（weighted 不归一化时的权重乘分数）。
fn multiply(weight: f64, value: &PyVal) -> Result<f64> {
    match value {
        PyVal::Bool(_) | PyVal::Int(_) | PyVal::BigInt(_) | PyVal::Float(_) => {
            Ok(weight * value.to_float()?)
        }
        PyVal::Str(_) | PyVal::List(_) => Err(invalid(
            "can't multiply sequence by non-int of type 'float'",
        )),
        other => Err(invalid(format!(
            "unsupported operand type(s) for *: 'float' and {}",
            py_repr(other.type_name())
        ))),
    }
}

/// Python `max(values)`：后面的值严格大于当前值才替换。
fn py_max(values: Vec<PyVal>) -> Result<PyVal> {
    let mut items = values.into_iter();
    let Some(mut best) = items.next() else {
        return Err(invalid("max() arg is an empty sequence"));
    };
    for value in items {
        if value.py_order(&best)? == Ordering::Greater && !is_nan(&value) && !is_nan(&best) {
            best = value;
        }
    }
    Ok(best)
}

/// Python `sum(values)`（起始值是整数 0）。全是整数时精确求和，结果仍是整数；有浮点时按 CPython 3.12 的
/// 补偿求和。超出 i128 的整数只在单个值时原样保留，多个这样的值相加按浮点近似（只能由多个同名字段
/// 盖成同一 `$id` 的宽整数 `$score` 触发）。
fn py_sum(values: &[PyVal]) -> Result<PyVal> {
    let mut left = "int";
    let mut any_float = false;
    let mut integer: Option<i128> = Some(0);
    let mut floats = Vec::with_capacity(values.len());
    for value in values {
        match value {
            PyVal::Bool(b) => {
                integer = integer.and_then(|n| n.checked_add(i128::from(*b)));
                floats.push(f64::from(u8::from(*b)));
            }
            PyVal::Int(i) => {
                integer = integer.and_then(|n| n.checked_add(i128::from(*i)));
                floats.push(*i as f64);
            }
            PyVal::BigInt(text) => {
                integer = integer
                    .zip(text.parse::<i128>().ok())
                    .and_then(|(n, v)| n.checked_add(v));
                floats.push(value.to_float().unwrap_or(f64::INFINITY));
            }
            PyVal::Float(f) => {
                any_float = true;
                left = "float";
                floats.push(*f);
            }
            other => {
                return Err(invalid(format!(
                    "unsupported operand type(s) for +: {} and {}",
                    py_repr(left),
                    py_repr(other.type_name())
                )));
            }
        }
    }
    if any_float {
        if floats.iter().any(|f| f.is_infinite())
            && values.iter().any(|v| matches!(v, PyVal::BigInt(_)))
        {
            return Err(invalid("int too large to convert to float"));
        }
        // 起始的整数 0 先转成 0.0：`sum([-0.0])` 是 0.0。
        return Ok(PyVal::Float(0.0 + python_sum(&floats)));
    }
    match integer {
        Some(n) => Ok(i64::try_from(n).map_or_else(|_| PyVal::BigInt(n.to_string()), PyVal::Int)),
        None if values.len() == 1 => Ok(values[0].clone()),
        None => Ok(PyVal::Float(python_sum(&floats))),
    }
}

/// `sum(...) / len(...)`：Python 的真除法。
fn py_divide(total: &PyVal, n: usize) -> Result<PyVal> {
    let value = match total {
        PyVal::Float(f) => *f,
        // 整数的真除法：超出 float64 的被除数在 3.x 是 OverflowError。
        other => other
            .to_float()
            .map_err(|_| invalid("integer division result too large for a float"))?,
    };
    Ok(PyVal::Float(value / n as f64))
}

/// Python 的哈希相等：`True`、`1`、`1.0` 是同一个键；每个 NaN 是不同的对象，各自成键；
/// list / dict 不可哈希。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Null,
    Int(i128),
    /// 超出 i128 的整数（含整数值的浮点），十进制文本。
    Big(String),
    Fraction(u64),
    Nan(usize),
    Str(String),
    DateTime(i64),
}

impl Key {
    fn of(value: &PyVal, nan: &mut usize) -> Result<Self> {
        Ok(match value {
            PyVal::None => Self::Null,
            PyVal::Bool(b) => Self::Int(i128::from(*b)),
            PyVal::Int(i) => Self::Int(i128::from(*i)),
            PyVal::BigInt(text) => text.parse::<i128>().map_or_else(
                |_| Self::Big(text.trim_start_matches('+').to_owned()),
                Self::Int,
            ),
            PyVal::Float(f) if f.is_nan() => {
                *nan += 1;
                Self::Nan(*nan)
            }
            PyVal::Float(f) if f.fract() == 0.0 && f.is_finite() => {
                if f.abs() < 1.7e38 {
                    Self::Int(*f as i128)
                } else {
                    Self::Big(format!("{f:.0}"))
                }
            }
            PyVal::Float(f) => Self::Fraction(f.to_bits()),
            PyVal::Str(s) => Self::Str(s.clone()),
            PyVal::DateTime(micros) => Self::DateTime(*micros),
            PyVal::List(_) | PyVal::Dict(_) => {
                return Err(invalid(format!(
                    "unhashable type: {}",
                    py_repr(value.type_name())
                )));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, score: f64) -> ChainRow {
        ChainRow {
            id: PyVal::Int(id),
            score: PyVal::Float(score),
            decay: None,
            group_score: None,
            fields: Vec::new(),
            source: id as usize,
        }
    }

    fn chain(merge: Merge, descending: bool, tail: Tail) -> Chain {
        Chain {
            merge,
            norms: vec![None],
            descending,
            decay: None,
            tail,
            names: Vec::new(),
        }
    }

    fn tail(limit: i64, offset: i64) -> Tail {
        Tail {
            limit,
            offset,
            group: None,
        }
    }

    fn ids(rows: &[ChainRow]) -> Vec<usize> {
        rows.iter().map(|r| r.source).collect()
    }

    #[test]
    fn rrf_scores_follow_rank_and_ties_break_on_id() {
        let c = chain(Merge::Rrf(1.0), true, tail(10, 0));
        let out = c
            .execute(vec![vec![vec![row(5, 0.9), row(3, 0.8), row(4, 0.1)]]])
            .unwrap();
        assert_eq!(ids(&out[0]), vec![5, 3, 4]);
        assert_eq!(Chain::score_of(&out[0][0]), 0.5);
        // 两路同名次同分时按 `$id` 升序，不按首次出现。
        let out = c
            .execute(vec![vec![
                vec![row(63, 0.0), row(1, 0.0)],
                vec![row(30, 0.0), row(2, 0.0)],
            ]])
            .unwrap();
        assert_eq!(ids(&out[0]), vec![30, 63, 1, 2]);
    }

    #[test]
    fn limit_uses_python_slices() {
        let rows = || vec![vec![(0..6).map(|i| row(i, 10.0 - i as f64)).collect()]];
        let run = |limit, offset| {
            ids(&chain(Merge::Rrf(60.0), true, tail(limit, offset))
                .execute(rows())
                .unwrap()[0])
        };
        assert_eq!(run(3, 2), vec![2, 3, 4]);
        // limit <= 0 不切片。
        assert_eq!(run(0, 5).len(), 6);
        // 负 offset：`chunk[-1:3]` 在候选多于 3 条时为空。
        assert_eq!(run(4, -1), Vec::<usize>::new());
        assert_eq!(run(3, 1000), Vec::<usize>::new());
    }

    #[test]
    fn group_slices_and_scores_like_python() {
        let mut c = chain(
            Merge::Rrf(60.0),
            true,
            Tail {
                limit: 2,
                offset: 0,
                group: Some(("g".to_owned(), 2)),
            },
        );
        c.names = vec!["g".to_owned()];
        let mk = |id, g: i64| {
            let mut r = row(id, 0.0);
            r.fields = vec![Some(PyVal::Int(g))];
            r
        };
        let rows = vec![vec![vec![mk(0, 1), mk(1, 2), mk(2, 1), mk(3, 3), mk(4, 1)]]];
        let out = c.execute(rows.clone()).unwrap();
        assert_eq!(ids(&out[0]), vec![0, 2, 1]);
        // 负 group_size：留下除最后 |n| 个之外的全部。
        c.tail.group = Some(("g".to_owned(), -1));
        c.tail.limit = 3;
        let out = c.execute(rows.clone()).unwrap();
        assert_eq!(ids(&out[0]), vec![0, 2]);
        // 负 offset 也是切片：共 3 组时 `[-1:2]` 为空，`[-3:0]` 也为空，`[-3:-1]` 取前两组。
        c.tail.group = Some(("g".to_owned(), 1));
        c.tail.offset = -1;
        c.tail.limit = 3;
        assert!(c.execute(rows.clone()).unwrap()[0].is_empty());
        c.tail.offset = -3;
        assert!(c.execute(rows.clone()).unwrap()[0].is_empty());
        c.tail.limit = 2;
        assert_eq!(ids(&c.execute(rows).unwrap()[0]), vec![0, 1]);
    }

    #[test]
    fn keys_follow_python_hash_equality() {
        let mut nan = 0;
        let key = |v: PyVal, nan: &mut usize| Key::of(&v, nan).unwrap();
        assert_eq!(
            key(PyVal::Bool(true), &mut nan),
            key(PyVal::Int(1), &mut nan)
        );
        assert_eq!(
            key(PyVal::Float(1.0), &mut nan),
            key(PyVal::Int(1), &mut nan)
        );
        assert_eq!(
            key(PyVal::Float(-0.0), &mut nan),
            key(PyVal::Int(0), &mut nan)
        );
        assert_ne!(
            key(PyVal::Float(f64::NAN), &mut nan),
            key(PyVal::Float(f64::NAN), &mut nan)
        );
        let big = 9_007_199_254_740_993_i64;
        assert_ne!(
            key(PyVal::Int(big), &mut nan),
            key(PyVal::Int(big - 1), &mut nan)
        );
        assert_eq!(
            key(PyVal::Float(9_007_199_254_740_992.0), &mut nan),
            key(PyVal::Int(big - 1), &mut nan)
        );
        assert_ne!(
            key(PyVal::Str("1".into()), &mut nan),
            key(PyVal::Int(1), &mut nan)
        );
        assert_eq!(
            Key::of(&PyVal::List(vec![]), &mut nan)
                .unwrap_err()
                .to_string(),
            EngineError::invalid("unhashable type: 'list'").to_string()
        );
    }

    #[test]
    fn decay_curve_matches_python() {
        let params = |pairs: &[(&str, Value)]| -> Map<String, Value> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect()
        };
        let base = |extra: &[(&str, Value)]| {
            let mut p = params(&[
                ("function", Value::from("gauss")),
                ("origin", Value::from(0)),
                ("scale", Value::from(8)),
            ]);
            p.extend(params(extra));
            p
        };
        let c = curve(&base(&[])).unwrap();
        assert_eq!(c.factor(0.0).unwrap(), 1.0);
        assert!((c.factor(8.0).unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(c.factor(f64::NAN).unwrap(), 1.0);
        let err = |extra: &[(&str, Value)]| curve(&base(extra)).unwrap_err().to_string();
        assert!(
            err(&[("decay", Value::from(true))]).contains("decay must be 0 < decay < 1, got True")
        );
        assert!(
            err(&[("decay", Value::from("0.5"))])
                .contains("'<' not supported between instances of 'int' and 'str'")
        );
        assert!(
            err(&[("scale", Value::from("5"))])
                .contains("'<=' not supported between instances of 'str' and 'int'")
        );
        assert!(err(&[("offset", Value::from(-1))]).contains("offset must be >= 0, got -1"));
        assert!(err(&[("function", Value::from(5))]).contains("invalid function 5"));
        assert!(err(&[("function", Value::from(vec![1]))]).contains("unhashable type: 'list'"));
        assert!(
            err(&[("origin", Value::from("x"))]).contains("could not convert string to float: 'x'")
        );
        let tiny = curve(&base(&[("scale", Value::from(1e-200))])).unwrap();
        assert!(
            tiny.factor(1.0)
                .unwrap_err()
                .to_string()
                .contains("float division by zero")
        );
    }

    #[test]
    fn resolve_follows_merge_behavior() {
        use Metric::*;
        assert_eq!(resolve(false, false, &[L2]), (false, vec![None]));
        assert_eq!(resolve(false, false, &[Ip]), (true, vec![None]));
        assert_eq!(
            resolve(false, true, &[L2]),
            (true, vec![Some(Norm::Distance)])
        );
        assert_eq!(
            resolve(false, false, &[Cosine, L2]),
            (true, vec![None, Some(Norm::Distance)])
        );
        assert_eq!(
            resolve(true, false, &[Ip]),
            (true, vec![Some(Norm::Positive)])
        );
    }

    #[test]
    fn weighted_skips_routes_whose_score_is_none() {
        let mut c = chain(Merge::Weighted(vec![0.5]), true, tail(10, 0));
        let mut none = row(1, 0.0);
        none.score = PyVal::None;
        let out = c.execute(vec![vec![vec![row(0, 0.8), none]]]).unwrap();
        assert_eq!(ids(&out[0]), vec![0, 1]);
        assert_eq!(Chain::score_of(&out[0][1]), 0.0);
        // 归一化时同样跳过，不对 None 做 float()。
        c.norms = vec![Some(Norm::Positive)];
        let mut none = row(1, 0.0);
        none.score = PyVal::None;
        assert!(c.execute(vec![vec![vec![none]]]).is_ok());
        // 不归一化时字符串分数是 Python 的序列乘法错误。
        c.norms = vec![None];
        let mut text = row(2, 0.0);
        text.score = PyVal::Str("x".into());
        assert!(
            c.execute(vec![vec![vec![text]]])
                .unwrap_err()
                .to_string()
                .contains("can't multiply sequence")
        );
    }

    #[test]
    fn decay_norm_score_uses_python_truthiness() {
        for (value, expected) in [
            (Value::from(false), false),
            (Value::from(0), false),
            (Value::from(""), false),
            (Value::Null, false),
            (Value::from("False"), true),
            (Value::from(vec![0]), true),
            (Value::from(0.5), true),
        ] {
            assert_eq!(truthy(&PyVal::from_json(&value)), expected, "{value}");
        }
    }

    #[test]
    fn wide_integers_are_numbers_like_in_python() {
        let wide = PyVal::BigInt("1180591620717411303424".into());
        assert_eq!(combine(&wide, 0.5).unwrap(), 1180591620717411303424.0 * 0.5);
        let huge = PyVal::BigInt(format!("1{}", "0".repeat(400)));
        assert!(
            combine(&huge, 0.5)
                .unwrap_err()
                .to_string()
                .contains("finite numeric non-bool")
        );
        // 整数精确求和，结果仍是整数。
        assert_eq!(py_sum(std::slice::from_ref(&wide)).unwrap(), wide);
        assert_eq!(
            py_sum(&[PyVal::Int(i64::MAX), PyVal::Int(1)]).unwrap(),
            PyVal::BigInt("9223372036854775808".into())
        );
        assert_eq!(py_sum(std::slice::from_ref(&huge)).unwrap(), huge);
        assert!(
            py_divide(&huge, 1)
                .unwrap_err()
                .to_string()
                .contains("too large for a float")
        );
        assert!(py_sum(&[huge, PyVal::Float(1.0)]).is_err());
    }

    #[test]
    fn python_arithmetic_on_overridden_scores() {
        assert_eq!(py_sum(&[PyVal::Float(-0.0)]).unwrap(), PyVal::Float(0.0));
        assert_eq!(
            py_sum(&[PyVal::Bool(true), PyVal::Int(2)]).unwrap(),
            PyVal::Int(3)
        );
        assert!(
            py_sum(&[PyVal::Str("a".into())])
                .unwrap_err()
                .to_string()
                .contains("'int' and 'str'")
        );
        assert!(
            multiply(0.5, &PyVal::Str("a".into()))
                .unwrap_err()
                .to_string()
                .contains("can't multiply sequence")
        );
        assert!(
            multiply(0.5, &PyVal::None)
                .unwrap_err()
                .to_string()
                .contains("'float' and 'NoneType'")
        );
        assert!(
            py_max(vec![PyVal::Float(f64::NAN), PyVal::Float(1.0)])
                .unwrap()
                .to_float()
                .unwrap()
                .is_nan()
        );
        assert_eq!(combine(&PyVal::None, 0.5).unwrap(), 0.0);
        assert!(combine(&PyVal::Bool(true), 0.5).is_err());
        assert!(combine(&PyVal::Float(f64::INFINITY), 0.5).is_err());
    }
}
