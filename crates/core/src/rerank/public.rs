//! 公共 function chain（0035）：与协议无关的链表示，以及按 schema 的校验。
//!
//! 对应 3.x 的 `function/repr.py` 与 `function/validator.py`。校验的先后与文案逐条照 3.x：
//! 每个算子的错误带 `function chain op[i]` 前缀，表达式内部的错误再带 `expression <名字>`。
//!
//! 执行（[`Plan`]）对应 3.x 的 `function/compiler.py`、`ops/` 与 `expr/`：一个算子对全部查询做完才进入
//! 下一个算子，运行期错误的先后因此与 3.x 相同。行里的值是 Python 值，算术与比较按 Python 的规则。

use std::cmp::Ordering;

use super::chain::{Curve, Function as DecayFunction, py_divide, py_sum};
use super::pyval::{PyVal, py_round};
use super::try_sort_by;
use crate::error::{EngineError, Result, py_repr};
use crate::schema::{CollectionSchema, DataType};

const ID_FIELD: &str = "$id";
const SCORE_FIELD: &str = "$score";
const L2_RERANK_STAGE: &str = "FunctionChainStageL2Rerank";

/// 解码后的参数值（3.x `decode_function_param_value` 的结果）。
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Array(Vec<Param>),
    Object(Vec<(String, Param)>),
}

impl Param {
    /// Python 的 `not isinstance(v, bool) and isinstance(v, (int, float))`。
    fn is_numeric(&self) -> bool {
        matches!(self, Self::Int(_) | Self::Float(_))
    }

    fn is_finite_numeric(&self) -> bool {
        match self {
            Self::Int(_) => true,
            Self::Float(f) => f.is_finite(),
            _ => false,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            // 与 Python 的 int / float 比较一致：这里只用于同 0、1 比较，int64 转 f64 的舍入不改变结果。
            Self::Int(i) => Some(*i as f64),
            Self::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// 3.x 把链表示冻结后（list → tuple、dict → mappingproxy）的 `type(v).__name__`。
    fn frozen_type_name(&self) -> &'static str {
        match self {
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::Float(_) => "float",
            Self::Str(_) => "str",
            Self::Bytes(_) => "bytes",
            Self::Array(_) => "tuple",
            Self::Object(_) => "mappingproxy",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    Column(String),
    Literal(Param),
}

/// 参数表。键唯一（来自 proto 的 map）。
pub type Params = Vec<(String, Param)>;

#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub name: String,
    pub args: Vec<Arg>,
    pub params: Params,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Op {
    pub op: String,
    pub expr: Option<Expr>,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub params: Params,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChainRepr {
    /// proto 枚举成员名，如 `FunctionChainStageL2Rerank`。
    pub stage: String,
    pub ops: Vec<Op>,
}

/// 通过校验的链。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedChain {
    pub repr: ChainRepr,
    /// 链读到的 schema 字段，按首次出现的顺序；不含 `$score`、`$id`、主键名与临时列。
    pub required_fields: Vec<String>,
    /// 链里有 `rerank_model`（0035 决策 B：通过校验后不执行）。
    pub uses_rerank_model: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Numeric,
    Text,
    Bool,
    Timestamp,
}

fn field_kind(data_type: DataType) -> Option<Kind> {
    Some(match data_type {
        DataType::Bool => Kind::Bool,
        DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::Float
        | DataType::Double => Kind::Numeric,
        DataType::VarChar => Kind::Text,
        DataType::Timestamptz => Kind::Timestamp,
        DataType::Json
        | DataType::Array
        | DataType::Geometry
        | DataType::FloatVector
        | DataType::SparseFloatVector => return None,
    })
}

/// 3.x `DataType.X.value`。
fn dtype_value(data_type: DataType) -> String {
    data_type.name().to_ascii_lowercase()
}

fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(EngineError::SchemaValidation(message.into()))
}

/// 3.x `_fail_with_context`：只给校验错误加前缀。
fn context<T>(prefix: &str, result: Result<T>) -> Result<T> {
    result.map_err(|error| match error {
        EngineError::SchemaValidation(message) => {
            EngineError::SchemaValidation(format!("{prefix}: {message}"))
        }
        other => other,
    })
}

fn get<'a>(params: &'a Params, key: &str) -> Option<&'a Param> {
    params.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// 名字到种类的表。主键的种类可能没有（3.x 的 `_FIELD_KINDS.get` 返回 None），这时它与没登记的名字一样
/// 继续往下解析。
struct Symbols<'a> {
    schema: &'a CollectionSchema,
    known: Vec<(String, Kind)>,
    required: Vec<String>,
}

impl Symbols<'_> {
    fn resolve(&mut self, name: &str) -> Result<Kind> {
        if let Some((_, kind)) = self.known.iter().find(|(n, _)| n == name) {
            return Ok(*kind);
        }
        if name.starts_with('$') {
            return fail(format!(
                "system input {} is not supported by L2 rerank function chain",
                py_repr(name)
            ));
        }
        let Some(field) = self.schema.field(name) else {
            return fail(format!(
                "function chain input {} is neither a previous output nor a collection field",
                py_repr(name)
            ));
        };
        let Some(kind) = field_kind(field.data_type) else {
            return fail(format!(
                "function chain input field {} has unsupported type {}",
                py_repr(name),
                dtype_value(field.data_type)
            ));
        };
        self.set(name, kind);
        if !self.required.iter().any(|n| n == name) {
            self.required.push(name.to_owned());
        }
        Ok(kind)
    }

    fn set(&mut self, name: &str, kind: Kind) {
        match self.known.iter_mut().find(|(n, _)| n == name) {
            Some(entry) => entry.1 = kind,
            None => self.known.push((name.to_owned(), kind)),
        }
    }
}

fn literal_kind(value: &Param) -> Result<Kind> {
    match value {
        Param::Bool(_) => Ok(Kind::Bool),
        Param::Int(_) => Ok(Kind::Numeric),
        Param::Float(f) if f.is_finite() => Ok(Kind::Numeric),
        Param::Float(_) => fail("numeric literal must be finite"),
        Param::Str(_) => Ok(Kind::Text),
        other => fail(format!(
            "unsupported function chain literal type: {}",
            other.frozen_type_name()
        )),
    }
}

fn validate_map(op: &Op) -> Result<&Expr> {
    let Some(expr) = &op.expr else {
        return fail("map operator requires an expression");
    };
    if !op.inputs.is_empty() {
        return fail("map operator must not declare op.inputs");
    }
    if op.outputs.len() != 1 {
        return fail("map operator requires exactly one output");
    }
    Ok(expr)
}

fn non_empty_string(value: Option<&Param>) -> Option<&str> {
    match value {
        Some(Param::Str(s)) if !s.is_empty() => Some(s),
        _ => None,
    }
}

fn validate_sort(op: &Op) -> Result<()> {
    if op.expr.is_some() || !op.outputs.is_empty() {
        return fail("sort operator must not contain expression or outputs");
    }
    let Some(column) = non_empty_string(get(&op.params, "column")) else {
        return fail("sort column must be a non-empty string");
    };
    if !matches!(get(&op.params, "desc"), None | Some(Param::Bool(_))) {
        return fail("sort desc must be a boolean");
    }
    let tie_break = match get(&op.params, "tie_break_col") {
        None => None,
        Some(value) => match non_empty_string(Some(value)) {
            Some(name) => Some(name),
            None => return fail("sort tie_break_col must be a non-empty string"),
        },
    };
    let expected: Vec<&str> = std::iter::once(column).chain(tie_break).collect();
    if op.inputs.iter().map(String::as_str).ne(expected) {
        return fail("sort inputs must match column and tie_break_col");
    }
    Ok(())
}

fn validate_limit(op: &Op) -> Result<()> {
    if op.expr.is_some() || !op.inputs.is_empty() || !op.outputs.is_empty() {
        return fail("limit operator must not contain expression, inputs, or outputs");
    }
    if !matches!(get(&op.params, "limit"), Some(Param::Int(n)) if *n > 0) {
        return fail("function chain limit must be a positive integer");
    }
    if !matches!(get(&op.params, "offset"), None | Some(Param::Int(0..))) {
        return fail("function chain offset must be a non-negative integer");
    }
    Ok(())
}

/// Python 的 `isinstance(v, Sequence) and not isinstance(v, (str, bytes, bytearray))`，作用在冻结后的值上。
fn sequence(value: Option<&Param>) -> Option<&[Param]> {
    match value {
        Some(Param::Array(items)) => Some(items),
        _ => None,
    }
}

fn validate_num_combine(expr: &Expr, kinds: &[Kind]) -> Result<Kind> {
    if kinds.len() < 2 || kinds.iter().any(|k| *k != Kind::Numeric) {
        return fail("num_combine requires at least two numeric arguments");
    }
    let mode = match get(&expr.params, "mode") {
        None => "sum",
        Some(Param::Str(mode)) => mode,
        Some(_) => return fail("num_combine mode must be a string"),
    };
    if !matches!(
        mode,
        "multiply" | "sum" | "max" | "min" | "avg" | "weighted"
    ) {
        return fail(format!("unsupported num_combine mode: {mode}"));
    }
    let weights = get(&expr.params, "weights");
    if mode == "weighted" {
        let Some(weights) = sequence(weights).filter(|w| w.len() == kinds.len()) else {
            return fail(format!(
                "weighted num_combine requires {} weights",
                kinds.len()
            ));
        };
        if weights.iter().any(|w| !w.is_numeric()) {
            return fail("num_combine weights must be numeric");
        }
        if weights.iter().any(|w| !w.is_finite_numeric()) {
            return fail("num_combine weights must be finite");
        }
    } else if weights.is_some() {
        return fail("num_combine weights require mode='weighted'");
    }
    Ok(Kind::Numeric)
}

