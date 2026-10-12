//! 顶层动态字段按批绑定类型；异构批按参照实现的 Python 数值与 Kleene 规则求值。

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Index;

use serde_json::{Map, Value};

use super::eval::{Comparison, Operand, Predicate, Term, validate_literal};
use super::parser::Literal;
use crate::data::ColumnData;
use crate::error::{EngineError, Result};
use crate::storage::visibility::SourceBits;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Shape {
    Null,
    Bool,
    Int,
    Float,
    String,
    Complex,
}

pub(crate) struct DynamicRow<'a> {
    pub(crate) columns: &'a [ColumnData],
    pub(crate) meta: Option<&'a Map<String, Value>>,
    pub(crate) row: usize,
}

pub(super) fn evaluate_rows(predicate: &Predicate, rows: &[DynamicRow<'_>]) -> Result<SourceBits> {
    let Some(first) = rows.first() else {
        return Ok(SourceBits::new(0));
    };
    let mut referenced = BTreeSet::new();
    for term in &predicate.terms {
        let operands = match term {
            Term::Compare { left, right, .. } => vec![left, right],
            Term::In { operand, .. } => vec![operand],
        };
        for op in operands {
            if let Operand::Column(i) = op {
                referenced.insert(*i);
            }
        }
    }
    let mut columns = Vec::with_capacity(first.columns.len());
    for i in 0..first.columns.len() {
        if !referenced.contains(&i) {
            columns.push(ColumnData::Bool(Vec::new()));
            continue;
        }
        // 仅物化表达式引用的标量列，向量、JSON 与整个 meta 对象仍借用原快照。
        let mut column = match &first.columns[i] {
            ColumnData::Int8(_) => ColumnData::Int8(Vec::new()),
            ColumnData::Int16(_) => ColumnData::Int16(Vec::new()),
            ColumnData::Int32(_) => ColumnData::Int32(Vec::new()),
            ColumnData::Int64(_) => ColumnData::Int64(Vec::new()),
            ColumnData::Float(_) => ColumnData::Float(Vec::new()),
            ColumnData::Double(_) => ColumnData::Double(Vec::new()),
            ColumnData::Bool(_) => ColumnData::Bool(Vec::new()),
            ColumnData::VarChar(_) => ColumnData::VarChar(Vec::new()),
            _ => {
                return Err(EngineError::Storage(
                    "dynamic scalar binding contains a non-scalar column".into(),
                ));
            }
        };
        for row in rows {
            column.push_row(&row.columns[i], row.row);
        }
        columns.push(column);
    }
    let meta: Vec<_> = rows.iter().map(|row| row.meta).collect();
    evaluate(
        predicate,
        &columns,
        &meta,
        &SourceBits::from_live(&vec![true; rows.len()]),
    )
}

fn overflow() -> EngineError {
    EngineError::invalid("Python int too large to convert to C long")
}

fn number_float(raw: &str) -> Result<f64> {
    let value = raw.parse::<f64>().map_err(|_| overflow())?;
    if !raw.contains(['.', 'e', 'E']) && value.is_infinite() {
        return Err(EngineError::invalid("int too large to convert to float"));
    }
    Ok(value)
}

// None 表示 Arrow 类型推断/安全转换会触发 hybrid 的整批 Python 路径；真实溢出仍向上传递。
fn infer(values: &[&Value]) -> Result<Option<Shape>> {
    let values: Vec<_> = values.iter().copied().filter(|v| !v.is_null()).collect();
    if values.is_empty() {
        return Ok(Some(Shape::Null));
    }
    if values.iter().all(|v| v.is_number()) {
        let float = values.iter().any(|v| {
            v.as_number()
                .is_some_and(|n| n.as_str().contains(['.', 'e', 'E']))
        });
        for value in &values {
            let Value::Number(number) = value else {
                return Ok(None);
            };
            if !float {
                number.as_str().parse::<i64>().map_err(|_| overflow())?;
            } else {
                number_float(number.as_str())?;
                if !number.as_str().contains(['.', 'e', 'E'])
                    && (int_cmp(number.as_str(), "-9007199254740992").is_lt()
                        || int_cmp(number.as_str(), "9007199254740992").is_gt())
                {
                    return Ok(None);
                }
            }
        }
        return Ok(Some(if float { Shape::Float } else { Shape::Int }));
    }
    if values.iter().all(|v| v.is_boolean()) {
        return Ok(Some(Shape::Bool));
    }
    if values.iter().all(|v| v.is_string()) {
        return Ok(Some(Shape::String));
    }
    if values.iter().all(|v| v.is_array()) {
        let inner: Vec<_> = values
            .iter()
            .filter_map(|v| v.as_array())
            .flatten()
            .collect();
        return Ok(infer(&inner)?.map(|_| Shape::Complex));
    }
    if values.iter().all(|v| v.is_object()) {
        let keys: BTreeSet<_> = values
            .iter()
            .filter_map(|v| v.as_object())
            .flat_map(|v| v.keys())
            .collect();
        for key in keys {
            let inner: Vec<_> = values.iter().filter_map(|v| v.get(key)).collect();
            if infer(&inner)?.is_none() {
                return Ok(None);
            }
        }
        return Ok(Some(Shape::Complex));
    }
    Ok(None)
}

fn meta_at<'a>(
    meta: &[Option<&'a Map<String, Value>>],
    key: &str,
    row: usize,
) -> Option<&'a Value> {
    // 有 meta 列时长度由写入/恢复校验保证；没有列与缺少键均沿用参照的 null 语义。
    meta[row]
        .and_then(|row| row.get(key))
        .filter(|v| !v.is_null())
}

