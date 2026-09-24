//! milvus-lite-rust 的 gRPC 层：Milvus 协议的解码 / 编码与错误映射，以及服务实例的启动与停止。
//! 业务语义全部在 `milvus-lite-core`（见 AGENTS.md「架构地图」）。

mod guard;
pub mod pb;
mod server;
mod service;

pub use server::{Server, ServerConfig, ServerError};
