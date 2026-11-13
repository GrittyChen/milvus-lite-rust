//! GEOMETRY 的 WKT 解析与校验（0034）。
//!
//! 3.x 用两个正则加 Python `float()` 解析 WKT，只接受 POINT 与单环 POLYGON。数据目录互通要求两边对同一
//! 原文的判定一致，所以这里按 3.x 的匹配规则逐项复刻，而不是按 WKT 标准解析：空白是 Python `\s` 的集合，
//! 关键字忽略大小写按 `re.IGNORECASE`，坐标按 `float()`，多边形的自交检查沿用 3.x 的运算顺序与容差。

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

#[cfg(test)]
mod tests {
    use super::*;

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