struct Columns<'a> {
    base: &'a [ColumnData],
    extra: Vec<ColumnData>,
}
impl Index<usize> for Columns<'_> {
    type Output = ColumnData;
    fn index(&self, i: usize) -> &Self::Output {
        if i < self.base.len() {
            &self.base[i]
        } else {
            &self.extra[i - self.base.len()]
        }
    }
}
struct Bound {
    index: usize,
    shape: Shape,
    live: SourceBits,
}

fn materialize(
    meta: &[Option<&Map<String, Value>>],
    key: &str,
    shape: Shape,
    live: &SourceBits,
) -> Result<(ColumnData, SourceBits)> {
    let mut valid = SourceBits::new(live.len);
    let values: Vec<_> = (0..live.len)
        .map(|row| {
            let value = live.get(row).then(|| meta_at(meta, key, row)).flatten();
            if value.is_some() {
                valid.set(row, true);
            }
            value
        })
        .collect();
    let column = match shape {
        Shape::Int => ColumnData::Int64(
            values
                .iter()
                .map(|v| match v {
                    Some(Value::Number(n)) => n.as_str().parse().map_err(|_| overflow()),
                    _ => Ok(0),
                })
                .collect::<Result<_>>()?,
        ),
        Shape::Float => ColumnData::Double(
            values
                .iter()
                .map(|v| match v {
                    Some(Value::Number(n)) => number_float(n.as_str()),
                    _ => Ok(0.0),
                })
                .collect::<Result<_>>()?,
        ),
        Shape::String => ColumnData::VarChar(
            values
                .iter()
                .map(|v| v.and_then(|v| v.as_str()).unwrap_or("").to_owned())
                .collect(),
        ),
        Shape::Bool => ColumnData::Bool(
            values
                .iter()
                .map(|v| v.and_then(|v| v.as_bool()).unwrap_or(false))
                .collect(),
        ),
        Shape::Null | Shape::Complex => ColumnData::Bool(vec![false; live.len]),
    };
    Ok((column, valid))
}

