//! GEOMETRY 的 WKT 解析、校验与几何谓词（0034）。
//!
//! 3.x 用两个正则加 Python `float()` 解析 WKT，只接受 POINT 与单环 POLYGON。数据目录互通要求两边对同一
//! 原文的判定一致，所以这里按 3.x 的匹配规则逐项复刻，而不是按 WKT 标准解析：空白是 Python `\s` 的集合，
//! 关键字忽略大小写按 `re.IGNORECASE`，坐标按 `float()`，多边形的自交检查与各谓词沿用 3.x 的运算顺序与容差。
//! 谓词是 3.x 自己的近似算法，不是 OGC 语义。

use crate::rerank::pyval::float_literal;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Point {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Geometry {
    Point(Point),
    /// 闭合的环：首尾两点相同，至少 4 个点。
    Polygon(Vec<Point>),
}

/// 3.x 各处比较用的容差。
const EPS: f64 = 1e-9;

/// Python `re` 的 `\s` 与 `str.split()` 的空白，两者是同一组 29 个码位（CPython 3.12）。
pub(crate) fn is_space(c: char) -> bool {
    matches!(
        c,
        '\u{09}'..='\u{0d}'
            | '\u{1c}'..='\u{20}'
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

/// `re.IGNORECASE` 下 `c` 是否匹配大写 ASCII 字母 `letter`。WKT 关键字用到的字母里只有 I 在 ASCII 大小写
/// 之外还匹配 U+0130 与 U+0131；这是正则的匹配规则，与 `str.lower()` 不同。
fn matches_letter(c: char, letter: char) -> bool {
    c.eq_ignore_ascii_case(&letter) || (letter == 'I' && matches!(c, '\u{130}' | '\u{131}'))
}

fn skip_space(s: &str) -> &str {
    s.trim_start_matches(is_space)
}

/// 去掉前导空白后匹配关键字、空白与一个 `(`，返回其后的文本。
fn keyword<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    let mut chars = skip_space(s).chars();
    for letter in word.chars() {
        if !matches_letter(chars.next()?, letter) {
            return None;
        }
    }
    skip_space(chars.as_str()).strip_prefix('(')
}

fn coordinate(token: &str) -> Option<f64> {
    float_literal(token).filter(|v| v.is_finite())
}

/// 正则里的坐标 `[^\s,()]+`。
fn is_token(s: &str) -> bool {
    !s.is_empty() && !s.contains(|c: char| is_space(c) || matches!(c, ',' | '(' | ')'))
}

/// `^\s*POINT\s*\(\s*([^\s,()]+)\s+([^\s,()]+)\s*\)\s*$` 的两个捕获组。
fn point_tokens(s: &str) -> Option<(&str, &str)> {
    let rest = skip_space(keyword(s, "POINT")?);
    let end = rest.find(is_space)?;
    let (x, rest) = rest.split_at(end);
    let rest = skip_space(rest);
    let end = rest.find(|c: char| is_space(c) || c == ')')?;
    let (y, rest) = rest.split_at(end);
    let rest = skip_space(rest).strip_prefix(')')?;
    (is_token(x) && is_token(y) && skip_space(rest).is_empty()).then_some((x, y))
}

/// `^\s*POLYGON\s*\(\s*\(\s*(.*?)\s*\)\s*\)\s*$` 的捕获组（`DOTALL`）。结尾必须是只隔着空白的两个 `)`，
/// 所以惰性匹配的结果唯一；捕获组两端的空白不影响后面按 `,` 切分再 `split()` 的结果。
fn polygon_body(s: &str) -> Option<&str> {
    let rest = keyword(s, "POLYGON")?;
    let rest = skip_space(rest).strip_prefix('(')?;
    let rest = rest.trim_end_matches(is_space).strip_suffix(')')?;
    rest.trim_end_matches(is_space).strip_suffix(')')
}

/// 3.x 的 `parse_wkt`；任何不被接受的文本返回 `None`（3.x 的各个调用方都只区分成败）。
pub(crate) fn parse(value: &str) -> Option<Geometry> {
    // 3.x 先试 POINT：正则匹配而坐标无效时直接失败，不再试 POLYGON。
    if let Some((x, y)) = point_tokens(value) {
        return Some(Geometry::Point(Point {
            x: coordinate(x)?,
            y: coordinate(y)?,
        }));
    }
    let mut points = Vec::new();
    for part in polygon_body(value)?.split(',') {
        let mut coords = part.split(is_space).filter(|t| !t.is_empty());
        let (x, y) = (coords.next()?, coords.next()?);
        if coords.next().is_some() {
            return None;
        }
        points.push(Point {
            x: coordinate(x)?,
            y: coordinate(y)?,
        });
    }
    if points.len() < 4 || points[0] != points[points.len() - 1] || self_intersects(&points) {
        return None;
    }
    Some(Geometry::Polygon(points))
}

pub(crate) fn is_valid(value: &str) -> bool {
    parse(value).is_some()
}

/// 3.x 的 `_validate_simple_polygon`：不相邻的两条边不得相交（首尾两条边相邻）。
fn self_intersects(points: &[Point]) -> bool {
    let edges = points.len() - 1;
    for i in 0..edges {
        for j in i + 2..edges {
            if i == 0 && j == edges - 1 {
                continue;
            }
            if segments_intersect(points[i], points[i + 1], points[j], points[j + 1]) {
                return true;
            }
        }
    }
    false
}

/// 3.x 的 `_orientation`。写法与 3.x 相同：中间结果为 NaN 时两个比较都不成立，得到 -1。
pub(crate) fn orientation(a: Point, b: Point, c: Point) -> i8 {
    let value = (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
    if value.abs() <= EPS {
        0
    } else if value > 0.0 {
        1
    } else {
        -1
    }
}

/// Python 的 `min(a, b)`：`b < a` 时取 b。
fn py_min(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

/// Python 的 `max(a, b)`：`b > a` 时取 b。
fn py_max(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

/// 3.x 的 `_point_on_segment`。`abs(cross) > 1e-9` 在 NaN 时不成立，与 3.x 一样继续做范围判断。
pub(crate) fn point_on_segment(p: Point, a: Point, b: Point) -> bool {
    let cross = (p.y - a.y) * (b.x - a.x) - (p.x - a.x) * (b.y - a.y);
    if cross.abs() > EPS {
        return false;
    }
    py_min(a.x, b.x) - EPS <= p.x
        && p.x <= py_max(a.x, b.x) + EPS
        && py_min(a.y, b.y) - EPS <= p.y
        && p.y <= py_max(a.y, b.y) + EPS
}

/// 3.x 的 `_segments_intersect`。
pub(crate) fn segments_intersect(a1: Point, a2: Point, b1: Point, b2: Point) -> bool {
    let d1 = orientation(a1, a2, b1);
    let d2 = orientation(a1, a2, b2);
    let d3 = orientation(b1, b2, a1);
    let d4 = orientation(b1, b2, a2);
    if (d1 == 0 && point_on_segment(b1, a1, a2))
        || (d2 == 0 && point_on_segment(b2, a1, a2))
        || (d3 == 0 && point_on_segment(a1, b1, b2))
        || (d4 == 0 && point_on_segment(a2, b1, b2))
    {
        return true;
    }
    (d1 > 0) != (d2 > 0) && (d3 > 0) != (d4 > 0)
}

/// 3.x 的 `_point_in_polygon`（`include_boundary=True`）：边界上的点算在内，其余用射线法。
fn point_in_polygon(p: Point, ring: &[Point]) -> bool {
    let mut inside = false;
    let mut j = ring.len() - 1;
    for (i, &pi) in ring.iter().enumerate() {
        let pj = ring[j];
        if point_on_segment(p, pj, pi) {
            return true;
        }
        if (pi.y > p.y) != (pj.y > p.y) {
            let x = (pj.x - pi.x) * (p.y - pi.y) / (pj.y - pi.y) + pi.x;
            if p.x < x {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

/// CPython 3.12 `math.hypot` 的两参数情形（`vector_norm`）。它不是 libm 的 `hypot`：两者在约 16% 的输入上
/// 相差 1 ULP，`ST_DWITHIN` 落在阈值上的行要与 3.x 一致就必须用同一个算法。
pub(crate) fn hypot(x: f64, y: f64) -> f64 {
    let (x, y) = (x.abs(), y.abs());
    let mut max = 0.0;
    if x > max {
        max = x;
    }
    if y > max {
        max = y;
    }
    vector_norm(x, y, max, x.is_nan() || y.is_nan())
}

/// C 的 `frexp` 给出的指数：`x = m × 2^e`，`0.5 <= m < 1`。只用于正的有限数。
fn frexp_exponent(x: f64) -> i32 {
    let bits = x.to_bits();
    let biased = (bits >> 52) as i32;
    if biased == 0 {
        // 次正规数：x = 尾数 × 2^-1074。
        (64 - bits.leading_zeros() as i32) - 1074
    } else {
        biased - 1022
    }
}

/// `ldexp(1.0, e)`，`e` 在 -1074..=1023 内，结果精确。调用处的 `e` 是 -1024..=1023，其中 -1024 与 -1023
/// 落在次正规数的分支。
fn power_of_two(e: i32) -> f64 {
    if e >= -1022 {
        f64::from_bits(((e + 1023) as u64) << 52)
    } else {
        f64::from_bits(1_u64 << (e + 1074))
    }
}

fn vector_norm(x: f64, y: f64, max: f64, found_nan: bool) -> f64 {
    const DBL_MIN: f64 = f64::MIN_POSITIVE;
    if max.is_infinite() {
        return max;
    }
    if found_nan {
        return f64::NAN;
    }
    if max == 0.0 {
        return max;
    }
    let max_e = frexp_exponent(max);
    if max_e < -1023 {
        // `ldexp(1.0, -max_e)` 会溢出：先把次正规数放大成正规数。
        return DBL_MIN * vector_norm(x / DBL_MIN, y / DBL_MIN, max / DBL_MIN, found_nan);
    }
    let scale = power_of_two(-max_e);
    // 无误差的乘法与加法：高位进 `csum`，乘法与加法的低位分别累加进 `frac1`、`frac2`；返回累加到此刻的
    // 平方和（`csum` 从 1 起算，所以减 1）。
    let (mut csum, mut frac1, mut frac2) = (1.0_f64, 0.0_f64, 0.0_f64);
    let mut add_product = |a: f64, b: f64| {
        let hi = a * b;
        frac1 += a.mul_add(b, -hi);
        let sum = csum + hi;
        frac2 += (csum - sum) + hi;
        csum = sum;
        csum - 1.0 + (frac1 + frac2)
    };
    // 第一项之后的部分和用不到，平方和在加完第二项后取。
    let _ = add_product(x * scale, x * scale);
    let mut h = add_product(y * scale, y * scale).sqrt();
    h += add_product(-h, h) / (2.0 * h);
    h / scale
}

/// 3.x 的 `_point_segment_distance`。
fn point_segment_distance(p: Point, a: Point, b: Point) -> f64 {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    if dx.abs() <= 1e-12 && dy.abs() <= 1e-12 {
        return hypot(p.x - a.x, p.y - a.y);
    }
    let t = ((p.x - a.x) * dx + (p.y - a.y) * dy) / (dx * dx + dy * dy);
    let t = py_max(0.0, py_min(1.0, t));
    hypot(p.x - (a.x + t * dx), p.y - (a.y + t * dy))
}

/// 3.x 的 `_segment_segment_distance`。
fn segment_segment_distance(a1: Point, a2: Point, b1: Point, b2: Point) -> f64 {
    if segments_intersect(a1, a2, b1, b2) {
        return 0.0;
    }
    [
        point_segment_distance(a2, b1, b2),
        point_segment_distance(b1, a1, a2),
        point_segment_distance(b2, a1, a2),
    ]
    .into_iter()
    .fold(point_segment_distance(a1, b1, b2), py_min)
}

fn edges(ring: &[Point]) -> impl Iterator<Item = (Point, Point)> + Clone + '_ {
    ring.windows(2).map(|w| (w[0], w[1]))
}

/// 3.x 的 `_polygon_contains_polygon`：顶点、各边中点都在 `a` 内，且两者的边不真正交叉。这是 3.x 自己的
/// 近似判断，不是 OGC 的包含。
fn polygon_contains_polygon(a: &[Point], b: &[Point]) -> bool {
    if !b[..b.len() - 1].iter().all(|&p| point_in_polygon(p, a)) {
        return false;
    }
    for (b1, b2) in edges(b) {
        let midpoint = Point {
            x: (b1.x + b2.x) / 2.0,
            y: (b1.y + b2.y) / 2.0,
        };
        if !point_in_polygon(midpoint, a) {
            return false;
        }
        for (a1, a2) in edges(a) {
            let collinear = orientation(a1, a2, b1) == 0 && orientation(a1, a2, b2) == 0;
            if segments_intersect(a1, a2, b1, b2)
                && !collinear
                && !point_on_segment(b1, a1, a2)
                && !point_on_segment(b2, a1, a2)
            {
                return false;
            }
        }
    }
    true
}

/// 3.x 的 `_polygons_intersect`。
fn polygons_intersect(a: &[Point], b: &[Point]) -> bool {
    edges(a).any(|(a1, a2)| edges(b).any(|(b1, b2)| segments_intersect(a1, a2, b1, b2)))
        || point_in_polygon(a[0], b)
        || point_in_polygon(b[0], a)
}

/// 3.x 的 `geometry_contains(a, b)`；`geometry_within(a, b)` 是 `contains(b, a)`。
pub(crate) fn contains(a: &Geometry, b: &Geometry) -> bool {
    match (a, b) {
        (Geometry::Polygon(a), Geometry::Point(b)) => point_in_polygon(*b, a),
        (Geometry::Point(a), Geometry::Point(b)) => a == b,
        (Geometry::Polygon(a), Geometry::Polygon(b)) => polygon_contains_polygon(a, b),
        (Geometry::Point(_), Geometry::Polygon(_)) => false,
    }
}

/// 3.x 的 `geometry_intersects`。
pub(crate) fn intersects(a: &Geometry, b: &Geometry) -> bool {
    match (a, b) {
        (Geometry::Point(a), Geometry::Point(b)) => a == b,
        (Geometry::Polygon(a), Geometry::Point(b)) => point_in_polygon(*b, a),
        (Geometry::Point(a), Geometry::Polygon(b)) => point_in_polygon(*a, b),
        (Geometry::Polygon(a), Geometry::Polygon(b)) => polygons_intersect(a, b),
    }
}

/// 3.x 的 `_geometry_distance`。
fn distance(a: &Geometry, b: &Geometry) -> f64 {
    if intersects(a, b) {
        return 0.0;
    }
    match (a, b) {
        (Geometry::Point(a), Geometry::Point(b)) => hypot(a.x - b.x, a.y - b.y),
        (Geometry::Point(p), Geometry::Polygon(ring))
        | (Geometry::Polygon(ring), Geometry::Point(p)) => edges(ring)
            .map(|(a, b)| point_segment_distance(*p, a, b))
            .reduce(py_min)
            .unwrap_or(f64::INFINITY),
        (Geometry::Polygon(a), Geometry::Polygon(b)) => edges(a)
            .flat_map(|(a1, a2)| {
                edges(b).map(move |(b1, b2)| segment_segment_distance(a1, a2, b1, b2))
            })
            .reduce(py_min)
            .unwrap_or(f64::INFINITY),
    }
}

/// 3.x 的 `geometry_dwithin`，`limit` 不为负（过滤的解析器保证）。
pub(crate) fn dwithin(a: &Geometry, b: &Geometry, limit: f64) -> bool {
    distance(a, b) <= limit + EPS
}

#[cfg(test)]
mod tests {
    use super::*;

    // 以下四张表由真实 3.x（1320d26，CPython 3.12）的 `geometry.py` 与 `math.hypot` 生成：
    // `geometry_contains(a, b)`、`geometry_intersects(a, b)`、`_geometry_distance(a, b)` 的位模式，以及
    // `math.hypot(x, y)` 的位模式（NaN 只比较是否为 NaN）。
    const SHAPES: [&str; 16] = [
        "POINT(1 1)",
        "POINT(5 5)",
        "POINT(10 5)",
        "POINT(13 14)",
        "POINT(2 3)",
        "POINT(0.3 0.1)",
        "POINT(1e308 1e308)",
        "POINT(-1e308 -1e308)",
        "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))",
        "POLYGON((2 2, 4 2, 4 4, 2 4, 2 2))",
        "POLYGON((8 8, 12 8, 12 12, 8 12, 8 8))",
        "POLYGON((20 20, 30 20, 30 30, 20 20))",
        "POLYGON((0 0, 4 0, 4 4, 2 1, 0 4, 0 0))",
        "POLYGON((0 0,0 0,0 0,0 0))",
        "POLYGON((10 0, 20 0, 20 10, 10 10, 10 0))",
        "POLYGON((0.1 0.2, 0.7 0.3, 0.4 0.9, 0.1 0.2))",
    ];
    const CONTAINS: [&str; 16] = [
        "1000000000000000",
        "0100000000000000",
        "0010000000000000",
        "0001000000000000",
        "0000100000000000",
        "0000010000000000",
        "0000001000000000",
        "0000000100000000",
        "1110110011001101",
        "0000100001000000",
        "0000000000100000",
        "0000000000010000",
        "1000010000001101",
        "0000000000000100",
        "0010000000000010",
        "0000000000000001",
    ];
    const INTERSECTS: [&str; 16] = [
        "1000000010001000",
        "0100000010000000",
        "0010000010000010",
        "0001000000000000",
        "0000100011000000",
        "0000010010001000",
        "0000001000000000",
        "0000000100000000",
        "1110110011101111",
        "0000100011001000",
        "0000000010100010",
        "0000000000010000",
        "1000010011001101",
        "0000000010001100",
        "0010000010100010",
        "0000000010001001",
    ];
    const DISTANCES: [[u64; 16]; 16] = [
        [
            0x0000000000000000,
            0x4016a09e667f3bcd,
            0x4023b29d7d635662,
            0x4031b11a32e8d06c,
            0x4001e3779b97f4a8,
            0x3ff23e2896280f23,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x0000000000000000,
            0x3ff6a09e667f3bcd,
            0x4023cc8a99af5453,
            0x403adebc19b71703,
            0x0000000000000000,
            0x3ff6a09e667f3bcd,
            0x4022000000000000,
            0x3fe29aa55a232767,
        ],
        [
            0x4016a09e667f3bcd,
            0x0000000000000000,
            0x4014000000000000,
            0x4028154be2773526,
            0x400cd82b446159f3,
            0x401b28a69cf70ba8,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x0000000000000000,
            0x3ff6a09e667f3bcd,
            0x4010f876ccdf6cd9,
            0x4035369480174810,
            0x3ff6a09e667f3bcd,
            0x401c48c6001f0ac0,
            0x4014000000000000,
            0x4018a5de234b0a70,
        ],
        [
            0x4023b29d7d635662,
            0x4014000000000000,
            0x0000000000000000,
            0x4022f9422c23c47e,
            0x40207e0f66afed07,
            0x4025bc198b6835be,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x0000000000000000,
            0x401854bfb363dc39,
            0x4008000000000000,
            0x4032071b0abcd838,
            0x401854bfb363dc39,
            0x40265c55827df1d2,
            0x0000000000000000,
            0x4024d7144d9625fb,
        ],
        [
            0x4031b11a32e8d06c,
            0x4028154be2773526,
            0x4022f9422c23c47e,
            0x0000000000000000,
            0x402f1cd9cceef239,
            0x4032d402f969c6d6,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x4014000000000000,
            0x402ae841693db8b4,
            0x4001e3779b97f4a8,
            0x4022706821902e9a,
            0x402ae841693db8b4,
            0x40331adf859f9e5e,
            0x4010000000000000,
            0x40322d13c9a716ed,
        ],
        [
            0x4001e3779b97f4a8,
            0x400cd82b446159f3,
            0x40207e0f66afed07,
            0x402f1cd9cceef239,
            0x0000000000000000,
            0x400ae472e42d478a,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x0000000000000000,
            0x0000000000000000,
            0x401f3db2174e7468,
            0x4038c2432101f926,
            0x3ff1c01aa03be896,
            0x400cd82b446159f3,
            0x4020000000000000,
            0x40051ee009c8a7ca,
        ],
        [
            0x3ff23e2896280f23,
            0x401b28a69cf70ba8,
            0x4025bc198b6835be,
            0x4032d402f969c6d6,
            0x400ae472e42d478a,
            0x0000000000000000,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x0000000000000000,
            0x400465655f122ff6,
            0x40261044755c0fea,
            0x403c0075065be82b,
            0x0000000000000000,
            0x3fd43d136248490f,
            0x4023666666666666,
            0x3fc0d59eed96bc59,
        ],
        [
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x0000000000000000,
            0x7ff0000000000000,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
        ],
        [
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7ff0000000000000,
            0x0000000000000000,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
        ],
        [
            0x0000000000000000,
            0x0000000000000000,
            0x0000000000000000,
            0x4014000000000000,
            0x0000000000000000,
            0x0000000000000000,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x0000000000000000,
            0x0000000000000000,
            0x0000000000000000,
            0x402c48c6001f0ac0,
            0x0000000000000000,
            0x0000000000000000,
            0x0000000000000000,
            0x0000000000000000,
        ],
        [
            0x3ff6a09e667f3bcd,
            0x3ff6a09e667f3bcd,
            0x401854bfb363dc39,
            0x402ae841693db8b4,
            0x0000000000000000,
            0x400465655f122ff6,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x0000000000000000,
            0x0000000000000000,
            0x4016a09e667f3bcd,
            0x4036a09e667f3bcd,
            0x0000000000000000,
            0x4006a09e667f3bcd,
            0x4018000000000000,
            0x3fff10fe50b256b1,
        ],
        [
            0x4023cc8a99af5453,
            0x4010f876ccdf6cd9,
            0x4008000000000000,
            0x4001e3779b97f4a8,
            0x401f3db2174e7468,
            0x40261044755c0fea,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x0000000000000000,
            0x4016a09e667f3bcd,
            0x0000000000000000,
            0x4026a09e667f3bcd,
            0x4016a09e667f3bcd,
            0x4026a09e667f3bcd,
            0x0000000000000000,
            0x4024cd0bd05d9866,
        ],
        [
            0x403adebc19b71703,
            0x4035369480174810,
            0x4032071b0abcd838,
            0x4022706821902e9a,
            0x4038c2432101f926,
            0x403c0075065be82b,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x402c48c6001f0ac0,
            0x4036a09e667f3bcd,
            0x4026a09e667f3bcd,
            0x0000000000000000,
            0x4036a09e667f3bcd,
            0x403c48c6001f0ac0,
            0x4024000000000000,
            0x403b5e087074e3d4,
        ],
        [
            0x0000000000000000,
            0x3ff6a09e667f3bcd,
            0x401854bfb363dc39,
            0x402ae841693db8b4,
            0x3ff1c01aa03be896,
            0x0000000000000000,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x0000000000000000,
            0x0000000000000000,
            0x4016a09e667f3bcd,
            0x4036a09e667f3bcd,
            0x0000000000000000,
            0x0000000000000000,
            0x4018000000000000,
            0x0000000000000000,
        ],
        [
            0x3ff6a09e667f3bcd,
            0x401c48c6001f0ac0,
            0x40265c55827df1d2,
            0x40331adf859f9e5e,
            0x400cd82b446159f3,
            0x3fd43d136248490f,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x0000000000000000,
            0x4006a09e667f3bcd,
            0x4026a09e667f3bcd,
            0x403c48c6001f0ac0,
            0x0000000000000000,
            0x0000000000000000,
            0x4024000000000000,
            0x3fcc9f25c5bfedda,
        ],
        [
            0x4022000000000000,
            0x4014000000000000,
            0x0000000000000000,
            0x4010000000000000,
            0x4020000000000000,
            0x4023666666666666,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x0000000000000000,
            0x4018000000000000,
            0x0000000000000000,
            0x4024000000000000,
            0x4018000000000000,
            0x4024000000000000,
            0x0000000000000000,
            0x402299999999999a,
        ],
        [
            0x3fe29aa55a232767,
            0x4018a5de234b0a70,
            0x4024d7144d9625fb,
            0x40322d13c9a716ed,
            0x40051ee009c8a7ca,
            0x3fc0d59eed96bc59,
            0x7fe92c80954c51f5,
            0x7fe92c80954c51f5,
            0x0000000000000000,
            0x3fff10fe50b256b1,
            0x4024cd0bd05d9866,
            0x403b5e087074e3d4,
            0x0000000000000000,
            0x3fcc9f25c5bfedda,
            0x402299999999999a,
            0x0000000000000000,
        ],
    ];
    const HYPOT: [(u64, u64, u64); 40] = [
        (0x4008000000000000, 0x4010000000000000, 0x4014000000000000),
        (0xc008000000000000, 0x4010000000000000, 0x4014000000000000),
        (0x0000000000000000, 0x0000000000000000, 0x0000000000000000),
        (0x8000000000000000, 0x0000000000000000, 0x0000000000000000),
        (0x0000000000000000, 0x0000000000000001, 0x0000000000000001),
        (0x0000000000000001, 0x0000000000000001, 0x0000000000000001),
        (0x00000000000007e8, 0x00000000000017b8, 0x0000000000001900),
        (0x0011fa182c40c60d, 0x0011fa182c40c60d, 0x00196c58276b9b78),
        (0x0010000000000000, 0x000012688b70e62b, 0x0010000a96eeeaa3),
        (0x16687e92154ef7ac, 0x16687e92154ef7ac, 0x167151f68876f410),
        (0x5fe7dddf6b095ff1, 0x5fe7dddf6b095ff1, 0x5ff0e0551a9edea1),
        (0x7fefffffffffffff, 0x7fefffffffffffff, 0x7ff0000000000000),
        (0x7fe1ccf385ebc8a0, 0x7fe1ccf385ebc8a0, 0x7fe92c80954c51f5),
        (0x7fe55c576d815726, 0x7c9008896bcf54fa, 0x7fe55c576d815726),
        (0x3ff0000000000000, 0x3c670ef54646d497, 0x3ff0000000000000),
        (0x3fb999999999999a, 0x3fc999999999999a, 0x3fcc9f25c5bfedda),
        (0x3fe6666666666666, 0x3fb999999999999a, 0x3fe6a09e667f3bcc),
        (0x4008000000036f9c, 0x4010000000000000, 0x40140000000107e2),
        (0x3e112e0be826d695, 0x3e112e0be826d695, 0x3e184bc6eb0aa98a),
        (0x405edd2f1a9fbe77, 0x408472916872b021, 0x4084ceed4c854db8),
        (0x7ff0000000000000, 0x7ff8000000000000, 0x7ff0000000000000),
        (0x7ff8000000000000, 0xfff0000000000000, 0x7ff0000000000000),
        (0x7ff8000000000000, 0x3ff0000000000000, 0x7ff8000000000000),
        (0x3ff0000000000000, 0x7ff0000000000000, 0x7ff0000000000000),
        (0x408c0d7271012352, 0xc062fb3ae5641048, 0x408c7378621fcdee),
        (0xc0783a1e980b76e0, 0xc0895779418fe710, 0x408c168a9b173399),
        (0x408b1c0014ad7fd2, 0x406632b4cfcc4b40, 0x408babeb966c610d),
        (0x407116114f7806e4, 0xc07a015e3b499606, 0x407f1dbd7fdd69cc),
        (0xc0747ca79119b9f2, 0xc076652a4c0a71bc, 0x407e5a2c3ac4a609),
        (0x4080da0c9cb41cb6, 0x407e37cc09ff0e10, 0x4086a21b0a2e4816),
        (0xc07bac354fd7a8c6, 0x408b20026754c302, 0x408e733d7666b5a4),
        (0x406cae33f5a71010, 0xc07e0226957f44b4, 0x4080a11fcb796bc0),
        (0xc0707ff96b18f796, 0x40839f895f21af7e, 0x40854970f4a54ba7),
        (0x408e8a010e241962, 0xc086035f45222b90, 0x4092d2ad6a5dc2de),
        (0xc087ba5d64e55278, 0xc08083594a12aac4, 0x408ce8910b898a04),
        (0x407bb295d9ae2e94, 0x4070dceb5dcc6a7c, 0x408036ab1e6701a1),
        (0x4021a9e083ed2a40, 0xc08cf47e6b3ed3bb, 0x408cf4d49ec09682),
        (0xc08b54124d211b90, 0xc08908f109ed0457, 0x409287ee7d76b009),
        (0xc0893b7ea825bae9, 0xc0530c00ade57448, 0x4089582fb9baaa3d),
        (0x408cea7c67353b02, 0x408ebf4d5a9beb56, 0x40951aa48d2ad8d8),
    ];

    fn shapes() -> Vec<Geometry> {
        SHAPES.iter().map(|text| parse(text).unwrap()).collect()
    }

    #[test]
    fn hypot_is_the_cpython_algorithm() {
        for (x, y, want) in HYPOT {
            let (x, y, want) = (f64::from_bits(x), f64::from_bits(y), f64::from_bits(want));
            let got = hypot(x, y);
            assert!(
                got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()),
                "hypot({x:?}, {y:?}) = {got:?}, want {want:?}"
            );
        }
        // 表里要有与朴素算法不同的输入，否则它证明不了算法。
        assert!(HYPOT.iter().any(|&(x, y, want)| {
            let (x, y) = (f64::from_bits(x), f64::from_bits(y));
            (x * x + y * y).sqrt().to_bits() != want && f64::from_bits(want).is_finite()
        }));
    }

    #[test]
    fn predicates_follow_the_reference_algorithms() {
        let shapes = shapes();
        for (i, a) in shapes.iter().enumerate() {
            for (j, b) in shapes.iter().enumerate() {
                let pair = format!("{} / {}", SHAPES[i], SHAPES[j]);
                let flag = |table: &[&str]| table[i].as_bytes()[j] == b'1';
                assert_eq!(contains(a, b), flag(&CONTAINS), "contains {pair}");
                assert_eq!(intersects(a, b), flag(&INTERSECTS), "intersects {pair}");
                let want = f64::from_bits(DISTANCES[i][j]);
                let got = distance(a, b);
                assert!(
                    got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()),
                    "distance {pair}: {got:?}, want {want:?}"
                );
            }
        }
    }

    #[test]
    fn dwithin_compares_against_the_limit_plus_tolerance() {
        let point = |text: &str| parse(text).unwrap();
        let (a, b) = (point("POINT(13 14)"), point("POINT(10 10)"));
        assert!(dwithin(&a, &b, 5.0));
        assert!(dwithin(&a, &b, 4.999_999_999_5));
        assert!(!dwithin(&a, &b, 4.999_999_998));
        assert!(dwithin(&a, &b, f64::INFINITY));
        assert!(dwithin(&a, &a, 0.0));
        assert!(dwithin(&a, &a, -0.0));
        // 距离溢出成无穷大：只有无穷大的上限能容纳。
        let (far, opposite) = (point("POINT(1e308 1e308)"), point("POINT(-1e308 -1e308)"));
        assert!(!dwithin(&far, &opposite, 1e308));
        assert!(dwithin(&far, &opposite, f64::INFINITY));
    }

    fn point(x: f64, y: f64) -> Option<Geometry> {
        Some(Geometry::Point(Point { x, y }))
    }

    #[test]
    fn whitespace_is_the_python_set() {
        let spaces: Vec<u32> = (0..=0x10ffff_u32)
            .filter(|c| char::from_u32(*c).is_some_and(is_space))
            .collect();
        let expected: Vec<u32> = [
            0x09..=0x0d,
            0x1c..=0x20,
            0x85..=0x85,
            0xa0..=0xa0,
            0x1680..=0x1680,
            0x2000..=0x200a,
            0x2028..=0x2029,
            0x202f..=0x202f,
            0x205f..=0x205f,
            0x3000..=0x3000,
        ]
        .into_iter()
        .flatten()
        .collect();
        assert_eq!(spaces, expected);
        assert_eq!(spaces.len(), 29);
    }

    #[test]
    fn keyword_letters_follow_re_ignorecase() {
        // 每个字母位置换成任意码位，只有 ASCII 大小写与 I 的两个额外码位被接受。
        for (word, tail) in [("POINT", "(1 1)"), ("POLYGON", "((0 0,1 0,1 1,0 0))")] {
            for (pos, letter) in word.char_indices() {
                let accepted: Vec<char> = (0..=0x10ffff_u32)
                    .filter_map(char::from_u32)
                    .filter(|c| {
                        let text = format!("{}{c}{}{tail}", &word[..pos], &word[pos + 1..]);
                        is_valid(&text)
                    })
                    .collect();
                let mut expected = vec![letter, letter.to_ascii_lowercase()];
                if letter == 'I' {
                    expected.extend(['\u{130}', '\u{131}']);
                }
                assert_eq!(accepted, expected, "{word} position {pos}");
            }
        }
    }

    #[test]
    fn point_follows_the_reference_regex_and_float() {
        assert_eq!(parse("POINT(1 1)"), point(1.0, 1.0));
        assert_eq!(parse("  point ( 1e1   -2.5 )  "), point(10.0, -2.5));
        assert_eq!(parse("POINT(1 1)\n\n"), point(1.0, 1.0));
        assert_eq!(parse("PO\u{130}NT(1 1)"), point(1.0, 1.0));
        assert_eq!(parse("PO\u{131}NT(1 1)"), point(1.0, 1.0));
        assert_eq!(parse("POINT(1\u{3000}2)"), point(1.0, 2.0));
        assert_eq!(parse("POINT(1\u{1c}2)"), point(1.0, 2.0));
        assert_eq!(parse("POINT(1_0 1)"), point(10.0, 1.0));
        assert_eq!(parse("POINT(1e1_0 1)"), point(1e10, 1.0));
        assert_eq!(parse("POINT(\u{661} \u{662})"), point(1.0, 2.0));
        assert_eq!(parse("POINT(\u{661}_\u{662} 1)"), point(12.0, 1.0));
        assert_eq!(parse("POINT(+1 -.5)"), point(1.0, -0.5));
        assert_eq!(parse("POINT(1. 1.)"), point(1.0, 1.0));
        for bad in [
            "",
            "not-wkt",
            "POINT(1)",
            "POINT(1 2 3)",
            "POINT(inf 1)",
            "POINT(nan 1)",
            "POINT(infinity 1)",
            "POINT(1e400 1)",
            "POINT(0x10 1)",
            "POINT(1__0 1)",
            "POINT(1_ 1)",
            "POINT(.e1 1)",
            "POINT(1e 1)",
            "POINT(1\u{200b}2)",
            "POINT(1 2)\0",
            "POINT Z(1 1 1)",
            "POINT EMPTY",
            "SRID=4326;POINT(1 1)",
            "MULTIPOINT(1 1)",
            "LINESTRING(0 0,1 1)",
            "xPOINT(1 1)",
            "POINT(1,2)",
            "POINT((1 2))",
            "POINT(1 2",
            "POINT 1 2)",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn polygon_follows_the_reference_rules() {
        let square = "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))";
        let Some(Geometry::Polygon(points)) = parse(square) else {
            panic!("square");
        };
        assert_eq!(points.len(), 5);
        assert_eq!(points[2], Point { x: 10.0, y: 10.0 });
        for good in [
            "polygon ( ( 0 0 ,1 0, 1 1 , 0 0 ) ) \n",
            "POLYGON((0 0,0 0,0 0,0 0))",
            "POLYGON((0\n0,1\u{a0}0,1 1,0 0))",
            // 首尾按浮点相等比较：-0.0 等于 0.0。
            "POLYGON((0 0,1 0,1 1,-0 -0.0))",
            // 凹多边形不自交。
            "POLYGON((0 0,4 0,4 4,2 1,0 4,0 0))",
        ] {
            assert!(is_valid(good), "{good:?}");
        }
        for bad in [
            "POLYGON(())",
            "POLYGON((0 0,1 0,0 0))",
            "POLYGON((0 0,1 0,1 1,0 1))",
            "POLYGON((0 0,2 2,2 0,0 2,0 0))",
            "POLYGON((0 0,1 0,1 1,0 0,0 0))",
            "POLYGON((0 0,4 0,4 4,0 4,0 0),(1 1,2 1,2 2,1 2,1 1))",
            "POLYGON((0 0,1 0,1 1,0 0)",
            "POLYGON(0 0,1 0,1 1,0 0)",
            "POLYGON((0 0 0,1 0 0,1 1 0,0 0 0))",
            "POLYGON((0 0,1 0,,1 1,0 0))",
            "POLYGON((0 0,1 0,1 inf,0 0))",
            "POLYGON((0 0,1 0,1 1,0 0)) x",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }
}
