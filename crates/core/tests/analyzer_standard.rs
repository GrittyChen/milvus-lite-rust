//! standard 分词与真实 3.x 的逐条对照；语料由 tests/differential/generate_analyzer_fixtures.py 导出。
use milvus_lite_core::analyzer::tokenize;
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    text: String,
    tokens: Vec<String>,
}

#[test]
fn matches_the_reference_tokenizer_on_the_exported_corpus() {
    let cases: Vec<Case> =
        milvus_lite_core::json_text::parse_str(include_str!("fixtures/analyzer_standard.json"))
            .unwrap();
    assert!(cases.len() > 500, "corpus shrank to {}", cases.len());
    for case in cases {
        assert_eq!(tokenize(&case.text), case.tokens, "{:?}", case.text);
    }
}
