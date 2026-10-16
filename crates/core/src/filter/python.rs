//! Python 路线与 hybrid 回退共用的值规则；不嵌入解释器。

use super::dynamic::DynamicRow;
use super::eval::{Comparison, Operand, Term};
use super::parser::{Kind, Literal, PathPart};
use super::python_json::{Document, JsonNode, decode};
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
}
enum Atom<'a> {
    Null,
    Bool(bool),
    Int(&'a str),
    Float(f64, bool),
    Text(Cow<'a, [u32]>),
    List(usize),
    Object(usize),
}
impl Value<'_> {
    fn atom(&self) -> Result<Atom<'_>> {
        Ok(match self {
            Self::Array(array, row) => Atom::List(array.range(*row).len()),
            Self::Null => Atom::Null,
            Self::Bool(b) => Atom::Bool(*b),
            Self::Int(s) => Atom::Int(s),
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
                Json::Number(n) => Atom::Int(n.as_str()),
                Json::String(s) => Atom::Text(Cow::Owned(s.chars().map(u32::from).collect())),
                Json::Array(v) => Atom::List(v.len()),
                Json::Object(v) => Atom::Object(v.len()),
            },
            Self::Decoded(doc, id) => match &doc.nodes[*id] {
                JsonNode::Null => Atom::Null,
                JsonNode::Bool(b) => Atom::Bool(*b),
                JsonNode::Int(s) => Atom::Int(s),
                JsonNode::Float(f, n) => Atom::Float(*f, *n),
                JsonNode::String(s) => Atom::Text(Cow::Borrowed(s)),
                JsonNode::List(v) => Atom::List(v.len()),
                JsonNode::Object(v) => Atom::Object(v.len()),
            },
            Self::Vector(v) => Atom::List(v.len()),
            Self::Targets(v) => Atom::List(v.len()),
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

pub(super) fn int_cmp(a: &str, b: &str) -> Ordering {
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
pub(super) fn int_float_cmp(integer: &str, float: f64) -> Option<Ordering> {
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
fn numeric<'a>(atom: &'a Atom<'_>) -> Option<Number<'a>> {
    match atom {
        Atom::Bool(v) => Some(Number::Int(if *v { "1" } else { "0" })),
        Atom::Int(v) => Some(Number::Int(v)),
        Atom::Float(v, _) => Some(Number::Float(*v)),
        _ => None,
    }
}
enum Number<'a> {
    Int(&'a str),
    Float(f64),
}
fn number_cmp(a: Number<'_>, b: Number<'_>) -> Option<Ordering> {
    match (a, b) {
        (Number::Int(a), Number::Int(b)) => Some(int_cmp(a, b)),
        (Number::Int(a), Number::Float(b)) => int_float_cmp(a, b),
        (Number::Float(a), Number::Int(b)) => int_float_cmp(b, a).map(Ordering::reverse),
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
        _ => return Err(super::eval::unsupported()),
    })
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
        }
    })
}

fn operand<'a>(op: &'a Operand, row: &DynamicRow<'a>) -> Result<Value<'a>> {
    Ok(match op {
        Operand::Literal(l) => return from_literal(l),
        Operand::Dynamic(key) => row
            .meta
            .and_then(|m| m.get(key))
            .map_or(Value::Null, Value::Json),
        Operand::Column(i) => column_value(&row.columns[*i], row.row)?,
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

enum Task<'a> {
    Visit(usize),
    Finish(usize),
    BooleanNext(usize, usize, bool),
    BooleanAfter(usize, usize, bool),
    Contains(usize),
    ContainsFinish(usize, Value<'a>),
}
fn node_value<'a>(values: &[Option<Value<'a>>], id: usize) -> Result<Value<'a>> {
    values[id]
        .clone()
        .ok_or_else(|| EngineError::Storage("Python filter lost evaluated operand".into()))
}
pub(super) fn evaluate_tree(tree: &Tree, rows: &[DynamicRow<'_>]) -> Result<SourceBits> {
    let mut matching = SourceBits::new(rows.len());
    for (row_index, row) in rows.iter().enumerate() {
        let mut values = vec![None; tree.ast.nodes.len()];
        let mut tasks = vec![Task::Visit(tree.ast.root)];
        while let Some(task) = tasks.pop() {
            match task {
                Task::Visit(id) => {
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
                                Kind::Cmp(_, a, b) => {
                                    tasks.extend([Task::Visit(*b), Task::Visit(*a)])
                                }
                                Kind::Not(value)
                                | Kind::Like { value, .. }
                                | Kind::Length(value)
                                | Kind::IsNull { value, .. }
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
                    let b = node_value(&values, ids[index])?.boolean()?;
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
                    let array = node_value(&values, *value)?.parsed()?;
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
                    let target = node_value(&values, *targets)?;
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
                            &node_value(&values, *a)?,
                            &node_value(&values, *b)?,
                            Comparison::parse(op)?,
                        )?
                        .map_or(Value::Null, Value::Bool),
                        Kind::Not(value) => node_value(&values, *value)?
                            .boolean()?
                            .map_or(Value::Null, |b| Value::Bool(!b)),
                        Kind::IsNull { value, negate, .. } => Value::Bool(
                            matches!(node_value(&values, *value)?.atom()?, Atom::Null) != *negate,
                        ),
                        Kind::Like { value, .. } => match node_value(&values, *value)?.atom()? {
                            Atom::Null => Value::Null,
                            Atom::Text(text) => Value::Bool(
                                tree.patterns[id]
                                    .as_ref()
                                    .ok_or_else(|| {
                                        EngineError::Storage("LIKE pattern was not prepared".into())
                                    })?
                                    .matches(&text, true),
                            ),
                            _ => Value::Bool(false),
                        },
                        Kind::Path { base, parts } => {
                            let mut value = node_value(&values, *base)?.parsed()?;
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
                            let value = node_value(&values, *value)?.parsed()?;
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
                            membership(&node_value(&values, *field)?, &literals)?
                                .map_or(Value::Null, |b| Value::Bool(b != *negate))
                        }
                        _ => return Err(super::eval::unsupported()),
                    });
                }
            }
        }
        matching.set(
            row_index,
            node_value(&values, tree.ast.root)?.boolean()? == Some(true),
        );
    }
    Ok(matching)
}

#[cfg(test)]
mod tests {
    use super::*;

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
