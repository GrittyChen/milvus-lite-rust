//! milvus-lite-rust 的引擎层。不依赖 protobuf 与 Python（见 AGENTS.md「架构地图」）。

pub mod collection;
pub mod compute;
pub mod data;
pub mod data_dir;
pub mod db;
pub mod error;
pub mod filter;
mod flat;
pub mod index;
pub mod lock_order;
mod maintenance;
#[cfg(feature = "fault-injection")]
pub use maintenance::{
    CompactionFault, CompactionPausePoint, MaintenanceReport,
    inject_start_failure_once as inject_maintenance_start_failure_once,
};
pub mod projection;
pub mod pyjson;
pub mod schema;
pub mod storage;

pub use collection::{
    Collection, GetSnapshot, Hits, LoadState, QueryResult, QueryVectors, ReadSnapshot,
    SearchRequest, SearchResult,
};
pub use compute::{Compute, ComputeError};
pub use data::{ColumnData, InputValues, InsertBatch, META_FIELD, Pk, Rows};
pub use data_dir::{DataDir, DataDirError};
pub use db::{CollectionHandle, DEFAULT_DATABASE, Db, InFlight};
pub use error::{EngineError, Result};
pub use index::{IndexRequest, IndexSpec, Metric};
pub use projection::{ApiKind, Projection};
pub use schema::{CollectionSchema, DataType, FieldSchema};
