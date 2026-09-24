//! milvus-lite-rust 的引擎层。不依赖 protobuf 与 Python（见 AGENTS.md「架构地图」）。

pub mod data_dir;

pub use data_dir::{DataDir, DataDirError};
