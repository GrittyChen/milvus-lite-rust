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

/// 3.x `term_to_id`：词的 UTF-8 字节上的 32 位 FNV-1a，结果可以是 `2^32-1`。
fn term_id(term: &str) -> u32 {
    term.bytes().fold(0x811c_9dc5, |hash: u32, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

/// BM25 function 的分词器（3.x `create_analyzer`）：standard 切词加停用词。只在 BM25 输入字段上构造，
/// `text_match` 仍不读 `analyzer_params`（0029）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Bm25Analyzer {
    stop_words: HashSet<String>,
}

impl Bm25Analyzer {
    /// `params` 是 `json.loads` 的原值，不经过 0029 的假值过滤：3.x 把它直接交给 `create_analyzer`，
    /// 只有 `None` 走默认分支，`0`、`[]`、`""` 等在 `params.get` 上抛 AttributeError。
    pub(crate) fn from_params(params: Option<&serde_json::Value>) -> crate::error::Result<Self> {
        use crate::error::{EngineError, py_json_repr};
        use crate::pyjson::{python_truthy, python_type_name};
        use serde_json::Value;
        let params = match params {
            None | Some(Value::Null) => return Ok(Self::default()),
            Some(Value::Object(params)) => params,
            Some(other) => {
                return Err(EngineError::invalid(format!(
                    "'{}' object has no attribute 'get'",
                    python_type_name(other)
                )));
            }
        };
        // `params.get("tokenizer") or params.get("type", "standard")`
        let fallback = Value::String("standard".to_owned());
        let tokenizer = params
            .get("tokenizer")
            .filter(|value| python_truthy(value))
            .or_else(|| params.get("type"))
            .unwrap_or(&fallback);
        let stop_words = stop_words(params);
        let name = match tokenizer {
            Value::String(_) => tokenizer,
            Value::Object(options) => options.get("type").unwrap_or(&fallback),
            other => {
                return Err(EngineError::schema(format!(
                    "analyzer_params 'tokenizer' must be a string or dict, got {}",
                    python_type_name(other)
                )));
            }
        };
        match name.as_str() {
            Some("standard") => Ok(Self { stop_words }),
            Some("jieba") => Err(EngineError::unsupported(
                "jieba tokenizer",
                "the jieba tokenizer is not supported in this build of milvus-lite-rust",
            )),
            _ => Err(EngineError::schema(format!(
                "unknown tokenizer type: {} (supported: 'standard', 'jieba')",
                py_json_repr(name)
            ))),
        }
    }

    /// 3.x `compute_tf(analyzer.analyze(text))`：按词项哈希计数，顺序是首次出现的顺序。3.x 用整数计数、
    /// 最后才 `float(c)`，所以这里也先整数计数，超过 2^24 次时与逐次 f32 累加的结果不同。
    pub(crate) fn term_frequencies(&self, text: &str) -> Vec<(u32, f32)> {
        let mut counts: Vec<(u32, u64)> = Vec::new();
        let mut positions: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
        for token in tokenize(text) {
            if self.stop_words.contains(&token) {
                continue;
            }
            let id = term_id(&token);
            match positions.get(&id) {
                Some(&at) => counts[at].1 += 1,
                None => {
                    positions.insert(id, counts.len());
                    counts.push((id, 1));
                }
            }
        }
        // 计数不超过文本长度，转换到 f32 的舍入与 Python `float(c)` 再存 float32 相同。
        counts
            .into_iter()
            .map(|(id, count)| (id, count as f32))
            .collect()
    }
}

/// 3.x `_extract_stop_words`：`filter` 列表里 `type == "stop"` 的对象，其 `stop_words` 为列表时逐项 `str()`。
/// 词已转小写且只含 `\w`：`True` / `False` / `None` 与浮点、列表、对象转成的文本都不可能命中，不收进集合。
fn stop_words(params: &serde_json::Map<String, serde_json::Value>) -> HashSet<String> {
    use serde_json::Value;
    let mut words = HashSet::new();
    let Some(Value::Array(filters)) = params.get("filter") else {
        return words;
    };
    for filter in filters {
        let Value::Object(filter) = filter else {
            continue;
        };
        if filter.get("type").and_then(Value::as_str) != Some("stop") {
            continue;
        }
        let Some(Value::Array(list)) = filter.get("stop_words") else {
            continue;
        };
        for word in list {
            match word {
                Value::String(word) => {
                    words.insert(word.clone());
                }
                Value::Number(n) if !n.as_str().contains(['.', 'e', 'E']) => {
                    words.insert(crate::error::py_json_repr(word));
                }
                _ => {}
            }
        }
    }
    words
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

    /// 期望值取自 3.x `term_to_id`。
    #[test]
    fn term_ids_are_fnv1a_of_utf8() {
        assert_eq!(term_id(""), 0x811c_9dc5);
        assert_eq!(term_id("fox"), 3069416270);
        assert_eq!(term_id("quick"), 4147758012);
        assert_eq!(term_id("the"), 3020861980);
    }

    fn analyzer(params: &str) -> crate::error::Result<Bm25Analyzer> {
        let value = crate::json_text::value_str(params).unwrap();
        Bm25Analyzer::from_params(Some(&value))
    }

    fn message(params: &str) -> String {
        analyzer(params).unwrap_err().to_string()
    }

    #[test]
    fn analyzer_params_follow_create_analyzer() {
        for ok in [
            "null",
            "{}",
            r#"{"tokenizer": "standard"}"#,
            r#"{"tokenizer": []}"#,
            r#"{"tokenizer": 0}"#,
            r#"{"tokenizer": ""}"#,
            r#"{"tokenizer": {}}"#,
            r#"{"tokenizer": {"mode": "x"}}"#,
            r#"{"type": "standard"}"#,
        ] {
            assert!(analyzer(ok).is_ok(), "{ok}");
        }
        for (params, kind) in [
            ("0", "int"),
            ("0.0", "float"),
            ("false", "bool"),
            ("true", "bool"),
            ("[]", "list"),
            (r#""""#, "str"),
            (r#""standard""#, "str"),
        ] {
            assert_eq!(
                message(params),
                format!("'{kind}' object has no attribute 'get'"),
                "{params}"
            );
        }
        assert_eq!(
            message(r#"{"tokenizer": 5}"#),
            "analyzer_params 'tokenizer' must be a string or dict, got int"
        );
        assert_eq!(
            message(r#"{"tokenizer": null, "type": null}"#),
            "analyzer_params 'tokenizer' must be a string or dict, got NoneType"
        );
        assert_eq!(
            message(r#"{"tokenizer": "Standard"}"#),
            "unknown tokenizer type: 'Standard' (supported: 'standard', 'jieba')"
        );
        assert_eq!(
            message(r#"{"tokenizer": {"type": null}}"#),
            "unknown tokenizer type: None (supported: 'standard', 'jieba')"
        );
        assert_eq!(
            message(r#"{"tokenizer": {"type": {"a": [1, "b"]}}}"#),
            "unknown tokenizer type: {'a': [1, 'b']} (supported: 'standard', 'jieba')"
        );
        assert!(matches!(
            analyzer(r#"{"type": "jieba"}"#),
            Err(crate::error::EngineError::Unsupported { .. })
        ));
    }

    #[test]
    fn stop_words_and_frequencies() {
        let a = analyzer(
            r#"{"filter": ["lowercase", {"type": "stop", "stop_words": ["fox", 1, true, null, 1.5, "The"]},
                {"type": "Stop", "stop_words": ["dog"]}, {"type": "stop", "stop_words": "cat"}]}"#,
        )
        .unwrap();
        let words: HashSet<&str> = a.stop_words.iter().map(String::as_str).collect();
        assert_eq!(words, HashSet::from(["fox", "1", "The"]));
        assert_eq!(
            a.term_frequencies("The fox the DOG 1 dog"),
            vec![(term_id("the"), 2.0), (term_id("dog"), 2.0)]
        );
        assert!(a.term_frequencies("fox 1").is_empty());
        assert!(Bm25Analyzer::default().term_frequencies("").is_empty());
    }

    #[test]
    fn last_token_is_visited() {
        assert!(TextQuery::new("end").matches(&points("the end")));
    }
}
