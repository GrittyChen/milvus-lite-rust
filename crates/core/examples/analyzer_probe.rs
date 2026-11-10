//! 不经服务的分词对照入口；逐行读 JSON 字符串，输出 standard 分词的结果。
use milvus_lite_core::analyzer::tokenize;
use std::io::{self, BufRead, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = io::stdin().lock();
    let mut output = io::BufWriter::new(io::stdout().lock());
    for line in input.lines() {
        let text: String = milvus_lite_core::json_text::parse_str(&line?)?;
        serde_json::to_writer(&mut output, &tokenize(&text))?;
        writeln!(output)?;
    }
    output.flush()?;
    Ok(())
}
