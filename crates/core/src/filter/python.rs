//! Python 路线与 hybrid 回退共用的值规则；不嵌入解释器。

use super::dynamic::DynamicRow;
use super::eval::{Comparison, Operand, Term};
use super::parser::{Kind, Literal, PathPart};
use super::python_json::{Document, JsonNode, decode};
use super::python_num::{
    self as num, Budget, FILTER_CODE_POINT_BYTES, FILTER_LIST_ELEMENT_BYTES, IntRef, Integer,
    Limits,
};
use super::tree::Tree;
use crate::data::ColumnData;
use crate::error::{EngineError, Result};
use crate::storage::visibility::SourceBits;
use serde_json::Value as Json;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(super) struct LikePattern {
    python: Vec<Token>,
    arrow: Vec<Token>,
}
#[derive(Debug, Clone, Copy)]
enum Token {
    Many,
    One,
    Char(u32),
}
impl LikePattern {
    pub(super) fn new(pattern: &str) -> Self {
        let python = pattern
            .chars()
            .map(|ch| match ch {
                '%' => Token::Many,
                '_' => Token::One,
                _ => Token::Char(u32::from(ch)),
            })
            .collect();
        let mut arrow = Vec::new();
        let mut chars = pattern.chars();
        while let Some(ch) = chars.next() {
            arrow.push(match ch {
                '\\' => {
                    let Some(ch) = chars.next() else {
                        break;
                    };
                    Token::Char(u32::from(ch))
                }
                '%' => Token::Many,
                '_' => Token::One,
                _ => Token::Char(u32::from(ch)),
            });
        }
        Self { python, arrow }
    }
    pub(super) fn matches(&self, text: &[u32], python: bool) -> bool {
        let mut reachable = vec![false; text.len() + 1];
        reachable[0] = true;
        for token in if python { &self.python } else { &self.arrow } {
            let mut next = vec![false; text.len() + 1];
            match token {
                Token::Many => {
                    next[0] = reachable[0];
                    for i in 1..next.len() {
                        next[i] = reachable[i] || next[i - 1];
                    }
                }
                Token::One => {
                    next[1..].copy_from_slice(&reachable[..text.len()]);
                }
                Token::Char(code) => {
                    for i in 0..text.len() {
                        next[i + 1] = reachable[i] && text[i] == *code;
                    }
                }
            }
            reachable = next;
        }
        reachable[text.len()] || (python && text.last() == Some(&10) && reachable[text.len() - 1])
    }
}

