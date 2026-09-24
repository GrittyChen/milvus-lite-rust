//! 过滤表达式。M1 只识别 `<主键> in [字面量, ...]` 这一种形式（0004「范围」），规则与 3.x 在
//! `_extract_pks_from_expr` 中用完整 filter parser 得到的结果一致：凡本模块接受的输入，3.x 都走按主键
//! 读取且得到相同的主键列表；本模块拒绝而 3.x 接受的输入只有非 ASCII 数字与未收录字符的主键名，
//! 见 0004「决策」。

/// 列表中的一个字面量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkLiteral {
    Int(i64),
    /// 超出 int64 的整数。3.x 不报错，只是查不到。
    IntOutOfRange,
    Str(String),
}

/// 3.x 词法中的关键字与布尔字面量，按 `str.lower()` 比较。
const RESERVED: [&str; 12] = [
    "and", "or", "not", "in", "like", "is", "null", "exists", "iso", "interval", "true", "false",
];
/// 3.x 的整数字面量经 Python `int()` 解析，超过 4300 位（含前导零）时报错，于是不走按主键读取。
const MAX_INT_DIGITS: usize = 4300;

/// 空白只认这四个字符（3.x `tokens.py`），不能用 `char::is_whitespace`。
fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// 3.x 用 `str.isalpha` / `str.isalnum` 切分标识符，其结果随 Python 的 Unicode 版本变化。这里只收录
/// 在 Python 3.10–3.13（Unicode 13.0–15.1）中逐码位核对过、全部判为字母的范围。
fn is_ident_letter(c: char) -> bool {
    matches!(c,
        'A'..='Z' | 'a'..='z' | '_'
        | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{17F}'
        | '\u{3041}'..='\u{3096}' | '\u{309D}'..='\u{309F}'
        | '\u{30A1}'..='\u{30FA}' | '\u{30FC}'..='\u{30FF}'
        | '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFC}'
        | '\u{AC00}'..='\u{D7A3}')
}

/// 主键名能否在 3.x 中被切分为单个标识符、且不是关键字。
fn pk_name_is_identifier(pk: &str) -> bool {
    let mut chars = pk.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_ident_letter(first)
        && chars.all(|c| is_ident_letter(c) || c.is_ascii_digit())
        && !RESERVED.contains(&pk.to_lowercase().as_str())
}

/// 识别 `<pk> in [...]`：不是这个形式时返回 None；否则按原顺序返回字面量，保留重复。
///
/// 形式：空白与括号可以包住整个表达式，也可以包住主键本身；`in` 不区分大小写；元素是整数（可带
/// 负号，负号后可有空白）或单 / 双引号字符串（只有 `\" \' \\ \n \r \t` 六种转义）；允许一个结尾逗号。
pub fn parse_pk_in(expr: &str, pk: &str) -> Option<Vec<PkLiteral>> {
    if !pk_name_is_identifier(pk) {
        return None;
    }
    let mut p = Cursor { s: expr, i: 0 };
    p.ws();
    let mut opens = 0usize;
    while p.eat('(') {
        opens += 1;
        p.ws();
    }
    if !p.rest().starts_with(pk) {
        return None;
    }
    p.i += pk.len();
    // 标识符取最长匹配：主键之后必须是空白或右括号。
    match p.peek() {
        Some(c) if is_ws(c) || c == ')' => {}
        _ => return None,
    }
    p.ws();
    let mut lhs_closes = 0usize;
    while lhs_closes < opens && p.eat(')') {
        lhs_closes += 1;
        p.ws();
    }
    let keyword = p.rest().get(..2)?;
    if !keyword.eq_ignore_ascii_case("in") {
        return None;
    }
    p.i += 2;
    match p.peek() {
        Some(c) if is_ws(c) || c == '[' => {}
        _ => return None,
    }
    p.ws();
    if !p.eat('[') {
        return None;
    }
    let literals = p.list()?;
    p.ws();
    let mut closes = 0usize;
    while p.eat(')') {
        closes += 1;
        p.ws();
    }
    (p.i == expr.len() && closes == opens - lhs_closes).then_some(literals)
}