fn evaluate(
    predicate: &Predicate,
    columns: &[ColumnData],
    meta: &[Option<&Map<String, Value>>],
    live: &SourceBits,
) -> Result<SourceBits> {
    if !live.any_live() {
        return Ok(live.clone());
    }
    let mut keys = BTreeSet::new();
    for term in &predicate.terms {
        match term {
            Term::Compare { left, right, .. } => {
                for operand in [left, right] {
                    if let Operand::Dynamic(key) = operand {
                        keys.insert(key.as_str());
                    }
                }
            }
            Term::In {
                operand: Operand::Dynamic(key),
                ..
            } => {
                keys.insert(key.as_str());
            }
            _ => {}
        }
    }
    let mut view = Columns {
        base: columns,
        extra: Vec::new(),
    };
    let mut bindings = BTreeMap::new();
    for key in &keys {
        let values: Vec<_> = live
            .live_rows()
            .filter_map(|row| meta_at(meta, key, row))
            .collect();
        let Some(shape) = infer(&values)? else {
            return evaluate_python(predicate, columns, meta, live);
        };
        let (column, valid) = materialize(meta, key, shape, live)?;
        let index = columns.len() + view.extra.len();
        view.extra.push(column);
        bindings.insert(
            *key,
            Bound {
                index,
                shape,
                live: valid,
            },
        );
    }
    let names = predicate
        .field_names
        .as_deref()
        .ok_or_else(|| EngineError::invalid("dynamic filter has no field bindings"))?;
    let collision = |operand: &Operand| {
        let name = match operand {
            Operand::Column(i) => Cow::Borrowed(names[*i].as_str()),
            Operand::Dynamic(key) => Cow::Owned(format!("__meta__{key}")),
            _ => return false,
        };
        names.iter().any(|n| n == name.as_ref())
            && keys.iter().any(|key| name == format!("__meta__{key}"))
    };
    let bind = |operand: &Operand| match operand {
        Operand::Dynamic(key) => bindings
            .get(key.as_str())
            .map(|b| Operand::Column(b.index))
            .ok_or_else(|| EngineError::invalid("dynamic filter binding is missing")),
        other => Ok(other.clone()),
    };
    let mut out = live.clone();
    for term in &predicate.terms {
        let (operands, dynamic, literals): (Vec<&Operand>, Option<&str>, Vec<&Literal>) = match term
        {
            Term::Compare { left, right, .. } => {
                let dynamic = [left, right].iter().find_map(|op| {
                    if let Operand::Dynamic(key) = op {
                        Some(key.as_str())
                    } else {
                        None
                    }
                });
                let literals = [left, right]
                    .iter()
                    .filter_map(|op| {
                        if let Operand::Literal(l) = op {
                            Some(l)
                        } else {
                            None
                        }
                    })
                    .collect();
                (vec![left, right], dynamic, literals)
            }
            Term::In { operand, values } => (
                vec![operand],
                if let Operand::Dynamic(key) = operand {
                    Some(key.as_str())
                } else {
                    None
                },
                values.iter().collect(),
            ),
        };
        // Arrow 先访问左操作数，再访问右操作数；同名合成列失败不能掩盖更早的整数溢出。
        for operand in operands {
            if collision(operand) {
                return evaluate_python(predicate, columns, meta, live);
            }
            validate_literal(operand)?;
        }
        for literal in &literals {
            validate_literal(&Operand::Literal((*literal).clone()))?;
        }
        let bound = dynamic.and_then(|key| bindings.get(key));
        let converted = if let Term::In { values, .. } = term {
            if values.iter().any(|v| matches!(v, Literal::Float(_))) && values.iter().any(|v| matches!(v, Literal::Int(text) if text.parse::<i64>().is_ok_and(|i| !(-9_007_199_254_740_992..=9_007_199_254_740_992).contains(&i)))) {
                return evaluate_python(predicate, columns, meta, live);
            }
            if let Some(bound) = bound {
                match members(bound.shape, values) {
                    Some(values) => Some(values),
                    None => return evaluate_python(predicate, columns, meta, live),
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some(bound) = bound {
            if matches!(term, Term::In { values, .. } if values.is_empty()) {
                out = SourceBits::new(live.len);
                continue;
            }
            if bound.shape == Shape::Null {
                out = SourceBits::new(live.len);
                continue;
            }
            let compatible = converted.is_some()
                || literals.iter().all(|literal| {
                    matches!(
                        (bound.shape, literal),
                        (
                            Shape::Int | Shape::Float,
                            Literal::Int(_) | Literal::Float(_)
                        ) | (Shape::Bool, Literal::Bool(_))
                            | (Shape::String, Literal::String(_))
                    )
                });
            if !compatible {
                return evaluate_python(predicate, columns, meta, live);
            }
        }
        let term = match term {
            Term::Compare { op, left, right } => Term::Compare {
                op: *op,
                left: bind(left)?,
                right: bind(right)?,
            },
            Term::In { operand, values } => Term::In {
                operand: bind(operand)?,
                values: converted.unwrap_or_else(|| values.clone()),
            },
        };
        let active = bound.map_or(live, |b| &b.live);
        let one = Predicate {
            terms: vec![term],
            field_names: None,
        };
        let matching = match one.evaluate_columns(&view, active) {
            Ok(mask) => mask,
            // 字面量已验证，支持列类型已绑定；此边界的 InvalidArgument 仅来自 Arrow 安全数值转换。
            Err(EngineError::InvalidArgument(_)) => {
                return evaluate_python(predicate, columns, meta, live);
            }
            Err(error) => return Err(error),
        };
        out.intersect_matching(live, |row| matching.get(row));
    }
    Ok(out)
}

fn int_string(raw: &str) -> Option<i64> {
    if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).ok().map(|v| v as i64);
    }
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| raw.parse().ok())
        .flatten()
}

