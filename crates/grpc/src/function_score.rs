//! `function_score` 的解码，与 3.x `rerank/boost.py` 的 `decode_hybrid_function_score` 一致：
//! boost 是引擎内的 L0，其余函数（rrf / weighted / decay / model）最多一个，在引擎外的 L2 执行。
//! Search 与 Hybrid 共用这条路径；`decode_boost_function_score` 是 3.x 未使用的旧路径，不是行为真值。

use std::collections::HashMap;

use milvus_lite_core::EngineError;
use milvus_lite_core::error::py_repr;
use milvus_lite_core::rerank::boost::{BoostFunction, BoostSpec, Mode};
use milvus_lite_core::rerank::chain::L2Function;
use serde_json::{Map, Value};

use crate::pb::common::KeyValuePair;
use crate::pb::schema as pb;
use crate::pyint::{self, PyError};
use crate::schema::truthy;

pub(crate) struct Decoded {
    pub(crate) boost: Option<BoostSpec>,
    /// 最多一个 L2 函数，解码期的规整与校验已经做完；其余校验在检索之后（3.x `_validate_rerank_func`）。
    pub(crate) l2: Option<L2Function>,
}

fn schema_error(message: String) -> EngineError {
    EngineError::SchemaValidation(message)
}

/// 3.x `decode_kv_pairs`：能按 JSON 解析就按 JSON，否则保留字符串；同名 key 以后者为准。
fn decode_kv_pairs(pairs: &[KeyValuePair]) -> HashMap<&str, Value> {
    pairs
        .iter()
        .map(|kv| {
            let value = milvus_lite_core::json_text::value_str(&kv.value)
                .unwrap_or_else(|_| Value::String(kv.value.clone()));
            (kv.key.as_str(), value)
        })
        .collect()
}

/// 3.x 的 `str(params.get(key, default)).lower()`，只有小写后是这两个字符串才合法。
fn mode(params: &HashMap<&str, Value>, key: &str) -> Result<Mode, EngineError> {
    let value = params
        .get(key)
        .map_or_else(|| "multiply".to_owned(), pyint::py_str);
    match value.to_lowercase().as_str() {
        "multiply" => Ok(Mode::Multiply),
        "sum" => Ok(Mode::Sum),
        _ => Err(schema_error(format!(
            "FunctionScore params.{key} must be 'Multiply' or 'Sum'"
        ))),
    }
}

/// 没写进 3.x 校验的 `float()` 异常原样上抛（code 1）。
fn py_float(value: &Value) -> Result<f64, EngineError> {
    pyint::float(value).map_err(|e| EngineError::InvalidArgument(e.message().to_owned()))
}

fn boost_function(
    function: &pb::FunctionSchema,
    params: &HashMap<&str, Value>,
) -> Result<BoostFunction, EngineError> {
    let name = py_repr(&function.name);
    if !function.input_field_names.is_empty() {
        return Err(schema_error(format!(
            "Boost Ranker function {name} requires empty input_field_names"
        )));
    }
    let Some(weight) = params.get("weight") else {
        return Err(schema_error(format!(
            "Boost Ranker function {name} requires params.weight"
        )));
    };
    let weight = match pyint::float(weight) {
        Ok(weight) => weight,
        // 3.x 只捕获 TypeError 与 ValueError；整数转浮点溢出是 OverflowError。
        Err(PyError::Overflow(message)) => return Err(EngineError::InvalidArgument(message)),
        Err(_) => {
            return Err(schema_error(format!(
                "Boost Ranker function {name} params.weight must be a number"
            )));
        }
    };
    let filter = match params.get("filter") {
        None | Some(Value::Null) => None,
        Some(Value::String(filter)) => Some(filter.clone()).filter(|f| !f.is_empty()),
        Some(_) => {
            return Err(schema_error(format!(
                "Boost Ranker function {name} params.filter must be a string"
            )));
        }
    };
    let not_object = || {
        schema_error(format!(
            "Boost Ranker function {name} params.random_score must be an object"
        ))
    };
    let random_score = match params.get("random_score") {
        None | Some(Value::Null) => false,
        Some(Value::Object(_)) => true,
        Some(Value::String(text)) => match milvus_lite_core::json_text::value_str(text) {
            Ok(Value::Object(_)) => true,
            _ => return Err(not_object()),
        },
        Some(_) => return Err(not_object()),
    };
    Ok(BoostFunction {
        weight,
        filter,
        random_score,
    })
}

