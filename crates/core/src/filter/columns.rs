//! 布尔树的列式求值；可回退的内核错误与不可回退的整数溢出分开传播。

use super::dynamic::{DynamicRow, Shape, infer, materialize, members};
use super::eval::{self, Comparison, Operand, Scalar};
use super::parser::{Kind, Literal};
use super::tree::{Backend, Tree};
use crate::data::ColumnData;
use crate::error::{EngineError, Result};
use crate::storage::visibility::SourceBits;
use std::borrow::Cow;
use std::ops::Index;

enum Failure {
    Kernel(EngineError),
    Fatal(EngineError),
}
type ColumnResult<T> = std::result::Result<T, Failure>;
fn kernel(reason: impl Into<String>) -> Failure {
    Failure::Kernel(EngineError::invalid(reason))
}
/// 时间戳与区间都以 int64 微秒存放；标签让它们不会与普通整数互相比较（Arrow 没有对应的核）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Temporal {
    None,
    Timestamp,
    Interval,
}
struct Column<'a> {
    data: Cow<'a, ColumnData>,
    valid: SourceBits,
    shape: Shape,
    scalar: bool,
    temporal: Temporal,
}
impl Column<'_> {
    fn index(&self, row: usize) -> usize {
        if self.scalar { 0 } else { row }
    }
    fn valid(&self, row: usize) -> bool {
        self.valid.get(self.index(row))
    }
    fn boolean(&self, row: usize) -> ColumnResult<bool> {
        if let ColumnData::Bool(values) = self.data.as_ref() {
            Ok(values[self.index(row)])
        } else {
            Err(Failure::Fatal(EngineError::Storage(
                "column boolean node lost type".into(),
            )))
        }
    }
}
fn booleans(values: Vec<bool>, valid: SourceBits) -> Column<'static> {
    Column {
        data: Cow::Owned(ColumnData::Bool(values)),
        valid,
        shape: Shape::Bool,
        scalar: false,
        temporal: Temporal::None,
    }
}
fn all(n: usize) -> SourceBits {
    SourceBits::from_live(&vec![true; n])
}
struct Pair<'a>(&'a ColumnData, &'a ColumnData);
impl Index<usize> for Pair<'_> {
    type Output = ColumnData;
    fn index(&self, index: usize) -> &Self::Output {
        if index == 0 { self.0 } else { self.1 }
    }
}
fn shape(column: &ColumnData) -> Shape {
    match column {
        ColumnData::Bool(_) => Shape::Bool,
        ColumnData::Int8(_)
        | ColumnData::Int16(_)
        | ColumnData::Int32(_)
        | ColumnData::Int64(_) => Shape::Int,
        ColumnData::Float(_) | ColumnData::Double(_) => Shape::Float,
        ColumnData::VarChar(_) => Shape::String,
        _ => Shape::Complex,
    }
}
fn integer_bits(column: &ColumnData) -> u32 {
    match column {
        ColumnData::Int8(_) => 8,
        ColumnData::Int16(_) => 16,
        ColumnData::Int32(_) => 32,
        _ => 64,
    }
}
fn int_fits(value: i64, bits: u32) -> bool {
    bits == 64 || (-(1_i64 << (bits - 1))..(1_i64 << (bits - 1))).contains(&value)
}
fn float_int(value: f64, bits: u32) -> Option<i64> {
    let limit = 2_f64.powi(bits as i32 - 1);
    (value.is_finite() && value.fract() == 0.0 && (-limit..limit).contains(&value))
        .then_some(value as i64)
}
fn integer_precision(integer: i64, float: u8, array: bool) -> ColumnResult<f64> {
    let bound = if float == 32 {
        16_777_216
    } else {
        9_007_199_254_740_992
    };
    if !(-bound..=bound).contains(&integer) {
        return Err(kernel(if array {
            format!(
                "Integer value {integer} is outside of the range exactly representable by a IEEE 754 double precision value"
            )
        } else {
            format!("Integer value {integer} not in range: -{bound} to {bound}")
        }));
    }
    Ok(if float == 32 {
        f64::from(integer as f32)
    } else {
        integer as f64
    })
}
fn scalar_compare(a: Scalar<'_>, b: Scalar<'_>, op: Comparison) -> ColumnResult<bool> {
    macro_rules! cmp {
        ($a:expr,$b:expr) => {
            Ok(match op {
                Comparison::Equal => $a == $b,
                Comparison::NotEqual => $a != $b,
                Comparison::Less => $a < $b,
                Comparison::LessEqual => $a <= $b,
                Comparison::Greater => $a > $b,
                Comparison::GreaterEqual => $a >= $b,
            })
        };
    }
    match (a, b) {
        (Scalar::Int(a), Scalar::Int(b)) => cmp!(a, b),
        (Scalar::Float(a), Scalar::Float(b)) => cmp!(a, b),
        (Scalar::Bool(a), Scalar::Bool(b)) => cmp!(a, b),
        (Scalar::String(a), Scalar::String(b)) => cmp!(a, b),
        _ => Err(kernel("no matching comparison kernel")),
    }
}
fn arrow_type(column: &ColumnData) -> String {
    match column.values() {
        ColumnData::Bool(_) => "bool".into(),
        ColumnData::Int8(_) => "int8".into(),
        ColumnData::Int16(_) => "int16".into(),
        ColumnData::Int32(_) => "int32".into(),
        ColumnData::Int64(_) => "int64".into(),
        ColumnData::Float(_) => "float".into(),
        ColumnData::Double(_) => "double".into(),
        ColumnData::VarChar(_) | ColumnData::Json(_) => "string".into(),
        ColumnData::Array(array) => format!(
            "list<{}: {}>",
            array.element_name,
            arrow_type(&array.values)
        ),
        _ => "null".into(),
    }
}
/// Arrow 的类型名：时间戳与区间在内部是 Int64，对外要按 pyarrow 的名字显示。
fn column_type(column: &Column<'_>) -> String {
    match column.temporal {
        Temporal::Timestamp => "timestamp[us, tz=UTC]".into(),
        Temporal::Interval => "duration[us]".into(),
        Temporal::None => arrow_type(&column.data),
    }
}
fn compare(
    a: &Column<'_>,
    b: &Column<'_>,
    op: Comparison,
    n: usize,
) -> ColumnResult<Column<'static>> {
    if a.shape == Shape::Null && b.shape == Shape::Null {
        return Err(kernel("no matching comparison kernel for null inputs"));
    }
    if matches!(a.shape, Shape::Complex) || matches!(b.shape, Shape::Complex) {
        if matches!(a.data.values(), ColumnData::Array(_))
            || matches!(b.data.values(), ColumnData::Array(_))
        {
            let name = match op {
                Comparison::Equal => "equal",
                Comparison::NotEqual => "not_equal",
                Comparison::Less => "less",
                Comparison::LessEqual => "less_equal",
                Comparison::Greater => "greater",
                Comparison::GreaterEqual => "greater_equal",
            };
            return Err(kernel(format!(
                "Function '{name}' has no kernel matching input types ({}, {})",
                column_type(a),
                column_type(b)
            )));
        }
        return Err(kernel("no matching comparison kernel"));
    }
    if a.temporal != b.temporal {
        return Err(kernel("no matching comparison kernel"));
    }
    let pair = Pair(&a.data, &b.data);
    let (left, right) = (Operand::Column(0), Operand::Column(1));
    let precision =
        eval::operand_precision(&left, &pair).max(eval::operand_precision(&right, &pair));
    let mut valid = SourceBits::new(n);
    let mut values = vec![false; n];
    for (row, value) in values.iter_mut().enumerate() {
        if a.valid(row) && b.valid(row) {
            let a = eval::value(&left, &pair, a.index(row), precision).map_err(Failure::Kernel)?;
            let b = eval::value(&right, &pair, b.index(row), precision).map_err(Failure::Kernel)?;
            *value = scalar_compare(a, b, op)?;
            valid.set(row, true);
        }
    }
    Ok(booleans(values, valid))
}

