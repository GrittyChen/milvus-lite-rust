//! L2 rerank 链（0026 第 4、7 节；3.x `build_hybrid_function_score_chain`）：合并（rrf / weighted /
//! max / sum / avg）→ decay → 分组或排序与切片。Search + L2 与 HybridSearch 共用。
//!
//! 候选行是扁平的名字到值的映射：`$id` 与 `$score` 先写，entity（不含主键）再覆盖，所以同名字段能盖掉
//! 虚拟值；`_decay_score`、`$group_score` 由链写入。各阶段都可能抛 Python 异常，阶段之间的先后要与
//! 3.x 相同，所以每个阶段先对全部查询做完再进入下一个阶段。

use std::cmp::Ordering;
use std::collections::HashMap;

use serde_json::{Map, Value};

use super::boost::ID_FIELD;
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
    /// BM25：`2·atan(x)/π`。链的输入已是正分（`-距离`），值域落在 [0, 1)。
    Bm25,
    /// L2 类：`1 − 2·atan(d)/π`。
    Distance,
}

impl Norm {
    fn apply(self, value: &PyVal) -> Result<f64> {
        let x = value.to_float()?;
        Ok(match self {
            Self::Cosine => (1.0 + x) * 0.5,
            Self::Positive => 0.5 + x.atan() / std::f64::consts::PI,
            Self::Bm25 => 2.0 * x.atan() / std::f64::consts::PI,
            Self::Distance => 1.0 - 2.0 * x.atan() / std::f64::consts::PI,
        })
    }
}

fn positively_related(metric: Metric) -> bool {
    matches!(metric, Metric::Cosine | Metric::Ip | Metric::Bm25)
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
                    Metric::Bm25 => Norm::Bm25,
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

/// `k` 与 `weights` 保留 Python 值：`rank_params` 路径不校验，参与运算时才按 Python 的规则报错
/// （`function_score` 路径已校验成浮点）。
#[derive(Debug)]
enum Merge {
    Rrf(PyVal),
    /// `len()` 与 `list()` 在执行时才做（3.x `MergeOp.execute_multi` / `_merge_weighted`）。
    Weighted(PyVal),
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

/// 候选行要从 entity 取值的非虚拟名字：分组字段与 decay 的输入字段。
fn tail_names(tail: &Tail, decay_input: Option<&str>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in tail
        .group
        .iter()
        .map(|(field, _)| field.as_str())
        .chain(decay_input)
    {
        if !is_virtual(name) && !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
    }
    names
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
        PyVal::Bytes(bytes) => !bytes.is_empty(),
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
                (Merge::Rrf(PyVal::Float(*k)), false, false, None)
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
                (
                    Merge::Weighted(PyVal::List(
                        weights.iter().map(|w| PyVal::Float(*w)).collect(),
                    )),
                    *normalize,
                    false,
                    None,
                )
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
        let names = tail_names(&tail, decay.as_ref().map(|(input, _)| input.as_str()));
        Ok(Self {
            merge,
            norms,
            descending,
            decay,
            tail,
            names,
        })
    }

    /// HybridSearch 的 `rank_params`（3.x `build_hybrid_rerank_chain`，检索之后才构造）：`strategy` 与
    /// `params` 是 `json.loads` 的原值，缺省分别为 `"rrf"` 与 `{}`。`k` / `weights` / `norm_score` 不校验，
    /// 原值参与运算（设计第 3 节）。
    pub(crate) fn rank_params(
        strategy: &PyVal,
        params: &PyVal,
        metrics: &[Metric],
        tail: Tail,
    ) -> Result<Self> {
        let get = |key: &str| -> Result<Option<PyVal>> {
            match params {
                PyVal::Dict(items) => {
                    Ok(items.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()))
                }
                other => Err(invalid(format!(
                    "{} object has no attribute 'get'",
                    py_repr(other.type_name())
                ))),
            }
        };
        let (merge, descending, norms) = match strategy {
            PyVal::Str(s) if s == "rrf" => (
                Merge::Rrf(get("k")?.unwrap_or(PyVal::Float(60.0))),
                true,
                Vec::new(),
            ),
            PyVal::Str(s) if s == "weighted" => {
                let weights = get("weights")?.unwrap_or(PyVal::List(Vec::new()));
                let normalize = get("norm_score")?.is_some_and(|v| truthy(&v));
                let (descending, norms) = resolve(normalize, false, metrics);
                (Merge::Weighted(weights), descending, norms)
            }
            other => {
                return Err(invalid(format!(
                    "Unsupported hybrid rerank strategy: {}",
                    other.repr()
                )));
            }
        };
        Ok(Self {
            merge,
            norms,
            descending,
            decay: None,
            names: tail_names(&tail, None),
            tail,
        })
    }

    /// 3.x `MergeOp.execute_multi` 在逐个查询合并之前的检查：`nqs` 是每一路的查询数。
    pub(crate) fn check_inputs(&self, nqs: &[usize]) -> Result<()> {
        let Some(&first) = nqs.first() else {
            return Err(invalid("MergeOp requires at least one input"));
        };
        if let Some((i, n)) = nqs.iter().enumerate().skip(1).find(|(_, n)| **n != first) {
            return Err(invalid(format!(
                "MergeOp requires all inputs to have the same number of chunks: input 0 has {first}, input {i} has {n}"
            )));
        }
        if let Merge::Weighted(weights) = &self.merge {
            let len = py_len(weights)?;
            if len != nqs.len() {
                return Err(invalid(format!(
                    "weighted merge requires {} weights, got {len}",
                    nqs.len()
                )));
            }
        }
        Ok(())
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
                        let rrf = rrf_term(k, rank)?;
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
                let weights = py_list(weights);
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
                            Some(norm) => py_mul(&weights[r], &PyVal::Float(norm.apply(score)?))?,
                            None => py_mul(&weights[r], score)?,
                        };
                        total = add_to_float(total, &value)?;
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
    // 3.x 先检查这一行的每个值（分数与因子）都是有限数，再相乘；NaN 因子报的是这一条。
    if !factor.is_finite() {
        return Err(not_numeric());
    }
    let result = 1.0 * value * factor;
    if !result.is_finite() {
        return Err(invalid("num_combine produced a non-finite result"));
    }
    Ok(result)
}

