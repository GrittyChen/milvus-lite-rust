//! 类型化内核与优化前逐行内核的随机对拍：结果、有效位、错误种类与文案都要相同。

use super::*;
use crate::filter::python::LikePattern;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const ROWS: [usize; 9] = [0, 1, 2, 63, 64, 65, 128, 129, 200];
const ALPHABET: [char; 7] = ['a', 'b', '%', '_', '\\', 'é', '\n'];

fn text(rng: &mut Rng, max: usize) -> String {
    (0..rng.below(max + 1))
        .map(|_| *rng.pick(&ALPHABET))
        .collect()
}

fn int_value(rng: &mut Rng) -> i64 {
    if rng.below(40) == 0 {
        *rng.pick(&[
            127,
            -128,
            300,
            16_777_216,
            16_777_217,
            -16_777_217,
            9_007_199_254_740_992,
            9_007_199_254_740_993,
            i64::MAX,
            i64::MIN,
        ])
    } else {
        rng.below(7) as i64 - 3
    }
}

fn float_value(rng: &mut Rng) -> f64 {
    *rng.pick(&[
        0.0,
        -0.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        1.0,
        2.5,
        -2.0,
        3.0,
        16_777_217.0,
        0.1,
        1e300,
    ])
}

/// 随机列：`kind` 0–7 依次为 int8/16/32/64、float、double、varchar、bool，8 为空类型。
fn random_column(rng: &mut Rng, n: usize, kind: usize, scalar: bool) -> Column<'static> {
    let rows = if scalar { 1 } else { n };
    let data = match kind {
        0 => ColumnData::Int8((0..rows).map(|_| int_value(rng) as i8).collect()),
        1 => ColumnData::Int16((0..rows).map(|_| int_value(rng) as i16).collect()),
        2 => ColumnData::Int32((0..rows).map(|_| int_value(rng) as i32).collect()),
        3 => ColumnData::Int64((0..rows).map(|_| int_value(rng)).collect()),
        4 => ColumnData::Float((0..rows).map(|_| float_value(rng) as f32).collect()),
        5 => ColumnData::Double((0..rows).map(|_| float_value(rng)).collect()),
        6 => ColumnData::VarChar((0..rows).map(|_| text(rng, 4)).collect()),
        _ => ColumnData::Bool((0..rows).map(|_| rng.below(2) == 0).collect()),
    };
    let mode = rng.below(3);
    let valid: Vec<bool> = (0..rows)
        .map(|_| kind != 8 && (mode == 0 || (mode == 1 && rng.below(2) == 0)))
        .collect();
    Column {
        shape: if kind == 8 { Shape::Null } else { shape(&data) },
        data: Cow::Owned(data),
        valid: SourceBits::from_live(&valid),
        scalar,
        temporal: Temporal::None,
    }
}

fn live(rng: &mut Rng, n: usize) -> SourceBits {
    let mode = rng.below(3);
    SourceBits::from_live(
        &(0..n)
            .map(|_| mode == 0 || (mode == 1 && rng.below(3) != 0))
            .collect::<Vec<_>>(),
    )
}

type Outcome = std::result::Result<(String, Vec<bool>, usize, bool, String), (bool, String)>;
fn outcome(result: ColumnResult<Column<'_>>) -> Outcome {
    match result {
        Ok(column) => Ok((
            format!("{:?}", column.data.as_ref()),
            (0..column.valid.len)
                .map(|row| column.valid.get(row))
                .collect(),
            column.valid.count_live(),
            column.scalar,
            format!("{:?}", column.shape),
        )),
        Err(Failure::Kernel(error)) => Err((true, error.to_string())),
        Err(Failure::Fatal(error)) => Err((false, error.to_string())),
    }
}

const OPS: [Comparison; 6] = [
    Comparison::Equal,
    Comparison::NotEqual,
    Comparison::Less,
    Comparison::LessEqual,
    Comparison::Greater,
    Comparison::GreaterEqual,
];