fn float_string(raw: &str) -> Option<f64> {
    let lower = raw.to_ascii_lowercase();
    let value = lower.strip_prefix(['+', '-']).unwrap_or(&lower);
    if let Some(payload) = value.strip_prefix("nan(").and_then(|s| s.strip_suffix(')'))
        && payload
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Some(f64::NAN);
    }
    raw.parse().ok()
}

// is_in 先把整个 value_set 安全转成列类型；不支持的类型配对才选择参照的 Python 路径。
fn members(shape: Shape, values: &[Literal]) -> Option<Vec<Literal>> {
    if values.is_empty() {
        return Some(values.to_vec());
    }
    if shape == Shape::Null {
        return (!values.iter().any(|v| matches!(v, Literal::String(_)))).then(|| values.to_vec());
    }
    values
        .iter()
        .map(|value| match (shape, value) {
            (Shape::Int, Literal::Bool(b)) => Some(Literal::Int(if *b { "1" } else { "0" }.into())),
            (Shape::Int, Literal::String(s)) => int_string(s).map(|i| Literal::Int(i.to_string())),
            (Shape::Float, Literal::Bool(b)) => Some(Literal::Float(if *b { 1.0 } else { 0.0 })),
            (Shape::Float, Literal::String(s)) => float_string(s).map(Literal::Float),
            (Shape::Bool, Literal::Int(s)) => Some(Literal::Bool(int_cmp(s, "0").is_ne())),
            (Shape::Bool, Literal::Float(f)) => Some(Literal::Bool(*f != 0.0)),
            (Shape::Bool, Literal::String(s)) => match s.to_ascii_lowercase().as_str() {
                "true" | "1" => Some(Literal::Bool(true)),
                "false" | "0" => Some(Literal::Bool(false)),
                _ => None,
            },
            (Shape::Int | Shape::Float, Literal::Int(_) | Literal::Float(_))
            | (Shape::Bool, Literal::Bool(_))
            | (Shape::String, Literal::String(_)) => Some(value.clone()),
            _ => None,
        })
        .collect()
}

enum PyValue<'a> {
    Null,
    Int(Cow<'a, str>),
    Float(f64),
    String(&'a str),
    List,
    Object,
}

fn json_value(value: Option<&Value>) -> Result<PyValue<'_>> {
    Ok(match value {
        None | Some(Value::Null) => PyValue::Null,
        Some(Value::Bool(b)) => PyValue::Int(Cow::Borrowed(if *b { "1" } else { "0" })),
        Some(Value::Number(n)) if n.as_str().contains(['.', 'e', 'E']) => {
            PyValue::Float(number_float(n.as_str())?)
        }
        Some(Value::Number(n)) => PyValue::Int(Cow::Borrowed(n.as_str())),
        Some(Value::String(s)) => PyValue::String(s),
        Some(Value::Array(_)) => PyValue::List,
        Some(Value::Object(_)) => PyValue::Object,
    })
}

