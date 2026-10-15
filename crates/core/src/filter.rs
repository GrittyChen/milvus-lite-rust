//! 3.x 完整过滤语法与三路线求值；范围与拒绝边界见 0006、0009 和 0014。

mod columns;
mod dynamic;
mod eval;
mod parser;
mod python;
mod python_json;
mod temporal;
mod tree;
#[cfg(test)]
mod tree_tests;
mod unicode;
pub(crate) use dynamic::DynamicRow;
pub(crate) use eval::Predicate;

/// 错误字符串与词法必须使用同一版 Unicode 属性，避免未分配码位的 repr 漂移。
pub(crate) fn printable_char(c: char) -> bool {
    unicode::printable(c)
}

use crate::error::Result;

/// 与 schema 无关的解析结果；适配层可在取状态锁之前解析，再保留原来的校验错误优先级。
pub struct ParsedFilter {
    source: String,
    ast: Option<parser::Ast>,
}
impl ParsedFilter {
    pub(crate) fn is_empty(&self) -> bool {
        self.ast.is_none()
    }
    pub fn parse(source: &str) -> Result<Self> {
        Ok(Self {
            source: source.into(),
            ast: if source.is_empty() {
                None
            } else {
                Some(parser::parse(source)?)
            },
        })
    }
    /// 非主键快路径仍须绑定 schema 并做语义检查；不得把无法识别当成无过滤。
    pub fn primary_keys(&self, pk: &str) -> Option<Vec<PkLiteral>> {
        use parser::{Kind, Literal};
        let ast = self.ast.as_ref()?;
        let Kind::In {
            field,
            list,
            negate: false,
        } = ast.nodes[ast.root].kind
        else {
            return None;
        };
        if !matches!(&ast.nodes[field].kind,Kind::Field(name) if name==pk) {
            return None;
        }
        let Kind::List(elements) = &ast.nodes[list].kind else {
            return None;
        };
        let mut out = Vec::with_capacity(elements.len());
        for &id in elements {
            match &ast.nodes[id].kind {
                Kind::Lit(Literal::Int(s)) => {
                    out.push(s.parse().map_or(PkLiteral::IntOutOfRange, PkLiteral::Int))
                }
                Kind::Lit(Literal::String(s)) => out.push(PkLiteral::Str(s.clone())),
                _ => return None,
            }
        }
        Some(out)
    }
}

/// 列表中的一个字面量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkLiteral {
    Int(i64),
    /// 超出 int64 的整数。3.x 不报错，只是查不到。
    IntOutOfRange,
    Str(String),
}

#[cfg(test)]
const MAX_INT_DIGITS: usize = 4300;

#[cfg(test)]
mod tests {
    use super::*;
    use PkLiteral::{Int, IntOutOfRange as Oor, Str};

    fn p(expr: &str) -> Option<Vec<PkLiteral>> {
        ParsedFilter::parse(expr)
            .ok()
            .and_then(|parsed| parsed.primary_keys("id"))
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
            ("id in [٣, １２, 1٣]", vec![Int(3), Int(12), Int(13)]),
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
            "नम",
            "x²",
            "\u{9ffd}",
        ] {
            assert_eq!(
                ParsedFilter::parse(&format!("{pk} in [1]"))
                    .ok()
                    .and_then(|parsed| parsed.primary_keys(pk)),
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
            "",
        ] {
            assert_eq!(
                ParsedFilter::parse(&format!("{pk} in [1]"))
                    .ok()
                    .and_then(|parsed| parsed.primary_keys(pk)),
                None,
                "{pk:?}"
            );
        }
    }
}