fn interval_micros(micros: f64) -> ColumnResult<i64> {
    if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&micros) {
        Ok(micros as i64)
    } else {
        Err(Failure::Fatal(EngineError::invalid(
            "Python int too large to convert to C long",
        )))
    }
}

/// 时间戳 ± 区间（类型检查已保证两边是时间戳与区间）；Arrow 的非检查运算按 int64 回绕。
fn arithmetic(op: &str, a: &Column<'_>, b: &Column<'_>, n: usize) -> ColumnResult<Column<'static>> {
    let (timestamp, interval) = match (a.temporal, b.temporal) {
        (Temporal::Timestamp, Temporal::Interval) => (a, b),
        (Temporal::Interval, Temporal::Timestamp) if op == "+" => (b, a),
        _ => return Err(kernel("no matching arithmetic kernel")),
    };
    let (ColumnData::Int64(left), ColumnData::Int64(right)) =
        (timestamp.data.as_ref(), interval.data.as_ref())
    else {
        return Err(Failure::Fatal(EngineError::Storage(
            "temporal arithmetic lost its integer columns".into(),
        )));
    };
    let scalar = a.scalar && b.scalar;
    let rows = if scalar { 1 } else { n };
    let mut valid = SourceBits::new(rows);
    let mut values = vec![0_i64; rows];
    for (row, out) in values.iter_mut().enumerate() {
        if timestamp.valid(row) && interval.valid(row) {
            let t = left[timestamp.index(row)];
            let d = right[interval.index(row)];
            *out = if op == "-" {
                t.wrapping_sub(d)
            } else {
                t.wrapping_add(d)
            };
            valid.set(row, true);
        }
    }
    Ok(Column {
        data: Cow::Owned(ColumnData::Int64(values)),
        valid,
        shape: Shape::Int,
        scalar,
        temporal: Temporal::Timestamp,
    })
}

