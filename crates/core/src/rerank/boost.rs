//! boost（0026 第 5 节）：在引擎的内部距离上变换，排序之后才转成对外分数。

use std::cmp::Ordering;

use super::pyval::PyVal;
use crate::error::{EngineError, Result};
use crate::filter::RowFilter;
use crate::index::Metric;

/// `$id`、`$distance`：boost 链的虚拟列名。同名的非主键字段或投影内的动态键会盖掉虚拟值，
/// 排序后又从 entity 剥掉。
pub(crate) const ID_FIELD: &str = "$id";
pub(crate) const DISTANCE_FIELD: &str = "$distance";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Multiply,
    Sum,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoostFunction {
    pub weight: f64,
    /// 空串已视为没有。
    pub filter: Option<String>,
    pub random_score: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoostSpec {
    pub functions: Vec<BoostFunction>,
    pub boost_mode: Mode,
    pub function_mode: Mode,
}

/// 一次检索的 boost：`keep` 是 offset 之后最多保留的命中数（3.x 的 `hits[offset : offset + top_k]`，
/// offset 不为正时是 `hits[:top_k]`）。它不能由 `limit` 推出：负 offset 缩小了 `limit + offset`，却不缩小这里。
#[derive(Debug, Clone, PartialEq)]
pub struct BoostPlan {
    pub spec: BoostSpec,
    pub keep: usize,
    /// boost 之后是否接 L2 rerank 链（Search + L2、HybridSearch 的各路）。只影响 BM25 对外分数的还原：
    /// 3.x 的对外换算对 BM25 是恒等，接链时链的入口再取负（[`public_score`]）。
    pub chained: bool,
}

fn random_score_unsupported() -> EngineError {
    EngineError::Unsupported {
        feature: "random_score in boost functions",
        reason: "not supported in this build of milvus-lite-rust yet".to_owned(),
    }
}

/// 编译好的 boost 函数；过滤表达式在没有候选时也要编译（3.x 的 `BoostOp.__init__`）。
pub(crate) struct CompiledBoost<'a> {
    spec: &'a BoostSpec,
    filters: Vec<Option<RowFilter>>,
    metric: Metric,
}

impl<'a> CompiledBoost<'a> {
    pub(crate) fn new(
        spec: &'a BoostSpec,
        metric: Metric,
        mut compile: impl FnMut(&str) -> Result<Option<RowFilter>>,
    ) -> Result<Self> {
        let mut filters = Vec::with_capacity(spec.functions.len());
        for function in &spec.functions {
            filters.push(match &function.filter {
                Some(expr) => compile(expr)?,
                None => None,
            });
        }
        Ok(Self {
            spec,
            filters,
            metric,
        })
    }

    /// 一行的 `BoostOp`：`matches(filter)` 判断这一行是否命中。没有函数命中时距离不变，也不读取它。
    pub(crate) fn apply(
        &self,
        distance: &mut PyVal,
        mut matches: impl FnMut(&RowFilter) -> Result<bool>,
    ) -> Result<()> {
        let mut values = Vec::with_capacity(self.spec.functions.len());
        for (function, filter) in self.spec.functions.iter().zip(&self.filters) {
            if let Some(filter) = filter
                && !matches(filter)?
            {
                continue;
            }
            if function.random_score {
                return Err(random_score_unsupported());
            }
            values.push(function.weight);
        }
        if values.is_empty() {
            return Ok(());
        }
        let combined = combine(&values, self.spec.function_mode);
        *distance = PyVal::Float(transform(
            distance.to_float()?,
            combined,
            self.spec.boost_mode,
            self.metric,
        ));
        Ok(())
    }
}

fn combine(values: &[f64], mode: Mode) -> f64 {
    match mode {
        Mode::Multiply => values.iter().product::<f64>(),
        Mode::Sum => super::pyval::python_sum(values.iter().copied()),
    }
}

/// 3.x `_apply_boost_to_distance`。L2 乘在开方后的距离上（对外分数是它的平方），IP 的内部距离是负内积。
fn transform(distance: f64, value: f64, mode: Mode, metric: Metric) -> f64 {
    match (mode, metric) {
        (Mode::Multiply, Metric::Cosine) => 1.0 - (1.0 - distance) * value,
        (Mode::Multiply, _) => distance * value,
        (Mode::Sum, Metric::Cosine | Metric::Ip | Metric::Bm25) => distance - value,
        (Mode::Sum, Metric::L2) => distance + value,
    }
}

/// 一个候选的 `$id` 与 `$distance`，都可能是同名字段的值。
#[derive(Debug)]
pub(crate) struct Row {
    pub(crate) id: PyVal,
    pub(crate) distance: PyVal,
    /// 候选在 `ranked` 中的下标。
    pub(crate) index: usize,
}

