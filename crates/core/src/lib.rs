//! milvus-lite-rust 的引擎层。不依赖 protobuf 与 Python（见 AGENTS.md「架构地图」）。

pub mod collection;
pub mod data;
pub mod data_dir;
pub mod db;
pub mod error;
mod flat;
pub mod index;
pub mod projection;
pub mod schema;

pub use collection::{Collection, Hits, LoadState, QueryResult, SearchRequest, SearchResult};
pub use data::{ColumnData, InputValues, InsertBatch, META_FIELD, Pk, Rows};
pub use data_dir::{DataDir, DataDirError};
pub use db::{CollectionHandle, Db};
pub use error::{EngineError, Result};
pub use index::{IndexRequest, IndexSpec, Metric};
pub use projection::{ApiKind, Projection};
pub use schema::{CollectionSchema, DataType, FieldSchema};
