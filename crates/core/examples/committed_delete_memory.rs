//! 独立删除表的组件 RSS 诊断；使用与 runtime 相同的 HashMap<Pk,u64>，不代表整库 RSS。
use milvus_lite_core::Pk;
use std::collections::HashMap;
use std::io::{self, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let count = args.next().ok_or("key count required")?.parse::<usize>()?;
    let kind = args.next().ok_or("int64/varchar required")?;
    if !matches!(kind.as_str(), "int64" | "varchar") {
        return Err("unknown primary key type".into());
    }
    let mut global = HashMap::<Pk, u64>::with_capacity(count);
    for i in 0..count {
        let pk = if kind == "int64" {
            Pk::Int64(i64::try_from(i)?)
        } else {
            Pk::VarChar(format!("{i:016}"))
        };
        global.insert(pk, u64::try_from(i)? + 1);
    }
    println!("BASE {count} {}", global.capacity());
    io::stdout().flush()?;
    let mut command = String::new();
    io::stdin().read_line(&mut command)?;
    if command.trim() != "clone" {
        return Err("clone command required".into());
    }
    let committed = global.clone();
    println!("CLONED {}", committed.len());
    io::stdout().flush()?;
    command.clear();
    io::stdin().read_line(&mut command)?;
    std::hint::black_box((&global, &committed));
    Ok(())
}
