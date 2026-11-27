//! 布尔树的列式求值；可回退的内核错误与不可回退的整数溢出分开传播。

use super::dynamic::{DynamicRow, Shape, infer, materialize_values, members};
use super::eval::{self, Comparison, Operand, Scalar};
use super::parser::{Kind, Literal};
use super::tree::{Backend, Tree};
use crate::data::ColumnData;
use crate::error::{EngineError, Result};
use crate::storage::visibility::SourceBits;
use std::borrow::Cow;
use std::collections::HashSet;
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
#[derive(Clone)]
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
    #[cfg(test)]
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
    // Arrow 按类型分派比较内核，不看值：没有有效行（例如空类型参与算术后的全 null 列）也报错。
    let class = |shape: Shape| match shape {
        Shape::Int | Shape::Float => Some(0),
        Shape::Bool => Some(1),
        Shape::String => Some(2),
        Shape::Null | Shape::Complex => None,
    };
    if let (Some(x), Some(y)) = (class(a.shape), class(b.shape))
        && x != y
    {
        return Err(kernel("no matching comparison kernel"));
    }
    let pair = Pair(&a.data, &b.data);
    let (left, right) = (Operand::Column(0), Operand::Column(1));
    let precision =
        eval::operand_precision(&left, &pair).max(eval::operand_precision(&right, &pair));
    // 整数一侧转浮点的安全检查按这一侧自己的有效位，与另一侧是否为 null 无关（3.x 先转换整列再比较）。
    if precision != 0 {
        for (side, operand) in [(a, &left), (b, &right)] {
            if side.shape == Shape::Int {
                for row in 0..extent(side, n) {
                    if side.valid.get(row) {
                        eval::value(operand, &pair, row, precision).map_err(Failure::Kernel)?;
                    }
                }
            }
        }
    }
    let valid = both_valid(a, b, n);
    if let Some(mut values) = typed_compare(a, b, op, precision, n) {
        mask(&mut values, &valid);
        return Ok(booleans(values, valid));
    }
    let mut values = vec![false; n];
    for (row, value) in values.iter_mut().enumerate() {
        if valid.get(row) {
            let a = eval::value(&left, &pair, a.index(row), precision).map_err(Failure::Kernel)?;
            let b = eval::value(&right, &pair, b.index(row), precision).map_err(Failure::Kernel)?;
            *value = scalar_compare(a, b, op)?;
        }
    }
    Ok(booleans(values, valid))
}

/// 一侧的有效位按 `n` 行展开成字：标量广播（填充位由 [`SourceBits::from_words`] 清掉）。
fn valid_words<'a>(column: &'a Column<'_>, n: usize) -> Cow<'a, [u64]> {
    if column.scalar {
        let word = if column.valid.get(0) { u64::MAX } else { 0 };
        Cow::Owned(vec![word; n.div_ceil(64)])
    } else {
        Cow::Borrowed(column.valid.words())
    }
}
fn both_valid(a: &Column<'_>, b: &Column<'_>, n: usize) -> SourceBits {
    let (x, y) = (valid_words(a, n), valid_words(b, n));
    SourceBits::from_words(n, x.iter().zip(y.iter()).map(|(x, y)| x & y).collect())
}
/// 无效行的值清成 false，与逐行写入的结果逐位相同。
fn mask(values: &mut [bool], valid: &SourceBits) {
    for (chunk, word) in values.chunks_mut(64).zip(valid.words()) {
        for (bit, value) in chunk.iter_mut().enumerate() {
            *value &= word >> bit & 1 == 1;
        }
    }
}
fn unpack(words: &[u64], n: usize) -> Vec<bool> {
    (0..n)
        .map(|row| words[row / 64] >> (row % 64) & 1 == 1)
        .collect()
}
/// 布尔列的值按字打包；不是布尔列时返回 None，由调用方决定是否报错。
fn bool_words(column: &Column<'_>, n: usize) -> Option<Vec<u64>> {
    let ColumnData::Bool(values) = column.data.as_ref() else {
        return None;
    };
    Some(if column.scalar {
        let word = if values[0] { u64::MAX } else { 0 };
        vec![word; n.div_ceil(64)]
    } else {
        SourceBits::from_live(&values[..n]).words().to_vec()
    })
}
fn lost_boolean() -> Failure {
    Failure::Fatal(EngineError::Storage("column boolean node lost type".into()))
}

