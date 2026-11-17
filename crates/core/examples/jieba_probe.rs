//! 不经服务的 jieba 对照入口（0036）。逐行读 JSON `{"words": [...], "texts": [...]}`，按这组用户词建分词器，
//! 输出 `{"total": ..., "cut": [...], "search": [...]}`；建不出来时输出 `{"error": "..."}`。
use milvus_lite_core::jieba::Segmenter;
use std::io::{self, BufRead, Write};

#[derive(serde::Deserialize)]
struct Case {
    words: Vec<String>,
    texts: Vec<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = io::stdin().lock();
    let mut output = io::BufWriter::new(io::stdout().lock());
    for line in input.lines() {
        let case: Case = milvus_lite_core::json_text::parse_str(&line?)?;
        let built = Segmenter::new(false, case.words.clone())
            .and_then(|cut| Ok((cut, Segmenter::new(true, case.words)?)));
        let result = match built {
            Ok((cut, search)) => serde_json::json!({
                "total": cut.total(),
                "cut": case.texts.iter().map(|t| cut.cut(t)).collect::<Vec<_>>(),
                "search": case.texts.iter().map(|t| search.cut(t)).collect::<Vec<_>>(),
            }),
            Err(error) => serde_json::json!({ "error": error.to_string() }),
        };
        serde_json::to_writer(&mut output, &result)?;
        writeln!(output)?;
    }
    output.flush()?;
    Ok(())
}