struct Cursor<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Cursor<'a> {
    fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.i += c.len_utf8();
            true
        } else {
            false
        }
    }

    fn ws(&mut self) {
        while self.peek().is_some_and(is_ws) {
            self.i += 1;
        }
    }

    /// `[` 之后的部分，直到并包括 `]`。
    fn list(&mut self) -> Option<Vec<PkLiteral>> {
        let mut out = Vec::new();
        self.ws();
        if self.eat(']') {
            return Some(out);
        }
        loop {
            out.push(self.literal()?);
            self.ws();
            if self.eat(',') {
                self.ws();
                if self.eat(']') {
                    return Some(out);
                }
                continue;
            }
            return self.eat(']').then_some(out);
        }
    }

    fn literal(&mut self) -> Option<PkLiteral> {
        match self.peek()? {
            '-' => {
                self.i += 1;
                self.ws();
                self.int(true)
            }
            '0'..='9' => self.int(false),
            q @ ('\'' | '"') => {
                self.i += 1;
                self.string(q)
            }
            _ => None,
        }
    }

    fn int(&mut self, negative: bool) -> Option<PkLiteral> {
        let digits_len = self.rest().bytes().take_while(u8::is_ascii_digit).count();
        if digits_len == 0 || digits_len > MAX_INT_DIGITS {
            return None;
        }
        let digits = &self.rest()[..digits_len];
        self.i += digits_len;
        match self.peek() {
            None => {}
            Some(c) if is_ws(c) || c == ',' || c == ']' => {}
            Some(_) => return None,
        }
        let text = if negative {
            format!("-{digits}")
        } else {
            digits.to_owned()
        };
        Some(match text.parse::<i64>() {
            Ok(v) => PkLiteral::Int(v),
            Err(_) => PkLiteral::IntOutOfRange,
        })
    }

    fn string(&mut self, quote: char) -> Option<PkLiteral> {
        let mut buf = String::new();
        loop {
            let c = self.peek()?;
            self.i += c.len_utf8();
            match c {
                c if c == quote => return Some(PkLiteral::Str(buf)),
                '\\' => {
                    let escaped = match self.peek()? {
                        '"' => '"',
                        '\'' => '\'',
                        '\\' => '\\',
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        _ => return None,
                    };
                    self.i += 1;
                    buf.push(escaped);
                }
                '\r' | '\n' => return None,
                c => buf.push(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PkLiteral::{Int, IntOutOfRange as Oor, Str};

    fn p(expr: &str) -> Option<Vec<PkLiteral>> {
        parse_pk_in(expr, "id")
    }

    fn s(v: &str) -> PkLiteral {
        Str(v.to_owned())
    }

    // 期望值取自 3.x（v3.2.1）的 `_extract_pks_from_expr` 实测。
    #[test]
    fn accepted_forms() {
        let cases: &[(&str, Vec<PkLiteral>)] = &[
            ("id in [1]", vec![Int(1)]),
            (
                "id in [3, 1, 3, 2, 1]",
                vec![Int(3), Int(1), Int(3), Int(2), Int(1)],
            ),
            ("id IN [1]", vec![Int(1)]),
            ("id In [1]", vec![Int(1)]),
            ("id iN [1]", vec![Int(1)]),
            ("id in[1]", vec![Int(1)]),
            ("id\tin\n[1]", vec![Int(1)]),
            ("(id)in[1]", vec![Int(1)]),
            ("id in []", vec![]),
            ("id in [ ]", vec![]),
            ("id in [1,]", vec![Int(1)]),
            ("id in [1 , 2 ,\n 3\t,\r\n]", vec![Int(1), Int(2), Int(3)]),
            ("  id in [1]  ", vec![Int(1)]),
            ("\n\tid in [1]\r\n", vec![Int(1)]),
            ("id in [-5, - 5, -\n5]", vec![Int(-5), Int(-5), Int(-5)]),
            ("id in [-0, 00, 007]", vec![Int(0), Int(0), Int(7)]),
            (
                "id in [9223372036854775807, -9223372036854775808]",
                vec![Int(i64::MAX), Int(i64::MIN)],
            ),
            (
                "id in [9223372036854775808, 1, -9223372036854775809]",
                vec![Oor, Int(1), Oor],
            ),
            ("id in ['a', \"a\"]", vec![s("a"), s("a")]),
            ("id in ['a\"b', \"a\\\"b\"]", vec![s("a\"b"), s("a\"b")]),
            ("id in [\"a'b\", 'a\\'b']", vec![s("a'b"), s("a'b")]),
            ("id in ['a\\\\b']", vec![s("a\\b")]),
            (
                "id in ['a\\nb', 'a\\rb', 'a\\tb']",
                vec![s("a\nb"), s("a\rb"), s("a\tb")],
            ),
            (
                "id in ['a\tb', 'a\u{0}b', '\u{2028}', '中文😀', '']",
                vec![s("a\tb"), s("a\u{0}b"), s("\u{2028}"), s("中文😀"), s("")],
            ),
            ("id in ['a','b',]", vec![s("a"), s("b")]),
            ("id in [1, 'a']", vec![Int(1), s("a")]),
            ("(id in [1])", vec![Int(1)]),
            ("((id in [1]))", vec![Int(1)]),
            ("( id in [1] )", vec![Int(1)]),
            ("(id) in [1]", vec![Int(1)]),
            ("((id)) in [1]", vec![Int(1)]),
            ("((id) in [1])", vec![Int(1)]),
            ("( ( id ) in [ 1 ] )", vec![Int(1)]),
        ];
        for (expr, want) in cases {
            assert_eq!(p(expr).as_ref(), Some(want), "{expr:?}");
        }
        let big = "1".repeat(MAX_INT_DIGITS);
        assert_eq!(p(&format!("id in [{big}]")), Some(vec![Oor]));
    }

    #[test]
    fn rejected_forms() {
        let over = format!("id in [{}]", "1".repeat(MAX_INT_DIGITS + 1));
        let zeros = format!("id in [{}1]", "0".repeat(MAX_INT_DIGITS));
        let cases = [
            "ID in [1]",
            "idx in [1]",
            "`id` in [1]",
            "$meta[\"id\"] in [1]",
            "id[0] in [1]",
            "id not in [1]",
            "id NOT IN [1]",
            "id !in [1]",
            "id ! in [1]",
            "not id in [1]",
            "!(id in [1])",
            "id in (1,2)",
            "id in 1",
            "id == 1",
            "idin [1]",
            "id inn [1]",
            "id in\u{c}[1]",
            "id\u{a0}in [1]",
            "id\u{b}in [1]",
            "id\u{3000}in [1]",
            "id in [,]",
            "id in [1,,]",
            "id in [,1]",
            "id in [[1]]",
            "id in [1 2]",
            "id in [1",
            "id in [1]]",
            "id in [--5]",
            "id in [-(5)]",
            "id in [+5]",
            "id in [0x10]",
            "id in [1_000]",
            "id in [1a]",
            "id in [1+1]",
            &over,
            &zeros,
            "id in [1e3]",
            "id in [1E3]",
            "id in [1.0]",
            "id in [1.]",
            "id in [.5]",
            "id in [1e]",
            "id in [true]",
            "id in [False]",
            "id in [tRuE]",
            "id in [null]",
            "id in [ISO '2025-01-01']",
            "id in [INTERVAL 'P1D']",
            "id in [x]",
            "id in [id]",
            "id in [²]",
            "id in [1²]",
            "id in [①]",
            "id in [٣]",
            "id in [１２]",
            "id in [1٣]",
            "id in ['\\x41']",
            "id in ['\\u4e2d']",
            "id in ['\\0']",
            "id in ['\\b']",
            "id in ['\\/']",
            "id in ['\\q']",
            "id in ['a\\\nb']",
            "id in ['a\nb']",
            "id in ['a\rb']",
            "id in ['a",
            "id in ['a]",
            "id in ['a\\']",
            "id in ['a'b']",
            "id in ['a''b']",
            "id in ['a' 'b']",
            "(id in [1]",
            "id in [1])",
            "(id)) in [1]",
            "((id) in [1]))",
            "id in ([1])",
            "()",
            "id in [1] and true",
            "id in [1] or id in [2]",
            "id in [1] && true",
            "id in [1] == true",
            "id in [1] in [2]",
            "id in [1] # c",
            "id in [1];",
            "",
            "   ",
            "id in {ids}",
            "id in [1, 1.5]",
            "id in [1, true]",
            "id",
            "id in",
        ];
        for expr in cases {
            assert_eq!(p(expr), None, "{expr:?}");
        }
    }

    #[test]
    fn primary_key_names() {
        for pk in [
            "ID",
            "_id",
            "text_match",
            "a1",
            "编号",
            "é",
            "ユーザー",
            "사용자",
            "ſ",
        ] {
            assert_eq!(
                parse_pk_in(&format!("{pk} in [1]"), pk),
                Some(vec![Int(1)]),
                "{pk}"
            );
        }
        // 关键字、布尔字面量（不区分大小写）与 3.x 切分不成单个标识符的名字。
        for pk in [
            "in",
            "and",
            "like",
            "LIKE",
            "Like",
            "liKe",
            "not",
            "is",
            "null",
            "exists",
            "or",
            "true",
            "True",
            "tRue",
            "iso",
            "INTERVAL",
            "नाम",
            "a-b",
            "a b",
            "1a",
            "my.id",
            "x²",
            "",
            "\u{9ffd}",
        ] {
            assert_eq!(parse_pk_in(&format!("{pk} in [1]"), pk), None, "{pk:?}");
        }
    }
}