/// 3.x RRF 的一项 `1.0 / (k + rank + 1)`：整数 `k` 先精确相加再转浮点，浮点 `k` 逐步相加；
/// 其他类型是 Python `+` 的异常。
fn rrf_term(k: &PyVal, rank: usize) -> Result<f64> {
    if let Some(k) = Wide::of(k) {
        let sum = k.add(&Wide::parse(&(rank as u64 + 1).to_string()));
        if sum.digits.is_empty() {
            return Err(invalid("float division by zero"));
        }
        return Ok(1.0 / sum.to_float()?);
    }
    let denominator = match k {
        PyVal::Float(k) => *k + rank as f64 + 1.0,
        PyVal::Str(_) => return Err(invalid("can only concatenate str (not \"int\") to str")),
        PyVal::List(_) => return Err(invalid("can only concatenate list (not \"int\") to list")),
        other => {
            return Err(invalid(format!(
                "unsupported operand type(s) for +: {} and 'int'",
                py_repr(other.type_name())
            )));
        }
    };
    if denominator == 0.0 {
        return Err(invalid("float division by zero"));
    }
    Ok(1.0 / denominator)
}

/// Python `len(v)`。
fn py_len(value: &PyVal) -> Result<usize> {
    match value {
        PyVal::List(items) => Ok(items.len()),
        PyVal::Dict(items) => Ok(items.len()),
        PyVal::Str(s) => Ok(s.chars().count()),
        other => Err(invalid(format!(
            "object of type {} has no len()",
            py_repr(other.type_name())
        ))),
    }
}