/// 逐行比较两侧的值，不看有效位（调用方再按有效位清零）。
fn rows<T: PartialOrd>(
    n: usize,
    a: impl Fn(usize) -> T,
    b: impl Fn(usize) -> T,
    op: Comparison,
) -> Vec<bool> {
    macro_rules! run {
        ($cmp:tt) => {
            (0..n).map(|row| a(row) $cmp b(row)).collect()
        };
    }
    match op {
        Comparison::Equal => run!(==),
        Comparison::NotEqual => run!(!=),
        Comparison::Less => run!(<),
        Comparison::LessEqual => run!(<=),
        Comparison::Greater => run!(>),
        Comparison::GreaterEqual => run!(>=),
    }
}
/// 比较的类型化内核：与 `eval::value` + `scalar_compare` 逐行求值的结果相同（转换的越界检查已在前面
/// 做完，这里不会出错）。至少一侧是列；类型组合不在这里的回到逐行路径。
fn typed_compare(
    a: &Column<'_>,
    b: &Column<'_>,
    op: Comparison,
    precision: u8,
    n: usize,
) -> Option<Vec<bool>> {
    // 列放在左侧；`a < b` 与 `b > a` 对浮点（含 NaN）同样成立。
    let (column, other, op) = if a.scalar {
        (b, a, op.reversed())
    } else {
        (a, b, op)
    };
    if column.scalar {
        return None;
    }
    // 下面按数据类型分派、按 shape 选内核；本模块构造的列两者一致（空类型与复合类型不进快路径）。
    debug_assert!(
        [column, other]
            .iter()
            .all(|side| matches!(side.shape, Shape::Null | Shape::Complex)
                || side.shape == shape(side.data.values()))
    );
    // 另一侧：标量绑成常量，列按行读。
    macro_rules! against {
        ($left:expr, $right:expr) => {{
            let right = $right;
            if other.scalar {
                let constant = right(0);
                rows(n, $left, |_| constant, op)
            } else {
                rows(n, $left, right, op)
            }
        }};
    }
    match (column.shape, other.shape) {
        (Shape::String, Shape::String) => {
            let (ColumnData::VarChar(x), ColumnData::VarChar(y)) =
                (column.data.values(), other.data.values())
            else {
                return None;
            };
            Some(against!(|row| x[row].as_str(), |row: usize| y[row].as_str()))
        }
        (Shape::Bool, Shape::Bool) => {
            let (ColumnData::Bool(x), ColumnData::Bool(y)) =
                (column.data.values(), other.data.values())
            else {
                return None;
            };
            Some(against!(|row| x[row], |row: usize| y[row]))
        }
        (Shape::Int | Shape::Float, Shape::Int | Shape::Float) => {
            numeric(column)?;
            numeric(other)?;
            if precision == 0 {
                // 两侧都是整数（任一侧是浮点时精度不为 0）。
                let y = &other.data;
                Some(match column.data.values() {
                    ColumnData::Int8(x) => against!(|row| i64::from(x[row]), |row| int_at(y, row)),
                    ColumnData::Int16(x) => {
                        against!(|row| i64::from(x[row]), |row| int_at(y, row))
                    }
                    ColumnData::Int32(x) => {
                        against!(|row| i64::from(x[row]), |row| int_at(y, row))
                    }
                    ColumnData::Int64(x) => against!(|row| x[row], |row| int_at(y, row)),
                    _ => return None,
                })
            } else {
                // 与 `eval::value` 相同：float32 升为 f64；整数已在前面检查落在目标精度能精确表示的范围内
                // （float32 为 ±2^24），先转 float32 再升 f64 与直接转 f64 相同。
                let convert = |data: &ColumnData, row: usize| match data.values() {
                    ColumnData::Float(v) => f64::from(v[row]),
                    ColumnData::Double(v) => v[row],
                    other => int_at(other, row) as f64,
                };
                let y = &other.data;
                // 另一侧是列时先整列转换，主循环只按主侧的类型分派。
                let converted: Option<Vec<f64>> =
                    (!other.scalar).then(|| (0..n).map(|row| convert(y, row)).collect());
                let right = |row: usize| match &converted {
                    Some(values) => values[row],
                    None => convert(y, row),
                };
                Some(match column.data.values() {
                    ColumnData::Float(x) => against!(|row| f64::from(x[row]), right),
                    ColumnData::Double(x) => against!(|row| x[row], right),
                    ColumnData::Int8(x) => against!(|row| f64::from(x[row]), right),
                    ColumnData::Int16(x) => against!(|row| f64::from(x[row]), right),
                    ColumnData::Int32(x) => against!(|row| f64::from(x[row]), right),
                    ColumnData::Int64(x) => against!(|row| x[row] as f64, right),
                    _ => return None,
                })
            }
        }
        _ => None,
    }
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

fn arithmetic(op: &str, a: &Column<'_>, b: &Column<'_>, n: usize) -> ColumnResult<Column<'static>> {
    if a.temporal != Temporal::None || b.temporal != Temporal::None {
        temporal_arithmetic(op, a, b, n)
    } else {
        numeric_arithmetic(op, a, b, n)
    }
}

