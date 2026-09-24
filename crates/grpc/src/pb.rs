//! 由 milvus-proto 生成的消息与服务定义（来源见 proto/README.md）。

#![allow(clippy::all, clippy::pedantic, deprecated, missing_docs)]

pub mod milvus {
    pub mod proto {
        pub mod common {
            tonic::include_proto!("milvus.proto.common");
        }
        pub mod schema {
            tonic::include_proto!("milvus.proto.schema");
        }
        pub mod feder {
            tonic::include_proto!("milvus.proto.feder");
        }
        pub mod msg {
            tonic::include_proto!("milvus.proto.msg");
        }
        pub mod rg {
            tonic::include_proto!("milvus.proto.rg");
        }
        pub mod milvus {
            tonic::include_proto!("milvus.proto.milvus");
        }
    }
}

pub use milvus::proto::{common, milvus as service, schema};
