//! 不经服务的语法对照入口；只读取表达式并输出诊断，不访问数据库或执行过滤。
use milvus_lite_core::filter::ParsedFilter;
use std::io::{self, BufRead, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = io::stdin().lock();
    let mut output = io::BufWriter::new(io::stdout().lock());
    for line in input.lines() {
        let source: String = serde_json::from_str(&line?)?;
        let error = ParsedFilter::parse(&source)
            .err()
            .map(|error| error.to_string());
        serde_json::to_writer(&mut output, &error)?;
        writeln!(output)?;
    }
    output.flush()?;
    Ok(())
}