/// 数值列的元素类型；类型提升与转换按 pyarrow 的 `add` / `subtract` / `multiply` / `divide`（0032）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Numeric {
    Int(u32),
    Float32,
    Float64,
}
fn numeric(column: &Column<'_>) -> Option<Numeric> {
    match column.data.values() {
        ColumnData::Int8(_) => Some(Numeric::Int(8)),
        ColumnData::Int16(_) => Some(Numeric::Int(16)),
        ColumnData::Int32(_) => Some(Numeric::Int(32)),
        ColumnData::Int64(_) => Some(Numeric::Int(64)),
        ColumnData::Float(_) => Some(Numeric::Float32),
        ColumnData::Double(_) => Some(Numeric::Float64),
        _ => None,
    }
}
fn int_at(data: &ColumnData, index: usize) -> i64 {
    match data.values() {
        ColumnData::Int8(v) => i64::from(v[index]),
        ColumnData::Int16(v) => i64::from(v[index]),
        ColumnData::Int32(v) => i64::from(v[index]),
        ColumnData::Int64(v) => v[index],
        // 调用方先经 `numeric` 判定为整数列。
        _ => {
            debug_assert!(false, "int_at on a non-integer column");
            0
        }
    }
}
fn float_at(data: &ColumnData, index: usize) -> f64 {
    match data.values() {
        ColumnData::Float(v) => f64::from(v[index]),
        ColumnData::Double(v) => v[index],
        // 整数列（调用方先经 `numeric` 判定为数值列）。
        other => int_at(other, index) as f64,
    }
}
/// 一侧（列或标量）的行数：标量只有一个值。
fn extent(column: &Column<'_>, n: usize) -> usize {
    if column.scalar { 1 } else { n }
}
/// Arrow 隐式把整数转成浮点时的安全检查：只看这一侧自己的有效位（与另一侧是否为 null 无关），按行序报第一个越界值。
fn check_int_to_float(column: &Column<'_>, float: Numeric, n: usize) -> ColumnResult<()> {
    let bound: i64 = if float == Numeric::Float32 {
        16_777_216
    } else {
        9_007_199_254_740_992
    };
    for row in 0..extent(column, n) {
        if column.valid.get(row) {
            let value = int_at(&column.data, row);
            if !(-bound..=bound).contains(&value) {
                return Err(kernel(format!(
                    "Integer value {value} not in range: -{bound} to {bound}"
                )));
            }
        }
    }
    Ok(())
}
/// 3.x `_to_float`：`/` 之前把整数列安全转换成 double，整数标量用 Python `float()`（正确舍入、不检查）。
fn divide_operand<'a>(column: &'a Column<'a>, n: usize) -> ColumnResult<Cow<'a, Column<'a>>> {
    if !matches!(numeric(column), Some(Numeric::Int(_))) || column.shape == Shape::Null {
        return Ok(Cow::Borrowed(column));
    }
    if !column.scalar {
        check_int_to_float(column, Numeric::Float64, n)?;
    }
    let rows = extent(column, n);
    Ok(Cow::Owned(Column {
        data: Cow::Owned(ColumnData::Double(
            (0..rows)
                .map(|row| int_at(&column.data, row) as f64)
                .collect(),
        )),
        valid: column.valid.clone(),
        shape: Shape::Float,
        scalar: column.scalar,
        temporal: Temporal::None,
    }))
}