#[test]
fn compare_matches_the_row_model() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut typed = 0;
    for round in 0..60_000 {
        let n = *rng.pick(&ROWS);
        let (ka, kb) = if rng.below(3) == 0 {
            (rng.below(9), rng.below(9))
        } else {
            // 多数轮取可比较的组合，才会走到类型化内核。
            const GROUPS: [&[usize]; 3] = [&[0, 1, 2, 3, 4, 5], &[6], &[7]];
            let group = GROUPS[rng.below(3)];
            (*rng.pick(group), *rng.pick(group))
        };
        let a = {
            let scalar = rng.below(4) == 0;
            random_column(&mut rng, n, ka, scalar)
        };
        let b = {
            let scalar = rng.below(3) == 0;
            random_column(&mut rng, n, kb, scalar)
        };
        let op = *rng.pick(&OPS);
        let expected = outcome(old_compare(&a, &b, op, n));
        let actual = outcome(compare(&a, &b, op, n));
        typed += usize::from(expected.is_ok() && !(a.scalar && b.scalar));
        assert_eq!(actual, expected, "round {round}: {ka} {kb} {op:?} n={n}");
    }
    assert!(typed > 20_000, "{typed}");
}

// 整数转浮点的合法边界附近（float32 为 ±2^24，double 为 ±2^53）比较要走到类型化内核并与模型一致；随机值
// 里越界值会让整轮报错，这里单独覆盖成功路径。
#[test]
fn compare_at_exact_float_bounds_matches_the_row_model() {
    let column = |data: ColumnData, scalar: bool| Column {
        shape: shape(&data),
        valid: SourceBits::from_live(&vec![true; if scalar { 1 } else { 4 }]),
        data: Cow::Owned(data),
        scalar,
        temporal: Temporal::None,
    };
    // 16_777_217 与 2^53 - 1 转 float32 会舍入：配 double 时必须按 f64 精确比较；配 float32 时越界报错。
    let ints = [16_777_215, 16_777_216, 16_777_217, 9_007_199_254_740_991];
    let doubles = [
        16_777_216.0,
        16_777_217.0,
        9_007_199_254_740_991.0,
        9_007_199_254_740_992.0,
    ];
    let mut typed = 0;
    for int in [
        ColumnData::Int64(ints.to_vec()),
        ColumnData::Int32(ints.iter().map(|v| *v as i32).collect()),
    ] {
        for float in [
            ColumnData::Double(doubles.to_vec()),
            ColumnData::Float(doubles.iter().map(|v| *v as f32).collect()),
        ] {
            for (scalar_int, scalar_float) in [(false, false), (false, true), (true, false)] {
                for (left_int, op) in [true, false]
                    .into_iter()
                    .flat_map(|l| OPS.map(|op| (l, op)))
                {
                    let pick = |data: &ColumnData, scalar: bool| {
                        let data = if scalar {
                            match data {
                                ColumnData::Int64(v) => ColumnData::Int64(vec![v[1]]),
                                ColumnData::Int32(v) => ColumnData::Int32(vec![v[1]]),
                                ColumnData::Double(v) => ColumnData::Double(vec![v[1]]),
                                ColumnData::Float(v) => ColumnData::Float(vec![v[1]]),
                                other => other.clone(),
                            }
                        } else {
                            data.clone()
                        };
                        column(data, scalar)
                    };
                    let (a, b) = (pick(&int, scalar_int), pick(&float, scalar_float));
                    let (a, b) = if left_int { (a, b) } else { (b, a) };
                    let expected = outcome(old_compare(&a, &b, op, 4));
                    typed += usize::from(expected.is_ok());
                    assert_eq!(
                        outcome(compare(&a, &b, op, 4)),
                        expected,
                        "{left_int} {scalar_int} {scalar_float} {op:?}"
                    );
                }
            }
        }
    }
    assert!(typed > 50, "{typed}");
}

#[test]
fn arithmetic_matches_the_row_model() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for round in 0..30_000 {
        let n = *rng.pick(&ROWS);
        let (ka, kb) = (rng.below(9), rng.below(9));
        let a = {
            let scalar = rng.below(4) == 0;
            random_column(&mut rng, n, ka, scalar)
        };
        let b = {
            let scalar = rng.below(3) == 0;
            random_column(&mut rng, n, kb, scalar)
        };
        let op = *rng.pick(&["+", "-", "*", "/"]);
        let expected = outcome(old_numeric_arithmetic(op, &a, &b, n));
        let actual = outcome(numeric_arithmetic(op, &a, &b, n));
        assert_eq!(actual, expected, "round {round}: {ka} {op} {kb} n={n}");
    }
}