fn in_list(
    column: &Column<'_>,
    literals: &[Literal],
    negate: bool,
    n: usize,
) -> ColumnResult<Column<'static>> {
    if literals.is_empty() {
        return Ok(booleans(vec![negate; n], all(n)));
    }
    for literal in literals {
        eval::validate_literal(&Operand::Literal(literal.clone())).map_err(Failure::Fatal)?;
    }
    let set_float = literals.iter().any(|l| matches!(l, Literal::Float(_)));
    if set_float {
        for literal in literals {
            if let Literal::Int(raw) = literal {
                let integer = raw.parse::<i64>().map_err(|_| {
                    Failure::Fatal(EngineError::invalid(
                        "Python int too large to convert to C long",
                    ))
                })?;
                integer_precision(integer, 64, true)?;
            }
        }
    }
    let Some(converted) = members(column.shape, literals) else {
        if matches!(column.data.values(), ColumnData::Array(_)) {
            return Err(kernel(format!(
                "Function 'is_in' has no kernel matching input types ({})",
                arrow_type(&column.data)
            )));
        }
        return Err(kernel("no matching membership kernel"));
    };
    if column.shape == Shape::Null {
        return Ok(booleans(vec![negate; n], all(n)));
    }
    let bits = integer_bits(&column.data);
    let target_float = if matches!(column.data.as_ref(), ColumnData::Float(_)) {
        32
    } else {
        64
    };
    let mut set = Vec::new();
    let mut cast_column = None;
    for literal in &converted {
        let value = match (column.shape, literal) {
            (Shape::Int, Literal::Int(raw)) => {
                let value = raw.parse::<i64>().map_err(|_| {
                    Failure::Fatal(EngineError::invalid(
                        "Python int too large to convert to C long",
                    ))
                })?;
                if !int_fits(value, bits) && set_float {
                    cast_column = Some(64);
                }
                Scalar::Int(value)
            }
            (Shape::Int, Literal::Float(value)) => {
                if let Some(integer) = float_int(*value, bits) {
                    Scalar::Int(integer)
                } else {
                    cast_column = Some(64);
                    Scalar::Float(*value)
                }
            }
            (Shape::Float, Literal::Int(raw)) => {
                let value = raw.parse::<i64>().map_err(|_| {
                    Failure::Fatal(EngineError::invalid(
                        "Python int too large to convert to C long",
                    ))
                })?;
                match integer_precision(value, target_float, false) {
                    Ok(value) => Scalar::Float(value),
                    Err(_) => {
                        cast_column = Some(0);
                        Scalar::Int(value)
                    }
                }
            }
            (Shape::Float, Literal::Float(value)) => Scalar::Float(if target_float == 32 {
                f64::from(*value as f32)
            } else {
                *value
            }),
            (Shape::Bool, Literal::Bool(value)) => Scalar::Bool(*value),
            (Shape::String, Literal::String(value)) => Scalar::String(value),
            _ => return Err(kernel("no matching membership kernel")),
        };
        set.push(value);
    }
    if let Some(float) = cast_column {
        set.clear();
        for literal in literals {
            set.push(match literal {
                Literal::Int(raw) => Scalar::Int(raw.parse::<i64>().map_err(|_| {
                    Failure::Fatal(EngineError::invalid(
                        "Python int too large to convert to C long",
                    ))
                })?),
                Literal::Float(value) => Scalar::Float(*value),
                _ => return Err(kernel("no matching membership kernel")),
            });
        }
        if float != 0 {
            for value in &mut set {
                if let Scalar::Int(integer) = value {
                    *value = Scalar::Float(integer_precision(*integer, float, true)?);
                }
            }
        }
    }
    let set: std::collections::HashSet<_> = set.into_iter().map(|value| value.key()).collect();
    let mut values = vec![false; n];
    let pair = Pair(&column.data, &column.data);
    let operand = Operand::Column(0);
    for (row, value) in values.iter_mut().enumerate() {
        let found = if !column.valid(row) {
            false
        } else {
            let precision = cast_column.unwrap_or(if column.shape == Shape::Float {
                target_float
            } else {
                0
            });
            let mut scalar = eval::value(&operand, &pair, column.index(row), precision)
                .map_err(Failure::Kernel)?;
            if cast_column == Some(0) && column.shape == Shape::Float {
                let Scalar::Float(number) = scalar else {
                    return Err(kernel("no matching membership kernel"));
                };
                let integer = float_int(number, 64).ok_or_else(|| {
                    kernel(format!(
                        "Float value {number:.6} was truncated converting to int64"
                    ))
                })?;
                scalar = Scalar::Int(integer);
            }
            set.contains(&scalar.key())
        };
        *value = found != negate;
    }
    Ok(booleans(values, all(n)))
}