/// 数值算术（0032）：整数按结果宽度回绕，整数转浮点带检查，浮点按结果类型的 IEEE 运算；
/// 只对两侧都有效的行计算，null 传播为 null。
fn numeric_arithmetic(
    op: &str,
    a: &Column<'_>,
    b: &Column<'_>,
    n: usize,
) -> ColumnResult<Column<'static>> {
    let (a, b) = if op == "/" {
        (divide_operand(a, n)?, divide_operand(b, n)?)
    } else {
        (Cow::Borrowed(a), Cow::Borrowed(b))
    };
    let (a, b) = (a.as_ref(), b.as_ref());
    let scalar = a.scalar && b.scalar;
    let rows = if scalar { 1 } else { n };
    let null = |data: ColumnData, shape: Shape| Column {
        data: Cow::Owned(data),
        valid: SourceBits::new(rows),
        shape,
        scalar,
        temporal: Temporal::None,
    };
    let kind = |column: &Column<'_>| {
        if column.shape == Shape::Null {
            Ok(None)
        } else {
            numeric(column).map(Some).ok_or(())
        }
    };
    let (left, right) = match (kind(a), kind(b)) {
        (Ok(left), Ok(right)) => (left, right),
        _ => {
            let name = match op {
                "+" => "add",
                "-" => "subtract",
                "*" => "multiply",
                _ => "divide",
            };
            // 空类型与有类型的一侧运算时先转成那一侧的类型，所以文案里两侧同型。
            let (ta, tb) = match (a.shape, b.shape) {
                (Shape::Null, _) => (column_type(b), column_type(b)),
                (_, Shape::Null) => (column_type(a), column_type(a)),
                _ => (column_type(a), column_type(b)),
            };
            return Err(kernel(format!(
                "Function '{name}' has no kernel matching input types ({ta}, {tb})"
            )));
        }
    };
    let target = match (left, right) {
        (None, None) => return Ok(null(ColumnData::Bool(vec![false; rows]), Shape::Null)),
        (Some(t), None) | (None, Some(t)) => t,
        (Some(Numeric::Int(x)), Some(Numeric::Int(y))) => Numeric::Int(x.max(y)),
        (Some(Numeric::Float64), _) | (_, Some(Numeric::Float64)) => Numeric::Float64,
        _ => Numeric::Float32,
    };
    // 空类型一侧：结果是目标类型的全 null 列。
    if left.is_none() || right.is_none() {
        return Ok(null(empty_numeric(target, rows), shape_of(target)));
    }
    if !matches!(target, Numeric::Int(_)) {
        for (side, kind) in [(a, left), (b, right)] {
            if matches!(kind, Some(Numeric::Int(_))) {
                check_int_to_float(side, target, n)?;
            }
        }
    }
    let valid = both_valid(a, b, rows);
    let data = match target {
        Numeric::Int(bits) => {
            let values = (0..rows).map(|row| {
                if !valid.get(row) {
                    return 0;
                }
                let (x, y) = (int_at(&a.data, a.index(row)), int_at(&b.data, b.index(row)));
                match op {
                    "+" => x.wrapping_add(y),
                    "-" => x.wrapping_sub(y),
                    _ => x.wrapping_mul(y),
                }
            });
            // 截断到结果宽度即按该宽度回绕（二进制补码）。
            match bits {
                8 => ColumnData::Int8(values.map(|v| v as i8).collect()),
                16 => ColumnData::Int16(values.map(|v| v as i16).collect()),
                32 => ColumnData::Int32(values.map(|v| v as i32).collect()),
                _ => ColumnData::Int64(values.collect()),
            }
        }
        Numeric::Float32 => ColumnData::Float(
            (0..rows)
                .map(|row| {
                    if !valid.get(row) {
                        return 0.0;
                    }
                    // 整数已检查在 ±2^24 内，转成 float32 是精确的；float32 运算保持单精度舍入。
                    let x = float_at(&a.data, a.index(row)) as f32;
                    let y = float_at(&b.data, b.index(row)) as f32;
                    float_op(op, x, y)
                })
                .collect(),
        ),
        Numeric::Float64 => ColumnData::Double(
            (0..rows)
                .map(|row| {
                    if !valid.get(row) {
                        return 0.0;
                    }
                    float_op(
                        op,
                        float_at(&a.data, a.index(row)),
                        float_at(&b.data, b.index(row)),
                    )
                })
                .collect(),
        ),
    };
    Ok(Column {
        data: Cow::Owned(data),
        valid,
        shape: shape_of(target),
        scalar,
        temporal: Temporal::None,
    })
}
fn float_op<
    T: std::ops::Add<Output = T>
        + std::ops::Sub<Output = T>
        + std::ops::Mul<Output = T>
        + std::ops::Div<Output = T>,