fn validate_decay(expr: &Expr, kinds: &[Kind]) -> Result<Kind> {
    if !matches!(kinds, [Kind::Numeric | Kind::Timestamp]) {
        return fail("decay requires exactly one numeric or timestamp argument");
    }
    let function = match get(&expr.params, "function") {
        Some(Param::Str(function)) => function,
        _ => return fail("decay function must be a string"),
    };
    if !matches!(function.as_str(), "gauss" | "exp" | "linear") {
        return fail(format!("unsupported decay function: {function}"));
    }
    let origin = get(&expr.params, "origin");
    let scale = get(&expr.params, "scale");
    let offset = get(&expr.params, "offset").unwrap_or(&Param::Int(0));
    let decay = get(&expr.params, "decay").unwrap_or(&Param::Float(0.5));
    let (Some(origin), Some(scale)) = (origin, scale) else {
        return fail("decay parameters must be numeric");
    };
    let values = [origin, scale, offset, decay];
    if values.iter().any(|v| !v.is_numeric()) {
        return fail("decay parameters must be numeric");
    }
    if values.iter().any(|v| !v.is_finite_numeric()) {
        return fail("decay parameters must be finite");
    }
    let number = |v: &Param| v.as_f64().unwrap_or(f64::NAN);
    let (scale, offset, decay) = (number(scale), number(offset), number(decay));
    if !(scale > 0.0 && offset >= 0.0 && 0.0 < decay && decay < 1.0) {
        return fail("decay requires scale > 0, offset >= 0, and 0 < decay < 1");
    }
    Ok(Kind::Numeric)
}

fn validate_round_decimal(expr: &Expr, kinds: &[Kind]) -> Result<Kind> {
    if kinds != [Kind::Numeric] {
        return fail("round_decimal requires exactly one numeric argument");
    }
    if !matches!(get(&expr.params, "decimal"), Some(Param::Int(0..=6))) {
        return fail("round_decimal decimal must be an integer in [0, 6]");
    }
    Ok(Kind::Numeric)
}

/// 3.x `rerank/factory.py` 里 cohere 接受的参数名。
const COHERE_PARAMS: [&str; 4] = ["provider", "model_name", "api_key", "base_url"];

fn validate_rerank_model(expr: &Expr, kinds: &[Kind], num_queries: usize) -> Result<Kind> {
    if kinds != [Kind::Text] {
        return fail("rerank_model requires exactly one text argument");
    }
    let lowered: Vec<String> = expr.params.iter().map(|(k, _)| k.to_lowercase()).collect();
    if lowered.iter().any(|k| k == "base_url") {
        return fail("rerank_model endpoint must be configured on the server");
    }
    if lowered
        .iter()
        .any(|k| matches!(k.as_str(), "api_key" | "token" | "secret"))
    {
        return fail("rerank_model credentials must be configured on the server");
    }
    let queries = sequence(get(&expr.params, "queries")).filter(|queries| {
        !queries.is_empty()
            && queries
                .iter()
                .all(|q| matches!(q, Param::Str(s) if !s.is_empty()))
    });
    let Some(queries) = queries else {
        return fail("rerank_model queries must be a non-empty sequence of strings");
    };
    if queries.len() != num_queries {
        return fail(format!(
            "rerank_model query count {} does not match search nq {num_queries}",
            queries.len()
        ));
    }
    let Some(provider) = non_empty_string(get(&expr.params, "provider")) else {
        return fail("rerank_model provider must be a non-empty string");
    };
    // 3.x `validate_rerank_provider_params`：ValueError 被转成校验错误。
    let provider = provider.to_lowercase();
    if provider != "cohere" {
        return fail(format!(
            "Unknown rerank provider: {}. Supported: 'cohere'",
            py_repr(&provider)
        ));
    }
    if !matches!(get(&expr.params, "model_name"), None | Some(Param::Str(_))) {
        return fail("Cohere rerank parameter 'model_name' must be a string");
    }
    // 3.x 按 `repr` 排序后报第一个不认识的参数名。
    let unsupported = expr
        .params
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| *k != "queries" && !COHERE_PARAMS.contains(k))
        .map(py_repr)
        .min();
    if let Some(name) = unsupported {
        return fail(format!("unsupported rerank_model parameter {name}"));
    }
    Ok(Kind::Numeric)
}

fn validate_expr(expr: &Expr, kinds: &[Kind], num_queries: usize) -> Result<Kind> {
    match expr.name.as_str() {
        "num_combine" => validate_num_combine(expr, kinds),
        "decay" => validate_decay(expr, kinds),
        "round_decimal" => validate_round_decimal(expr, kinds),
        "rerank_model" => validate_rerank_model(expr, kinds, num_queries),
        name => fail(format!("unsupported function chain expression: {name}")),
    }
}

/// 3.x `validate_function_chain`。`num_queries` 是请求的 nq。
pub fn validate(
    chain: ChainRepr,
    schema: &CollectionSchema,
    num_queries: usize,
) -> Result<ValidatedChain> {
    if chain.stage != L2_RERANK_STAGE {
        return fail(format!(
            "function chain stage {} is not supported in search request",
            chain.stage
        ));
    }
    if chain.ops.is_empty() {
        return fail("function chain must contain at least one operator");
    }
    let Some(pk) = schema.primary_position().map(|i| &schema.fields[i]) else {
        return fail("function chain schema has no primary key field");
    };
    if pk.name == ID_FIELD || pk.name == SCORE_FIELD {
        let name = py_repr(&pk.name);
        return fail(format!(
            "function chain primary key field {name} conflicts with reserved system column {name}"
        ));
    }
    let mut symbols = Symbols {
        schema,
        known: vec![(SCORE_FIELD.to_owned(), Kind::Numeric)],
        required: Vec::new(),
    };
    if let Some(kind) = field_kind(pk.data_type) {
        symbols.set(ID_FIELD, kind);
        symbols.set(&pk.name, kind);
    }
    let mut uses_rerank_model = false;

    for (index, op) in chain.ops.iter().enumerate() {
        let op_context = format!("function chain op[{index}]");
        match op.op.as_str() {
            "map" => {
                let expr = context(&op_context, validate_map(op))?;
                let expr_context = format!("{op_context} expression {}", expr.name);
                let kinds: Result<Vec<Kind>> = expr
                    .args
                    .iter()
                    .map(|arg| match arg {
                        Arg::Column(name) => symbols.resolve(name),
                        Arg::Literal(value) => literal_kind(value),
                    })
                    .collect();
                let output_kind = context(
                    &expr_context,
                    kinds.and_then(|kinds| validate_expr(expr, &kinds, num_queries)),
                )?;
                let output = &op.outputs[0];
                if output == ID_FIELD || (output.starts_with('$') && output != SCORE_FIELD) {
                    return fail(format!(
                        "{op_context}: system output {} is not writable by L2 rerank function chain",
                        py_repr(output)
                    ));
                }
                if schema.field(output).is_some() {
                    return fail(format!(
                        "{op_context}: function chain output {} conflicts with a collection field",
                        py_repr(output)
                    ));
                }
                symbols.set(output, output_kind);
                uses_rerank_model |= expr.name == "rerank_model";
            }
            "sort" => context(
                &op_context,
                validate_sort(op).and_then(|()| {
                    op.inputs
                        .iter()
                        .try_for_each(|name| symbols.resolve(name).map(drop))
                }),
            )?,
            "limit" => context(&op_context, validate_limit(op))?,
            other => {
                return fail(format!(
                    "{op_context}: unsupported function chain operator: {other}"
                ));
            }
        }
    }

    Ok(ValidatedChain {
        required_fields: symbols.required,
        uses_rerank_model,
        repr: chain,
    })
}

const SCORE_SLOT: usize = 0;
const ID_SLOT: usize = 1;

const ROW_VALUES_ERROR: &str = "num_combine row values must be finite numeric non-bool values";
const NON_FINITE_ERROR: &str = "num_combine produced a non-finite result";

/// 校验保证不会出现的形态；出现说明校验与执行不一致，按内部错误报出而不是 panic。
fn inconsistent(what: &str) -> EngineError {
    EngineError::Storage(format!("validated function chain is inconsistent: {what}"))
}

/// 链执行时的一行（3.x 的行字典）：`$score`、`$id`（主键名是它的别名）、链读到的 schema 字段与临时列。
#[derive(Debug, Clone)]
pub(crate) struct Row {
    /// 调用者给的命中标识，链不解释它。
    pub(crate) source: usize,
    values: Vec<PyVal>,
}

impl Row {
    /// 链结束时的 `$score`。
    pub(crate) fn score(&self) -> &PyVal {
        &self.values[SCORE_SLOT]
    }
}

#[derive(Debug)]
enum Input {
    Slot(usize),
    Literal(PyVal),
}

#[derive(Debug)]
enum Combine {
    Sum,
    Multiply,
    Max,
    Min,
    Avg,
    Weighted(Vec<f64>),
}

#[derive(Debug)]
enum Function {
    Combine(Combine),
    Decay(Curve),
    Round(i64),
}

#[derive(Debug)]
enum Step {
    Map {
        function: Function,
        inputs: Vec<Input>,
        output: usize,
    },
    Sort {
        column: usize,
        descending: bool,
        tie_break: Option<usize>,
    },
    Limit {
        limit: usize,
        offset: usize,
    },
}

/// 编译好的链（3.x `compile_function_chain`）。
#[derive(Debug)]
pub(crate) struct Plan {
    steps: Vec<Step>,
    /// 行里各列的名字：`$score`、`$id`、链读到的 schema 字段，然后是临时列。
    names: Vec<String>,
    fields: usize,
}

fn number(params: &Params, key: &str, default: Option<Param>) -> Result<Param> {
    match get(params, key).cloned().or(default) {
        Some(value) if value.is_finite_numeric() => Ok(value),
        _ => Err(inconsistent(key)),
    }
}