/// Python `list(v)`：字符串拆成字符，dict 取键；只在 [`py_len`] 通过之后调用。
fn py_list(value: &PyVal) -> Vec<PyVal> {
    match value {
        PyVal::List(items) => items.clone(),
        PyVal::Dict(items) => items.iter().map(|(k, _)| PyVal::Str(k.clone())).collect(),
        PyVal::Str(s) => s.chars().map(|c| PyVal::Str(c.to_string())).collect(),
        _ => Vec::new(),
    }
}

fn is_int(value: &PyVal) -> bool {
    matches!(value, PyVal::Bool(_) | PyVal::Int(_) | PyVal::BigInt(_))
}

fn is_sequence(value: &PyVal) -> bool {
    matches!(value, PyVal::Str(_) | PyVal::List(_))
}

/// Python `a * b`（weighted 的权重乘分数）。整数相乘是精确的 Python 整数；序列乘整数是重复，
/// 结果仍是同类型的序列，内容这里用不到（下一步与浮点相加必然报错），所以返回空序列。
fn py_mul(a: &PyVal, b: &PyVal) -> Result<PyVal> {
    let non_int = |other: &PyVal| {
        invalid(format!(
            "can't multiply sequence by non-int of type {}",
            py_repr(other.type_name())
        ))
    };
    if let (Some(x), Some(y)) = (Wide::of(a), Wide::of(b)) {
        return Ok(x.mul(&y).into_pyval());
    }
    match (a, b) {
        (PyVal::Float(x), PyVal::Float(y)) => Ok(PyVal::Float(x * y)),
        (PyVal::Float(x), y) if is_int(y) => Ok(PyVal::Float(x * y.to_float()?)),
        (x, PyVal::Float(y)) if is_int(x) => Ok(PyVal::Float(x.to_float()? * y)),
        (s, n) | (n, s) if is_sequence(s) && is_int(n) => Ok(match s {
            PyVal::Str(_) => PyVal::Str(String::new()),
            _ => PyVal::List(Vec::new()),
        }),
        (s, other) if is_sequence(s) => Err(non_int(other)),
        (other, s) if is_sequence(s) => Err(non_int(other)),
        _ => Err(invalid(format!(
            "unsupported operand type(s) for *: {} and {}",
            py_repr(a.type_name()),
            py_repr(b.type_name())
        ))),
    }
}

