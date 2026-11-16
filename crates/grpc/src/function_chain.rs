//! 公共 function chain（0035）的协议解码，与 3.x `translators/function_chain.py`、
//! `function_chain.py` 的 `prepare_search_function_chain` 一致。按 schema 的校验在引擎层
//! （`rerank::public`）。

use milvus_lite_core::error::py_repr;
use milvus_lite_core::rerank::public::{
    self, Arg, ChainRepr, Expr, Op, Param, Params, ValidatedChain,
};
use milvus_lite_core::{CollectionSchema, EngineError};

use crate::pb::schema as pb;
use crate::pb::schema::function_chain_expr_arg::Arg as PbArg;
use crate::pb::schema::function_param_value::Value as PbValue;
use crate::schema::unsupported;

/// Search 请求里与链有关的部分，由 `search::parse` 在取 collection 之前摘出。
pub(crate) struct Request {
    chains: Vec<pb::FunctionChain>,
    has_function_score: bool,
    /// `order_by_fields` 按 JSON 解析后为真（3.x 只用它判冲突）。
    order_by: bool,
    num_queries: usize,
}

impl Request {
    pub(crate) fn new(
        chains: &[pb::FunctionChain],
        has_function_score: bool,
        order_by: bool,
        num_queries: usize,
    ) -> Self {
        Self {
            chains: chains.to_vec(),
            has_function_score,
            order_by,
            num_queries,
        }
    }

    /// 3.x `prepare_search_function_chain` 加上链的编译：晚于度量解析、早于 load。
    /// 没有链时返回 None。
    pub(crate) fn prepare(
        &self,
        schema: &CollectionSchema,
    ) -> Result<Option<ValidatedChain>, EngineError> {
        let fail = |m: &str| Err(EngineError::SchemaValidation(m.to_owned()));
        if self.chains.is_empty() {
            return Ok(None);
        }
        if self.has_function_score {
            return fail("function_score and function_chains cannot be used together");
        }
        if self.order_by {
            return fail("order_by and function rerank cannot be used together");
        }
        let [chain] = self.chains.as_slice() else {
            return fail("ordinary search supports exactly one function chain");
        };
        let validated = public::validate(decode(chain)?, schema, self.num_queries)?;
        // 3.x 在这里创建 provider（没有 key 时报错，有 key 时之后调用外部服务）；本实现不执行（0035 决策 B）。
        if validated.uses_rerank_model {
            return Err(unsupported("function chains: rerank_model"));
        }
        Ok(Some(validated))
    }
}

fn fail<T>(message: String) -> Result<T, EngineError> {
    Err(EngineError::SchemaValidation(message))
}

fn decode_value(value: &pb::FunctionParamValue, path: &str) -> Result<Param, EngineError> {
    Ok(match &value.value {
        None => return fail(format!("{path}: value is not set")),
        Some(PbValue::BoolValue(v)) => Param::Bool(*v),
        Some(PbValue::Int64Value(v)) => Param::Int(*v),
        Some(PbValue::DoubleValue(v)) => Param::Float(*v),
        Some(PbValue::StringValue(v)) => Param::Str(v.clone()),
        Some(PbValue::BytesValue(v)) => Param::Bytes(v.clone()),
        Some(PbValue::ArrayValue(array)) => Param::Array(
            array
                .values
                .iter()
                .enumerate()
                .map(|(index, item)| decode_value(item, &format!("{path}[{index}]")))
                .collect::<Result<_, _>>()?,
        ),
        Some(PbValue::ObjectValue(object)) => {
            Param::Object(decode_map(&object.fields, path, true)?)
        }
    })
}

/// proto 的 map 没有顺序（3.x 按它自己的迭代顺序报第一个错误）；这里按键排序，让结果确定。
fn decode_map(
    map: &std::collections::HashMap<String, pb::FunctionParamValue>,
    path: &str,
    object: bool,
) -> Result<Params, EngineError> {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    keys.into_iter()
        .map(|key| {
            if object && key.is_empty() {
                return fail(format!("{path}: object key must not be empty"));
            }
            let value = decode_value(&map[key], &format!("{path}[{}]", py_repr(key)))?;
            Ok((key.clone(), value))
        })
        .collect()
}

fn decode_expr(expr: &pb::FunctionChainExpr, path: &str) -> Result<Expr, EngineError> {
    if expr.name.is_empty() {
        return fail(format!("{path} expression name must not be empty"));
    }
    let args = expr
        .args
        .iter()
        .enumerate()
        .map(|(index, arg)| {
            let path = format!("{path}.expr.args[{index}]");
            match &arg.arg {
                Some(PbArg::Column(column)) if column.name.is_empty() => {
                    fail(format!("{path} column name must not be empty"))
                }
                Some(PbArg::Column(column)) => Ok(Arg::Column(column.name.clone())),
                Some(PbArg::Literal(value)) => Ok(Arg::Literal(decode_value(
                    value,
                    &format!("{path}.literal"),
                )?)),
                None => fail(format!("{path} argument is not set")),
            }
        })
        .collect::<Result<_, _>>()?;
    Ok(Expr {
        name: expr.name.clone(),
        args,
        params: decode_map(&expr.params, &format!("{path}.expr.params"), false)?,
    })
}