fn literal(rng: &mut Rng, kind: usize) -> Literal {
    match kind {
        0 => Literal::Int(
            rng.pick(&[
                "0",
                "1",
                "-1",
                "2",
                "3",
                "127",
                "300",
                "-129",
                "16777217",
                "9007199254740993",
                "99999999999999999999",
            ])
            .to_string(),
        ),
        1 => Literal::Float(*rng.pick(&[
            0.0,
            -0.0,
            1.0,
            2.5,
            -2.0,
            3.0,
            f64::INFINITY,
            16_777_217.0,
            1e19,
            0.1,
        ])),
        2 => Literal::Bool(rng.below(2) == 0),
        _ => Literal::String(text(rng, 2)),
    }
}

#[test]
fn in_list_matches_the_row_model() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut typed = 0;
    for round in 0..60_000 {
        let n = *rng.pick(&ROWS);
        let kind = rng.below(9);
        let column = {
            let scalar = rng.below(8) == 0;
            random_column(&mut rng, n, kind, scalar)
        };
        let family = match kind {
            0..=5 => [0, 1],
            6 => [3, 3],
            7 => [2, 2],
            _ => [0, 3],
        };
        let literals: Vec<_> = (0..rng.below(6))
            .map(|_| {
                let kind = if rng.below(10) == 0 {
                    rng.below(4)
                } else {
                    *rng.pick(&family)
                };
                literal(&mut rng, kind)
            })
            .collect();
        let negate = rng.below(2) == 0;
        let expected = outcome(old_in_list(&column, &literals, negate, n));
        let actual = outcome(in_list(&column, &literals, negate, n));
        typed += usize::from(expected.is_ok() && !literals.is_empty());
        assert_eq!(
            actual, expected,
            "round {round}: kind {kind} {literals:?} negate={negate} n={n}"
        );
    }
    assert!(typed > 20_000, "{typed}");
}