>(
    op: &str,
    x: T,
    y: T,
) -> T {
    match op {
        "+" => x + y,
        "-" => x - y,
        "*" => x * y,
        _ => x / y,
    }
}
fn shape_of(numeric: Numeric) -> Shape {
    if matches!(numeric, Numeric::Int(_)) {
        Shape::Int
    } else {
        Shape::Float
    }
}
fn empty_numeric(numeric: Numeric, rows: usize) -> ColumnData {
    match numeric {
        Numeric::Int(8) => ColumnData::Int8(vec![0; rows]),
        Numeric::Int(16) => ColumnData::Int16(vec![0; rows]),
        Numeric::Int(32) => ColumnData::Int32(vec![0; rows]),
        Numeric::Int(_) => ColumnData::Int64(vec![0; rows]),
        Numeric::Float32 => ColumnData::Float(vec![0.0; rows]),
        Numeric::Float64 => ColumnData::Double(vec![0.0; rows]),
    }
}

/// 时间戳 ± 区间（类型检查已保证两边是时间戳与区间）；Arrow 的非检查运算按 int64 回绕。
fn temporal_arithmetic(
    op: &str,
    a: &Column<'_>,
    b: &Column<'_>,
    n: usize,
) -> ColumnResult<Column<'static>> {
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

