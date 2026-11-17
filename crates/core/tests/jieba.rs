//! jieba 切分与 jieba 0.42.1 的逐条对照（0036）；真值由 tests/differential/generate_jieba_fixtures.py 导出。
use milvus_lite_core::jieba::Segmenter;
use serde::Deserialize;

#[derive(Deserialize)]
struct Segments {
    text: String,
    cut: Vec<String>,
    search: Vec<String>,
}

#[derive(Deserialize)]
struct User {
    words: Vec<String>,
    total: u64,
    texts: Vec<String>,
    cut: Vec<Vec<String>>,
    search: Vec<Vec<String>>,
}

#[derive(Deserialize)]
struct Fixture {
    segments: Vec<Segments>,
    user: Vec<User>,
}

#[allow(clippy::unwrap_used)] // fixture 随仓库生成，解析失败即测试失败。
fn fixture() -> Fixture {
    milvus_lite_core::json_text::parse_str(include_str!("fixtures/jieba.json")).unwrap()
}

#[test]
fn matches_jieba_without_user_words() {
    let segments = fixture().segments;
    assert!(segments.len() > 1500, "corpus shrank to {}", segments.len());
    let cut = Segmenter::new(false, Vec::new()).unwrap();
    let search = Segmenter::new(true, Vec::new()).unwrap();
    for case in segments {
        assert_eq!(cut.cut(&case.text), case.cut, "{:?}", case.text);
        assert_eq!(search.cut(&case.text), case.search, "{:?}", case.text);
    }
}

#[test]
fn matches_add_word_on_a_fresh_tokenizer() {
    let groups = fixture().user;
    assert!(groups.len() >= 50, "groups shrank to {}", groups.len());
    for group in groups {
        let cut = Segmenter::new(false, group.words.clone()).unwrap();
        let search = Segmenter::new(true, group.words.clone()).unwrap();
        assert_eq!(cut.total(), group.total, "{:?}", group.words);
        for (at, text) in group.texts.iter().enumerate() {
            assert_eq!(cut.cut(text), group.cut[at], "{:?} {text:?}", group.words);
            assert_eq!(
                search.cut(text),
                group.search[at],
                "{:?} {text:?}",
                group.words
            );
        }
    }
}
