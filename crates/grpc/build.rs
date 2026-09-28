//! 用纯 Rust 的 protox 解析 proto（不依赖系统 protoc），再交给 tonic / prost 生成代码。
//!
//! 只为 `IMPLEMENTED_METHODS` 生成 `MilvusService` 的 trait 方法：已实现的 RPC 由编译器保证都有实现，
//! 其余 RPC 由 `guard` 统一返回 UNIMPLEMENTED（不变量 15）。同一份列表写到 OUT_DIR 供 `guard` 使用，
//! 两处不会不一致。

use std::error::Error;
use std::fmt::Write as _;
use std::path::PathBuf;

const MILVUS_SERVICE: &str = "MilvusService";

const IMPLEMENTED_METHODS: &[&str] = &[
    "Connect",
    "GetVersion",
    "CreateCollection",
    "DropCollection",
    "DescribeCollection",
    "ShowCollections",
    "GetCollectionStatistics",
    "CreateIndex",
    "DescribeIndex",
    "LoadCollection",
    "ReleaseCollection",
    "GetLoadingProgress",
    "GetLoadState",
    "Insert",
    "Flush",
    "GetFlushState",
    "Query",
    "Search",
];

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=proto");
    println!("cargo:rerun-if-changed=build.rs");

    let mut fds = protox::compile(["milvus.proto"], ["proto"])?;
    let mut found = Vec::new();
    for file in &mut fds.file {
        // ClientTelemetryService、ProxyService 整体不生成，请求同样落到 guard。
        file.service.retain(|s| s.name() == MILVUS_SERVICE);
        for service in &mut file.service {
            service
                .method
                .retain(|m| IMPLEMENTED_METHODS.contains(&m.name()));
            found.extend(service.method.iter().map(|m| m.name().to_owned()));
        }
    }
    for name in IMPLEMENTED_METHODS {
        if !found.iter().any(|f| f == name) {
            return Err(format!("{MILVUS_SERVICE}.{name} not found in proto").into());
        }
    }

    tonic_prost_build::configure()
        .build_client(false)
        .compile_fds(fds)?;

    let mut list = String::from("pub(crate) const IMPLEMENTED_METHODS: &[&str] = &[\n");
    for name in IMPLEMENTED_METHODS {
        writeln!(list, "    {name:?},")?;
    }
    list.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out.join("implemented_methods.rs"), list)?;
    Ok(())
}