/// Python 的 `total += value`，`total` 是浮点。
fn add_to_float(total: f64, value: &PyVal) -> Result<f64> {
    match value {
        PyVal::Float(f) => Ok(total + f),
        v if is_int(v) => Ok(total + v.to_float()?),
        other => Err(invalid(format!(
            "unsupported operand type(s) for +=: 'float' and {}",
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

/// Python 整数（任意精度）：符号加 10⁹ 进制的小端绝对值，零是空且非负。只做 `sum` 与 `/` 用到的运算。
#[derive(Debug, Clone, PartialEq)]
struct Wide {
    negative: bool,
    digits: Vec<u32>,
}

const WIDE_BASE: u64 = 1_000_000_000;

impl Wide {
    fn of(value: &PyVal) -> Option<Self> {
        match value {
            PyVal::Bool(b) => Some(Self::parse(if *b { "1" } else { "0" })),
            PyVal::Int(i) => Some(Self::parse(&i.to_string())),
            PyVal::BigInt(text) => Some(Self::parse(text)),
            _ => None,
        }
    }

    /// 十进制文本（可带符号）；`BigInt` 的文本来自 JSON 整数或本类型的输出，总是合法。
    fn parse(text: &str) -> Self {
        let (negative, body) = match text.as_bytes().first() {
            Some(b'-') => (true, &text[1..]),
            Some(b'+') => (false, &text[1..]),
            _ => (false, text),
        };
        let bytes = body.as_bytes();
        let mut digits = Vec::with_capacity(bytes.len() / 9 + 1);
        let mut end = bytes.len();
        while end > 0 {
            let start = end.saturating_sub(9);
            let chunk = bytes[start..end]
                .iter()
                .fold(0u32, |acc, b| acc * 10 + u32::from(b.wrapping_sub(b'0')));
            digits.push(chunk);
            end = start;
        }
        Self::normalized(negative, digits)
    }

    fn normalized(negative: bool, mut digits: Vec<u32>) -> Self {
        while digits.last() == Some(&0) {
            digits.pop();
        }
        let negative = negative && !digits.is_empty();
        Self { negative, digits }
    }

    fn magnitude_cmp(a: &[u32], b: &[u32]) -> Ordering {
        a.len()
            .cmp(&b.len())
            .then_with(|| a.iter().rev().cmp(b.iter().rev()))
    }

    fn add(&self, other: &Self) -> Self {
        let (a, b) = (&self.digits, &other.digits);
        if self.negative == other.negative {
            let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
            let mut carry = 0u64;
            for i in 0..a.len().max(b.len()) {
                let sum =
                    u64::from(*a.get(i).unwrap_or(&0)) + u64::from(*b.get(i).unwrap_or(&0)) + carry;
                out.push((sum % WIDE_BASE) as u32);
                carry = sum / WIDE_BASE;
            }
            if carry > 0 {
                out.push(carry as u32);
            }
            return Self::normalized(self.negative, out);
        }
        // 符号不同：大减小，符号随绝对值大的一方。
        let (big, small, negative) = match Self::magnitude_cmp(a, b) {
            Ordering::Less => (b, a, other.negative),
            _ => (a, b, self.negative),
        };
        let mut out = Vec::with_capacity(big.len());
        let mut borrow = 0i64;
        for (i, digit) in big.iter().enumerate() {
            let mut d = i64::from(*digit) - i64::from(*small.get(i).unwrap_or(&0)) - borrow;
            borrow = i64::from(d < 0);
            if d < 0 {
                d += WIDE_BASE as i64;
            }
            out.push(d as u32);
        }
        Self::normalized(negative, out)
    }

    fn mul(&self, other: &Self) -> Self {
        let mut out = vec![0u64; self.digits.len() + other.digits.len() + 1];
        for (i, a) in self.digits.iter().enumerate() {
            let mut carry = 0u64;
            for (j, b) in other.digits.iter().enumerate() {
                let cell = out[i + j] + u64::from(*a) * u64::from(*b) + carry;
                out[i + j] = cell % WIDE_BASE;
                carry = cell / WIDE_BASE;
            }
            let mut k = i + other.digits.len();
            while carry > 0 {
                let cell = out[k] + carry;
                out[k] = cell % WIDE_BASE;
                carry = cell / WIDE_BASE;
                k += 1;
            }
        }
        Self::normalized(
            self.negative != other.negative,
            out.into_iter().map(|d| d as u32).collect(),
        )
    }

    fn text(&self) -> String {
        let Some((top, rest)) = self.digits.split_last() else {
            return "0".to_owned();
        };
        let mut out = String::with_capacity(self.digits.len() * 9 + 1);
        if self.negative {
            out.push('-');
        }
        out.push_str(&top.to_string());
        for chunk in rest.iter().rev() {
            out.push_str(&format!("{chunk:09}"));
        }
        out
    }

    fn into_pyval(self) -> PyVal {
        let text = self.text();
        text.parse()
            .map_or_else(|_| PyVal::BigInt(text), PyVal::Int)
    }

    /// Python `float(int)`：正确舍入，舍入后超出 float64 是 OverflowError。
    fn to_float(&self) -> Result<f64> {
        PyVal::BigInt(self.text()).to_float()
    }

    /// Python `int / n` 的真除法：商正确舍入（不是先转浮点再除），超出 float64 是 OverflowError。
    fn true_divide(&self, n: u64) -> Result<f64> {
        let n = u128::from(n);
        let mut quotient = vec![0u32; self.digits.len()];
        let mut remainder = 0u128;
        for (i, d) in self.digits.iter().enumerate().rev() {
            let current = remainder * u128::from(WIDE_BASE) + u128::from(*d);
            // 商的这一位小于 10⁹：remainder < n，所以 current < n·10⁹。
            quotient[i] = (current / n) as u32;
            remainder = current % n;
        }
        let mut text = Self::normalized(false, quotient).text();
        if self.negative {
            text.insert(0, '-');
        }
        // 小数部分写到 400 位，余数仍非零时再补一位 1 作为粘滞位：被除数在 float64 范围内时，相邻浮点之间的
        // 中点最多有几十位小数，截断再加粘滞位不会越过中点，所以解析结果就是正确舍入的商。
        if remainder != 0 {
            text.push('.');
            for _ in 0..400 {
                remainder *= 10;
                text.push(char::from(b'0' + (remainder / n) as u8));
                remainder %= n;
            }
            if remainder != 0 {
                text.push('1');
            }
        }
        text.parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| invalid("integer division result too large for a float"))
    }
}

/// `sum` 的累加结果：Python 的 `int` 或 `float`。
enum Total {
    Int(Wide),
    Float(f64),
}

impl Total {
    fn type_name(&self) -> &'static str {
        match self {
            Self::Int(_) => "int",
            Self::Float(_) => "float",
        }
    }

    /// Python 的 `total + item`（通用路径，不补偿）。
    fn add(self, item: &PyVal) -> Result<Self> {
        let unsupported = |left: &str| {
            invalid(format!(
                "unsupported operand type(s) for +: {} and {}",
                py_repr(left),
                py_repr(item.type_name())
            ))
        };
        match (self, item) {
            (Self::Float(f), PyVal::Float(x)) => Ok(Self::Float(f + x)),
            (Self::Int(n), PyVal::Float(x)) => Ok(Self::Float(n.to_float()? + x)),
            (total, other) => match (total, Wide::of(other)) {
                (Self::Int(n), Some(b)) => Ok(Self::Int(n.add(&b))),
                (Self::Float(f), Some(b)) => Ok(Self::Float(f + b.to_float()?)),
                (total, None) => Err(unsupported(total.type_name())),
            },
        }
    }
}

/// `PyLong_AsLongAndOverflow`：int 与 bool 且在 C long（64 位平台为 i64）范围内。
fn small_int(value: &PyVal) -> Option<i64> {
    match value {
        PyVal::Bool(b) => Some(i64::from(*b)),
        PyVal::Int(i) => Some(*i),
        PyVal::BigInt(text) => text.parse().ok(),
        _ => None,
    }
}

/// Python `sum(values)`（起始值是整数 0），照 CPython 3.12 `builtin_sum_impl` 的三段：
/// 1. 整数快路径：在 i64 内累加；遇到非整数或溢出时，用 Python 加法并入这一项后离开。
/// 2. 结果是浮点时进入浮点路径：浮点按 Neumaier 补偿求和，i64 范围内的整数直接加、**不补偿**；
///    其他值先把补偿加回，再用 Python 加法并入后离开。
/// 3. 通用路径：逐项 Python 加法，整数精确、浮点不补偿。
///
/// 所以宽整数相加是精确的 Python 整数（保留符号），之后再遇到浮点才转换。
fn py_sum(values: &[PyVal]) -> Result<PyVal> {
    let mut items = values.iter();
    let mut small = 0i64;
    let mut total = loop {
        let Some(item) = items.next() else {
            return Ok(PyVal::Int(small));
        };
        if let Some(sum) = small_int(item).and_then(|b| small.checked_add(b)) {
            small = sum;
            continue;
        }
        break Total::Int(Wide::parse(&small.to_string())).add(item)?;
    };
    if let Total::Float(start) = total {
        let (mut sum, mut compensation) = (start, 0.0_f64);
        let settle = |sum: f64, compensation: f64| {
            if compensation != 0.0 && compensation.is_finite() {
                sum + compensation
            } else {
                sum
            }
        };
        total = loop {
            let Some(item) = items.next() else {
                return Ok(PyVal::Float(settle(sum, compensation)));
            };
            if let PyVal::Float(x) = item {
                let next = sum + x;
                if sum.abs() >= x.abs() {
                    compensation += (sum - next) + x;
                } else {
                    compensation += (x - next) + sum;
                }
                sum = next;
                continue;
            }
            if let Some(value) = small_int(item) {
                sum += value as f64;
                continue;
            }
            break Total::Float(settle(sum, compensation)).add(item)?;
        };
    }
    for item in items {
        total = total.add(item)?;
    }
    Ok(match total {
        Total::Int(n) => n.into_pyval(),
        Total::Float(f) => PyVal::Float(f),
    })
}

/// `sum(...) / len(...)`：Python 的真除法；整数被除数按整数真除法正确舍入。
fn py_divide(total: &PyVal, n: usize) -> Result<PyVal> {
    if let PyVal::Float(f) = total {
        return Ok(PyVal::Float(f / n as f64));
    }
    let Some(wide) = Wide::of(total) else {
        return Err(invalid(format!(
            "unsupported operand type(s) for /: {} and 'int'",
            py_repr(total.type_name())
        )));
    };
    Ok(PyVal::Float(wide.true_divide(n as u64)?))
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
    Bytes(Vec<u8>),
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
            PyVal::Bytes(bytes) => Self::Bytes(bytes.clone()),
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
        let c = chain(Merge::Rrf(PyVal::Float(1.0)), true, tail(10, 0));
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
            ids(
                &chain(Merge::Rrf(PyVal::Float(60.0)), true, tail(limit, offset))
                    .execute(rows())
                    .unwrap()[0],
            )
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
            Merge::Rrf(PyVal::Float(60.0)),
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
        // 3.x `_positively_related` 含 BM25：与 IP 同向、不混合；归一化用 `2·atan(x)/π`。
        assert_eq!(resolve(false, false, &[Bm25, Ip]), (true, vec![None, None]));
        assert_eq!(
            resolve(false, false, &[Bm25, L2]),
            (true, vec![None, Some(Norm::Distance)])
        );
        assert_eq!(
            resolve(true, false, &[Bm25]),
            (true, vec![Some(Norm::Bm25)])
        );
        assert_eq!(Norm::Bm25.apply(&PyVal::Float(1.0)).unwrap(), 0.5);
        assert_eq!(Norm::Bm25.apply(&PyVal::Float(0.0)).unwrap(), 0.0);
    }

    #[test]
    fn weighted_skips_routes_whose_score_is_none() {
        let mut c = chain(
            Merge::Weighted(PyVal::List(vec![PyVal::Float(0.5)])),
            true,
            tail(10, 0),
        );
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

    /// 期望值取自 CPython 3.12.14（3.x 参照环境）的 `sum(v)` 与 `sum(v) / len(v)`。
    #[test]
    fn sum_and_average_follow_cpython() {
        let big = |text: &str| PyVal::BigInt(text.to_owned());
        let p64 = || big("18446744073709551616");
        let n64 = || big("-18446744073709551616");
        let huge = || big(&format!("1{}", "0".repeat(400)));
        let neg_huge = || big(&format!("-1{}", "0".repeat(400)));
        let ok = |values: &[PyVal]| py_sum(values).unwrap();
        let err = |values: &[PyVal]| py_sum(values).unwrap_err().to_string();
        let f = PyVal::Float;

        // 多个宽整数：精确求和、保留符号。
        assert_eq!(ok(&[p64(), p64()]), big("36893488147419103232"));
        assert_eq!(ok(&[n64(), n64()]), big("-36893488147419103232"));
        assert_eq!(ok(&[huge(), neg_huge()]), PyVal::Int(0));
        assert_eq!(
            ok(&[huge(), PyVal::Int(1)]),
            big(&format!("1{}1", "0".repeat(399)))
        );
        assert_eq!(ok(&[PyVal::Bool(true), p64()]), big("18446744073709551617"));
        assert_eq!(ok(&[n64(), p64(), PyVal::Int(-5)]), PyVal::Int(-5));
        // 溢出 i64 后走通用路径：浮点不再补偿。
        assert_eq!(ok(&[p64(), f(0.5), f(1e-30), n64()]), f(0.0));
        assert_eq!(
            ok(&[f(0.5), p64(), f(1e-30), f(1e-30)]),
            f(1.8446744073709552e19)
        );
        assert_eq!(
            ok(&[PyVal::Int(i64::MAX), PyVal::Int(1), f(0.1)]),
            f(9.223372036854776e18)
        );
        assert_eq!(ok(&[PyVal::Int(i64::MAX), f(1.5)]), f(9.223372036854776e18));
        // 浮点路径里 i64 范围内的整数直接加，不补偿。
        assert_eq!(ok(&[f(1e16), PyVal::Int(1), f(1.0)]), f(1e16));
        assert_eq!(ok(&[f(1e16), f(1.0), f(1.0)]), f(1.0000000000000002e16));
        assert_eq!(
            ok(&[f(0.1), PyVal::Int(2), f(0.2), PyVal::Bool(true), f(0.3)]),
            f(3.6)
        );
        assert_eq!(ok(&[f(-0.0)]), f(0.0));
        // 转不成 float64 的整数遇到浮点。
        assert_eq!(
            err(&[huge(), f(0.5)]),
            invalid("int too large to convert to float").to_string()
        );
        assert_eq!(
            err(&[f(0.5), huge()]),
            invalid("int too large to convert to float").to_string()
        );
        // 类型错误的左操作数是当时的累加类型。
        let unsupported = |left: &str, right: &str| {
            invalid(format!(
                "unsupported operand type(s) for +: '{left}' and '{right}'"
            ))
            .to_string()
        };
        assert_eq!(
            err(&[p64(), PyVal::Str("a".into())]),
            unsupported("int", "str")
        );
        assert_eq!(
            err(&[f(0.5), PyVal::Str("a".into())]),
            unsupported("float", "str")
        );
        assert_eq!(err(&[PyVal::None]), unsupported("int", "NoneType"));
        assert_eq!(
            err(&[f(0.5), p64(), PyVal::None]),
            unsupported("float", "NoneType")
        );

        // 平均：整数的真除法正确舍入，不是 float(sum) / n。
        let avg = |values: Vec<PyVal>| {
            let n = values.len();
            py_divide(&py_sum(&values)?, n)
        };
        assert_eq!(
            avg(vec![
                big("23086038869017488826317"),
                big("3771551294065249164509"),
                big("36858321200630379061502"),
            ])
            .unwrap(),
            f(2.1238637121237707e22)
        );
        let e309 = || big(&format!("1{}", "0".repeat(309)));
        assert_eq!(
            avg(vec![e309(), e309()]).unwrap_err().to_string(),
            invalid("integer division result too large for a float").to_string()
        );
        assert_eq!(
            avg(vec![e309(), big(&format!("-9{}", "0".repeat(308)))]).unwrap(),
            f(5e307)
        );
        assert_eq!(
            avg(vec![n64(), n64(), PyVal::Int(1)]).unwrap(),
            f(-1.2297829382473034e19)
        );
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
            py_mul(&PyVal::Float(0.5), &PyVal::Str("a".into()))
                .unwrap_err()
                .to_string()
                .contains("can't multiply sequence")
        );
        assert!(
            py_mul(&PyVal::Float(0.5), &PyVal::None)
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
        // 因子非有限时报行值检查（3.x NumCombineExpr），分数为 None 时仍是 0。
        for factor in [f64::NAN, f64::INFINITY] {
            assert!(
                combine(&PyVal::Float(0.5), factor)
                    .unwrap_err()
                    .to_string()
                    .contains("row values must be finite numeric non-bool values")
            );
            assert_eq!(combine(&PyVal::None, factor).unwrap(), 0.0);
        }
    }
}