fn py_value<'a>(
    operand: &'a Operand,
    columns: &'a [ColumnData],
    meta: &[Option<&'a Map<String, Value>>],
    row: usize,
) -> Result<PyValue<'a>> {
    Ok(match operand {
        Operand::Dynamic(key) => return json_value(meta_at(meta, key, row)),
        Operand::Literal(Literal::Int(s)) => PyValue::Int(Cow::Borrowed(s)),
        Operand::Literal(Literal::Float(f)) => PyValue::Float(*f),
        Operand::Literal(Literal::String(s)) => PyValue::String(s),
        Operand::Literal(Literal::Bool(b)) => {
            PyValue::Int(Cow::Borrowed(if *b { "1" } else { "0" }))
        }
        Operand::Column(i) => match &columns[*i] {
            ColumnData::Int8(v) => PyValue::Int(Cow::Owned(v[row].to_string())),
            ColumnData::Int16(v) => PyValue::Int(Cow::Owned(v[row].to_string())),
            ColumnData::Int32(v) => PyValue::Int(Cow::Owned(v[row].to_string())),
            ColumnData::Int64(v) => PyValue::Int(Cow::Owned(v[row].to_string())),
            ColumnData::Float(v) => PyValue::Float(f64::from(v[row])),
            ColumnData::Double(v) => PyValue::Float(v[row]),
            ColumnData::Bool(v) => PyValue::Int(Cow::Borrowed(if v[row] { "1" } else { "0" })),
            ColumnData::VarChar(v) => PyValue::String(&v[row]),
            _ => return Err(EngineError::invalid("unsupported Python filter column")),
        },
        _ => return Err(EngineError::invalid("unsupported Python filter literal")),
    })
}

fn int_cmp(a: &str, b: &str) -> Ordering {
    fn split(raw: &str) -> (bool, &str) {
        let digits = raw.trim_start_matches('-').trim_start_matches('0');
        (raw.starts_with('-') && !digits.is_empty(), digits)
    }
    let (an, a) = split(a);
    let (bn, b) = split(b);
    if an != bn {
        return bn.cmp(&an);
    }
    let order = a.len().cmp(&b.len()).then_with(|| a.cmp(b));
    if an { order.reverse() } else { order }
}