// 优化前 and / or、not、is null、like 与根节点的逐行写法。
fn old_logic(
    and: bool,
    children: &[&Column<'_>],
    live: &SourceBits,
    n: usize,
) -> ColumnResult<Column<'static>> {
    let mut data = vec![and; n];
    let mut valid = live.clone();
    for child in children {
        for (i, out) in data.iter_mut().enumerate() {
            let c = child.boolean(i)?;
            let decisive = (valid.get(i) && *out != and) || (child.valid(i) && c != and);
            let known = decisive || (valid.get(i) && child.valid(i));
            *out = if decisive { !and } else { and };
            valid.set(i, known);
        }
    }
    Ok(booleans(data, valid))
}
fn old_not(child: &Column<'_>, n: usize) -> ColumnResult<Column<'static>> {
    let mut data = Vec::with_capacity(n);
    let mut valid = SourceBits::new(n);
    for i in 0..n {
        data.push(!child.boolean(i)?);
        valid.set(i, child.valid(i));
    }
    Ok(booleans(data, valid))
}
fn old_is_null(value: &Column<'_>, negate: bool, live: &SourceBits, n: usize) -> Column<'static> {
    booleans(
        (0..n).map(|i| !value.valid(i) != negate).collect(),
        live.clone(),
    )
}
fn old_like(value: &Column<'_>, pattern: &LikePattern, n: usize) -> ColumnResult<Column<'static>> {
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
    Ok(booleans(data, valid))
}
fn old_matching(root: &Column<'_>, n: usize) -> ColumnResult<SourceBits> {
    let mut matching = vec![false; n];
    for (row, out) in matching.iter_mut().enumerate() {
        *out = root.valid(row) && root.boolean(row)?;
    }
    Ok(SourceBits::from_live(&matching))
}

/// 布尔节点的输入：多数是布尔列，偶尔换成别的类型以覆盖类型丢失的错误。
fn boolean_input(rng: &mut Rng, n: usize) -> Column<'static> {
    let kind = if rng.below(30) == 0 { rng.below(9) } else { 7 };
    {
        let scalar = rng.below(5) == 0;
        random_column(rng, n, kind, scalar)
    }
}

#[test]
fn boolean_nodes_match_the_row_model() {
    let mut rng = Rng(0x0bad_cafe_dead_beef);
    for round in 0..40_000 {
        let n = *rng.pick(&ROWS);
        let live = live(&mut rng, n);
        let children: Vec<_> = (0..1 + rng.below(3))
            .map(|_| boolean_input(&mut rng, n))
            .collect();
        let refs: Vec<_> = children.iter().collect();
        let and = rng.below(2) == 0;
        assert_eq!(
            outcome(logic(and, refs.iter().map(|c| Ok(*c)), &live, n)),
            outcome(old_logic(and, &refs, &live, n)),
            "round {round}: and={and} n={n}"
        );
        assert_eq!(
            outcome(negation(&children[0], n)),
            outcome(old_not(&children[0], n)),
            "round {round}: not n={n}"
        );
        let flag = rng.below(2) == 0;
        assert_eq!(
            outcome(Ok(is_null(&children[0], flag, &live, n))),
            outcome(Ok(old_is_null(&children[0], flag, &live, n))),
            "round {round}: is null n={n}"
        );
        let root = |r: ColumnResult<SourceBits>| match r {
            Ok(bits) => Ok((
                (0..bits.len).map(|row| bits.get(row)).collect::<Vec<_>>(),
                bits.count_live(),
            )),
            Err(Failure::Kernel(error)) => Err((true, error.to_string())),
            Err(Failure::Fatal(error)) => Err((false, error.to_string())),
        };
        assert_eq!(
            root(matching(&children[0], n)),
            root(old_matching(&children[0], n)),
            "round {round}: root n={n}"
        );
    }
}

// 每个子节点都要取到（不短路），没有行时也一样：取值的错误照样传出。
#[test]
fn logic_reads_every_child_even_without_rows() {
    let child = booleans(Vec::new(), SourceBits::new(0));
    let children = [
        Ok(&child),
        Err(Failure::Fatal(EngineError::Storage("lost".into()))),
    ];
    assert!(matches!(
        logic(true, children.into_iter(), &SourceBits::new(0), 0),
        Err(Failure::Fatal(_))
    ));
}

fn pattern(rng: &mut Rng) -> String {
    // 偏向不含 `_` 的模式，让字面段的快路径得到足够的覆盖。
    if rng.below(2) == 0 {
        (0..rng.below(6))
            .map(|_| *rng.pick(&['a', 'b', '%', '%', 'é', '\n', '\\']))
            .collect()
    } else {
        text(rng, 5)
    }
}

#[test]
fn arrow_like_matcher_agrees_with_the_code_point_automaton() {
    let mut rng = Rng(0x5151_7a7a_0f0f_3c3c);
    for _ in 0..200_000 {
        let source = pattern(&mut rng);
        let like = LikePattern::new(&source);
        let mut matcher = like.arrow_matcher();
        for _ in 0..4 {
            let value = text(&mut rng, 6);
            let expected = like.matches(&value.chars().map(u32::from).collect::<Vec<_>>(), false);
            assert_eq!(matcher.matches(&value), expected, "{source:?} {value:?}");
        }
    }
}

#[test]
fn like_node_matches_the_row_model() {
    let mut rng = Rng(0x7777_1111_2222_3333);
    for round in 0..20_000 {
        let n = *rng.pick(&ROWS);
        let mut value = {
            let scalar = rng.below(5) == 0;
            random_column(&mut rng, n, 6, scalar)
        };
        if rng.below(20) == 0 {
            // 形状是字符串、数据却不是字符串列：只在有有效行时报错。
            value.data = Cow::Owned(ColumnData::Int64(vec![0; value.valid.len]));
        }
        let compiled = LikePattern::new(&pattern(&mut rng));
        assert_eq!(
            outcome(like(&value, &compiled, n)),
            outcome(old_like(&value, &compiled, n)),
            "round {round}: n={n}"
        );
    }
}

fn old_compare(
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

fn old_in_list(
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

fn old_numeric_arithmetic(
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
    let mut valid = SourceBits::new(rows);
    for row in 0..rows {
        valid.set(row, a.valid(row) && b.valid(row));
    }
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