/// 3.x `_normalize_l2_params` 与 `_get_reranker_type`：rrf 的 `k`、weighted 的 `weights` 元素在这里经过
/// `float()`，`norm_score` 规整为布尔；decay 与 model 的参数原样保留。
fn l2_function(
    reranker: &str,
    function: &pb::FunctionSchema,
    params: &HashMap<&str, Value>,
) -> Result<L2Function, EngineError> {
    let owned = || -> Map<String, Value> {
        params
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    };
    Ok(match reranker {
        "rrf" => L2Function::Rrf {
            k: params.get("k").map(py_float).transpose()?.unwrap_or(60.0),
        },
        "weighted" => {
            let mut weights = Vec::new();
            if let Some(value) = params.get("weights") {
                let not_array =
                    || schema_error("Weighted reranker params.weights must be an array".to_owned());
                let parsed;
                let value = match value {
                    Value::String(text) => {
                        parsed = milvus_lite_core::json_text::value_str(text)
                            .map_err(|_| not_array())?;
                        &parsed
                    }
                    other => other,
                };
                let Value::Array(items) = value else {
                    return Err(not_array());
                };
                for item in items {
                    weights.push(py_float(item)?);
                }
            }
            let normalize = match params.get("norm_score") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(Value::String(text)) if text.eq_ignore_ascii_case("true") => true,
                Some(Value::String(text)) if text.eq_ignore_ascii_case("false") => false,
                Some(_) => {
                    return Err(schema_error(
                        "Weighted reranker params.norm_score must be bool".to_owned(),
                    ));
                }
            };
            L2Function::Weighted { weights, normalize }
        }
        "decay" => {
            // 3.x 的规整不看类型：带 `provider` 的 decay 也要先通过 `queries` 的校验。
            if params.get("provider").is_some_and(truthy) {
                check_model_queries(params)?;
            }
            L2Function::Decay {
                inputs: function.input_field_names.clone(),
                params: owned(),
            }
        }
        // `reranker` 是 model，或不在已知类型里但带 `provider`。
        _ => {
            check_model_queries(params)?;
            L2Function::Model {
                inputs: function.input_field_names.clone(),
                params: owned(),
            }
        }
    })
}

fn check_model_queries(params: &HashMap<&str, Value>) -> Result<(), EngineError> {
    let Some(queries) = params.get("queries") else {
        return Err(schema_error(
            "Model reranker requires params.queries".to_owned(),
        ));
    };
    let parsed;
    let queries = match queries {
        Value::String(text) => {
            parsed = milvus_lite_core::json_text::value_str(text).map_err(|_| {
                schema_error("Model reranker params.queries must be a JSON array".to_owned())
            })?;
            &parsed
        }
        other => other,
    };
    let items = match queries {
        Value::Array(items) if !items.is_empty() => items,
        _ => {
            return Err(schema_error(
                "Model reranker params.queries must be a non-empty array".to_owned(),
            ));
        }
    };
    if !items.iter().all(Value::is_string) {
        return Err(schema_error(
            "Model reranker params.queries must contain only strings".to_owned(),
        ));
    }
    Ok(())
}