fn int_float_cmp(integer: &str, float: f64) -> Option<Ordering> {
    if float.is_nan() {
        return None;
    }
    if float.is_infinite() {
        return Some(if float.is_sign_positive() {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    // Python 不把宽整数先转成 f64；与浮点的整数下界按精确十进制比较，保留 2^53/2^63 边界。
    let floor = format!("{:.0}", float.floor());
    let order = int_cmp(integer, &floor);
    Some(if order.is_eq() && float.fract() != 0.0 {
        Ordering::Less
    } else {
        order
    })
}

fn compare(a: &PyValue<'_>, b: &PyValue<'_>, op: Comparison) -> Option<bool> {
    if matches!(a, PyValue::Null) || matches!(b, PyValue::Null) {
        return None;
    }
    let order = match (a, b) {
        (PyValue::Int(a), PyValue::Int(b)) => Some(int_cmp(a, b)),
        (PyValue::Int(a), PyValue::Float(b)) => int_float_cmp(a, *b),
        (PyValue::Float(a), PyValue::Int(b)) => int_float_cmp(b, *a).map(Ordering::reverse),
        (PyValue::Float(a), PyValue::Float(b)) => a.partial_cmp(b),
        (PyValue::String(a), PyValue::String(b)) => Some(a.cmp(b)),
        // Python 的跨类型大小比较是 TypeError（Unknown）；数值 NaN 的无序比较仍是 False。
        _ => {
            return match op {
                Comparison::Equal => Some(false),
                Comparison::NotEqual => Some(true),
                _ => None,
            };
        }
    };
    Some(match op {
        Comparison::Equal => order.is_some_and(|o| o.is_eq()),
        Comparison::NotEqual => !order.is_some_and(|o| o.is_eq()),
        Comparison::Less => order.is_some_and(|o| o.is_lt()),
        Comparison::LessEqual => order.is_some_and(|o| o.is_le()),
        Comparison::Greater => order.is_some_and(|o| o.is_gt()),
        Comparison::GreaterEqual => order.is_some_and(|o| o.is_ge()),
    })
}

fn evaluate_python(
    predicate: &Predicate,
    columns: &[ColumnData],
    meta: &[Option<&Map<String, Value>>],
    live: &SourceBits,
) -> Result<SourceBits> {
    let mut out = SourceBits::new(live.len);
    for row in live.live_rows() {
        let mut unknown = false;
        let mut matched = true;
        for term in &predicate.terms {
            let result = match term {
                Term::Compare { op, left, right } => compare(
                    &py_value(left, columns, meta, row)?,
                    &py_value(right, columns, meta, row)?,
                    *op,
                ),
                Term::In { operand, values } => {
                    let value = py_value(operand, columns, meta, row)?;
                    if matches!(value, PyValue::Null) {
                        None
                    } else {
                        match value {
                            PyValue::List => {
                                return Err(EngineError::invalid("unhashable type: 'list'"));
                            }
                            PyValue::Object => {
                                return Err(EngineError::invalid("unhashable type: 'dict'"));
                            }
                            _ => {}
                        }
                        let mut found = false;
                        for literal in values {
                            let operand = Operand::Literal(literal.clone());
                            if compare(
                                &value,
                                &py_value(&operand, columns, meta, row)?,
                                Comparison::Equal,
                            ) == Some(true)
                            {
                                found = true;
                                break;
                            }
                        }
                        Some(found)
                    }
                }
            };
            match result {
                Some(false) => {
                    matched = false;
                    break;
                }
                None => unknown = true,
                Some(true) => {}
            }
        }
        if matched && !unknown {
            out.set(row, true);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{CollectionSchema, DataType, FieldSchema};

    #[test]
    fn python_comparison_separates_nan_from_unknown_type_ordering() {
        let ops = [
            Comparison::Equal,
            Comparison::NotEqual,
            Comparison::Less,
            Comparison::LessEqual,
            Comparison::Greater,
            Comparison::GreaterEqual,
        ];
        let numeric = PyValue::Int(Cow::Borrowed("1"));
        let string = PyValue::String("c");
        for value in [
            PyValue::Int(Cow::Borrowed("1")),
            PyValue::Float(1.5),
            PyValue::List,
            PyValue::Object,
        ] {
            for op in ops {
                let expected = match op {
                    Comparison::Equal => Some(false),
                    Comparison::NotEqual => Some(true),
                    _ => None,
                };
                assert_eq!(compare(&value, &string, op), expected);
                assert_eq!(compare(&string, &value, op), expected);
            }
        }
        for value in [PyValue::List, PyValue::Object] {
            for op in ops {
                let expected = match op {
                    Comparison::Equal => Some(false),
                    Comparison::NotEqual => Some(true),
                    _ => None,
                };
                assert_eq!(compare(&value, &numeric, op), expected);
                assert_eq!(compare(&numeric, &value, op), expected);
            }
        }
        for value in [
            PyValue::Int(Cow::Borrowed("1")),
            PyValue::Float(1.5),
            PyValue::Float(f64::NAN),
        ] {
            for op in ops {
                let expected = Some(matches!(op, Comparison::NotEqual));
                assert_eq!(compare(&PyValue::Float(f64::NAN), &value, op), expected);
                assert_eq!(compare(&value, &PyValue::Float(f64::NAN), op), expected);
            }
        }
        for op in ops {
            assert_eq!(compare(&PyValue::Null, &numeric, op), None);
        }
    }

    #[test]
    fn dynamic_binding_uses_original_live_rows_and_does_not_change_old_masks() {
        let schema = CollectionSchema {
            fields: vec![FieldSchema {
                is_primary: true,
                ..FieldSchema::new("id", DataType::Int64)
            }],
            enable_dynamic_field: true,
            properties: vec![],
        };
        let columns = vec![ColumnData::Int64(vec![0, 1, 2])];
        let meta = vec![
            serde_json::json!({"x":9223372036854775808_u64}),
            serde_json::json!({"x":1}),
            serde_json::json!({"x":null}),
        ];
        let meta: Vec<_> = meta
            .into_iter()
            .map(|v| v.as_object().unwrap().clone())
            .collect();
        let live = SourceBits::from_live(&[false, true, true]);
        let predicate = Predicate::compile("x in [1]", &schema).unwrap();
        let got = predicate
            .evaluate_with_meta(&columns, Some(&meta), &live)
            .unwrap();
        assert_eq!(got.live_rows().collect::<Vec<_>>(), vec![1]);
        assert_eq!(live.live_rows().collect::<Vec<_>>(), vec![1, 2]);
        assert!(
            predicate
                .evaluate_with_meta(&columns, None, &live)
                .unwrap()
                .live_rows()
                .next()
                .is_none()
        );
        assert!(
            Predicate::compile("x == 9223372036854775808", &schema)
                .unwrap()
                .evaluate_with_meta(&columns, Some(&meta), &SourceBits::new(3))
                .unwrap()
                .live_rows()
                .next()
                .is_none()
        );
    }

    #[test]
    fn dynamic_batches_match_reference_masks_and_errors() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/filter_dynamic.json")).unwrap();
        let mut failures = Vec::new();
        for (index, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
            let collision = case["collision"].as_bool().unwrap();
            let mut schema = CollectionSchema {
                fields: vec![
                    FieldSchema {
                        is_primary: true,
                        ..FieldSchema::new("id", DataType::Int64)
                    },
                    FieldSchema::new("age", DataType::Int64),
                    FieldSchema {
                        dim: Some(2),
                        ..FieldSchema::new("vector", DataType::FloatVector)
                    },
                ],
                enable_dynamic_field: case["dynamic"].as_bool().unwrap(),
                properties: vec![],
            };
            let mut columns = vec![
                ColumnData::Int64(vec![0, 1, 2]),
                ColumnData::Int64(vec![9_007_199_254_740_993, 1, 1]),
                ColumnData::FloatVector {
                    dim: 2,
                    values: vec![0.0; 6],
                },
            ];
            if collision {
                schema
                    .fields
                    .push(FieldSchema::new("__meta__x", DataType::VarChar));
                columns.push(ColumnData::VarChar(vec!["sentinel".into(); 3]));
            }
            let key = case["key"].as_str().unwrap_or("x");
            let meta: Vec<Map<String, Value>> = case["values"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(i, value)| {
                    let mut row: Map<String, Value> =
                        [(key.into(), value.clone())].into_iter().collect();
                    if let Some(extra) = case["extra"].as_object() {
                        for (key, values) in extra {
                            row.insert(key.clone(), values[i].clone());
                        }
                    }
                    row
                })
                .collect();
            let live = SourceBits::from_live(&[true, true, true]);
            let expr = case["expr"].as_str().unwrap();
            let got = Predicate::compile(expr, &schema)
                .and_then(|p| p.evaluate_with_meta(&columns, Some(&meta), &live));
            let actual = match got {
                Ok(mask) => {
                    serde_json::json!({"mask":(0..3).map(|row|mask.get(row)).collect::<Vec<_>>()})
                }
                Err(error) => serde_json::json!({"error":error.to_string()}),
            };
            if actual.get("mask") != case.get("mask") || actual.get("error") != case.get("error") {
                failures.push(format!(
                    "case {index}: {expr}, values={}, expected={}/{}, got={actual}",
                    case["values"], case["mask"], case["error"]
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} divergences:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