/// 3.x `SortOp`（升序，平局键 `$id`）：`None` 与 NaN 最后，相等时按 `$id` 升序。
pub(crate) fn sort_order(a: &Row, b: &Row) -> Result<Ordering> {
    match (a.distance.is_missing(), b.distance.is_missing()) {
        (true, true) => tie(a, b),
        (true, false) => Ok(Ordering::Greater),
        (false, true) => Ok(Ordering::Less),
        (false, false) if a.distance.py_eq(&b.distance) => tie(a, b),
        (false, false) => a.distance.py_order(&b.distance),
    }
}

fn tie(a: &Row, b: &Row) -> Result<Ordering> {
    match (a.id.is_missing(), b.id.is_missing()) {
        (true, true) => Ok(Ordering::Equal),
        (true, false) => Ok(Ordering::Greater),
        (false, true) => Ok(Ordering::Less),
        (false, false) => a.id.py_order(&b.id),
    }
}

/// 3.x `_internal_distance_to_milvus_distance` 再加响应编码的 `float()`。输入可能是同名字段盖掉的值。
/// BM25 的换算是恒等：不接链时只有响应编码的 `float()`（`"1.5"` 得 1.5）；接链时链的入口
/// `hit_score_for_chain` 先做一元负号，非数值报 Python 的类型错误（链再对返回值取负）。
pub(crate) fn public_score(distance: &PyVal, metric: Metric, chained: bool) -> Result<f64> {
    let wrong = |message: String| Err(EngineError::invalid(message));
    let kind = distance.type_name();
    match metric {
        Metric::Cosine => match distance {
            PyVal::Float(_) | PyVal::Int(_) | PyVal::BigInt(_) | PyVal::Bool(_) => {
                Ok(metric.public_from_internal(distance.to_float()?))
            }
            _ => wrong(format!(
                "unsupported operand type(s) for -: 'float' and '{kind}'"
            )),
        },
        Metric::Ip => match distance {
            PyVal::Float(_) | PyVal::Int(_) | PyVal::BigInt(_) | PyVal::Bool(_) => {
                Ok(metric.public_from_internal(distance.to_float()?))
            }
            _ => wrong(format!("bad operand type for unary -: '{kind}'")),
        },
        Metric::Bm25 => match distance {
            PyVal::Float(_) | PyVal::Int(_) | PyVal::BigInt(_) | PyVal::Bool(_) => {
                distance.to_float()
            }
            _ if chained => wrong(format!("bad operand type for unary -: '{kind}'")),
            _ => distance.to_float(),
        },
        Metric::L2 => match distance {
            PyVal::Float(value) => Ok(metric.public_from_internal(*value)),
            // Python 的整数平方是精确的，转成浮点时才舍入。
            PyVal::Int(value) => Ok((i128::from(*value) * i128::from(*value)) as f64),
            PyVal::Bool(value) => Ok(f64::from(u8::from(*value))),
            // 平方仍是 Python 整数，转浮点时才可能溢出，而不是得到无穷大。
            PyVal::BigInt(_) => {
                let squared = metric.public_from_internal(distance.to_float()?);
                if squared.is_finite() {
                    Ok(squared)
                } else {
                    wrong("int too large to convert to float".to_owned())
                }
            }
            PyVal::Str(_) | PyVal::List(_) => wrong(format!(
                "can't multiply sequence by non-int of type '{kind}'"
            )),
            _ => wrong(format!(
                "unsupported operand type(s) for *: '{kind}' and '{kind}'"
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rerank::try_sort_by;

    fn spec(functions: Vec<(f64, bool)>, boost_mode: Mode, function_mode: Mode) -> BoostSpec {
        BoostSpec {
            functions: functions
                .into_iter()
                .map(|(weight, random_score)| BoostFunction {
                    weight,
                    filter: None,
                    random_score,
                })
                .collect(),
            boost_mode,
            function_mode,
        }
    }

    fn message<T: std::fmt::Debug>(result: Result<T>) -> String {
        match result.unwrap_err() {
            EngineError::InvalidArgument(message) => message,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn transform_table_operates_on_internal_distance() {
        use Metric::{Bm25, Cosine, Ip, L2};
        use Mode::{Multiply, Sum};
        // COSINE：内部距离 0.25 即相似度 0.75；乘 2 得相似度 1.5，内部距离 -0.5。
        assert_eq!(transform(0.25, 2.0, Multiply, Cosine), -0.5);
        assert_eq!(transform(0.25, 2.0, Sum, Cosine), -1.75);
        // IP：内部距离是负内积。
        assert_eq!(transform(-3.0, 2.0, Multiply, Ip), -6.0);
        assert_eq!(transform(-3.0, 2.0, Sum, Ip), -5.0);
        // L2：内部距离是开方后的距离。
        assert_eq!(transform(3.0, 2.0, Multiply, L2), 6.0);
        assert_eq!(transform(3.0, 2.0, Sum, L2), 5.0);
        // BM25：内部距离就是 `-score`，与 IP 同向（3.x `metric_type in ("IP", "BM25")`）。
        assert_eq!(transform(-3.0, 2.0, Multiply, Bm25), -6.0);
        assert_eq!(transform(-3.0, 2.0, Sum, Bm25), -5.0);
    }

    #[test]
    fn two_spaces_convert_once() {
        // 对外分数：L2 的权重乘在开方距离上，所以分数变为 4 倍，而不是 2 倍。
        let score = 9.0_f32;
        let internal = f64::from(Metric::L2.internal(score));
        assert_eq!(
            Metric::L2.public_from_internal(transform(internal, 2.0, Mode::Multiply, Metric::L2)),
            36.0
        );
        let score = 0.75_f32;
        let internal = f64::from(Metric::Cosine.internal(score));
        assert_eq!(
            Metric::Cosine.public_from_internal(transform(
                internal,
                2.0,
                Mode::Multiply,
                Metric::Cosine
            )),
            1.5
        );
        let score = 3.0_f32;
        let internal = f64::from(Metric::Ip.internal(score));
        assert_eq!(internal, -3.0);
        assert_eq!(
            Metric::Ip.public_from_internal(transform(internal, 2.0, Mode::Multiply, Metric::Ip)),
            6.0
        );
    }

    #[test]
    fn function_modes_combine_matching_weights() {
        let compile = |_: &str| -> Result<Option<RowFilter>> { Ok(None) };
        let run = |spec: &BoostSpec| {
            let boost = CompiledBoost::new(spec, Metric::L2, compile).unwrap();
            let mut distance = PyVal::Float(1.0);
            boost.apply(&mut distance, |_| Ok(true)).unwrap();
            distance
        };
        let two = vec![(2.0, false), (3.0, false)];
        assert_eq!(
            run(&spec(two.clone(), Mode::Multiply, Mode::Multiply)),
            PyVal::Float(6.0)
        );
        assert_eq!(
            run(&spec(two.clone(), Mode::Multiply, Mode::Sum)),
            PyVal::Float(5.0)
        );
        assert_eq!(
            run(&spec(two, Mode::Sum, Mode::Multiply)),
            PyVal::Float(7.0)
        );
        // 没有函数：距离不变。
        assert_eq!(
            run(&spec(vec![], Mode::Multiply, Mode::Multiply)),
            PyVal::Float(1.0)
        );
    }

    #[test]
    fn sum_uses_compensated_summation_like_python() {
        // Python 3.12：sum([1e100, 1.0, -1e100, 1e-20]) == 1.0
        assert_eq!(
            crate::rerank::pyval::python_sum([1e100, 1.0, -1e100, 1e-20]),
            1.0
        );
        assert_eq!(
            crate::rerank::pyval::python_sum([0.1, 0.2]),
            0.30000000000000004
        );
        assert_eq!(
            crate::rerank::pyval::python_sum([f64::INFINITY, 1.0]),
            f64::INFINITY
        );
        assert!(crate::rerank::pyval::python_sum([f64::INFINITY, f64::NEG_INFINITY]).is_nan());
    }

    #[test]
    fn random_score_is_rejected_only_when_a_row_uses_it() {
        let spec = spec(vec![(2.0, true)], Mode::Multiply, Mode::Multiply);
        let boost = CompiledBoost::new(&spec, Metric::L2, |_| Ok(None)).unwrap();
        let mut distance = PyVal::Float(1.0);
        assert!(matches!(
            boost.apply(&mut distance, |_| Ok(true)),
            Err(EngineError::Unsupported { .. })
        ));
    }

    #[test]
    fn non_numeric_distance_is_only_read_when_a_function_matches() {
        let spec = spec(vec![(2.0, false)], Mode::Multiply, Mode::Multiply);
        let boost = CompiledBoost::new(&spec, Metric::Cosine, |_| Ok(None)).unwrap();
        let mut distance = PyVal::Str("x".into());
        assert_eq!(
            message(boost.apply(&mut distance, |_| Ok(true))),
            "could not convert string to float: 'x'"
        );
        // 数字字符串命中后变成数字。
        let mut distance = PyVal::Str("1.5".into());
        boost.apply(&mut distance, |_| Ok(true)).unwrap();
        assert_eq!(distance, PyVal::Float(1.0 - (1.0 - 1.5) * 2.0));
    }

    #[test]
    fn sort_puts_missing_last_and_breaks_ties_by_id() {
        let row = |id: PyVal, distance: PyVal, index| Row {
            id,
            distance,
            index,
        };
        let sorted = try_sort_by(
            vec![
                row(PyVal::Int(1), PyVal::Float(f64::NAN), 0),
                row(PyVal::Int(5), PyVal::Float(2.0), 1),
                row(PyVal::Int(3), PyVal::Float(2.0), 2),
                row(PyVal::Int(0), PyVal::Float(f64::NAN), 3),
                row(PyVal::Int(9), PyVal::Float(1.0), 4),
                row(PyVal::Int(2), PyVal::None, 5),
            ],
            &mut sort_order,
        )
        .unwrap();
        let ids: Vec<_> = sorted.iter().map(|r| r.index).collect();
        // NaN 与 None 都排在最后，它们之间按 $id 升序。
        assert_eq!(ids, [4, 2, 1, 3, 0, 5]);

        // VARCHAR 主键按码点。
        let sorted = try_sort_by(
            ["k10", "é", "a", "Z", "k2", "中", "k1", "k9"]
                .iter()
                .enumerate()
                .map(|(i, s)| row(PyVal::Str((*s).into()), PyVal::Float(1.0), i))
                .collect(),
            &mut sort_order,
        )
        .unwrap();
        let names: Vec<_> = sorted
            .iter()
            .map(|r| match &r.id {
                PyVal::Str(s) => s.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(names, ["Z", "a", "k1", "k10", "k2", "k9", "é", "中"]);
    }

    #[test]
    fn sort_reports_mixed_types() {
        let row = |distance| Row {
            id: PyVal::Int(0),
            distance,
            index: 0,
        };
        let failed = try_sort_by(
            vec![row(PyVal::Float(1.0)), row(PyVal::Str("a".into()))],
            &mut sort_order,
        );
        assert!(message(failed).contains("not supported between instances of"));
    }

    #[test]
    fn public_score_reproduces_python_arithmetic_errors() {
        let str_value = PyVal::Str("1".into());
        assert_eq!(
            message(public_score(&str_value, Metric::Cosine, false)),
            "unsupported operand type(s) for -: 'float' and 'str'"
        );
        assert_eq!(
            message(public_score(&str_value, Metric::Ip, false)),
            "bad operand type for unary -: 'str'"
        );
        assert_eq!(
            message(public_score(&str_value, Metric::L2, false)),
            "can't multiply sequence by non-int of type 'str'"
        );
        assert_eq!(
            message(public_score(&PyVal::None, Metric::L2, false)),
            "unsupported operand type(s) for *: 'NoneType' and 'NoneType'"
        );
        assert_eq!(
            public_score(&PyVal::Bool(true), Metric::Cosine, false).unwrap(),
            0.0
        );
        assert_eq!(
            public_score(&PyVal::Int(100), Metric::Ip, false).unwrap(),
            -100.0
        );
        assert_eq!(
            public_score(&PyVal::Int(100), Metric::L2, false).unwrap(),
            10000.0
        );
        // 10^200 本身能转浮点，平方不能。
        let big = PyVal::BigInt(format!("1{}", "0".repeat(200)));
        assert_eq!(
            message(public_score(&big, Metric::L2, false)),
            "int too large to convert to float"
        );
        assert_eq!(public_score(&big, Metric::Ip, false).unwrap(), -1e200);
        let wide = (1_i64 << 53) + 1;
        assert_eq!(
            public_score(&PyVal::Int(wide), Metric::L2, false).unwrap(),
            (i128::from(wide) * i128::from(wide)) as f64
        );
    }

    /// 3.x 实测（boost 的过滤不命中，`$distance` 是 VARCHAR 字段）：不接链时 `float("1.5")` 得 1.5，
    /// 接 rrf 时报 `bad operand type for unary -: 'str'`；数值原样（链再取负）。
    #[test]
    fn bm25_public_score_is_identity_with_python_float() {
        let text = PyVal::Str("1.5".into());
        assert_eq!(public_score(&text, Metric::Bm25, false).unwrap(), 1.5);
        assert_eq!(
            message(public_score(&text, Metric::Bm25, true)),
            "bad operand type for unary -: 'str'"
        );
        assert_eq!(
            message(public_score(&PyVal::Str("x".into()), Metric::Bm25, false)),
            "could not convert string to float: 'x'"
        );
        assert_eq!(
            message(public_score(&PyVal::None, Metric::Bm25, false)),
            "float() argument must be a string or a real number, not 'NoneType'"
        );
        for chained in [false, true] {
            assert_eq!(
                public_score(&PyVal::Float(-2.5), Metric::Bm25, chained).unwrap(),
                -2.5
            );
            assert_eq!(
                public_score(&PyVal::Bool(true), Metric::Bm25, chained).unwrap(),
                1.0
            );
        }
    }
}