/// Kleene and / or，按字计算：每个子节点都求值、不短路（3.x 的 Arrow 内核全量计算）。无效行的值与逐行写法
/// 相同（and 为真、or 为假），下游只在有效行读值。
fn logic<'a, 'b: 'a>(
    and: bool,
    children: impl Iterator<Item = ColumnResult<&'a Column<'b>>>,
    live: &SourceBits,
    n: usize,
) -> ColumnResult<Column<'static>> {
    let mut data = vec![if and { u64::MAX } else { 0 }; n.div_ceil(64)];
    let mut valid = live.words().to_vec();
    for child in children {
        let child = child?;
        if n == 0 {
            continue;
        }
        let values = bool_words(child, n).ok_or_else(lost_boolean)?;
        let child_valid = valid_words(child, n);
        for (((x, vx), y), vy) in data
            .iter_mut()
            .zip(valid.iter_mut())
            .zip(&values)
            .zip(child_valid.iter())
        {
            let decisive = if and {
                (*vx & !*x) | (vy & !y)
            } else {
                (*vx & *x) | (vy & y)
            };
            *vx = decisive | (*vx & vy);
            *x = if and { !decisive } else { decisive };
        }
    }
    Ok(booleans(unpack(&data, n), SourceBits::from_words(n, valid)))
}
fn negation(child: &Column<'_>, n: usize) -> ColumnResult<Column<'static>> {
    if n == 0 {
        return Ok(booleans(Vec::new(), SourceBits::new(0)));
    }
    let ColumnData::Bool(values) = child.data.as_ref() else {
        return Err(lost_boolean());
    };
    let data = (0..n).map(|row| !values[child.index(row)]).collect();
    Ok(booleans(
        data,
        SourceBits::from_words(n, valid_words(child, n).into_owned()),
    ))
}
fn is_null(value: &Column<'_>, negate: bool, live: &SourceBits, n: usize) -> Column<'static> {
    let words = valid_words(value, n);
    let data = if negate {
        unpack(&words, n)
    } else {
        unpack(&words.iter().map(|word| !word).collect::<Vec<_>>(), n)
    };
    booleans(data, live.clone())
}
fn like(
    value: &Column<'_>,
    pattern: &super::python::LikePattern,
    n: usize,
) -> ColumnResult<Column<'static>> {
    let valid = SourceBits::from_words(n, valid_words(value, n).into_owned());
    let ColumnData::VarChar(strings) = value.data.as_ref() else {
        // 只在有有效行时才需要读值，也才报错。
        if valid.any_live() {
            return Err(kernel("no matching LIKE kernel"));
        }
        return Ok(booleans(vec![false; n], valid));
    };
    let mut matcher = pattern.arrow_matcher();
    let data = (0..n)
        .map(|row| valid.get(row) && matcher.matches(&strings[value.index(row)]))
        .collect();
    Ok(booleans(data, valid))
}
/// 根节点为真且有效的行；只在有效行读值，没有有效行时不要求是布尔列。
fn matching(root: &Column<'_>, n: usize) -> ColumnResult<SourceBits> {
    let valid = valid_words(root, n);
    let Some(values) = bool_words(root, n) else {
        if valid.iter().any(|word| *word != 0) {
            return Err(lost_boolean());
        }
        return Ok(SourceBits::new(n));
    };
    Ok(SourceBits::from_words(
        n,
        values
            .iter()
            .zip(valid.iter())
            .map(|(x, v)| x & v)
            .collect(),
    ))
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
    if cast_column.is_none()
        && !column.scalar
        && let Some(found) = typed_members(column, &set, n)
    {
        let values = found
            .into_iter()
            .enumerate()
            .map(|(row, found)| (found && column.valid(row)) != negate)
            .collect();
        return Ok(booleans(values, all(n)));
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

/// 不需要转换列类型时的成员判定：集合按列的类型排序后二分查找，与按 `ScalarKey` 哈希的结果相同（浮点按位
/// 比较，区分 ±0 与 NaN 的位型）。不看有效位；类型不在这里的回到逐行路径。
fn typed_members(column: &Column<'_>, set: &[Scalar<'_>], n: usize) -> Option<Vec<bool>> {
    fn sorted<T: Ord>(mut keys: Vec<T>) -> Vec<T> {
        keys.sort_unstable();
        keys.dedup();
        keys
    }
    let ints = || {
        sorted(
            set.iter()
                .filter_map(|value| match value {
                    Scalar::Int(value) => Some(*value),
                    _ => None,
                })
                .collect(),
        )
    };
    let floats = || {
        sorted(
            set.iter()
                .filter_map(|value| match value {
                    Scalar::Float(value) => Some(value.to_bits()),
                    _ => None,
                })
                .collect(),
        )
    };
    macro_rules! lookup {
        ($keys:expr, $values:expr, $key:expr) => {{
            let (keys, values) = ($keys, $values);
            (0..n)
                .map(|row| keys.binary_search(&$key(&values[row])).is_ok())
                .collect()
        }};
    }
    Some(match column.data.values() {
        ColumnData::Int8(values) => lookup!(ints(), values, |v: &i8| i64::from(*v)),
        ColumnData::Int16(values) => lookup!(ints(), values, |v: &i16| i64::from(*v)),
        ColumnData::Int32(values) => lookup!(ints(), values, |v: &i32| i64::from(*v)),
        ColumnData::Int64(values) => lookup!(ints(), values, |v: &i64| *v),
        ColumnData::Float(values) => {
            lookup!(floats(), values, |v: &f32| f64::from(*v).to_bits())
        }
        ColumnData::Double(values) => lookup!(floats(), values, |v: &f64| v.to_bits()),
        ColumnData::VarChar(values) => {
            let keys = sorted(
                set.iter()
                    .filter_map(|value| match value {
                        Scalar::String(value) => Some(*value),
                        _ => None,
                    })
                    .collect(),
            );
            (0..n)
                .map(|row| keys.binary_search(&values[row].as_str()).is_ok())
                .collect()
        }
        ColumnData::Bool(values) => {
            let keys = sorted(
                set.iter()
                    .filter_map(|value| match value {
                        Scalar::Bool(value) => Some(*value),
                        _ => None,
                    })
                    .collect(),
            );
            lookup!(keys, values, |v: &bool| *v)
        }
        _ => return None,
    })
}

/// `live` 只屏蔽叶子列的有效位：读值的内核都以有效位为门，`live` 之外的行的值不会被读到；但字面量、
/// `in` / `not in`、`is null` 等节点在这些行上仍可能为真。调用方要么只传存活行（整源路径），要么自行与
/// `live` 取交（按块路径）。
fn evaluate(
    tree: &Tree,
    base: &[ColumnData],
    meta: &[Option<&serde_json::Map<String, serde_json::Value>>],
    n: usize,
    live: &SourceBits,
) -> ColumnResult<SourceBits> {
    let mut keys = std::collections::BTreeSet::new();
    for operand in tree.operands.iter().flatten() {
        if let Operand::Dynamic(key) = operand {
            keys.insert(key.as_str());
        }
    }
    let mut bound = std::collections::BTreeMap::new();
    for key in &keys {
        // 每个键只逐行查一遍 meta：推断看全部行的原始取值（含 null），物化只取存活行的非 null 值。
        // 实际的调用方都只传存活行、`live` 全真；按块路径不含动态键。
        let raw: Vec<_> = meta
            .iter()
            .map(|m| m.and_then(|m| super::dynamic::get_key(m, key)))
            .collect();
        let Some(shape) =
            infer(&raw.iter().flatten().copied().collect::<Vec<_>>()).map_err(Failure::Fatal)?
        else {
            return Err(kernel("heterogeneous dynamic column"));
        };
        let values: Vec<_> = raw
            .into_iter()
            .enumerate()
            .map(|(row, value)| value.filter(|v| live.get(row) && !v.is_null()))
            .collect();
        let (data, valid) = materialize_values(&values, shape).map_err(Failure::Fatal)?;
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
                    // 只有 RowFilter 的树含这个操作数，它们固定走 Python 路线。
                    Operand::Absent => return Err(kernel("absent operand in columnar route")),
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
                Kind::TextMatch { .. } => return Err(kernel("unknown AST node: TextMatchOp")),
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
                let children = ids.iter().map(|&child| get(child));
                logic(and, children, live, n)?
            }
            Kind::Not(child) => negation(get(*child)?, n)?,
            Kind::IsNull { value, negate, .. } => is_null(get(*value)?, *negate, live, n),
            Kind::Like { value, .. } => {
                let value = get(*value)?;
                if value.shape != Shape::String {
                    // Arrow 路线上只有 ARRAY 字段能走到这里（语义类型是动态值）；文案按 pc.match_like。
                    return Err(kernel(format!(
                        "Function 'match_like' has no kernel matching input types ({})",
                        column_type(value)
                    )));
                }
                let pattern = tree.patterns[id].as_ref().ok_or_else(|| {
                    Failure::Fatal(EngineError::Storage("LIKE lost pattern".into()))
                })?;
                like(value, pattern, n)?
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
    matching(root, n)
}
pub(super) fn evaluate_rows(tree: &Tree, rows: &[DynamicRow<'_>]) -> Result<SourceBits> {
    // text_match 的字段只在逐行回退里读取，不必物化；按节点排除，同一列被别的节点引用时仍要保留。
    let text_fields: HashSet<usize> = tree
        .ast
        .nodes
        .iter()
        .filter_map(|node| match node.kind {
            Kind::TextMatch { field, .. } => Some(field),
            _ => None,
        })
        .collect();
    let referenced = tree
        .operands
        .iter()
        .enumerate()
        .filter(|(id, _)| !text_fields.contains(id))
        .filter_map(|(_, operand)| operand.as_ref())
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
    match evaluate(tree, &columns, &meta, rows.len(), &all(rows.len())) {
        Ok(mask) => Ok(mask),
        Err(Failure::Kernel(_)) if tree.backend == Backend::Hybrid => {
            super::python::evaluate_tree(tree, rows)
        }
        Err(Failure::Kernel(error) | Failure::Fatal(error)) => Err(error),
    }
}

/// 在一个块的原列上求值，只用于 [`Tree::chunk_local`] 的树：列式路线上每行的结果只取决于本行，与整源路径
/// 在成功时逐行相同，也就不必按源的遍历顺序逐行复制列。任何错误都返回 None：报错的行序与文案取决于整源
/// 的遍历顺序，调用方丢弃按块结果、回到整源路径重新求值。
pub(super) fn evaluate_chunk(
    tree: &Tree,
    columns: &[ColumnData],
    live: &SourceBits,
) -> Option<SourceBits> {
    let mut mask = evaluate(tree, columns, &[], live.len, live).ok()?;
    mask.intersect(live);
    Some(mask)
}

#[cfg(test)]
mod kernel_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn column(data: ColumnData, valid: &[bool], scalar: bool) -> Column<'static> {
        Column {
            shape: shape(&data),
            data: Cow::Owned(data),
            valid: SourceBits::from_live(valid),
            scalar,
            temporal: Temporal::None,
        }
    }

    // Arrow 按类型分派比较内核：全 null 的整数列（空类型参与算术的结果）与字符串比较也报内核错误，
    // hybrid 据此回退；空类型本身与字符串比较不报。
    #[test]
    fn comparison_kernel_is_chosen_by_type_not_by_values() {
        let text = column(ColumnData::VarChar(vec!["a".into()]), &[true], true);
        let ints = column(ColumnData::Int64(vec![0, 0]), &[false, false], false);
        assert!(matches!(
            compare(&ints, &text, Comparison::Equal, 2),
            Err(Failure::Kernel(_))
        ));
        let flags = column(ColumnData::Bool(vec![false, false]), &[false, false], false);
        assert!(matches!(
            compare(&flags, &ints, Comparison::Less, 2),
            Err(Failure::Kernel(_))
        ));
        let mut null = column(ColumnData::Bool(vec![false, false]), &[false, false], false);
        null.shape = Shape::Null;
        assert!(compare(&null, &text, Comparison::Equal, 2).is_ok());
        let floats = column(ColumnData::Double(vec![0.0, 0.0]), &[false, false], false);
        assert!(compare(&ints, &floats, Comparison::Equal, 2).is_ok());
    }
}
