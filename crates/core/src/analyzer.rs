//! 3.x 的 standard 分词：`re.findall(r"\w+", text.lower())`。
//!
//! 分词结果要与 3.x 逐位一致（0002 约束 8），所以大小写和 `\w` 都用固定的 CPython 3.12 表，
//! 不用 Rust 标准库的大小写表：后者随工具链的 Unicode 版本变化。输入按码位处理，
//! 孤立代理（只可能来自解码后的 JSON 值）既没有大小写，也不是词字符。

use crate::filter::unicode;
use std::collections::HashSet;

fn scalar(c: u32) -> Option<char> {
    char::from_u32(c)
}

fn is_word(c: u32) -> bool {
    if c < 0x80 {
        return (c as u8).is_ascii_alphanumeric() || c == u32::from(b'_');
    }
    scalar(c).is_some_and(unicode::alnum)
}

fn cased(c: u32) -> bool {
    scalar(c).is_some_and(unicode::cased)
}

fn case_ignorable(c: u32) -> bool {
    scalar(c).is_some_and(unicode::case_ignorable)
}

/// CPython 的 `handle_capital_sigma`：前面（跳过可忽略字符）是 cased，后面（同样跳过）不是 cased。
fn final_sigma(text: &[u32], at: usize) -> bool {
    let before = text[..at].iter().rev().find(|c| !case_ignorable(**c));
    if !before.is_some_and(|c| cased(*c)) {
        return false;
    }
    let after = text[at + 1..].iter().find(|c| !case_ignorable(**c));
    !after.is_some_and(|c| cased(*c))
}

/// `text[at]` 在 `str.lower()` 下的结果；第二个码位只有 U+0130 才有。
fn lower(text: &[u32], at: usize) -> (u32, Option<u32>) {
    let c = text[at];
    if c < 0x80 {
        return (u32::from((c as u8).to_ascii_lowercase()), None);
    }
    match c {
        0x130 => (0x69, Some(0x307)),
        0x3a3 if final_sigma(text, at) => (0x3c2, None),
        _ => (
            scalar(c).and_then(unicode::lower).map_or(c, u32::from),
            None,
        ),
    }
}

/// 逐词回调，回调返回 true 时提前结束并返回 true。
fn visit_tokens(text: &[u32], mut visit: impl FnMut(&[u32]) -> bool) -> bool {
    let mut token = Vec::new();
    let mut push = |c: u32, token: &mut Vec<u32>| {
        if is_word(c) {
            token.push(c);
            return false;
        }
        let found = !token.is_empty() && visit(token);
        token.clear();
        found
    };
    for at in 0..text.len() {
        let (first, second) = lower(text, at);
        if push(first, &mut token) || second.is_some_and(|c| push(c, &mut token)) {
            return true;
        }
    }
    // 用一个非词字符收尾，让最后一个词也经过回调。
    push(0x20, &mut token)
}

/// `StandardAnalyzer().tokenize(text)`，不带停用词。
pub fn tokenize(text: &str) -> Vec<String> {
    let points: Vec<u32> = text.chars().map(u32::from).collect();
    let mut tokens = Vec::new();
    visit_tokens(&points, |token| {
        tokens.push(token.iter().copied().filter_map(scalar).collect());
        false
    });
    tokens
}

/// 编译后的 `text_match` 查询词：3.x 是 OR 语义，只要文档含任一查询词即命中。
#[derive(Debug)]
pub(crate) struct TextQuery {
    terms: HashSet<Vec<u32>>,
}

impl TextQuery {
    pub(crate) fn new(query: &str) -> Self {
        let points: Vec<u32> = query.chars().map(u32::from).collect();
        let mut terms = HashSet::new();
        visit_tokens(&points, |token| {
            terms.insert(token.to_vec());
            false
        });
        Self { terms }
    }

    pub(crate) fn matches(&self, text: &[u32]) -> bool {
        !self.terms.is_empty() && visit_tokens(text, |token| self.terms.contains(token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(text: &str) -> Vec<u32> {
        text.chars().map(u32::from).collect()
    }

    #[test]
    fn splits_on_non_word_and_lowers() {
        assert_eq!(
            tokenize("Hello, World! foo_bar"),
            ["hello", "world", "foo_bar"]
        );
        assert!(tokenize("!!! ...").is_empty());
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn follows_cpython_case_rules() {
        assert_eq!(tokenize("ΣΑΣ"), ["σας"]);
        assert_eq!(tokenize("Σ"), ["σ"]);
        // `.` 是 case-ignorable，往后还能看到 Β，所以不算词尾；空格不可忽略。
        assert_eq!(tokenize("ΑΣ.Β"), ["ασ", "β"]);
        assert_eq!(tokenize("ΑΣ Β"), ["ας", "β"]);
        assert_eq!(tokenize("İstanbul"), ["i", "stanbul"]);
        assert_eq!(tokenize("x²y"), ["x²y"]);
        assert_eq!(tokenize("re\u{301}sume\u{301}"), ["re", "sume"]);
        assert_eq!(tokenize("中文 ΑΒ"), ["中文", "αβ"]);
    }

    #[test]
    fn newer_unicode_is_not_lowered() {
        // U+A7CB 在 Unicode 16 才有小写映射；固定在 15.0，既不转换，也不是词字符。
        assert!(tokenize("\u{a7cb}").is_empty());
    }

    #[test]
    fn lone_surrogates_are_separators() {
        let query = TextQuery::new("ab");
        assert!(query.matches(&[0x61, 0x62, 0xd800, 0x63]));
        assert!(!query.matches(&[0x61, 0xd800, 0x62]));
    }

    #[test]
    fn query_is_or_and_empty_query_never_matches() {
        let query = TextQuery::new("missing HELLO");
        assert!(query.matches(&points("hello world")));
        assert!(!query.matches(&points("world")));
        assert!(!TextQuery::new("!!!").matches(&points("!!!")));
        assert!(!TextQuery::new("").matches(&points("")));
    }

    #[test]
    fn last_token_is_visited() {
        assert!(TextQuery::new("end").matches(&points("the end")));
    }
}