fn compile_expr(expr: &Expr) -> Result<Function> {
    Ok(match expr.name.as_str() {
        "num_combine" => Function::Combine(match get(&expr.params, "mode") {
            None => Combine::Sum,
            Some(Param::Str(mode)) => match mode.as_str() {
                "sum" => Combine::Sum,
                "multiply" => Combine::Multiply,
                "max" => Combine::Max,
                "min" => Combine::Min,
                "avg" => Combine::Avg,
                "weighted" => Combine::Weighted(
                    sequence(get(&expr.params, "weights"))
                        .and_then(|weights| weights.iter().map(Param::as_f64).collect())
                        .ok_or_else(|| inconsistent("weights"))?,
                ),
                _ => return Err(inconsistent("mode")),
            },
            Some(_) => return Err(inconsistent("mode")),
        }),
        "decay" => {
            let function = match get(&expr.params, "function") {
                Some(Param::Str(name)) if name == "gauss" => DecayFunction::Gauss,
                Some(Param::Str(name)) if name == "exp" => DecayFunction::Exp,
                Some(Param::Str(name)) if name == "linear" => DecayFunction::Linear,
                _ => return Err(inconsistent("function")),
            };
            let float = |value: &Param| value.as_f64().ok_or_else(|| inconsistent("decay"));
            let scale = number(&expr.params, "scale", None)?;
            // 3.x 用参数原值算 `scale * scale`：整数是精确乘积，再转成浮点。
            let scale_sq = match scale {
                Param::Int(i) => (i128::from(i) * i128::from(i)) as f64,
                _ => float(&scale)? * float(&scale)?,
            };
            Function::Decay(Curve::new(
                function,
                float(&number(&expr.params, "origin", None)?)?,
                (float(&scale)?, scale_sq),
                float(&number(&expr.params, "offset", Some(Param::Int(0)))?)?,
                float(&number(&expr.params, "decay", Some(Param::Float(0.5)))?)?,
            ))
        }
        "round_decimal" => match get(&expr.params, "decimal") {
            Some(Param::Int(decimal)) => Function::Round(*decimal),
            _ => return Err(inconsistent("decimal")),
        },
        // 0035 决策 B：`rerank_model` 只校验，不执行。
        "rerank_model" => {
            return Err(EngineError::Unsupported {
                feature: "function chains: rerank_model",
                reason: "not supported in this build of milvus-lite-rust yet".to_owned(),
            });
        }
        _ => return Err(inconsistent("expression")),
    })
}

impl Plan {
    pub(crate) fn new(chain: &ValidatedChain, pk_name: &str) -> Result<Self> {
        let mut names = vec![SCORE_FIELD.to_owned(), ID_FIELD.to_owned()];
        names.extend(chain.required_fields.iter().cloned());
        let fields = names.len();
        let find = |names: &[String], name: &str| {
            if name == pk_name {
                Some(ID_SLOT)
            } else {
                names.iter().position(|n| n == name)
            }
        };
        let read = |names: &[String], name: &str| {
            find(names, name).ok_or_else(|| inconsistent("input column"))
        };
        let mut steps = Vec::with_capacity(chain.repr.ops.len());
        for op in &chain.repr.ops {
            steps.push(match op.op.as_str() {
                "map" => {
                    let expr = op.expr.as_ref().ok_or_else(|| inconsistent("map"))?;
                    let mut inputs = Vec::with_capacity(expr.args.len());
                    for arg in &expr.args {
                        inputs.push(match arg {
                            Arg::Column(name) => Input::Slot(read(&names, name)?),
                            Arg::Literal(Param::Bool(b)) => Input::Literal(PyVal::Bool(*b)),
                            Arg::Literal(Param::Int(i)) => Input::Literal(PyVal::Int(*i)),
                            Arg::Literal(Param::Float(f)) => Input::Literal(PyVal::Float(*f)),
                            Arg::Literal(Param::Str(s)) => Input::Literal(PyVal::Str(s.clone())),
                            Arg::Literal(_) => return Err(inconsistent("literal")),
                        });
                    }
                    let function = compile_expr(expr)?;
                    let name = op.outputs.first().ok_or_else(|| inconsistent("output"))?;
                    let output = match find(&names, name) {
                        Some(slot) => slot,
                        None => {
                            names.push(name.clone());
                            names.len() - 1
                        }
                    };
                    Step::Map {
                        function,
                        inputs,
                        output,
                    }
                }
                "sort" => Step::Sort {
                    column: match get(&op.params, "column") {
                        Some(Param::Str(name)) => read(&names, name)?,
                        _ => return Err(inconsistent("sort column")),
                    },
                    descending: !matches!(get(&op.params, "desc"), Some(Param::Bool(false))),
                    tie_break: match get(&op.params, "tie_break_col") {
                        None => None,
                        Some(Param::Str(name)) => Some(read(&names, name)?),
                        Some(_) => return Err(inconsistent("sort tie_break_col")),
                    },
                },
                "limit" => {
                    let count = |key: &str, default: Option<i64>| {
                        match get(&op.params, key) {
                            Some(Param::Int(n)) => Some(*n),
                            None => default,
                            Some(_) => None,
                        }
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or_else(|| inconsistent("limit"))
                    };
                    Step::Limit {
                        limit: count("limit", None)?,
                        offset: count("offset", Some(0))?,
                    }
                }
                _ => return Err(inconsistent("operator")),
            });
        }
        Ok(Self {
            steps,
            names,
            fields,
        })
    }

    /// 一个命中的行。`score` 是链入口的分数（BM25 已取负），`field(name)` 给出链读到的 schema 字段的值。
    pub(crate) fn row(
        &self,
        source: usize,
        id: PyVal,
        score: f64,
        mut field: impl FnMut(&str) -> Result<PyVal>,
    ) -> Result<Row> {
        let mut values = Vec::with_capacity(self.names.len());
        values.push(PyVal::Float(score));
        values.push(id);
        for name in &self.names[2..self.fields] {
            values.push(field(name)?);
        }
        values.resize(self.names.len(), PyVal::None);
        Ok(Row { source, values })
    }

    /// 执行整条链；`chunks[q]` 是第 q 个查询的命中，按引擎返回的顺序。
    pub(crate) fn execute(&self, mut chunks: Vec<Vec<Row>>) -> Result<Vec<Vec<Row>>> {
        for step in &self.steps {
            match step {
                Step::Map {
                    function,
                    inputs,
                    output,
                } => {
                    for chunk in &mut chunks {
                        // 先算出整列再写回：输出列可以同时是输入列。
                        let mut column = Vec::with_capacity(chunk.len());
                        for row in chunk.iter() {
                            let args: Vec<&PyVal> = inputs
                                .iter()
                                .map(|input| match input {
                                    Input::Slot(slot) => &row.values[*slot],
                                    Input::Literal(value) => value,
                                })
                                .collect();
                            column.push(function.apply(&args)?);
                        }
                        for (row, value) in chunk.iter_mut().zip(column) {
                            row.values[*output] = value;
                        }
                    }
                }
                Step::Sort {
                    column,
                    descending,
                    tie_break,
                } => {
                    for chunk in &mut chunks {
                        *chunk = try_sort_by(std::mem::take(chunk), &mut |a, b| {
                            compare(a, b, *column, *descending, *tie_break)
                        })?;
                    }
                }
                Step::Limit { limit, offset } => {
                    for chunk in &mut chunks {
                        let start = (*offset).min(chunk.len());
                        chunk.truncate(start.saturating_add(*limit).min(chunk.len()));
                        chunk.drain(..start);
                    }
                }
            }
        }
        Ok(chunks)
    }
}

/// 3.x `SortOp`：`None` 与 NaN 不论升降序都在最后；主列相等时按平局列升序，没有平局列时保持原顺序。
fn compare(
    a: &Row,
    b: &Row,
    column: usize,
    descending: bool,
    tie_break: Option<usize>,
) -> Result<Ordering> {
    let tie = || -> Result<Ordering> {
        let Some(slot) = tie_break else {
            return Ok(Ordering::Equal);
        };
        let (x, y) = (&a.values[slot], &b.values[slot]);
        match (x.is_missing(), y.is_missing()) {
            (true, true) => Ok(Ordering::Equal),
            (true, false) => Ok(Ordering::Greater),
            (false, true) => Ok(Ordering::Less),
            (false, false) => x.py_order(y),
        }
    };
    let (x, y) = (&a.values[column], &b.values[column]);
    match (x.is_missing(), y.is_missing()) {
        (true, true) => tie(),
        (true, false) => Ok(Ordering::Greater),
        (false, true) => Ok(Ordering::Less),
        (false, false) if x.py_eq(y) => tie(),
        (false, false) if descending => y.py_order(x),
        (false, false) => x.py_order(y),
    }
}

impl Function {
    fn apply(&self, args: &[&PyVal]) -> Result<PyVal> {
        match self {
            Self::Combine(mode) => mode.apply(args),
            Self::Decay(curve) => {
                let value = match args.first() {
                    Some(PyVal::None) => return Ok(PyVal::Float(0.0)),
                    // `datetime.timestamp()`：UTC 微秒整数除以 10⁶ 的正确舍入结果。
                    Some(PyVal::DateTime(micros)) => {
                        py_divide(&PyVal::Int(*micros), 1_000_000)?.to_float()?
                    }
                    Some(value) => value.to_float()?,
                    None => return Err(inconsistent("decay argument")),
                };
                curve.factor(value).map(PyVal::Float)
            }
            Self::Round(decimal) => Ok(match args.first() {
                Some(PyVal::Float(f)) => PyVal::Float(py_round(*f, *decimal)),
                // `None` 保持 `None`；整数的 `round(i, n >= 0)` 是它自己。
                Some(value) => (*value).clone(),
                None => return Err(inconsistent("round_decimal argument")),
            }),
        }
    }
}

/// Python 的 `isinstance(v, Real) and not bool` 且 `math.isfinite(v)`（整数转不成浮点时为假）。
fn finite_number(value: &PyVal) -> bool {
    match value {
        PyVal::Int(_) => true,
        PyVal::BigInt(_) => value.to_float().is_ok(),
        PyVal::Float(f) => f.is_finite(),
        _ => false,
    }
}