#[derive(Clone)]
pub(super) enum Value<'a> {
    Array(&'a crate::data::ArrayData, usize),
    Null,
    Bool(bool),
    Int(Cow<'a, str>),
    Float(f64, bool),
    ArrayFloat(f64, &'a ColumnData, usize),
    Text(Cow<'a, [u32]>),
    Json(&'a Json),
    Decoded(Arc<Document>, usize),
    Vector(&'a [f32]),
    Targets(Vec<Value<'a>>),
    /// `datetime`：UTC 微秒。
    Timestamp(i64),
    /// `timedelta`：微秒数（构造时已按 Python 的范围检查）。
    Interval(i128),
    /// 算术得到的整数，以及编译期解析的算术字面量。
    Integer(Integer),
    /// 算术得到的字符串（码位）。
    Str(Arc<Vec<u32>>),
    /// 算术得到的列表；元素只能是 O(1) 克隆的共享形式。
    List(Arc<Vec<Shared<'a>>>),
}

/// 算术结果列表的元素：只含克隆为 O(1) 的形式（不含 `Cow::Owned`），按固定常量计费由类型保证。
/// 借用的 JSON 节点、`Decoded` 与「借用列 + 下标」的 `ArrayFloat` 保留 Python 对象身份，
/// 列表相等比较对同一个 NaN 对象按身份判等。
#[derive(Clone)]
pub(super) enum Shared<'a> {
    Null,
    Bool(bool),
    Float(f64, bool),
    Integer(Integer),
    Json(&'a Json),
    Decoded(Arc<Document>, usize),
    ArrayFloat(f64, &'a ColumnData, usize),
    Str(Arc<Vec<u32>>),
    List(Arc<Vec<Shared<'a>>>),
}
const _: () = assert!(std::mem::size_of::<Shared<'static>>() as u64 <= FILTER_LIST_ELEMENT_BYTES);

impl<'a> Shared<'a> {
    fn value(&self) -> Value<'a> {
        match self {
            Self::Null => Value::Null,
            Self::Bool(b) => Value::Bool(*b),
            Self::Float(v, nan) => Value::Float(*v, *nan),
            Self::Integer(v) => Value::Integer(v.clone()),
            Self::Json(v) => Value::Json(v),
            Self::Decoded(doc, id) => Value::Decoded(Arc::clone(doc), *id),
            Self::ArrayFloat(v, column, index) => Value::ArrayFloat(*v, column, *index),
            Self::Str(v) => Value::Str(Arc::clone(v)),
            Self::List(v) => Value::List(Arc::clone(v)),
        }
    }
    /// 列表元素转成共享形式。新得到并被结果保留的字符串按码位计费：借用的先按预算拷贝，
    /// 已拥有的（读列时为这一行新建的码位数组）直接移入。
    fn new(value: Value<'a>, budget: &mut Budget) -> Result<Self> {
        Ok(match value {
            Value::Null => Self::Null,
            Value::Bool(b) => Self::Bool(b),
            Value::Float(v, nan) => Self::Float(v, nan),
            Value::ArrayFloat(v, column, index) => Self::ArrayFloat(v, column, index),
            Value::Int(text) => Self::Integer(Integer::from_ref(IntRef::Text(&text), budget)?),
            Value::Integer(v) => Self::Integer(v),
            // JSON 列表里的 null 元素就是 None，没有对象身份可保留。
            Value::Json(Json::Null) => Self::Null,
            Value::Json(v) => Self::Json(v),
            Value::Decoded(doc, id) if matches!(doc.nodes[id], JsonNode::Null) => Self::Null,
            Value::Decoded(doc, id) => Self::Decoded(doc, id),
            Value::Str(v) => Self::Str(v),
            Value::List(v) => Self::List(v),
            Value::Text(text) => {
                let len = text.len() as u128;
                Self::Str(Arc::new(match text {
                    Cow::Owned(text) => {
                        budget.charge(len * u128::from(FILTER_CODE_POINT_BYTES))?;
                        text
                    }
                    Cow::Borrowed(text) => {
                        let mut copy = budget.vec(len, FILTER_CODE_POINT_BYTES)?;
                        copy.extend_from_slice(text);
                        copy
                    }
                }))
            }
            Value::Array(..)
            | Value::Vector(_)
            | Value::Targets(_)
            | Value::Timestamp(_)
            | Value::Interval(_) => {
                return Err(EngineError::Storage(
                    "list element has no shared Python form".into(),
                ));
            }
        })
    }
}

enum Atom<'a> {
    Null,
    Bool(bool),
    Int(IntRef<'a>),
    Float(f64, bool),
    Text(Cow<'a, [u32]>),
    List(usize),
    Object(usize),
    Timestamp(i64),
    Interval(i128),
}
impl Value<'_> {
    fn atom(&self) -> Result<Atom<'_>> {
        Ok(match self {
            Self::Array(array, row) => Atom::List(array.range(*row).len()),
            Self::Null => Atom::Null,
            Self::Bool(b) => Atom::Bool(*b),
            Self::Int(s) => Atom::Int(IntRef::Text(s)),
            Self::Integer(v) => Atom::Int(v.as_ref()),
            Self::Str(v) => Atom::Text(Cow::Borrowed(v)),
            Self::List(v) => Atom::List(v.len()),
            Self::Float(v, nan) => Atom::Float(*v, *nan),
            Self::ArrayFloat(v, _, _) => Atom::Float(*v, false),
            Self::Text(s) => Atom::Text(Cow::Borrowed(s)),
            Self::Json(value) => match value {
                Json::Null => Atom::Null,
                Json::Bool(b) => Atom::Bool(*b),
                Json::Number(n) if n.as_str().contains(['.', 'e', 'E']) => Atom::Float(
                    n.as_str()
                        .parse()
                        .map_err(|_| EngineError::invalid("invalid Python numeric value"))?,
                    false,
                ),
                Json::Number(n) => Atom::Int(IntRef::Text(n.as_str())),
                Json::String(s) => Atom::Text(Cow::Owned(s.chars().map(u32::from).collect())),
                Json::Array(v) => Atom::List(v.len()),
                Json::Object(v) => Atom::Object(v.len()),
            },
            Self::Decoded(doc, id) => match &doc.nodes[*id] {
                JsonNode::Null => Atom::Null,
                JsonNode::Bool(b) => Atom::Bool(*b),
                JsonNode::Int(s) => Atom::Int(IntRef::Text(s)),
                JsonNode::Float(f, n) => Atom::Float(*f, *n),
                JsonNode::String(s) => Atom::Text(Cow::Borrowed(s)),
                JsonNode::List(v) => Atom::List(v.len()),
                JsonNode::Object(v) => Atom::Object(v.len()),
            },
            Self::Vector(v) => Atom::List(v.len()),
            Self::Targets(v) => Atom::List(v.len()),
            Self::Timestamp(v) => Atom::Timestamp(*v),
            Self::Interval(v) => Atom::Interval(*v),
        })
    }
    fn boolean(&self) -> Result<Option<bool>> {
        match self.atom()? {
            Atom::Null => Ok(None),
            Atom::Bool(b) => Ok(Some(b)),
            _ => Err(EngineError::Storage(
                "bound boolean node produced a non-boolean value".into(),
            )),
        }
    }
    fn list_item(&self, i: usize) -> Result<Option<Self>> {
        Ok(match self {
            Self::Array(array, row) => {
                let range = array.range(*row);
                if i < range.len() {
                    let index = range.start + i;
                    Some(match column_value(&array.values, index)? {
                        Self::Float(value, _) if value.is_nan() => {
                            Self::ArrayFloat(value, &array.values, index)
                        }
                        value => value,
                    })
                } else {
                    None
                }
            }
            Self::Json(Json::Array(values)) => values.get(i).map(Self::Json),
            Self::Decoded(doc, id) => {
                if let JsonNode::List(values) = &doc.nodes[*id] {
                    values.get(i).map(|id| Self::Decoded(Arc::clone(doc), *id))
                } else {
                    None
                }
            }
            Self::Vector(values) => values.get(i).map(|v| Self::Float(f64::from(*v), false)),
            Self::Targets(values) => values.get(i).cloned(),
            Self::List(values) => values.get(i).map(Shared::value),
            _ => None,
        })
    }
    fn keys(&self) -> Vec<Vec<u32>> {
        match self {
            Self::Json(Json::Object(values)) => values
                .keys()
                .map(|k| k.chars().map(u32::from).collect())
                .collect(),
            Self::Decoded(doc, id) => {
                if let JsonNode::Object(values) = &doc.nodes[*id] {
                    values.keys().cloned().collect()
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }
    fn key(&self, key: &[u32]) -> Option<Self> {
        match self {
            Self::Json(Json::Object(values)) => {
                let key = key
                    .iter()
                    .copied()
                    .map(char::from_u32)
                    .collect::<Option<String>>()?;
                values.get(&key).map(Self::Json)
            }
            Self::Decoded(doc, id) => {
                if let JsonNode::Object(values) = &doc.nodes[*id] {
                    values.get(key).map(|id| Self::Decoded(Arc::clone(doc), id))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
    fn parsed(self) -> Result<Self> {
        if let Atom::Text(text) = self.atom()? {
            return Ok(match decode(&text)? {
                Some(doc) => {
                    let root = doc.root;
                    Self::Decoded(doc, root)
                }
                None => Self::Null,
            });
        }
        Ok(self)
    }
}

pub(crate) fn int_cmp(a: &str, b: &str) -> Ordering {
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
pub(crate) fn int_float_cmp(integer: &str, float: f64) -> Option<Ordering> {
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
    let order = int_cmp(integer, &format!("{:.0}", float.floor()));
    Some(if order.is_eq() && float.fract() != 0.0 {
        Ordering::Less
    } else {
        order
    })
}
fn numeric<'a>(atom: &Atom<'a>) -> Option<Number<'a>> {
    match atom {
        Atom::Bool(v) => Some(Number::Int(IntRef::Small(i64::from(*v)))),
        Atom::Int(v) => Some(Number::Int(*v)),
        Atom::Float(v, _) => Some(Number::Float(*v)),
        _ => None,
    }
}
#[derive(Clone, Copy)]
enum Number<'a> {
    Int(IntRef<'a>),
    Float(f64),
}
fn number_cmp(a: Number<'_>, b: Number<'_>) -> Option<Ordering> {
    match (a, b) {
        (Number::Int(a), Number::Int(b)) => Some(num::int_order(a, b)),
        (Number::Int(a), Number::Float(b)) => num::int_float_order(a, b),
        (Number::Float(a), Number::Int(b)) => num::int_float_order(b, a).map(Ordering::reverse),
        (Number::Float(a), Number::Float(b)) => a.partial_cmp(&b),
    }
}
fn equal(a: &Value<'_>, b: &Value<'_>, identity: bool) -> Result<bool> {
    let mut work = vec![(a.clone(), b.clone(), identity)];
    while let Some((a, b, identity)) = work.pop() {
        if identity {
            match (&a, &b) {
                (Value::Decoded(ad, ai), Value::Decoded(bd, bi))
                    if ai == bi && Arc::ptr_eq(ad, bd) =>
                {
                    continue;
                }
                (Value::Json(a), Value::Json(b)) if std::ptr::eq(*a, *b) => continue,
                (Value::ArrayFloat(_, ac, ai), Value::ArrayFloat(_, bc, bi))
                    if ai == bi && std::ptr::eq(*ac, *bc) =>
                {
                    continue;
                }
                _ => {}
            }
        }
        let (av, bv) = (a.atom()?, b.atom()?);
        if identity && matches!((&av, &bv), (Atom::Float(_, true), Atom::Float(_, true))) {
            continue;
        }
        if let (Some(a), Some(b)) = (numeric(&av), numeric(&bv)) {
            if !number_cmp(a, b).is_some_and(|order| order.is_eq()) {
                return Ok(false);
            }
            continue;
        }
        match (av, bv) {
            (Atom::Null, Atom::Null) => {}
            (Atom::Text(a), Atom::Text(b)) if a == b => {}
            (Atom::Timestamp(a), Atom::Timestamp(b)) if a == b => {}
            (Atom::Interval(a), Atom::Interval(b)) if a == b => {}
            (Atom::List(an), Atom::List(bn)) if an == bn => {
                for i in (0..an).rev() {
                    let (Some(av), Some(bv)) = (a.list_item(i)?, b.list_item(i)?) else {
                        return Err(EngineError::Storage("Python list lost element".into()));
                    };
                    work.push((av, bv, true));
                }
            }
            (Atom::Object(an), Atom::Object(bn)) if an == bn => {
                for key in a.keys() {
                    let Some(av) = a.key(&key) else {
                        return Err(EngineError::Storage("Python object lost key".into()));
                    };
                    let Some(bv) = b.key(&key) else {
                        return Ok(false);
                    };
                    work.push((av, bv, true));
                }
            }
            _ => return Ok(false),
        }
    }
    Ok(true)
}
enum Ordered {
    Comparable(Ordering),
    NumericUnordered,
    TypeError,
}

fn ordered(a: &Value<'_>, b: &Value<'_>) -> Result<Ordered> {
    let (mut a, mut b) = (a.clone(), b.clone());
    loop {
        let (av, bv) = (a.atom()?, b.atom()?);
        if let (Some(a), Some(b)) = (numeric(&av), numeric(&bv)) {
            return Ok(number_cmp(a, b).map_or(Ordered::NumericUnordered, Ordered::Comparable));
        }
        match (av, bv) {
            (Atom::Text(a), Atom::Text(b)) => return Ok(Ordered::Comparable(a.cmp(&b))),
            (Atom::Timestamp(a), Atom::Timestamp(b)) => return Ok(Ordered::Comparable(a.cmp(&b))),
            (Atom::Interval(a), Atom::Interval(b)) => return Ok(Ordered::Comparable(a.cmp(&b))),
            (Atom::List(an), Atom::List(bn)) => {
                let mut next = None;
                for i in 0..an.min(bn) {
                    let (Some(av), Some(bv)) = (a.list_item(i)?, b.list_item(i)?) else {
                        return Err(EngineError::Storage("Python list lost element".into()));
                    };
                    if !equal(&av, &bv, true)? {
                        next = Some((av, bv));
                        break;
                    }
                }
                if let Some(pair) = next {
                    (a, b) = pair;
                } else {
                    return Ok(Ordered::Comparable(an.cmp(&bn)));
                }
            }
            _ => return Ok(Ordered::TypeError),
        }
    }
}
pub(super) fn compare(a: &Value<'_>, b: &Value<'_>, op: Comparison) -> Result<Option<bool>> {
    if matches!(a.atom()?, Atom::Null) || matches!(b.atom()?, Atom::Null) {
        return Ok(None);
    }
    if matches!(op, Comparison::Equal | Comparison::NotEqual) {
        let eq = equal(a, b, false)?;
        return Ok(Some(if matches!(op, Comparison::Equal) {
            eq
        } else {
            !eq
        }));
    }
    let order = match ordered(a, b)? {
        Ordered::Comparable(order) => order,
        Ordered::NumericUnordered => return Ok(Some(false)),
        Ordered::TypeError => return Ok(None),
    };
    Ok(Some(match op {
        Comparison::Less => order.is_lt(),
        Comparison::LessEqual => order.is_le(),
        Comparison::Greater => order.is_gt(),
        Comparison::GreaterEqual => order.is_ge(),
        _ => false,
    }))
}

fn from_literal(l: &Literal) -> Result<Value<'_>> {
    Ok(match l {
        Literal::Int(s) => Value::Int(Cow::Borrowed(s)),
        Literal::Float(v) => Value::Float(*v, false),
        Literal::String(s) => Value::Text(Cow::Owned(s.chars().map(u32::from).collect())),
        Literal::Bool(v) => Value::Bool(*v),
        Literal::Timestamp(v) => Value::Timestamp(*v),
        Literal::Interval(v) => Value::Interval(timedelta(*v)?),
    })
}

/// Python `timedelta(microseconds=int(v))` 的范围：天数超过 999999999 报 OverflowError，
/// 超过 C int 时是另一条信息（3.x 的 python 后端在每行求值区间字面量时报错）。
fn timedelta(micros: f64) -> Result<i128> {
    // 浮点数的整数值与 Python `int()` 相同；天数用整数除法，避免 f64 除法在大数上舍入。
    let micros = micros as i128;
    let days = micros.div_euclid(86_400_000_000);
    if days > i128::from(i32::MAX) {
        return Err(EngineError::invalid(
            "Python int too large to convert to C int",
        ));
    }
    if days > 999_999_999 {
        return Err(EngineError::invalid(format!(
            "days={days}; must have magnitude <= 999999999"
        )));
    }
    Ok(micros)
}

/// `datetime ± timedelta`：结果超出 `[0001-01-01, 9999-12-31T23:59:59.999999]` 报 OverflowError。
fn shifted(timestamp: i64, interval: i128, subtract: bool) -> Result<Value<'static>> {
    let delta = if subtract { -interval } else { interval };
    let micros = i128::from(timestamp) + delta;
    i64::try_from(micros)
        .ok()
        .filter(|m| crate::timestamptz::check_micros(*m).is_ok())
        .map(Value::Timestamp)
        .ok_or_else(|| EngineError::invalid("date value out of range"))
}
fn column_value(column: &ColumnData, index: usize) -> Result<Value<'_>> {
    Ok(if column.is_null(index) {
        Value::Null
    } else {
        match column.values() {
            ColumnData::Array(array) => Value::Array(array, index),
            ColumnData::Nullable { .. } => {
                return Err(EngineError::Storage("nested nullable column".into()));
            }
            ColumnData::Bool(v) => Value::Bool(v[index]),
            ColumnData::Int8(v) => Value::Int(Cow::Owned(v[index].to_string())),
            ColumnData::Int16(v) => Value::Int(Cow::Owned(v[index].to_string())),
            ColumnData::Int32(v) => Value::Int(Cow::Owned(v[index].to_string())),
            ColumnData::Int64(v) => Value::Int(Cow::Owned(v[index].to_string())),
            ColumnData::Float(v) => Value::Float(f64::from(v[index]), false),
            ColumnData::Double(v) => Value::Float(v[index], false),
            ColumnData::VarChar(v) => {
                Value::Text(Cow::Owned(v[index].chars().map(u32::from).collect()))
            }
            ColumnData::Json(v) => Value::Text(Cow::Owned(
                std::str::from_utf8(&v[index])
                    .map_err(|e| EngineError::Storage(e.to_string()))?
                    .chars()
                    .map(u32::from)
                    .collect(),
            )),
            ColumnData::FloatVector { dim, values } => {
                Value::Vector(&values[index * dim..(index + 1) * dim])
            }
            // 绑定阶段已拒绝在过滤中引用稀疏字段。
            ColumnData::SparseFloatVector(_) => {
                return Err(EngineError::Storage(
                    "sparse vector column reached filter evaluation".into(),
                ));
            }
        }
    })
}

fn operand<'a>(op: &'a Operand, row: &DynamicRow<'a>) -> Result<Value<'a>> {
    Ok(match op {
        Operand::Literal(l) => return from_literal(l),
        Operand::Absent => Value::Null,
        Operand::Dynamic(key) => row
            .meta
            .and_then(|m| m.get(key))
            .map_or(Value::Null, Value::Json),
        Operand::Column(i) => column_value(&row.columns[*i], row.row)?,
        Operand::Timestamp(i) => {
            let column = &row.columns[*i];
            match column.values() {
                _ if column.is_null(row.row) => Value::Null,
                ColumnData::Int64(v) => Value::Timestamp(v[row.row]),
                _ => return Err(EngineError::Storage("timestamp column lost type".into())),
            }
        }
    })
}
fn membership(value: &Value<'_>, literals: &[Literal]) -> Result<Option<bool>> {
    match value.atom()? {
        Atom::Null => return Ok(None),
        Atom::List(_) => return Err(EngineError::invalid("unhashable type: 'list'")),
        Atom::Object(_) => return Err(EngineError::invalid("unhashable type: 'dict'")),
        _ => {}
    }
    for literal in literals {
        if equal(value, &from_literal(literal)?, true)? {
            return Ok(Some(true));
        }
    }
    Ok(Some(false))
}
pub(super) fn evaluate_terms(terms: &[Term], rows: &[DynamicRow<'_>]) -> Result<SourceBits> {
    let mut matching = SourceBits::new(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let mut unknown = false;
        let mut matched = true;
        for term in terms {
            let value = match term {
                Term::Compare { left, right, op } => {
                    compare(&operand(left, row)?, &operand(right, row)?, *op)?
                }
                Term::In {
                    operand: field,
                    values,
                } => membership(&operand(field, row)?, values)?,
            };
            if value == Some(false) {
                matched = false;
                break;
            }
            unknown |= value.is_none();
        }
        matching.set(i, matched && !unknown);
    }
    Ok(matching)
}

/// 参与算术的 Python 值的类别；按 `Atom` 穷尽划分，新增的值形式必须在这里决定归类。
#[derive(Clone, Copy)]
enum Class<'a> {
    Absent,
    Temporal,
    Number(Number<'a>),
    Text,
    List,
    Dict,
}
fn class<'a>(atom: &Atom<'a>) -> Class<'a> {
    match atom {
        Atom::Null => Class::Absent,
        Atom::Timestamp(_) | Atom::Interval(_) => Class::Temporal,
        Atom::Bool(v) => Class::Number(Number::Int(IntRef::Small(i64::from(*v)))),
        Atom::Int(v) => Class::Number(Number::Int(*v)),
        Atom::Float(v, _) => Class::Number(Number::Float(*v)),
        Atom::Text(_) => Class::Text,
        Atom::List(_) => Class::List,
        Atom::Object(_) => Class::Dict,
    }
}

/// Python `operator.add / sub / mul / truediv`（3.x `_eval_row` 的 ArithOp）：任一侧为 None 得 None；
/// TypeError 与 ZeroDivisionError 吞成未知；其余异常（OverflowError 与本实现的资源错误）向上传播。
fn arithmetic<'a>(
    op: &str,
    a: &Value<'a>,
    b: &Value<'a>,
    budget: &mut Budget,
) -> Result<Value<'a>> {
    use Class::{Absent, Dict, List, Temporal, Text};
    let (x, y) = (a.atom()?, b.atom()?);
    Ok(match (class(&x), class(&y), op) {
        (Absent, _, _) | (_, Absent, _) => Value::Null,
        (Temporal, Temporal, _) => match (&x, &y, op) {
            (Atom::Timestamp(t), Atom::Interval(d), "+" | "-") => shifted(*t, *d, op == "-")?,
            (Atom::Interval(d), Atom::Timestamp(t), "+") => shifted(*t, *d, false)?,
            // 其余时间值组合在语义检查中已被拒绝；到达时得到未知，与 0025 已实现的相同。
            _ => Value::Null,
        },
        (Class::Number(p), Class::Number(q), _) => numbers(op, p, q, budget)?,
        (Text | List, Class::Number(Number::Int(count)), "*") => repeat(a, &x, count, budget)?,
        (Class::Number(Number::Int(count)), Text | List, "*") => repeat(b, &y, count, budget)?,
        (Text, Text, "+") => {
            let (Atom::Text(p), Atom::Text(q)) = (&x, &y) else {
                return Err(EngineError::Storage("Python text lost its value".into()));
            };
            let mut text =
                budget.vec(p.len() as u128 + q.len() as u128, FILTER_CODE_POINT_BYTES)?;
            text.extend_from_slice(p);
            text.extend_from_slice(q);
            Value::Str(Arc::new(text))
        }
        (List, List, "+") => {
            let (Atom::List(p), Atom::List(q)) = (&x, &y) else {
                return Err(EngineError::Storage("Python list lost its length".into()));
            };
            let (p, q) = (shared_list(a, *p, budget)?, shared_list(b, *q, budget)?);
            let mut list =
                budget.vec(p.len() as u128 + q.len() as u128, FILTER_LIST_ELEMENT_BYTES)?;
            list.extend(p.iter().cloned());
            list.extend(q.iter().cloned());
            Value::List(Arc::new(list))
        }
        // 0032 表中的 TypeError 一行：时间值配其他值、字符串或列表配 float、str 与 list 互相、
        // `-` 与 `/` 作用于序列、dict 参与，3.x 都吞成未知。
        (Temporal, Class::Number(_) | Text | List | Dict, _)
        | (Class::Number(_) | Text | List | Dict, Temporal, _)
        | (Text | List, Class::Number(_), _)
        | (Class::Number(_), Text | List, _)
        | (Text, List, _)
        | (List, Text, _)
        | (Text, Text, _)
        | (List, List, _)
        | (Dict, _, _)
        | (_, Dict, _) => Value::Null,
    })
}

/// int、float（bool 当作 int）之间的运算。
fn numbers<'a>(op: &str, a: Number<'_>, b: Number<'_>, budget: &mut Budget) -> Result<Value<'a>> {
    if let (Number::Int(a), Number::Int(b)) = (a, b) {
        let (a, b) = (Integer::from_ref(a, budget)?, Integer::from_ref(b, budget)?);
        if op == "/" {
            return Ok(num::true_divide(&a, &b)?.map_or(Value::Null, |v| Value::Float(v, false)));
        }
        return Ok(Value::Integer(Integer::apply(op, &a, &b, budget)?));
    }
    // int 先正确舍入成 float，超出 double 报 OverflowError（先于判零）。
    let float = |n: Number<'_>| match n {
        Number::Int(i) => i.to_f64(),
        Number::Float(v) => Ok(v),
    };
    let (a, b) = (float(a)?, float(b)?);
    Ok(Value::Float(
        match op {
            "+" => a + b,
            "-" => a - b,
            "*" => a * b,
            // ZeroDivisionError（含除以 -0.0）：未知。
            _ if b == 0.0 => return Ok(Value::Null),
            _ => a / b,
        },
        false,
    ))
}

/// 序列重复（`str * int`、`list * int` 及交换）。判定顺序与 CPython 相同：次数须能放进 `Py_ssize_t`；
/// 次数 ≤ 0 或基值为空得到空值；长度乘积超过 `Py_ssize_t` 时字符串报 OverflowError、列表是
/// MemoryError（本实现报资源错误）；最后检查本实现的上限并分配。不是字符串或列表时是 TypeError。
fn repeat<'a>(
    base: &Value<'a>,
    atom: &Atom<'_>,
    count: IntRef<'_>,
    budget: &mut Budget,
) -> Result<Value<'a>> {
    let len = match atom {
        Atom::Text(text) => text.len(),
        Atom::List(len) => *len,
        _ => return Ok(Value::Null),
    };
    let count = count.repeat_count()?;
    let text = matches!(atom, Atom::Text(_));
    if count <= 0 || len == 0 {
        return Ok(if text {
            Value::Str(Arc::new(Vec::new()))
        } else {
            Value::List(Arc::new(Vec::new()))
        });
    }
    let total = len as u128 * count as u128;
    if total > i64::MAX as u128 {
        return Err(if text {
            EngineError::invalid("repeated string is too long")
        } else {
            budget.value_error()
        });
    }
    Ok(match atom {
        Atom::Text(source) => {
            let mut out = budget.vec(total, FILTER_CODE_POINT_BYTES)?;
            for _ in 0..count {
                out.extend_from_slice(source);
            }
            Value::Str(Arc::new(out))
        }
        _ => {
            let items = shared_list(base, len, budget)?;
            let mut out = budget.vec(total, FILTER_LIST_ELEMENT_BYTES)?;
            for _ in 0..count {
                out.extend(items.iter().cloned());
            }
            Value::List(Arc::new(out))
        }
    })
}

/// 列表操作数转成共享元素：已是算术结果的直接共享；其余按元素数计费后逐个转换。
fn shared_list<'a>(
    value: &Value<'a>,
    len: usize,
    budget: &mut Budget,
) -> Result<Arc<Vec<Shared<'a>>>> {
    if let Value::List(items) = value {
        return Ok(Arc::clone(items));
    }
    let mut items = budget.vec(len as u128, FILTER_LIST_ELEMENT_BYTES)?;
    for i in 0..len {
        let item = value
            .list_item(i)?
            .ok_or_else(|| EngineError::Storage("Python list lost element".into()))?;
        items.push(Shared::new(item, budget)?);
    }
    Ok(Arc::new(items))
}

enum Task<'a> {
    Visit(usize),
    Finish(usize),
    BooleanNext(usize, usize, bool),
    BooleanAfter(usize, usize, bool),
    Contains(usize),
    ContainsFinish(usize, Value<'a>),
}
/// 每个节点的值只被它的父节点读一次，读时取走，不复制。
fn node_value<'a>(values: &mut [Option<Value<'a>>], id: usize) -> Result<Value<'a>> {
    values[id]
        .take()
        .ok_or_else(|| EngineError::Storage("Python filter lost evaluated operand".into()))
}
pub(super) fn evaluate_tree(tree: &Tree, rows: &[DynamicRow<'_>]) -> Result<SourceBits> {
    evaluate_tree_with(tree, rows, num::LIMITS)
}
/// `limits` 只在测试中替换，用小尺寸覆盖边界与分配失败路径。
pub(super) fn evaluate_tree_with(
    tree: &Tree,
    rows: &[DynamicRow<'_>],
    limits: Limits,
) -> Result<SourceBits> {
    let mut matching = SourceBits::new(rows.len());
    let mut budget = Budget::new(limits);
    for (row_index, row) in rows.iter().enumerate() {
        budget.reset();
        let mut values = vec![None; tree.ast.nodes.len()];
        let mut tasks = vec![Task::Visit(tree.ast.root)];
        while let Some(task) = tasks.pop() {
            match task {
                Task::Visit(id) => {
                    if let Some(integer) = &tree.integers[id] {
                        values[id] = Some(Value::Integer(integer.clone()));
                        continue;
                    }
                    if let Some(op) = &tree.operands[id] {
                        values[id] = Some(operand(op, row)?);
                        continue;
                    }
                    match &tree.ast.nodes[id].kind {
                        Kind::Field(name) if name == "_seq" => {
                            values[id] = Some(row.sequence.map_or(Value::Null, |seq| {
                                Value::Int(Cow::Owned(seq.to_string()))
                            }));
                        }
                        Kind::Field(name) if name == "_partition" => {
                            values[id] = Some(Value::Null);
                        }
                        Kind::And(_) | Kind::Or(_) => tasks.push(Task::BooleanNext(id, 0, false)),
                        Kind::Contains { value, .. } => {
                            tasks.extend([Task::Contains(id), Task::Visit(*value)]);
                        }
                        Kind::List(_) => {
                            return Err(EngineError::invalid("unknown AST node: ListLit"));
                        }
                        kind => {
                            tasks.push(Task::Finish(id));
                            match kind {
                                Kind::Cmp(_, a, b) | Kind::Arith(_, a, b) => {
                                    tasks.extend([Task::Visit(*b), Task::Visit(*a)])
                                }
                                Kind::Not(value)
                                | Kind::Like { value, .. }
                                | Kind::Length(value)
                                | Kind::IsNull { value, .. }
                                | Kind::TextMatch { field: value, .. }
                                | Kind::Geometry { field: value, .. }
                                | Kind::In { field: value, .. } => tasks.push(Task::Visit(*value)),
                                Kind::Path { base, .. } => tasks.push(Task::Visit(*base)),
                                _ => return Err(super::eval::unsupported()),
                            }
                        }
                    }
                }
                Task::BooleanNext(id, index, unknown) => {
                    let ids = match &tree.ast.nodes[id].kind {
                        Kind::And(ids) | Kind::Or(ids) => ids,
                        _ => {
                            return Err(EngineError::Storage(
                                "Python boolean continuation lost node".into(),
                            ));
                        }
                    };
                    if index == ids.len() {
                        values[id] = Some(if unknown {
                            Value::Null
                        } else {
                            Value::Bool(matches!(tree.ast.nodes[id].kind, Kind::And(_)))
                        });
                    } else {
                        tasks.extend([
                            Task::BooleanAfter(id, index, unknown),
                            Task::Visit(ids[index]),
                        ]);
                    }
                }
                Task::BooleanAfter(id, index, unknown) => {
                    let ids = match &tree.ast.nodes[id].kind {
                        Kind::And(ids) | Kind::Or(ids) => ids,
                        _ => {
                            return Err(EngineError::Storage(
                                "Python boolean continuation lost node".into(),
                            ));
                        }
                    };
                    let b = node_value(&mut values, ids[index])?.boolean()?;
                    let short = if matches!(tree.ast.nodes[id].kind, Kind::And(_)) {
                        Some(false)
                    } else {
                        Some(true)
                    };
                    if b == short {
                        values[id] = Some(Value::Bool(b.ok_or_else(|| {
                            EngineError::Storage("Python short circuit lost bool".into())
                        })?));
                    } else {
                        tasks.push(Task::BooleanNext(id, index + 1, unknown || b.is_none()));
                    }
                }
                Task::Contains(id) => {
                    let Kind::Contains {
                        value,
                        targets,
                        mode,
                    } = &tree.ast.nodes[id].kind
                    else {
                        return Err(EngineError::Storage(
                            "Python contains continuation lost node".into(),
                        ));
                    };
                    let array = node_value(&mut values, *value)?.parsed()?;
                    if !matches!(array.atom()?, Atom::List(_)) {
                        values[id] = Some(Value::Bool(false));
                        continue;
                    }
                    if mode != "any_one"
                        && let Kind::List(ids) = &tree.ast.nodes[*targets].kind
                    {
                        let mut target_values = Vec::new();
                        for &child in ids {
                            let op = tree.operands[child]
                                .as_ref()
                                .ok_or_else(|| EngineError::invalid("unknown AST node: ListLit"))?;
                            target_values.push(operand(op, row)?);
                        }
                        values[*targets] = Some(Value::Targets(target_values));
                        tasks.push(Task::ContainsFinish(id, array));
                        continue;
                    }
                    tasks.extend([Task::ContainsFinish(id, array), Task::Visit(*targets)]);
                }
                Task::ContainsFinish(id, array) => {
                    let Kind::Contains { targets, mode, .. } = &tree.ast.nodes[id].kind else {
                        return Err(EngineError::Storage(
                            "Python contains continuation lost node".into(),
                        ));
                    };
                    let target = node_value(&mut values, *targets)?;
                    let targets = if mode == "any_one" {
                        vec![target]
                    } else if let Atom::List(len) = target.atom()? {
                        (0..len)
                            .map(|i| {
                                target.list_item(i)?.ok_or_else(|| {
                                    EngineError::Storage("Python targets lost element".into())
                                })
                            })
                            .collect::<Result<Vec<_>>>()?
                    } else {
                        vec![target]
                    };
                    let Atom::List(len) = array.atom()? else {
                        return Err(EngineError::Storage("Python contains lost array".into()));
                    };
                    let mut result = mode == "all";
                    for target in targets {
                        let mut found = false;
                        for i in 0..len {
                            let item = array.list_item(i)?.ok_or_else(|| {
                                EngineError::Storage("Python contains lost element".into())
                            })?;
                            if equal(&target, &item, true)? {
                                found = true;
                                break;
                            }
                        }
                        if mode == "all" && !found {
                            result = false;
                            break;
                        }
                        if mode != "all" && found {
                            result = true;
                            break;
                        }
                    }
                    values[id] = Some(Value::Bool(result));
                }
                Task::Finish(id) => {
                    values[id] = Some(match &tree.ast.nodes[id].kind {
                        Kind::Cmp(op, a, b) => compare(
                            &node_value(&mut values, *a)?,
                            &node_value(&mut values, *b)?,
                            Comparison::parse(op)?,
                        )?
                        .map_or(Value::Null, Value::Bool),
                        Kind::Arith(op, a, b) => {
                            let (a, b) =
                                (node_value(&mut values, *a)?, node_value(&mut values, *b)?);
                            arithmetic(op, &a, &b, &mut budget)?
                        }
                        Kind::Not(value) => node_value(&mut values, *value)?
                            .boolean()?
                            .map_or(Value::Null, |b| Value::Bool(!b)),
                        Kind::IsNull { value, negate, .. } => Value::Bool(
                            matches!(node_value(&mut values, *value)?.atom()?, Atom::Null)
                                != *negate,
                        ),
                        Kind::Like { value, .. } => {
                            match node_value(&mut values, *value)?.atom()? {
                                Atom::Null => Value::Null,
                                Atom::Text(text) => Value::Bool(
                                    tree.patterns[id]
                                        .as_ref()
                                        .ok_or_else(|| {
                                            EngineError::Storage(
                                                "LIKE pattern was not prepared".into(),
                                            )
                                        })?
                                        .matches(&text, true),
                                ),
                                _ => Value::Bool(false),
                            }
                        }
                        Kind::TextMatch { field, .. } => {
                            let query = tree.text_queries[id].as_ref().ok_or_else(|| {
                                EngineError::Storage("text_match lost its query".into())
                            })?;
                            // 3.x 只对 str 分词；NULL 和其他类型都是 False，不是未知。
                            let value = node_value(&mut values, *field)?;
                            Value::Bool(match value.atom()? {
                                Atom::Text(text) => query.matches(&text),
                                _ => false,
                            })
                        }
                        Kind::Geometry { field, .. } => {
                            let test = tree.geometries[id].as_ref().ok_or_else(|| {
                                EngineError::Storage("geometry predicate lost its literal".into())
                            })?;
                            // 3.x：NULL 是 False 而不是未知；值不是合法 WKT 时谓词内部的异常也当作 False。
                            let value = node_value(&mut values, *field)?;
                            Value::Bool(match value.atom()? {
                                Atom::Text(text) => text
                                    .iter()
                                    .map(|c| char::from_u32(*c))
                                    .collect::<Option<String>>()
                                    .is_some_and(|text| test.matches(&text)),
                                _ => false,
                            })
                        }
                        Kind::Path { base, parts } => {
                            let mut value = node_value(&mut values, *base)?.parsed()?;
                            for part in parts {
                                value = match part {
                                    PathPart::Key(key) => value
                                        .key(&key.chars().map(u32::from).collect::<Vec<_>>())
                                        .unwrap_or(Value::Null),
                                    PathPart::Index(index) => match index.parse::<usize>() {
                                        Ok(i) => value.list_item(i)?.unwrap_or(Value::Null),
                                        Err(_) => Value::Null,
                                    },
                                };
                            }
                            value
                        }
                        Kind::Length(value) => {
                            let value = node_value(&mut values, *value)?.parsed()?;
                            Value::Int(Cow::Owned(
                                if let Atom::List(len) = value.atom()? {
                                    len
                                } else {
                                    0
                                }
                                .to_string(),
                            ))
                        }
                        Kind::In {
                            field,
                            list,
                            negate,
                        } => {
                            let Kind::List(ids) = &tree.ast.nodes[*list].kind else {
                                return Err(EngineError::Storage("bound IN node lost list".into()));
                            };
                            let literals = ids
                                .iter()
                                .map(|&id| match &tree.ast.nodes[id].kind {
                                    Kind::Lit(l) => Ok(l.clone()),
                                    _ => Err(super::eval::unsupported()),
                                })
                                .collect::<Result<Vec<_>>>()?;
                            membership(&node_value(&mut values, *field)?, &literals)?
                                .map_or(Value::Null, |b| Value::Bool(b != *negate))
                        }
                        _ => return Err(super::eval::unsupported()),
                    });
                }
            }
        }
        matching.set(
            row_index,
            node_value(&mut values, tree.ast.root)?.boolean()? == Some(true),
        );
    }
    Ok(matching)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_timedelta_and_datetime_ranges_match_3x_messages() {
        // 天数 ≤ 999999999 的区间可构造；再大按 C int 分界报两种信息。
        let day = 86_400_000_000.0;
        assert_eq!(timedelta(1_000.0 * day).unwrap(), 1_000 * 86_400_000_000);
        // 'P999999999D' 的浮点微秒数舍入后天数为 999999998，仍可构造（3.x 在这里不报 timedelta 错误）。
        assert_eq!(
            timedelta(999_999_999.0 * 86_400.0 * 1_000_000.0).unwrap(),
            86_399_999_913_599_991_808
        );
        assert_eq!(
            timedelta(999_999_999.0 * day).unwrap(),
            86_399_999_913_599_991_808
        );
        // 比 10^9 天小一个浮点间隔（16384 微秒）：天数恰为 999999999，仍可构造。
        assert!(timedelta(1_000_000_000.0 * day - 16_384.0).is_ok());
        assert_eq!(
            timedelta(1_000_000_000.0 * day).unwrap_err().to_string(),
            "days=1000000000; must have magnitude <= 999999999"
        );
        assert_eq!(
            timedelta(2_147_483_520.0 * day).unwrap_err().to_string(),
            "days=2147483520; must have magnitude <= 999999999"
        );
        assert_eq!(
            timedelta(2_147_483_648.0 * day).unwrap_err().to_string(),
            "Python int too large to convert to C int"
        );
        assert_eq!(
            timedelta(1e300).unwrap_err().to_string(),
            "Python int too large to convert to C int"
        );
        // datetime ± timedelta 超出 [0001, 9999] 报 date value out of range，边界上的结果可用。
        let min = crate::timestamptz::MIN_MICROS;
        let max = crate::timestamptz::MAX_MICROS;
        assert!(matches!(shifted(max, 0, false), Ok(Value::Timestamp(v)) if v == max));
        assert!(matches!(shifted(max, 1, true), Ok(Value::Timestamp(v)) if v == max - 1));
        assert!(matches!(shifted(min, 1, false), Ok(Value::Timestamp(v)) if v == min + 1));
        for (t, d, subtract) in [
            (max, 1, false),
            (min, 1, true),
            (0, i128::from(i64::MAX) * 4, false),
        ] {
            assert_eq!(
                shifted(t, d, subtract)
                    .err()
                    .map(|e| e.to_string())
                    .as_deref(),
                Some("date value out of range")
            );
        }
    }

    #[test]
    fn list_null_elements_become_shared_null() {
        // JSON 列表的 null 元素就是 None：serde 节点与平面解码的节点都归一为 Shared::Null，
        // 其余元素保留原来的借用 / 节点形式（对象身份）。
        let mut budget = Budget::new(num::LIMITS);
        let json = serde_json::json!([null, 1, [null]]);
        let items = shared_list(&Value::Json(&json), 3, &mut budget).unwrap();
        assert!(matches!(items[0], Shared::Null));
        assert!(matches!(items[1], Shared::Json(Json::Number(_))));
        assert!(matches!(items[2], Shared::Json(Json::Array(_))));
        let text: Vec<u32> = "[null, 1, [null]]".chars().map(u32::from).collect();
        let doc = decode(&text).unwrap().unwrap();
        let root = doc.root;
        let items = shared_list(&Value::Decoded(doc, root), 3, &mut budget).unwrap();
        assert!(matches!(items[0], Shared::Null));
        assert!(
            matches!(&items[1], Shared::Decoded(doc, id) if matches!(doc.nodes[*id], JsonNode::Int(_)))
        );
        assert!(
            matches!(&items[2], Shared::Decoded(doc, id) if matches!(doc.nodes[*id], JsonNode::List(_)))
        );
    }

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
        let list = serde_json::json!([]);
        let object = serde_json::json!({});
        let numeric = Value::Int(Cow::Borrowed("1"));
        let text = Value::Text(Cow::Owned(vec![u32::from('c')]));
        for value in [
            Value::Int(Cow::Borrowed("1")),
            Value::Float(1.5, false),
            Value::Json(&list),
            Value::Json(&object),
        ] {
            for op in ops {
                let expected = match op {
                    Comparison::Equal => Some(false),
                    Comparison::NotEqual => Some(true),
                    _ => None,
                };
                assert_eq!(compare(&value, &text, op).unwrap(), expected);
                assert_eq!(compare(&text, &value, op).unwrap(), expected);
            }
        }
        for value in [Value::Json(&list), Value::Json(&object)] {
            for op in ops {
                let expected = match op {
                    Comparison::Equal => Some(false),
                    Comparison::NotEqual => Some(true),
                    _ => None,
                };
                assert_eq!(compare(&value, &numeric, op).unwrap(), expected);
                assert_eq!(compare(&numeric, &value, op).unwrap(), expected);
            }
        }
        for value in [
            Value::Int(Cow::Borrowed("1")),
            Value::Float(1.5, false),
            Value::Float(f64::NAN, false),
        ] {
            for op in ops {
                let expected = Some(matches!(op, Comparison::NotEqual));
                assert_eq!(
                    compare(&Value::Float(f64::NAN, false), &value, op).unwrap(),
                    expected
                );
                assert_eq!(
                    compare(&value, &Value::Float(f64::NAN, false), op).unwrap(),
                    expected
                );
            }
        }
        for op in ops {
            assert_eq!(compare(&Value::Null, &numeric, op).unwrap(), None);
        }
    }
}
