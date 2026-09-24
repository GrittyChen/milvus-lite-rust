//! milvus-lite-rust 的 gRPC 层：Milvus 协议的解码 / 编码与错误映射，以及服务实例的启动与停止。
//! 业务语义全部在 `milvus-lite-core`（见 AGENTS.md「架构地图」）。

mod guard;
mod index;
mod insert;
mod output;
pub mod pb;
mod pyint;
mod query;
mod schema;
mod search;
mod server;
mod service;
mod status;

pub use server::{Server, ServerConfig, ServerError};

/// 与 3.x（及 Milvus Standalone）一致的消息大小上限，收发两个方向相同；tonic 默认解码上限只有 4 MB。
pub(crate) const MAX_MESSAGE_SIZE: usize = 256 * 1024 * 1024;