impl Combine {
    /// 3.x `NumCombineExpr.execute` 的一行。算术里的 OverflowError 都报成非有限结果。
    fn apply(&self, values: &[&PyVal]) -> Result<PyVal> {
        if values.iter().any(|v| **v == PyVal::None) {
            return Ok(PyVal::Float(0.0));
        }
        if !values.iter().all(|v| finite_number(v)) {
            return Err(EngineError::invalid(ROW_VALUES_ERROR));
        }
        let non_finite = || EngineError::invalid(NON_FINITE_ERROR);
        let float = |value: &PyVal| value.to_float().map_err(|_| non_finite());
        let owned = || values.iter().map(|v| (*v).clone()).collect::<Vec<_>>();
        let result = match self {
            Self::Multiply => {
                let mut product = 1.0_f64;
                for value in values {
                    product *= float(value)?;
                }
                PyVal::Float(product)
            }
            Self::Sum => py_sum(&owned()).map_err(|_| non_finite())?,
            // Python 的 `max` / `min`：后面的元素严格更大（更小）才替换，返回元素本身。
            Self::Max | Self::Min => {
                let wanted = if matches!(self, Self::Max) {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
                let mut best = values[0];
                for value in &values[1..] {
                    if value.py_order(best)? == wanted {
                        best = value;
                    }
                }
                best.clone()
            }
            Self::Avg => {
                let mut terms = Vec::with_capacity(values.len());
                for value in values {
                    let term = py_divide(value, values.len()).map_err(|_| non_finite())?;
                    terms.push(float(&term)?);
                }
                PyVal::Float(fsum(&terms).ok_or_else(non_finite)?)
            }
            Self::Weighted(weights) => {
                let mut products = Vec::with_capacity(values.len());
                for (value, weight) in values.iter().zip(weights) {
                    products.push(PyVal::Float(float(value)? * weight));
                }
                py_sum(&products).map_err(|_| non_finite())?
            }
        };
        if finite_number(&result) {
            Ok(result)
        } else {
            Err(non_finite())
        }
    }
}

/// CPython `math.fsum`（Shewchuk 的精确部分和，结果正确舍入）。输入都是有限数；中间结果溢出时 CPython 报
/// OverflowError，这里返回 None。
fn fsum(values: &[f64]) -> Option<f64> {
    let mut partials: Vec<f64> = Vec::new();
    for &value in values {
        let mut x = value;
        let mut kept = 0;
        for j in 0..partials.len() {
            let mut y = partials[j];
            if x.abs() < y.abs() {
                std::mem::swap(&mut x, &mut y);
            }
            let hi = x + y;
            let lo = y - (hi - x);
            if lo != 0.0 {
                partials[kept] = lo;
                kept += 1;
            }
            x = hi;
        }
        partials.truncate(kept);
        if x != 0.0 {
            if !x.is_finite() {
                return None;
            }
            partials.push(x);
        }
    }
    let mut hi = match partials.pop() {
        Some(top) => top,
        None => return Some(0.0),
    };
    let mut lo = 0.0;
    while let Some(y) = partials.pop() {
        let x = hi;
        hi = x + y;
        lo = y - (hi - x);
        if lo != 0.0 {
            break;
        }
    }
    // 舍入到偶数时再看下一个部分和：它与余项同号说明真值越过了中点。
    if let Some(&next) = partials.last()
        && ((lo < 0.0 && next < 0.0) || (lo > 0.0 && next > 0.0))
    {
        let y = lo * 2.0;
        let x = hi + y;
        if y == x - hi {
            hi = x;
        }
    }
    Some(hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::FieldSchema;

    fn schema() -> CollectionSchema {
        let mut pk = FieldSchema::new("id", DataType::Int64);
        pk.is_primary = true;
        let field = FieldSchema::new;
        CollectionSchema {
            fields: vec![
                pk,
                field("ts", DataType::Int64),
                field("f", DataType::Float),
                field("b", DataType::Bool),
                field("s", DataType::VarChar),
                field("tz", DataType::Timestamptz),
                field("j", DataType::Json),
                field("arr", DataType::Array),
                field("geo", DataType::Geometry),
                field("vector", DataType::FloatVector),
                field("sv", DataType::SparseFloatVector),
            ],
            enable_dynamic_field: true,
            properties: Vec::new(),
            functions: Vec::new(),
        }
    }

    fn col(name: &str) -> Arg {
        Arg::Column(name.to_owned())
    }

    fn params(items: &[(&str, Param)]) -> Params {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    fn text(s: &str) -> Param {
        Param::Str(s.to_owned())
    }

    fn expr(name: &str, args: Vec<Arg>, items: &[(&str, Param)]) -> Expr {
        Expr {
            name: name.to_owned(),
            args,
            params: params(items),
        }
    }

    fn map(output: &str, expr: Expr) -> Op {
        Op {
            op: "map".to_owned(),
            expr: Some(expr),
            inputs: Vec::new(),
            outputs: vec![output.to_owned()],
            params: Vec::new(),
        }
    }

    fn sort(inputs: &[&str], items: &[(&str, Param)]) -> Op {
        Op {
            op: "sort".to_owned(),
            expr: None,
            inputs: inputs.iter().map(|s| (*s).to_owned()).collect(),
            outputs: Vec::new(),
            params: params(items),
        }
    }

    fn limit(items: &[(&str, Param)]) -> Op {
        Op {
            op: "limit".to_owned(),
            expr: None,
            inputs: Vec::new(),
            outputs: Vec::new(),
            params: params(items),
        }
    }

    fn nc(args: Vec<Arg>, items: &[(&str, Param)]) -> Expr {
        expr("num_combine", args, items)
    }

    /// `base` 里的同名项被 `extra` 替换。
    fn overlay(
        mut base: Vec<(&'static str, Param)>,
        extra: &[(&'static str, Param)],
    ) -> Vec<(&'static str, Param)> {
        for (key, value) in extra {
            base.retain(|(k, _)| k != key);
            base.push((key, value.clone()));
        }
        base
    }

    fn check(ops: Vec<Op>) -> Result<ValidatedChain> {
        check_with(ops, &schema(), 1)
    }

    fn check_with(ops: Vec<Op>, schema: &CollectionSchema, nq: usize) -> Result<ValidatedChain> {
        validate(
            ChainRepr {
                stage: L2_RERANK_STAGE.to_owned(),
                ops,
            },
            schema,
            nq,
        )
    }

    fn message(ops: Vec<Op>) -> String {
        match check(ops) {
            Err(EngineError::SchemaValidation(message)) => message,
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    // 以下期望文案全部取自真实 3.x（`1320d26`）对同一条链的响应。

    #[test]
    fn accepted_chains_report_the_fields_they_read() {
        let chain = check(vec![
            map("t", nc(vec![col("$score"), col("ts"), col("f")], &[])),
            map("$score", nc(vec![col("t"), col("ts"), col("id")], &[])),
            sort(
                &["$score", "$id"],
                &[
                    ("column", text("$score")),
                    ("tie_break_col", text("$id")),
                    ("desc", Param::Bool(false)),
                ],
            ),
            sort(&["s"], &[("column", text("s")), ("zz", Param::Int(1))]),
            limit(&[("limit", Param::Int(2)), ("offset", Param::Int(0))]),
        ])
        .unwrap();
        assert_eq!(chain.required_fields, ["ts", "f", "s"]);
        assert!(!chain.uses_rerank_model);
        // 输出名可以是空白，也可以与动态字段同名。
        check(vec![
            map(" ", nc(vec![col("$score"), col("ts")], &[])),
            sort(&[" "], &[("column", text(" "))]),
        ])
        .unwrap();
    }

    #[test]
    fn chain_level_checks() {
        let stage = |stage: &str| {
            validate(
                ChainRepr {
                    stage: stage.to_owned(),
                    ops: vec![limit(&[("limit", Param::Int(1))])],
                },
                &schema(),
                1,
            )
            .unwrap_err()
            .to_string()
        };
        assert_eq!(
            stage("FunctionChainStageUnspecified"),
            "function chain stage FunctionChainStageUnspecified is not supported in search request"
        );
        assert_eq!(
            stage("FunctionChainStageL0Rerank"),
            "function chain stage FunctionChainStageL0Rerank is not supported in search request"
        );
        assert_eq!(
            message(Vec::new()),
            "function chain must contain at least one operator"
        );
        for name in ["$score", "$id"] {
            let mut schema = schema();
            schema.fields[0].name = name.to_owned();
            assert_eq!(
                check_with(vec![limit(&[("limit", Param::Int(1))])], &schema, 1)
                    .unwrap_err()
                    .to_string(),
                format!(
                    "function chain primary key field '{name}' conflicts with reserved system column '{name}'"
                )
            );
        }
        assert_eq!(
            message(vec![Op {
                op: "filter".to_owned(),
                ..limit(&[])
            }]),
            "function chain op[0]: unsupported function chain operator: filter"
        );
        assert_eq!(
            message(vec![
                limit(&[("limit", Param::Int(1))]),
                Op {
                    op: "MAP".to_owned(),
                    ..map("$score", nc(vec![col("$score"), col("ts")], &[]))
                }
            ]),
            "function chain op[1]: unsupported function chain operator: MAP"
        );
    }

    #[test]
    fn names_follow_the_reference_rules() {
        let read = |name: &str| message(vec![map("x", nc(vec![col("$score"), col(name)], &[]))]);
        let prefix = "function chain op[0] expression num_combine: ";
        assert_eq!(
            read("dyn"),
            format!(
                "{prefix}function chain input 'dyn' is neither a previous output nor a collection field"
            )
        );
        assert_eq!(
            read("$distance"),
            format!(
                "{prefix}system input '$distance' is not supported by L2 rerank function chain"
            )
        );
        for (name, value) in [
            ("j", "json"),
            ("arr", "array"),
            ("geo", "geometry"),
            ("vector", "float_vector"),
            ("sv", "sparse_float_vector"),
        ] {
            assert_eq!(
                read(name),
                format!("{prefix}function chain input field '{name}' has unsupported type {value}")
            );
        }
        // 临时列要先写后读。
        assert_eq!(
            message(vec![
                map("$score", nc(vec![col("$score"), col("t")], &[])),
                map("t", nc(vec![col("$score"), col("ts")], &[])),
            ]),
            format!(
                "{prefix}function chain input 't' is neither a previous output nor a collection field"
            )
        );
        let write = |name: &str| message(vec![map(name, nc(vec![col("$score"), col("ts")], &[]))]);
        assert_eq!(
            write("ts"),
            "function chain op[0]: function chain output 'ts' conflicts with a collection field"
        );
        assert_eq!(
            write("id"),
            "function chain op[0]: function chain output 'id' conflicts with a collection field"
        );
        for name in ["$id", "$x"] {
            assert_eq!(
                write(name),
                format!(
                    "function chain op[0]: system output '{name}' is not writable by L2 rerank function chain"
                )
            );
        }
        // 字符串主键：`$id` 与主键名是文本，不能进 num_combine，可以排序。
        let mut schema = schema();
        schema.fields[0].data_type = DataType::VarChar;
        assert_eq!(
            check_with(
                vec![map("$score", nc(vec![col("$score"), col("$id")], &[]))],
                &schema,
                1
            )
            .unwrap_err()
            .to_string(),
            format!("{prefix}num_combine requires at least two numeric arguments")
        );
        let sorted = check_with(
            vec![sort(
                &["ts", "id"],
                &[("column", text("ts")), ("tie_break_col", text("id"))],
            )],
            &schema,
            1,
        )
        .unwrap();
        assert_eq!(sorted.required_fields, ["ts"]);
    }

    #[test]
    fn map_shape_and_literals() {
        let bare = |op: Op| message(vec![op]);
        let good = || nc(vec![col("$score"), col("ts")], &[]);
        assert_eq!(
            bare(Op {
                expr: None,
                ..map("x", good())
            }),
            "function chain op[0]: map operator requires an expression"
        );
        assert_eq!(
            bare(Op {
                inputs: vec!["ts".to_owned()],
                ..map("x", good())
            }),
            "function chain op[0]: map operator must not declare op.inputs"
        );
        for outputs in [vec![], vec!["x".to_owned(), "y".to_owned()]] {
            assert_eq!(
                bare(Op {
                    outputs,
                    ..map("x", good())
                }),
                "function chain op[0]: map operator requires exactly one output"
            );
        }
        assert_eq!(
            bare(map("x", expr("sum", vec![col("ts"), col("f")], &[]))),
            "function chain op[0] expression sum: unsupported function chain expression: sum"
        );
        let literal = |value: Param| {
            message(vec![map(
                "$score",
                nc(vec![col("ts"), Arg::Literal(value)], &[]),
            )])
        };
        let prefix = "function chain op[0] expression num_combine: ";
        assert_eq!(
            literal(Param::Float(f64::INFINITY)),
            format!("{prefix}numeric literal must be finite")
        );
        assert_eq!(
            literal(Param::Array(vec![Param::Int(1)])),
            format!("{prefix}unsupported function chain literal type: tuple")
        );
        assert_eq!(
            literal(Param::Object(Vec::new())),
            format!("{prefix}unsupported function chain literal type: mappingproxy")
        );
        assert_eq!(
            literal(Param::Bytes(b"x".to_vec())),
            format!("{prefix}unsupported function chain literal type: bytes")
        );
        for value in [Param::Bool(true), text("x")] {
            assert_eq!(
                literal(value),
                format!("{prefix}num_combine requires at least two numeric arguments")
            );
        }
        for value in [Param::Int(5), Param::Float(0.5)] {
            check(vec![map(
                "$score",
                nc(vec![col("ts"), Arg::Literal(value)], &[]),
            )])
            .unwrap();
        }
        // 参数错误先于表达式自身的检查，表达式的检查先于输出名。
        assert_eq!(
            message(vec![map("$id", nc(vec![col("nope")], &[]))]),
            format!(
                "{prefix}function chain input 'nope' is neither a previous output nor a collection field"
            )
        );
        assert_eq!(
            message(vec![map("$id", nc(vec![col("ts")], &[]))]),
            format!("{prefix}num_combine requires at least two numeric arguments")
        );
    }

    #[test]
    fn num_combine_parameters() {
        let run = |args: &[&str], items: &[(&str, Param)]| {
            check(vec![map(
                "$score",
                nc(args.iter().map(|a| col(a)).collect(), items),
            )])
            .map(drop)
            .map_err(|e| {
                e.to_string()
                    .trim_start_matches("function chain op[0] expression num_combine: ")
                    .to_owned()
            })
        };
        let two = ["ts", "f"];
        for mode in ["sum", "multiply", "max", "min", "avg"] {
            assert_eq!(run(&two, &[("mode", text(mode))]), Ok(()));
        }
        assert_eq!(run(&two, &[("zzz", Param::Int(3))]), Ok(()));
        let numeric = "num_combine requires at least two numeric arguments".to_owned();
        assert_eq!(run(&["ts"], &[]), Err(numeric.clone()));
        for other in ["b", "s", "tz"] {
            assert_eq!(run(&["ts", other], &[]), Err(numeric.clone()));
        }
        assert_eq!(
            run(&two, &[("mode", text("SUM"))]),
            Err("unsupported num_combine mode: SUM".to_owned())
        );
        assert_eq!(
            run(&two, &[("mode", Param::Int(3))]),
            Err("num_combine mode must be a string".to_owned())
        );
        let weighted =
            |weights: Param| run(&two, &[("mode", text("weighted")), ("weights", weights)]);
        assert_eq!(
            weighted(Param::Array(vec![Param::Float(0.5), Param::Int(2)])),
            Ok(())
        );
        let count = Err("weighted num_combine requires 2 weights".to_owned());
        assert_eq!(run(&two, &[("mode", text("weighted"))]), count);
        assert_eq!(weighted(Param::Array(vec![Param::Int(1)])), count);
        assert_eq!(weighted(text("ab")), count);
        assert_eq!(weighted(Param::Object(Vec::new())), count);
        assert_eq!(
            weighted(Param::Array(vec![Param::Bool(true), Param::Int(1)])),
            Err("num_combine weights must be numeric".to_owned())
        );
        assert_eq!(
            weighted(Param::Array(vec![Param::Float(f64::NAN), Param::Int(1)])),
            Err("num_combine weights must be finite".to_owned())
        );
        assert_eq!(
            run(&two, &[("weights", Param::Array(Vec::new()))]),
            Err("num_combine weights require mode='weighted'".to_owned())
        );
    }

    #[test]
    fn decay_and_round_decimal_parameters() {
        let decay = |arg: &str, items: &[(&str, Param)]| {
            check(vec![map("$score", expr("decay", vec![col(arg)], items))])
                .map(drop)
                .map_err(|e| {
                    e.to_string()
                        .trim_start_matches("function chain op[0] expression decay: ")
                        .to_owned()
                })
        };
        let base = |extra: &[(&'static str, Param)]| {
            overlay(
                vec![
                    ("function", text("gauss")),
                    ("origin", Param::Int(20)),
                    ("scale", Param::Int(10)),
                ],
                extra,
            )
        };
        for arg in ["ts", "f", "tz", "$score"] {
            assert_eq!(decay(arg, &base(&[])), Ok(()));
        }
        assert_eq!(
            decay(
                "ts",
                &base(&[
                    ("offset", Param::Float(0.1)),
                    ("decay", Param::Float(0.3)),
                    ("zz", Param::Int(1))
                ])
            ),
            Ok(())
        );
        let argument = Err("decay requires exactly one numeric or timestamp argument".to_owned());
        assert_eq!(decay("s", &base(&[])), argument);
        assert_eq!(decay("b", &base(&[])), argument);
        assert_eq!(
            decay("ts", &base(&[("function", text("Gauss"))])),
            Err("unsupported decay function: Gauss".to_owned())
        );
        assert_eq!(
            decay("ts", &base(&[("function", Param::Int(1))])),
            Err("decay function must be a string".to_owned())
        );
        let numeric = Err("decay parameters must be numeric".to_owned());
        assert_eq!(
            decay(
                "ts",
                &[("function", text("gauss")), ("scale", Param::Int(10))]
            ),
            numeric
        );
        assert_eq!(decay("ts", &base(&[("origin", text("20"))])), numeric);
        assert_eq!(decay("ts", &base(&[("scale", Param::Bool(true))])), numeric);
        assert_eq!(
            decay("ts", &base(&[("origin", Param::Float(f64::INFINITY))])),
            Err("decay parameters must be finite".to_owned())
        );
        let range = Err("decay requires scale > 0, offset >= 0, and 0 < decay < 1".to_owned());
        assert_eq!(decay("ts", &base(&[("scale", Param::Int(0))])), range);
        assert_eq!(decay("ts", &base(&[("decay", Param::Float(1.0))])), range);
        assert_eq!(decay("ts", &base(&[("offset", Param::Int(-1))])), range);

        let round = |arg: &str, items: &[(&str, Param)]| {
            check(vec![map(
                "$score",
                expr("round_decimal", vec![col(arg)], items),
            )])
            .map(drop)
            .map_err(|e| {
                e.to_string()
                    .trim_start_matches("function chain op[0] expression round_decimal: ")
                    .to_owned()
            })
        };
        assert_eq!(round("$score", &[("decimal", Param::Int(0))]), Ok(()));
        assert_eq!(round("ts", &[("decimal", Param::Int(6))]), Ok(()));
        let decimal = Err("round_decimal decimal must be an integer in [0, 6]".to_owned());
        for value in [
            Some(Param::Int(7)),
            Some(Param::Int(-1)),
            Some(Param::Float(2.0)),
            Some(Param::Bool(true)),
            None,
        ] {
            let items: Vec<(&str, Param)> = value.into_iter().map(|v| ("decimal", v)).collect();
            assert_eq!(round("$score", &items), decimal);
        }
        let argument = Err("round_decimal requires exactly one numeric argument".to_owned());
        assert_eq!(round("s", &[("decimal", Param::Int(1))]), argument);
        assert_eq!(round("tz", &[("decimal", Param::Int(1))]), argument);
    }

    #[test]
    fn sort_and_limit_parameters() {
        let run = |op: Op| {
            check(vec![op]).map(drop).map_err(|e| {
                e.to_string()
                    .trim_start_matches("function chain op[0]: ")
                    .to_owned()
            })
        };
        for column in ["ts", "f", "b", "s", "tz", "$id", "id", "$score"] {
            assert_eq!(run(sort(&[column], &[("column", text(column))])), Ok(()));
        }
        let inputs = Err("sort inputs must match column and tie_break_col".to_owned());
        assert_eq!(run(sort(&[], &[("column", text("ts"))])), inputs);
        assert_eq!(run(sort(&["ts", "f"], &[("column", text("ts"))])), inputs);
        assert_eq!(
            run(sort(
                &["ts"],
                &[("column", text("ts")), ("tie_break_col", text("f"))]
            )),
            inputs
        );
        let column = Err("sort column must be a non-empty string".to_owned());
        assert_eq!(run(sort(&["ts"], &[("column", Param::Int(3))])), column);
        assert_eq!(run(sort(&[], &[("column", text(""))])), column);
        assert_eq!(run(sort(&[], &[])), column);
        assert_eq!(
            run(sort(
                &["ts"],
                &[("column", text("ts")), ("desc", Param::Int(1))]
            )),
            Err("sort desc must be a boolean".to_owned())
        );
        for tie in [text(""), Param::Int(5)] {
            assert_eq!(
                run(sort(
                    &["ts"],
                    &[("column", text("ts")), ("tie_break_col", tie)]
                )),
                Err("sort tie_break_col must be a non-empty string".to_owned())
            );
        }
        let shape = Err("sort operator must not contain expression or outputs".to_owned());
        assert_eq!(
            run(Op {
                outputs: vec!["x".to_owned()],
                ..sort(&["ts"], &[("column", text("ts"))])
            }),
            shape
        );
        assert_eq!(
            run(Op {
                expr: Some(nc(vec![col("ts"), col("f")], &[])),
                ..sort(&["ts"], &[("column", text("ts"))])
            }),
            shape
        );
        assert_eq!(
            run(sort(&["j"], &[("column", text("j"))])),
            Err("function chain input field 'j' has unsupported type json".to_owned())
        );
        assert_eq!(
            run(sort(&["dyn"], &[("column", text("dyn"))])),
            Err(
                "function chain input 'dyn' is neither a previous output nor a collection field"
                    .to_owned()
            )
        );

        assert_eq!(
            run(limit(&[
                ("limit", Param::Int(i64::MAX)),
                ("zz", Param::Int(1))
            ])),
            Ok(())
        );
        let positive = Err("function chain limit must be a positive integer".to_owned());
        for value in [
            Some(Param::Int(0)),
            Some(Param::Int(-1)),
            Some(Param::Float(2.0)),
            Some(Param::Bool(true)),
            Some(text("2")),
            None,
        ] {
            let items: Vec<(&str, Param)> = value.into_iter().map(|v| ("limit", v)).collect();
            assert_eq!(run(limit(&items)), positive);
        }
        for value in [Param::Int(-1), Param::Bool(false)] {
            assert_eq!(
                run(limit(&[("limit", Param::Int(2)), ("offset", value)])),
                Err("function chain offset must be a non-negative integer".to_owned())
            );
        }
        assert_eq!(
            run(Op {
                inputs: vec!["ts".to_owned()],
                ..limit(&[("limit", Param::Int(2))])
            }),
            Err("limit operator must not contain expression, inputs, or outputs".to_owned())
        );
    }

    #[test]
    fn rerank_model_parameters() {
        let run = |arg: &str, items: &[(&str, Param)], nq: usize| {
            check_with(
                vec![map("$score", expr("rerank_model", vec![col(arg)], items))],
                &schema(),
                nq,
            )
            .map(|chain| chain.uses_rerank_model)
            .map_err(|e| {
                e.to_string()
                    .trim_start_matches("function chain op[0] expression rerank_model: ")
                    .to_owned()
            })
        };
        let queries = |n: usize| Param::Array(vec![text("q"); n]);
        let base = |extra: &[(&'static str, Param)]| {
            overlay(
                vec![("provider", text("cohere")), ("queries", queries(1))],
                extra,
            )
        };
        assert_eq!(run("s", &base(&[]), 1), Ok(true));
        assert_eq!(
            run(
                "s",
                &base(&[("provider", text("Cohere")), ("model_name", text("x"))]),
                1
            ),
            Ok(true)
        );
        assert_eq!(
            run("ts", &base(&[]), 1),
            Err("rerank_model requires exactly one text argument".to_owned())
        );
        assert_eq!(
            run("s", &base(&[("Base_URL", text("x"))]), 1),
            Err("rerank_model endpoint must be configured on the server".to_owned())
        );
        for key in ["api_key", "Token", "SECRET"] {
            assert_eq!(
                run("s", &base(&[(key, text("x"))]), 1),
                Err("rerank_model credentials must be configured on the server".to_owned())
            );
        }
        let sequence =
            Err("rerank_model queries must be a non-empty sequence of strings".to_owned());
        for value in [
            queries(0),
            text("q"),
            Param::Array(vec![text("")]),
            Param::Array(vec![Param::Int(1)]),
        ] {
            assert_eq!(run("s", &base(&[("queries", value)]), 1), sequence);
        }
        assert_eq!(run("s", &[("provider", text("cohere"))], 1), sequence);
        assert_eq!(
            run("s", &base(&[("queries", queries(2))]), 1),
            Err("rerank_model query count 2 does not match search nq 1".to_owned())
        );
        assert_eq!(run("s", &base(&[("queries", queries(2))]), 2), Ok(true));
        let provider = Err("rerank_model provider must be a non-empty string".to_owned());
        assert_eq!(run("s", &[("queries", queries(1))], 1), provider);
        assert_eq!(run("s", &base(&[("provider", text(""))]), 1), provider);
        assert_eq!(
            run("s", &base(&[("provider", text("ZZ"))]), 1),
            Err("Unknown rerank provider: 'zz'. Supported: 'cohere'".to_owned())
        );
        assert_eq!(
            run("s", &base(&[("model_name", Param::Int(3))]), 1),
            Err("Cohere rerank parameter 'model_name' must be a string".to_owned())
        );
        assert_eq!(
            run(
                "s",
                &base(&[("zz", Param::Int(1)), ("aa", Param::Int(2))]),
                1
            ),
            Err("unsupported rerank_model parameter 'aa'".to_owned())
        );
    }

    // ---- 执行。期望值取自真实 3.x（`1320d26`，CPython 3.12）的 NumCombineExpr / DecayExpr /
    // RoundDecimalExpr / SortOp 对同样输入的结果（`repr`）。

    const MAX: i64 = i64::MAX;

    fn pow2(exponent: u32) -> num_bigint::BigInt {
        num_bigint::BigInt::from(1) << exponent
    }

    fn big(value: num_bigint::BigInt) -> PyVal {
        PyVal::BigInt(value.to_string())
    }

    fn float(value: f64) -> PyVal {
        PyVal::Float(value)
    }

    fn outcome(result: Result<PyVal>) -> String {
        match result {
            Ok(value) => value.repr(),
            Err(EngineError::InvalidArgument(message)) => format!("ERR {message}"),
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }

    fn combine_rows() -> Vec<[PyVal; 3]> {
        let int = PyVal::Int;
        vec![
            [int(1), int(2), int(3)],
            [int(MAX), int(MAX), int(1)],
            [int(MAX), int(MAX), float(0.5)],
            [float(0.1), float(0.2), float(0.3)],
            [float(1e16), float(1.0), float(-1e16)],
            [PyVal::None, int(1), int(2)],
            [int(1), float(2.5), int(-7)],
            [int(3), float(3.0), int(2)],
            [float(-0.0), int(0), float(-0.0)],
            [float(1e308), float(1e308), int(1)],
            [big(pow2(1023)), big(pow2(1023)), int(1)],
            [float(f64::NAN), int(1), int(2)],
            [float(f64::INFINITY), int(1), int(2)],
            [float(1e308), float(1e308), float(-1e308)],
            [big(pow2(1023)), big(pow2(1023) - pow2(970)), int(0)],
            [int(5), int(7), float(0.1)],
        ]
    }

    fn combined(mode: &Combine, rows: &[[PyVal; 3]]) -> Vec<String> {
        rows.iter()
            .map(|row| outcome(mode.apply(&row.iter().collect::<Vec<_>>())))
            .collect()
    }

    const NON_FINITE: &str = "ERR num_combine produced a non-finite result";
    const ROW_VALUES: &str = "ERR num_combine row values must be finite numeric non-bool values";

    #[test]
    fn num_combine_follows_python_arithmetic() {
        let rows = combine_rows();
        let huge = pow2(1023).to_string();
        assert_eq!(
            combined(&Combine::Sum, &rows),
            [
                "6",
                "18446744073709551615",
                "1.8446744073709552e+19",
                "0.6",
                "1.0",
                "0.0",
                "-3.5",
                "8.0",
                "0.0",
                NON_FINITE,
                NON_FINITE,
                ROW_VALUES,
                ROW_VALUES,
                NON_FINITE,
                NON_FINITE,
                "12.1",
            ]
        );
        assert_eq!(
            combined(&Combine::Multiply, &rows),
            [
                "6.0",
                "8.507059173023462e+37",
                "4.253529586511731e+37",
                "0.006000000000000001",
                "-1e+32",
                "0.0",
                "-17.5",
                "18.0",
                "0.0",
                NON_FINITE,
                NON_FINITE,
                ROW_VALUES,
                ROW_VALUES,
                NON_FINITE,
                NON_FINITE,
                "3.5",
            ]
        );
        assert_eq!(
            combined(&Combine::Max, &rows),
            [
                "3",
                "9223372036854775807",
                "9223372036854775807",
                "0.3",
                "1e+16",
                "0.0",
                "2.5",
                "3",
                "-0.0",
                "1e+308",
                huge.as_str(),
                ROW_VALUES,
                ROW_VALUES,
                "1e+308",
                huge.as_str(),
                "7",
            ]
        );
        assert_eq!(
            combined(&Combine::Min, &rows),
            [
                "1", "1", "0.5", "0.1", "-1e+16", "0.0", "-7", "2", "-0.0", "1", "1", ROW_VALUES,
                ROW_VALUES, "-1e+308", "0", "0.1",
            ]
        );
        assert_eq!(
            combined(&Combine::Avg, &rows),
            [
                "2.0",
                "6.148914691236517e+18",
                "6.148914691236517e+18",
                "0.19999999999999998",
                "0.3333333333333333",
                "0.0",
                "-1.1666666666666667",
                "2.6666666666666665",
                "0.0",
                "6.666666666666666e+307",
                "5.992310449541053e+307",
                ROW_VALUES,
                ROW_VALUES,
                "3.333333333333333e+307",
                "5.992310449541053e+307",
                "4.033333333333333",
            ]
        );
        assert_eq!(
            combined(&Combine::Weighted(vec![0.1, 0.2, 0.7]), &rows),
            [
                "2.5999999999999996",
                "2.767011611056433e+18",
                "2.767011611056433e+18",
                "0.26",
                "-6000000000000000.0",
                "0.0",
                "-4.3",
                "2.3",
                "0.0",
                "3.0000000000000003e+307",
                "2.6965397022934743e+307",
                ROW_VALUES,
                ROW_VALUES,
                "-3.999999999999999e+307",
                "2.696539702293474e+307",
                "1.9700000000000002",
            ]
        );
        let int = PyVal::Int;
        assert_eq!(
            combined(
                &Combine::Weighted(vec![10.0, 10.0, -1.0]),
                &[
                    [float(1e308), float(1e308), int(1)],
                    [int(MAX), int(1), int(1)],
                    [int(3), int(4), int(5)],
                ]
            ),
            [NON_FINITE, "9.223372036854776e+19", "65.0"]
        );
        // 布尔、字符串不是数值（校验已排除，执行时仍按 3.x 报错）。
        let row = [PyVal::Bool(true), int(1)];
        assert_eq!(
            outcome(Combine::Sum.apply(&row.iter().collect::<Vec<_>>())),
            ROW_VALUES
        );
    }

    #[test]
    fn fsum_is_correctly_rounded_and_reports_overflow() {
        assert_eq!(fsum(&[]), Some(0.0));
        assert_eq!(fsum(&[0.1; 10]), Some(1.0));
        assert_eq!(fsum(&[1e16, 1.0, -1e16]), Some(1.0));
        // CPython 的例子：中点之上的余项让结果进位。
        assert_eq!(fsum(&[1e16, 1.0, 1e-16]), Some(1.000_000_000_000_000_2e16));
        assert_eq!(fsum(&[1e308, 1e308, -1e308]), None);
        assert_eq!(fsum(&[-0.0, -0.0]), Some(0.0));
    }

    fn decay(items: &[(&'static str, Param)]) -> Function {
        compile_expr(&expr("decay", vec![col("ts")], items)).unwrap()
    }

    #[test]
    fn decay_follows_the_3x_curves() {
        let values = [
            PyVal::None,
            PyVal::Int(0),
            PyVal::Int(10),
            PyVal::Int(25),
            float(30.5),
            float(f64::NAN),
            float(f64::INFINITY),
            float(-1e300),
            big(pow2(70)),
            PyVal::DateTime(1_767_225_600_000_000),
            PyVal::DateTime(1_767_225_600_000_001),
            PyVal::DateTime(-1),
            PyVal::DateTime(9_007_199_254_740_993),
        ];
        let run = |function: Function| -> Vec<String> {
            values
                .iter()
                .map(|value| outcome(function.apply(&[value])))
                .collect()
        };
        let name = |function: &str| ("function", text(function));
        assert_eq!(
            run(decay(&[
                name("gauss"),
                ("origin", Param::Int(20)),
                ("scale", Param::Int(10))
            ])),
            [
                "0.0",
                "0.0625",
                "0.5",
                "0.8408964152537145",
                "0.46570878431485163",
                "1.0",
                "0.0",
                "0.0",
                "0.0",
                "0.0",
                "0.0",
                "0.062499982671322445",
                "0.0",
            ]
        );
        assert_eq!(
            run(decay(&[
                name("exp"),
                ("origin", Param::Int(20)),
                ("scale", Param::Int(10)),
                ("offset", Param::Int(2)),
                ("decay", Param::Float(0.3)),
            ])),
            [
                "0.0",
                "0.11450336728854528",
                "0.3816778909618176",
                "0.6968453019359488",
                "0.35937930668721074",
                "1.0",
                "0.0",
                "0.0",
                "0.0",
                "0.0",
                "0.0",
                "0.11450335350265202",
                "0.0",
            ]
        );
        assert_eq!(
            run(decay(&[
                name("linear"),
                ("origin", Param::Float(20.5)),
                ("scale", Param::Int(4)),
                ("offset", Param::Float(0.5)),
                ("decay", Param::Float(0.25)),
            ])),
            [
                "0.0", "0.0", "0.0", "0.25", "0.0", "1.0", "0.0", "0.0", "0.0", "0.0", "0.0",
                "0.0", "0.0",
            ]
        );
        // 整数 scale 的平方是精确乘积再转浮点；TIMESTAMPTZ 取 epoch 秒（微秒整数的正确舍入商）。
        assert_eq!(
            run(decay(&[
                name("gauss"),
                ("origin", Param::Int(0)),
                ("scale", Param::Int(9_007_199_254_740_993)),
                ("decay", Param::Float(0.5)),
            ])),
            [
                "0.0",
                "1.0",
                "1.0",
                "1.0",
                "1.0",
                "1.0",
                "0.0",
                "0.0",
                "0.0",
                "0.9999999999999734",
                "0.9999999999999734",
                "1.0",
                "0.9999999999993069",
            ]
        );
        assert_eq!(
            run(decay(&[
                name("linear"),
                ("origin", Param::Int(1_767_225_600)),
                ("scale", Param::Int(86400)),
            ])),
            [
                "0.0",
                "0.0",
                "0.0",
                "0.0",
                "0.0",
                "1.0",
                "0.0",
                "0.0",
                "0.0",
                "1.0",
                "0.9999999999944811",
                "0.0",
                "0.0",
            ]
        );
        let zero = "ERR float division by zero";
        assert_eq!(
            run(decay(&[
                name("gauss"),
                ("origin", Param::Int(0)),
                ("scale", Param::Float(1e-200)),
            ])),
            [
                "0.0", "1.0", zero, zero, zero, "1.0", zero, zero, zero, zero, zero, zero, zero
            ]
        );
    }

    #[test]
    fn round_decimal_keeps_none_and_integers() {
        let values = [
            PyVal::None,
            PyVal::Int(5),
            big(pow2(63)),
            float(2.5),
            float(0.125),
            float(1.005),
            float(-0.5),
            float(2.675),
            float(f64::NAN),
            float(f64::INFINITY),
            float(1e300),
            float(123_456.789_123_4),
        ];
        let run = |decimal: i64| -> Vec<String> {
            values
                .iter()
                .map(|value| outcome(Function::Round(decimal).apply(&[value])))
                .collect()
        };
        let fixed = ["None", "5", "9223372036854775808"];
        let tail = ["nan", "inf", "1e+300"];
        assert_eq!(
            run(0),
            [
                &fixed[..],
                &["2.0", "0.0", "1.0", "-0.0", "3.0"],
                &tail[..],
                &["123457.0"]
            ]
            .concat()
        );
        assert_eq!(
            run(2),
            [
                &fixed[..],
                &["2.5", "0.12", "1.0", "-0.5", "2.67"],
                &tail[..],
                &["123456.79"]
            ]
            .concat()
        );
        assert_eq!(
            run(6),
            [
                &fixed[..],
                &["2.5", "0.125", "1.005", "-0.5", "2.675"],
                &tail[..],
                &["123456.789123"]
            ]
            .concat()
        );
    }

    /// 排序后各行原来的下标。`values` 是主列，`ties` 是平局列。
    fn sorted(values: &[PyVal], descending: bool, ties: Option<&[PyVal]>) -> Vec<usize> {
        let rows: Vec<Row> = values
            .iter()
            .enumerate()
            .map(|(i, value)| Row {
                source: i,
                values: vec![
                    value.clone(),
                    ties.map_or(PyVal::None, |ties| ties[i].clone()),
                ],
            })
            .collect();
        try_sort_by(rows, &mut |a, b| {
            compare(a, b, 0, descending, ties.map(|_| 1))
        })
        .unwrap()
        .iter()
        .map(|row| row.source)
        .collect()
    }

    #[test]
    fn sort_puts_missing_values_last_and_breaks_ties_ascending() {
        let int = PyVal::Int;
        let nan = float(f64::NAN);
        let values = [
            int(3),
            PyVal::None,
            float(1.5),
            nan.clone(),
            float(3.0),
            int(MAX),
            big(pow2(63)),
            int(-2),
            PyVal::None,
            float(1.5),
        ];
        assert_eq!(sorted(&values, true, None), [6, 5, 0, 4, 2, 9, 7, 1, 3, 8]);
        assert_eq!(sorted(&values, false, None), [7, 2, 9, 0, 4, 5, 6, 1, 3, 8]);
        let ties = [
            int(5),
            int(1),
            PyVal::None,
            int(2),
            int(1),
            int(0),
            int(0),
            nan,
            int(0),
            int(2),
        ];
        assert_eq!(
            sorted(&values, true, Some(&ties)),
            [6, 5, 4, 0, 9, 2, 7, 8, 1, 3]
        );
        assert_eq!(
            sorted(&values, false, Some(&ties)),
            [7, 9, 2, 4, 0, 5, 6, 8, 1, 3]
        );

        let s = |value: &str| PyVal::Str(value.to_owned());
        let strings = [
            s("b"),
            s("a"),
            PyVal::None,
            s("é"),
            s("Z"),
            s("a"),
            s(""),
            s("ab"),
        ];
        assert_eq!(sorted(&strings, false, None), [6, 4, 1, 5, 7, 0, 3, 2]);
        let ties: Vec<PyVal> = [3, 2, 1, 0, 9, 1, 0, 0].into_iter().map(int).collect();
        assert_eq!(
            sorted(&strings, true, Some(&ties)),
            [3, 0, 7, 5, 1, 4, 6, 2]
        );

        let bools = [
            PyVal::Bool(true),
            PyVal::Bool(false),
            PyVal::None,
            PyVal::Bool(true),
            PyVal::Bool(false),
        ];
        assert_eq!(sorted(&bools, true, None), [0, 3, 1, 4, 2]);
        assert_eq!(sorted(&bools, false, None), [1, 4, 0, 3, 2]);

        let times = [
            PyVal::DateTime(5),
            PyVal::DateTime(-3),
            PyVal::None,
            PyVal::DateTime(5),
            PyVal::DateTime(0),
        ];
        assert_eq!(sorted(&times, true, None), [0, 3, 4, 1, 2]);
        assert_eq!(sorted(&times, false, None), [1, 4, 0, 3, 2]);
    }

    fn plan(ops: Vec<Op>) -> Plan {
        Plan::new(&check(ops).unwrap(), "id").unwrap()
    }

    /// 每行是（id, 分数, ts）；`ts` 为 None 表示空值。
    fn chunk(plan: &Plan, rows: &[(i64, f64, Option<i64>)]) -> Vec<Row> {
        rows.iter()
            .enumerate()
            .map(|(i, (id, score, ts))| {
                plan.row(i, PyVal::Int(*id), *score, |name| {
                    assert_eq!(name, "ts");
                    Ok(ts.map_or(PyVal::None, PyVal::Int))
                })
                .unwrap()
            })
            .collect()
    }

    fn summary(chunks: &[Vec<Row>]) -> Vec<Vec<(usize, String)>> {
        chunks
            .iter()
            .map(|rows| {
                rows.iter()
                    .map(|row| (row.source, row.score().repr()))
                    .collect()
            })
            .collect()
    }

    fn pairs(items: &[(usize, &str)]) -> Vec<(usize, String)> {
        items.iter().map(|(i, s)| (*i, (*s).to_owned())).collect()
    }

    #[test]
    fn plan_runs_ops_in_order_on_each_chunk() {
        // 临时列、主键名作 `$id` 的别名、输出列同时是输入列、每个查询各自排序与截断。
        let plan = plan(vec![
            map(
                "t",
                nc(vec![col("ts"), col("id")], &[("mode", text("multiply"))]),
            ),
            map("$score", nc(vec![col("$score"), col("t")], &[])),
            map(
                "$score",
                expr(
                    "round_decimal",
                    vec![col("$score")],
                    &[("decimal", Param::Int(1))],
                ),
            ),
            sort(
                &["$score", "$id"],
                &[("column", text("$score")), ("tie_break_col", text("$id"))],
            ),
            limit(&[("limit", Param::Int(2)), ("offset", Param::Int(1))]),
        ]);
        assert_eq!(plan.names, ["$score", "$id", "ts", "t"]);
        let chunks = vec![
            chunk(
                &plan,
                &[
                    (1, 0.25, Some(10)),
                    (2, 0.26, Some(5)),
                    (3, 0.5, None),
                    (4, 0.0, Some(5)),
                ],
            ),
            chunk(&plan, &[(7, 1.0, Some(1))]),
            chunk(&plan, &[]),
        ];
        assert_eq!(
            summary(&plan.execute(chunks).unwrap()),
            [pairs(&[(1, "10.3"), (0, "10.2")]), Vec::new(), Vec::new()]
        );

        // 只有 limit 的链不改分数；offset 超出行数时为空。
        let only_limit = super::Plan::new(
            &check(vec![limit(&[
                ("limit", Param::Int(5)),
                ("offset", Param::Int(1)),
            ])])
            .unwrap(),
            "id",
        )
        .unwrap();
        let rows = chunk(&only_limit, &[(1, 0.5, None), (2, 0.25, None)]);
        assert_eq!(
            summary(&only_limit.execute(vec![rows, Vec::new()]).unwrap()),
            [pairs(&[(1, "0.25")]), Vec::new()]
        );
    }

    #[test]
    fn runtime_errors_follow_operator_order_across_chunks() {
        // 第一个算子在第二个查询上出错，第二个算子在第一个查询上出错：3.x 一个算子做完全部查询才进入下一个。
        let plan = plan(vec![
            map(
                "t",
                nc(vec![col("$score"), Arg::Literal(Param::Int(1))], &[]),
            ),
            map(
                "$score",
                nc(
                    vec![col("t"), Arg::Literal(Param::Float(1e308))],
                    &[("mode", text("multiply"))],
                ),
            ),
        ]);
        let chunks = vec![
            chunk(&plan, &[(1, 1e10, None)]),
            chunk(&plan, &[(2, f64::NAN, None)]),
        ];
        assert_eq!(
            outcome(plan.execute(chunks).map(|_| PyVal::None)),
            ROW_VALUES
        );
        let chunks = vec![chunk(&plan, &[(1, 1e10, None)])];
        assert_eq!(
            outcome(plan.execute(chunks).map(|_| PyVal::None)),
            NON_FINITE
        );
    }

    /// 真实 3.x 的组件真值，由 tests/differential/generate_function_chain_fixtures.py 导出：3.x 解码出的链表示、
    /// 各查询的输入行，以及 3.x 编译、执行后每行的 `$id`、`$score` 与临时列的 `repr`，或运行期错误的文案。
    /// 经过 decay 的浮点值允许的差（按位相邻的个数），见 `execution_matches_the_reference_fixture`。
    const DECAY_ULPS: u64 = 4;

    #[test]
    fn execution_matches_the_reference_fixture() {
        use serde_json::Value;

        fn float(value: &Value) -> Option<f64> {
            let hex = value.get("f")?.as_str()?;
            Some(f64::from_bits(u64::from_str_radix(hex, 16).ok()?))
        }
        fn param(value: &Value) -> Param {
            match value {
                Value::Bool(b) => Param::Bool(*b),
                Value::Number(n) => Param::Int(n.as_i64().unwrap()),
                Value::String(s) => Param::Str(s.clone()),
                _ => {
                    if let Some(f) = float(value) {
                        Param::Float(f)
                    } else if let Some(items) = value.get("a") {
                        Param::Array(items.as_array().unwrap().iter().map(param).collect())
                    } else if let Some(hex) = value.get("y").and_then(Value::as_str) {
                        Param::Bytes(
                            (0..hex.len())
                                .step_by(2)
                                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                                .collect(),
                        )
                    } else {
                        Param::Object(params_of(&value["o"]))
                    }
                }
            }
        }
        fn params_of(value: &Value) -> Params {
            value
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), param(v)))
                .collect()
        }
        fn strings(value: &Value) -> Vec<String> {
            value
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect()
        }
        fn value_of(value: &Value) -> PyVal {
            match value {
                Value::Null => PyVal::None,
                Value::Bool(b) => PyVal::Bool(*b),
                Value::Number(n) => PyVal::Int(n.as_i64().unwrap()),
                Value::String(s) => PyVal::Str(s.clone()),
                _ => match float(value) {
                    Some(f) => PyVal::Float(f),
                    None => PyVal::DateTime(value["t"].as_i64().unwrap()),
                },
            }
        }
        let types = [
            DataType::Bool,
            DataType::Int8,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::Float,
            DataType::Double,
            DataType::VarChar,
            DataType::Timestamptz,
        ];

        let fixture = crate::json_text::value_str(include_str!(
            "../../tests/fixtures/function_chain_execution.json"
        ))
        .unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 688);
        for (index, case) in cases.iter().enumerate() {
            let what = format!("case {index} ({})", case["label"].as_str().unwrap());
            let fields: Vec<FieldSchema> = fixture["schemas"][case["schema"].as_str().unwrap()]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| {
                    let name = field[1].as_str().unwrap();
                    let data_type = *types.iter().find(|t| t.name() == name).unwrap();
                    let mut schema = FieldSchema::new(field[0].as_str().unwrap(), data_type);
                    schema.is_primary = field[2].as_bool().unwrap();
                    schema
                })
                .collect();
            let pk = fields[0].name.clone();
            let schema = CollectionSchema {
                fields,
                enable_dynamic_field: true,
                properties: Vec::new(),
                functions: Vec::new(),
            };
            let chain = &case["chain"];
            let ops = chain["ops"]
                .as_array()
                .unwrap()
                .iter()
                .map(|op| Op {
                    op: op["op"].as_str().unwrap().to_owned(),
                    expr: (!op["expr"].is_null()).then(|| Expr {
                        name: op["expr"]["name"].as_str().unwrap().to_owned(),
                        args: op["expr"]["args"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|arg| match arg.get("column") {
                                Some(name) => Arg::Column(name.as_str().unwrap().to_owned()),
                                None => Arg::Literal(param(&arg["literal"])),
                            })
                            .collect(),
                        params: params_of(&op["expr"]["params"]),
                    }),
                    inputs: strings(&op["inputs"]),
                    outputs: strings(&op["outputs"]),
                    params: params_of(&op["params"]),
                })
                .collect();
            let repr = ChainRepr {
                stage: chain["stage"].as_str().unwrap().to_owned(),
                ops,
            };
            let uses_decay = repr
                .ops
                .iter()
                .any(|op| op.expr.as_ref().is_some_and(|expr| expr.name == "decay"));
            let rows = case["rows"].as_array().unwrap();
            let validated = validate(repr, &schema, rows.len()).unwrap();
            let plan = Plan::new(&validated, &pk).unwrap();
            let temps = strings(&case["temps"]);
            assert_eq!(plan.names[plan.fields..], temps, "{what}");
            let chunks = rows
                .iter()
                .map(|chunk| {
                    chunk
                        .as_array()
                        .unwrap()
                        .iter()
                        .enumerate()
                        .map(|(i, row)| {
                            let score = float(&row[1]).unwrap();
                            plan.row(i, value_of(&row[0]), score, |name| {
                                Ok(value_of(&row[2][name]))
                            })
                            .unwrap()
                        })
                        .collect()
                })
                .collect();
            match (plan.execute(chunks), case.get("error")) {
                (Err(EngineError::InvalidArgument(message)), Some(expected)) => {
                    assert_eq!(message, expected.as_str().unwrap(), "{what}")
                }
                (Ok(output), None) => {
                    let got: Vec<Vec<Vec<String>>> = output
                        .iter()
                        .map(|chunk| {
                            chunk
                                .iter()
                                .map(|row| {
                                    let mut cells =
                                        vec![row.values[ID_SLOT].repr(), row.score().repr()];
                                    cells.extend(row.values[plan.fields..].iter().map(PyVal::repr));
                                    cells
                                })
                                .collect()
                        })
                        .collect();
                    let expected: Vec<Vec<Vec<String>>> = case["output"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|chunk| chunk.as_array().unwrap().iter().map(strings).collect())
                        .collect();
                    if uses_decay {
                        // decay 的曲线经过 `exp` / `ln`：3.x 的 `math.exp` 与本实现的 `f64::exp` 都调用平台 libm，
                        // 真值在 macOS 上生成，glibc 在少数输入上差最后一位。这类用例的浮点值允许几个 ULP，
                        // 其余单元格（id、整数、None、错误）与其他用例仍逐位比较。
                        let close = |a: &str, b: &str| {
                            a == b
                                || matches!((a.parse::<f64>(), b.parse::<f64>()), (Ok(x), Ok(y))
                                    if x.is_finite() && y.is_finite() && x.signum() == y.signum()
                                        && x.to_bits().abs_diff(y.to_bits()) <= DECAY_ULPS)
                        };
                        let same = got.len() == expected.len()
                            && got.iter().zip(&expected).all(|(g, e)| {
                                g.len() == e.len()
                                    && g.iter().zip(e).all(|(gr, er)| {
                                        gr.len() == er.len()
                                            && gr.iter().zip(er).all(|(a, b)| close(a, b))
                                    })
                            });
                        assert!(same, "{what}\n got {got:?}\n expected {expected:?}");
                    } else {
                        assert_eq!(got, expected, "{what}");
                    }
                }
                (got, expected) => panic!("{what}: got {got:?}, expected {expected:?}"),
            }
        }
    }

    #[test]
    fn rerank_model_is_never_compiled() {
        let chain = check_with(
            vec![map(
                "$score",
                expr(
                    "rerank_model",
                    vec![col("s")],
                    &[
                        ("provider", text("cohere")),
                        ("queries", Param::Array(vec![text("q")])),
                    ],
                ),
            )],
            &schema(),
            1,
        )
        .unwrap();
        assert!(matches!(
            Plan::new(&chain, "id"),
            Err(EngineError::Unsupported { .. })
        ));
    }
}