/// 先校验两个 mode，再按函数顺序逐个解码。函数的 `type` 被忽略（3.x 同）。
pub(crate) fn decode(function_score: Option<&pb::FunctionScore>) -> Result<Decoded, EngineError> {
    let (functions, score_params): (&[pb::FunctionSchema], &[KeyValuePair]) = match function_score {
        Some(score) => (&score.functions, &score.params),
        None => (&[], &[]),
    };
    let score_params = decode_kv_pairs(score_params);
    let boost_mode = mode(&score_params, "boost_mode")?;
    let function_mode = mode(&score_params, "function_mode")?;
    let mut boost = Vec::new();
    let mut l2 = None;
    for function in functions {
        let params = decode_kv_pairs(&function.params);
        let reranker = params
            .get("reranker")
            .map_or_else(String::new, pyint::py_str)
            .to_lowercase();
        if reranker == "boost" {
            boost.push(boost_function(function, &params)?);
            continue;
        }
        if l2.is_some() {
            return Err(schema_error(
                "Hybrid FunctionScore supports at most one non-boost rerank function".to_owned(),
            ));
        }
        if !matches!(reranker.as_str(), "rrf" | "weighted" | "decay" | "model")
            && !params.get("provider").is_some_and(truthy)
        {
            return Err(schema_error(format!(
                "Unsupported Hybrid FunctionScore reranker {}",
                py_repr(&reranker)
            )));
        }
        l2 = Some(l2_function(&reranker, function, &params)?);
    }
    Ok(Decoded {
        boost: (!boost.is_empty()).then_some(BoostSpec {
            functions: boost,
            boost_mode,
            function_mode,
        }),
        l2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(key: &str, value: &str) -> KeyValuePair {
        KeyValuePair {
            key: key.to_owned(),
            value: value.to_owned(),
        }
    }

    fn function(params: &[(&str, &str)]) -> pb::FunctionSchema {
        pb::FunctionSchema {
            name: "f".to_owned(),
            params: params.iter().map(|(k, v)| kv(k, v)).collect(),
            ..Default::default()
        }
    }

    fn score(functions: Vec<pb::FunctionSchema>, params: &[(&str, &str)]) -> pb::FunctionScore {
        pb::FunctionScore {
            functions,
            params: params.iter().map(|(k, v)| kv(k, v)).collect(),
        }
    }

    fn error(functions: Vec<pb::FunctionSchema>, params: &[(&str, &str)]) -> (i32, String) {
        match decode(Some(&score(functions, params))) {
            Ok(_) => panic!("expected an error"),
            Err(EngineError::SchemaValidation(message)) => (6, message),
            Err(EngineError::InvalidArgument(message)) => (1, message),
            Err(other) => panic!("{other:?}"),
        }
    }

    fn boost(params: &[(&str, &str)]) -> BoostFunction {
        let mut all = vec![("reranker", "boost")];
        all.extend_from_slice(params);
        decode(Some(&score(vec![function(&all)], &[])))
            .unwrap()
            .boost
            .unwrap()
            .functions
            .remove(0)
    }

    #[test]
    fn absent_and_empty_function_score_decode_to_nothing() {
        for input in [None, Some(&pb::FunctionScore::default())] {
            let decoded = decode(input).unwrap();
            assert!(decoded.boost.is_none() && decoded.l2.is_none());
        }
    }

    #[test]
    fn boost_weight_accepts_what_python_float_accepts() {
        assert_eq!(boost(&[("weight", "2")]).weight, 2.0);
        assert_eq!(boost(&[("weight", "\"2.5\"")]).weight, 2.5);
        assert_eq!(boost(&[("weight", "2.5")]).weight, 2.5);
        assert_eq!(boost(&[("weight", "true")]).weight, 1.0);
        assert!(boost(&[("weight", "nan")]).weight.is_nan());
        assert_eq!(boost(&[("weight", "inf")]).weight, f64::INFINITY);
        for bad in ["abc", "null", "[1]", "{}"] {
            assert_eq!(
                error(
                    vec![function(&[("reranker", "boost"), ("weight", bad)])],
                    &[]
                ),
                (
                    6,
                    "Boost Ranker function 'f' params.weight must be a number".into()
                ),
                "{bad}"
            );
        }
        assert_eq!(
            error(vec![function(&[("reranker", "boost")])], &[]),
            (6, "Boost Ranker function 'f' requires params.weight".into())
        );
    }

    #[test]
    fn boost_filter_and_random_score_shapes() {
        assert_eq!(
            boost(&[("weight", "1"), ("filter", "age > 1")])
                .filter
                .as_deref(),
            Some("age > 1")
        );
        assert_eq!(boost(&[("weight", "1"), ("filter", "")]).filter, None);
        assert_eq!(boost(&[("weight", "1"), ("filter", "null")]).filter, None);
        // `true` 经 JSON 解码后是布尔，不是字符串。
        assert_eq!(
            error(
                vec![function(&[
                    ("reranker", "boost"),
                    ("weight", "1"),
                    ("filter", "true")
                ])],
                &[]
            ),
            (
                6,
                "Boost Ranker function 'f' params.filter must be a string".into()
            )
        );
        assert!(boost(&[("weight", "1"), ("random_score", "{\"seed\":1}")]).random_score);
        assert!(boost(&[("weight", "1"), ("random_score", "\"{}\"")]).random_score);
        assert!(!boost(&[("weight", "1"), ("random_score", "null")]).random_score);
        for bad in ["abc", "5", "[1]", "\"[1]\""] {
            assert_eq!(
                error(
                    vec![function(&[
                        ("reranker", "boost"),
                        ("weight", "1"),
                        ("random_score", bad)
                    ])],
                    &[]
                ),
                (
                    6,
                    "Boost Ranker function 'f' params.random_score must be an object".into()
                ),
                "{bad}"
            );
        }
        let mut with_input = function(&[("reranker", "boost"), ("weight", "1")]);
        with_input.input_field_names.push("age".to_owned());
        assert_eq!(
            error(vec![with_input], &[]),
            (
                6,
                "Boost Ranker function 'f' requires empty input_field_names".into()
            )
        );
    }

    #[test]
    fn modes_are_checked_first_and_case_insensitive() {
        let decoded = decode(Some(&score(
            vec![function(&[("reranker", "BOOST"), ("weight", "1")])],
            &[("boost_mode", "Sum"), ("function_mode", "MULTIPLY")],
        )))
        .unwrap()
        .boost
        .unwrap();
        assert_eq!(decoded.boost_mode, Mode::Sum);
        assert_eq!(decoded.function_mode, Mode::Multiply);
        // mode 错误先于函数错误。
        assert_eq!(
            error(
                vec![function(&[("reranker", "nope")])],
                &[("function_mode", "max")]
            ),
            (
                6,
                "FunctionScore params.function_mode must be 'Multiply' or 'Sum'".into()
            )
        );
        assert_eq!(
            error(vec![], &[("boost_mode", "5")]),
            (
                6,
                "FunctionScore params.boost_mode must be 'Multiply' or 'Sum'".into()
            )
        );
    }

    #[test]
    fn l2_selection_and_decode_time_errors() {
        let l2 = |params: &[(&str, &str)]| decode(Some(&score(vec![function(params)], &[])));
        assert_eq!(
            l2(&[("reranker", "RRF")]).unwrap().l2,
            Some(L2Function::Rrf { k: 60.0 })
        );
        assert_eq!(
            l2(&[("reranker", "rrf"), ("k", "\" 5 \"")]).unwrap().l2,
            Some(L2Function::Rrf { k: 5.0 })
        );
        assert_eq!(
            l2(&[
                ("reranker", "weighted"),
                ("weights", "\"[0.7, \\\"1\\\"]\""),
                ("norm_score", "\"TRUE\"")
            ])
            .unwrap()
            .l2,
            Some(L2Function::Weighted {
                weights: vec![0.7, 1.0],
                normalize: true
            })
        );
        assert_eq!(
            l2(&[("reranker", "weighted")]).unwrap().l2,
            Some(L2Function::Weighted {
                weights: vec![],
                normalize: false
            })
        );
        assert!(matches!(
            l2(&[("reranker", "decay"), ("scale", "x")]).unwrap().l2,
            Some(L2Function::Decay { params, .. }) if params["scale"] == "x"
        ));
        assert_eq!(
            error(
                vec![function(&[("reranker", "decay"), ("provider", "x")])],
                &[]
            ),
            (6, "Model reranker requires params.queries".into())
        );
        assert_eq!(
            error(vec![function(&[("strategy", "rrf")])], &[]),
            (6, "Unsupported Hybrid FunctionScore reranker ''".into())
        );
        assert_eq!(
            error(vec![function(&[("reranker", "5")])], &[]),
            (6, "Unsupported Hybrid FunctionScore reranker '5'".into())
        );
        assert_eq!(
            error(
                vec![
                    function(&[("reranker", "rrf")]),
                    function(&[("reranker", "decay")])
                ],
                &[]
            ),
            (
                6,
                "Hybrid FunctionScore supports at most one non-boost rerank function".into()
            )
        );
        // k 与权重元素的转换失败是 code 1，其余形态错是 code 6。
        assert_eq!(
            error(vec![function(&[("reranker", "rrf"), ("k", "abc")])], &[]),
            (1, "could not convert string to float: 'abc'".into())
        );
        assert_eq!(
            error(
                vec![function(&[("reranker", "weighted"), ("weights", "5")])],
                &[]
            ),
            (
                6,
                "Weighted reranker params.weights must be an array".into()
            )
        );
        assert_eq!(
            error(
                vec![function(&[
                    ("reranker", "weighted"),
                    ("weights", "[\"x\"]")
                ])],
                &[]
            ),
            (1, "could not convert string to float: 'x'".into())
        );
        assert_eq!(
            error(
                vec![function(&[("reranker", "weighted"), ("norm_score", "1")])],
                &[]
            ),
            (6, "Weighted reranker params.norm_score must be bool".into())
        );
        assert!(l2(&[("reranker", "weighted"), ("norm_score", "\"TRUE\"")]).is_ok());
        // decay 的参数不在解码期校验。
        assert!(l2(&[("reranker", "decay"), ("scale", "x"), ("function", "5")]).is_ok());
    }

    #[test]
    fn model_requires_queries_also_through_provider() {
        let missing = (6, "Model reranker requires params.queries".to_string());
        assert_eq!(
            error(vec![function(&[("reranker", "model")])], &[]),
            missing
        );
        assert_eq!(
            error(vec![function(&[("provider", "cohere")])], &[]),
            missing
        );
        assert_eq!(
            error(
                vec![function(&[("reranker", "model"), ("queries", "[]")])],
                &[]
            ),
            (
                6,
                "Model reranker params.queries must be a non-empty array".into()
            )
        );
        assert_eq!(
            error(
                vec![function(&[("reranker", "model"), ("queries", "[1]")])],
                &[]
            ),
            (
                6,
                "Model reranker params.queries must contain only strings".into()
            )
        );
        assert!(
            decode(Some(&score(
                vec![function(&[("provider", "x"), ("queries", "[\"q\"]")])],
                &[]
            )))
            .unwrap()
            .l2
            .is_some()
        );
    }
}