/// 3.x `function_chain_to_repr`：所有算子解码完才检查阶段的枚举值。
fn decode(chain: &pb::FunctionChain) -> Result<ChainRepr, EngineError> {
    let mut ops = Vec::with_capacity(chain.ops.len());
    for (index, op) in chain.ops.iter().enumerate() {
        let path = format!("function chain op[{index}]");
        if op.op.is_empty() {
            return fail(format!("{path} name must not be empty"));
        }
        let expr = op
            .expr
            .as_ref()
            .map(|expr| decode_expr(expr, &path))
            .transpose()?;
        for (what, names) in [("input", &op.inputs), ("output", &op.outputs)] {
            if let Some(position) = names.iter().position(String::is_empty) {
                return fail(format!("{path} {what}[{position}] name must not be empty"));
            }
        }
        ops.push(Op {
            op: op.op.clone(),
            expr,
            inputs: op.inputs.clone(),
            outputs: op.outputs.clone(),
            params: decode_map(&op.params, &format!("{path}.params"), false)?,
        });
    }
    let Ok(stage) = pb::FunctionChainStage::try_from(chain.stage) else {
        return fail(format!(
            "unknown function chain stage value: {}",
            chain.stage
        ));
    };
    Ok(ChainRepr {
        stage: stage.as_str_name().to_owned(),
        ops,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn value(v: PbValue) -> pb::FunctionParamValue {
        pb::FunctionParamValue { value: Some(v) }
    }

    fn column(name: &str) -> pb::FunctionChainExprArg {
        pb::FunctionChainExprArg {
            arg: Some(PbArg::Column(pb::FunctionChainColumnArg {
                name: name.to_owned(),
            })),
        }
    }

    fn num_combine(args: Vec<pb::FunctionChainExprArg>) -> pb::FunctionChainExpr {
        pb::FunctionChainExpr {
            name: "num_combine".to_owned(),
            args,
            params: HashMap::new(),
        }
    }

    fn map(expr: pb::FunctionChainExpr) -> pb::FunctionChainOp {
        pb::FunctionChainOp {
            op: "map".to_owned(),
            expr: Some(expr),
            outputs: vec!["$score".to_owned()],
            ..Default::default()
        }
    }

    fn chain(ops: Vec<pb::FunctionChainOp>) -> pb::FunctionChain {
        pb::FunctionChain {
            name: String::new(),
            stage: pb::FunctionChainStage::L2Rerank as i32,
            ops,
        }
    }

    fn error(chain: pb::FunctionChain) -> String {
        match decode(&chain) {
            Err(EngineError::SchemaValidation(message)) => message,
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    // 期望文案取自真实 3.x（`1320d26`）对同一请求的响应。
    #[test]
    fn decode_errors_match_the_reference() {
        let op = |edit: &dyn Fn(&mut pb::FunctionChainOp)| {
            let mut op = map(num_combine(vec![column("$score"), column("ts")]));
            edit(&mut op);
            error(chain(vec![op]))
        };
        assert_eq!(
            op(&|o| o.op.clear()),
            "function chain op[0] name must not be empty"
        );
        assert_eq!(
            op(&|o| o.expr.as_mut().unwrap().name.clear()),
            "function chain op[0] expression name must not be empty"
        );
        assert_eq!(
            op(&|o| o.expr.as_mut().unwrap().args[0] = pb::FunctionChainExprArg { arg: None }),
            "function chain op[0].expr.args[0] argument is not set"
        );
        assert_eq!(
            op(&|o| o.expr.as_mut().unwrap().args[1] = column("")),
            "function chain op[0].expr.args[1] column name must not be empty"
        );
        assert_eq!(
            op(
                &|o| o.expr.as_mut().unwrap().args[1] = pb::FunctionChainExprArg {
                    arg: Some(PbArg::Literal(pb::FunctionParamValue { value: None }))
                }
            ),
            "function chain op[0].expr.args[1].literal: value is not set"
        );
        assert_eq!(
            op(&|o| {
                o.expr
                    .as_mut()
                    .unwrap()
                    .params
                    .insert("mode".to_owned(), pb::FunctionParamValue { value: None });
            }),
            "function chain op[0].expr.params['mode']: value is not set"
        );
        assert_eq!(
            op(&|o| {
                let object = pb::FunctionParamObject {
                    fields: HashMap::from([(String::new(), value(PbValue::Int64Value(1)))]),
                };
                o.expr
                    .as_mut()
                    .unwrap()
                    .params
                    .insert("weights".to_owned(), value(PbValue::ObjectValue(object)));
            }),
            "function chain op[0].expr.params['weights']: object key must not be empty"
        );
        assert_eq!(
            op(&|o| {
                let array = pb::FunctionParamArray {
                    values: vec![
                        value(PbValue::Int64Value(1)),
                        pb::FunctionParamValue { value: None },
                    ],
                };
                o.params
                    .insert("p".to_owned(), value(PbValue::ArrayValue(array)));
            }),
            "function chain op[0].params['p'][1]: value is not set"
        );
        assert_eq!(
            op(&|o| o.inputs = vec!["a".to_owned(), String::new()]),
            "function chain op[0] input[1] name must not be empty"
        );
        assert_eq!(
            op(&|o| o.outputs = vec![String::new()]),
            "function chain op[0] output[0] name must not be empty"
        );
        // 后面算子的解码错误先于阶段的检查；未知的阶段值在解码的最后报。
        let mut bad = chain(vec![
            map(num_combine(vec![column("$score"), column("ts")])),
            pb::FunctionChainOp::default(),
        ]);
        bad.stage = 99;
        assert_eq!(
            error(bad.clone()),
            "function chain op[1] name must not be empty"
        );
        bad.ops.pop();
        assert_eq!(error(bad), "unknown function chain stage value: 99");
    }

    /// 真实 3.x 的组件真值，由 tests/differential/generate_function_chain_fixtures.py 导出：
    /// 每条链的线格式字节经本实现解码与校验，结果与 3.x `function_chain_to_repr` + `validate_function_chain` 相同。
    #[test]
    fn validation_matches_the_reference_fixture() {
        use milvus_lite_core::{DataType, FieldSchema};
        use prost::Message;
        use serde_json::Value;

        let fixture = milvus_lite_core::json_text::value_str(include_str!(
            "../tests/fixtures/function_chain_validation.json"
        ))
        .unwrap();
        let types = [
            DataType::Bool,
            DataType::Int8,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::Float,
            DataType::Double,
            DataType::VarChar,
            DataType::Json,
            DataType::Array,
            DataType::Timestamptz,
            DataType::Geometry,
            DataType::FloatVector,
            DataType::SparseFloatVector,
        ];
        let schemas: HashMap<&str, CollectionSchema> = fixture["schemas"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, fields)| {
                let fields = fields
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|field| {
                        let name = field[1].as_str().unwrap();
                        let data_type = *types.iter().find(|t| t.name() == name).unwrap();
                        let mut field_schema =
                            FieldSchema::new(field[0].as_str().unwrap(), data_type);
                        field_schema.is_primary = field[2].as_bool().unwrap();
                        field_schema
                    })
                    .collect();
                let schema = CollectionSchema {
                    fields,
                    enable_dynamic_field: true,
                    properties: Vec::new(),
                    functions: Vec::new(),
                };
                (key.as_str(), schema)
            })
            .collect();
        let cases = fixture["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 280);
        for case in cases {
            let hex = case["wire"].as_str().unwrap();
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            let chain = pb::FunctionChain::decode(bytes.as_slice()).unwrap();
            let schema = &schemas[case["schema"].as_str().unwrap()];
            let nq = case["nq"].as_u64().unwrap() as usize;
            let got = decode(&chain).and_then(|repr| public::validate(repr, schema, nq));
            match (got, case.get("error").and_then(Value::as_str)) {
                (Err(EngineError::SchemaValidation(message)), Some(expected)) => {
                    assert_eq!(message, expected, "{hex}")
                }
                (Ok(validated), None) => {
                    let required: Vec<&str> = case["required"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap())
                        .collect();
                    assert_eq!(validated.required_fields, required, "{hex}");
                    assert_eq!(
                        validated.uses_rerank_model,
                        case["rerank_model"].as_bool().unwrap(),
                        "{hex}"
                    );
                }
                (got, expected) => panic!("{hex}: got {got:?}, expected {expected:?}"),
            }
        }
    }

    #[test]
    fn decoded_values_keep_their_types() {
        let mut expr = num_combine(vec![
            column("ts"),
            pb::FunctionChainExprArg {
                arg: Some(PbArg::Literal(value(PbValue::DoubleValue(0.5)))),
            },
        ]);
        let weights = pb::FunctionParamArray {
            values: vec![
                value(PbValue::Int64Value(-3)),
                value(PbValue::BoolValue(true)),
                value(PbValue::BytesValue(b"x".to_vec())),
            ],
        };
        expr.params
            .insert("weights".to_owned(), value(PbValue::ArrayValue(weights)));
        expr.params
            .insert("mode".to_owned(), value(PbValue::StringValue("sum".into())));
        let mut stage = chain(vec![map(expr)]);
        stage.stage = pb::FunctionChainStage::L0Rerank as i32;
        let repr = decode(&stage).unwrap();
        assert_eq!(repr.stage, "FunctionChainStageL0Rerank");
        let expr = repr.ops[0].expr.as_ref().unwrap();
        assert_eq!(
            expr.args,
            [
                Arg::Column("ts".to_owned()),
                Arg::Literal(Param::Float(0.5))
            ]
        );
        assert_eq!(
            expr.params,
            [
                ("mode".to_owned(), Param::Str("sum".to_owned())),
                (
                    "weights".to_owned(),
                    Param::Array(vec![
                        Param::Int(-3),
                        Param::Bool(true),
                        Param::Bytes(b"x".to_vec())
                    ])
                ),
            ]
        );
    }
}