fn evaluate(
    tree: &Tree,
    base: &[ColumnData],
    meta: &[Option<&serde_json::Map<String, serde_json::Value>>],
    n: usize,
) -> ColumnResult<SourceBits> {
    let live = all(n);
    let mut keys = std::collections::BTreeSet::new();
    for operand in tree.operands.iter().flatten() {
        if let Operand::Dynamic(key) = operand {
            keys.insert(key.as_str());
        }
    }
    let mut bound = std::collections::BTreeMap::new();
    for key in &keys {
        let values: Vec<_> = meta
            .iter()
            .filter_map(|m| m.and_then(|m| m.get(*key)))
            .collect();
        let Some(shape) = infer(&values).map_err(Failure::Fatal)? else {
            return Err(kernel("heterogeneous dynamic column"));
        };
        let (data, valid) = materialize(meta, key, shape, &live).map_err(Failure::Fatal)?;
        bound.insert(
            *key,
            Column {
                data: Cow::Owned(data),
                valid,
                shape,
                scalar: false,
                temporal: Temporal::None,
            },
        );
    }
    let collision = |operand: &Operand| match operand {
        Operand::Dynamic(key) => tree
            .names
            .iter()
            .any(|name| name == &format!("__meta__{key}")),
        Operand::Column(i) | Operand::Timestamp(i) => keys
            .iter()
            .any(|key| tree.names[*i] == format!("__meta__{key}")),
        _ => false,
    };
    let mut work = vec![(tree.ast.root, false)];
    let mut values: Vec<Option<Column<'_>>> = (0..tree.ast.nodes.len()).map(|_| None).collect();
    while let Some((id, finish)) = work.pop() {
        if !finish {
            if let Some(op) = &tree.operands[id] {
                if collision(op) {
                    return Err(kernel("ambiguous synthetic column"));
                }
                values[id] = Some(match op {
                    Operand::Column(i) => {
                        let column = &base[*i];
                        let mut valid = live.clone();
                        if let Some(mask) = column.validity() {
                            valid.intersect(mask.bits());
                        }
                        Column {
                            data: Cow::Borrowed(column.values()),
                            shape: shape(column.values()),
                            valid,
                            scalar: false,
                            temporal: Temporal::None,
                        }
                    }
                    Operand::Timestamp(i) => {
                        let column = &base[*i];
                        let mut valid = live.clone();
                        if let Some(mask) = column.validity() {
                            valid.intersect(mask.bits());
                        }
                        Column {
                            data: Cow::Borrowed(column.values()),
                            shape: shape(column.values()),
                            valid,
                            scalar: false,
                            temporal: Temporal::Timestamp,
                        }
                    }
                    Operand::Dynamic(key) => {
                        let column = bound.get(key.as_str()).ok_or_else(|| {
                            Failure::Fatal(EngineError::Storage(
                                "dynamic column lost binding".into(),
                            ))
                        })?;
                        Column {
                            data: Cow::Borrowed(column.data.as_ref()),
                            shape: column.shape,
                            valid: column.valid.clone(),
                            scalar: false,
                            temporal: Temporal::None,
                        }
                    }
                    Operand::Literal(literal) => {
                        eval::validate_literal(op).map_err(Failure::Fatal)?;
                        let data = match literal {
                            Literal::Int(v) => {
                                ColumnData::Int64(vec![v.parse().map_err(|_| {
                                    Failure::Fatal(EngineError::invalid(
                                        "Python int too large to convert to C long",
                                    ))
                                })?])
                            }
                            Literal::Float(v) => ColumnData::Double(vec![*v]),
                            Literal::Bool(v) => ColumnData::Bool(vec![*v]),
                            Literal::String(v) => ColumnData::VarChar(vec![v.clone()]),
                            Literal::Timestamp(v) => ColumnData::Int64(vec![*v]),
                            // 区间超出 int64 时，3.x 在把标量交给 Arrow 的位置报 OverflowError。
                            Literal::Interval(v) => ColumnData::Int64(vec![interval_micros(*v)?]),
                        };
                        Column {
                            shape: shape(&data),
                            data: Cow::Owned(data),
                            valid: all(1),
                            scalar: true,
                            temporal: match literal {
                                Literal::Timestamp(_) => Temporal::Timestamp,
                                Literal::Interval(_) => Temporal::Interval,
                                _ => Temporal::None,
                            },
                        }
                    }
                });
                continue;
            }
            work.push((id, true));
            match &tree.ast.nodes[id].kind {
                Kind::And(ids) | Kind::Or(ids) => {
                    work.extend(ids.iter().rev().map(|&id| (id, false)))
                }
                Kind::Cmp(_, a, b) | Kind::Arith(_, a, b) => {
                    work.extend([(*b, false), (*a, false)])
                }
                Kind::Not(child)
                | Kind::Like { value: child, .. }
                | Kind::IsNull { value: child, .. }
                | Kind::In { field: child, .. } => work.push((*child, false)),
                _ => {
                    return Err(Failure::Fatal(EngineError::Storage(
                        "Python-only node reached column route".into(),
                    )));
                }
            }
            continue;
        }
        let get = |id: usize| {
            values[id].as_ref().ok_or_else(|| {
                Failure::Fatal(EngineError::Storage("column tree lost operand".into()))
            })
        };
        let column = match &tree.ast.nodes[id].kind {
            Kind::Cmp(op, a, b) => compare(
                get(*a)?,
                get(*b)?,
                Comparison::parse(op).map_err(Failure::Fatal)?,
                n,
            )?,
            Kind::Arith(op, a, b) => arithmetic(op, get(*a)?, get(*b)?, n)?,
            Kind::And(ids) | Kind::Or(ids) => {
                let and = matches!(tree.ast.nodes[id].kind, Kind::And(_));
                let mut data = vec![and; n];
                let mut valid = live.clone();
                for &child in ids {
                    let child = get(child)?;
                    for (i, out) in data.iter_mut().enumerate() {
                        let c = child.boolean(i)?;
                        let decisive =
                            (valid.get(i) && *out != and) || (child.valid(i) && c != and);
                        let known = decisive || (valid.get(i) && child.valid(i));
                        *out = if decisive { !and } else { and };
                        valid.set(i, known);
                    }
                }
                booleans(data, valid)
            }
            Kind::Not(child) => {
                let child = get(*child)?;
                let mut data = Vec::with_capacity(n);
                let mut valid = SourceBits::new(n);
                for i in 0..n {
                    data.push(!child.boolean(i)?);
                    valid.set(i, child.valid(i));
                }
                booleans(data, valid)
            }
            Kind::IsNull { value, negate, .. } => {
                let value = get(*value)?;
                booleans(
                    (0..n).map(|i| !value.valid(i) != *negate).collect(),
                    live.clone(),
                )
            }
            Kind::Like { value, .. } => {
                let value = get(*value)?;
                if value.shape != Shape::String {
                    return Err(kernel("no matching LIKE kernel"));
                }
                let pattern = tree.patterns[id].as_ref().ok_or_else(|| {
                    Failure::Fatal(EngineError::Storage("LIKE lost pattern".into()))
                })?;
                let mut data = vec![false; n];
                let mut valid = SourceBits::new(n);
                for (row, out) in data.iter_mut().enumerate() {
                    if value.valid(row) {
                        let ColumnData::VarChar(strings) = value.data.as_ref() else {
                            return Err(kernel("no matching LIKE kernel"));
                        };
                        *out = pattern.matches(
                            &strings[value.index(row)]
                                .chars()
                                .map(u32::from)
                                .collect::<Vec<_>>(),
                            false,
                        );
                        valid.set(row, true);
                    }
                }
                booleans(data, valid)
            }
            Kind::In {
                field,
                list,
                negate,
            } => {
                let Kind::List(ids) = &tree.ast.nodes[*list].kind else {
                    return Err(Failure::Fatal(EngineError::Storage(
                        "IN lost literal list".into(),
                    )));
                };
                let literals = ids
                    .iter()
                    .map(|&id| match &tree.ast.nodes[id].kind {
                        Kind::Lit(l) => Ok(l.clone()),
                        _ => Err(Failure::Fatal(eval::unsupported())),
                    })
                    .collect::<ColumnResult<Vec<_>>>()?;
                in_list(get(*field)?, &literals, *negate, n)?
            }
            _ => {
                return Err(Failure::Fatal(EngineError::Storage(
                    "column route lost AST node".into(),
                )));
            }
        };
        values[id] = Some(column);
    }
    let root = values[tree.ast.root]
        .as_ref()
        .ok_or_else(|| Failure::Fatal(EngineError::Storage("column route lost root".into())))?;
    let mut matching = SourceBits::new(n);
    for row in 0..n {
        matching.set(row, root.valid(row) && root.boolean(row)?);
    }
    Ok(matching)
}
pub(super) fn evaluate_rows(tree: &Tree, rows: &[DynamicRow<'_>]) -> Result<SourceBits> {
    let referenced = tree
        .operands
        .iter()
        .flatten()
        .filter_map(|operand| {
            if let Operand::Column(i) | Operand::Timestamp(i) = operand {
                Some(*i)
            } else {
                None
            }
        })
        .collect();
    let columns = super::dynamic::source_columns(rows, &referenced, true)?;
    let meta: Vec<_> = rows.iter().map(|row| row.meta).collect();
    match evaluate(tree, &columns, &meta, rows.len()) {
        Ok(mask) => Ok(mask),
        Err(Failure::Kernel(_)) if tree.backend == Backend::Hybrid => {
            super::python::evaluate_tree(tree, rows)
        }
        Err(Failure::Kernel(error) | Failure::Fatal(error)) => Err(error),
    }
}
