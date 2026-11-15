//! 公共 function chain（0035）：与协议无关的链表示，以及按 schema 的校验。
//!
//! 对应 3.x 的 `function/repr.py` 与 `function/validator.py`。校验的先后与文案逐条照 3.x：
//! 每个算子的错误带 `function chain op[i]` 前缀，表达式内部的错误再带 `expression <名字>`。

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
}
