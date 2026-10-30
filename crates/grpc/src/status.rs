//! 引擎错误到 Milvus 状态的映射，与 3.x `adapter/grpc/errors.py` 一致：业务错误放在 gRPC OK 的
//! 响应体 status 里，同时设置 `code` 与旧版 `error_code`；只有本实现尚不支持的功能返回 gRPC UNIMPLEMENTED。

use milvus_lite_core::EngineError;

use crate::pb::common;

/// 3.x servicer 的两种异常分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Branch {
    /// `except MilvusLiteError` 按异常类映射，其余异常为 code 1、error_code 0。
    Engine,
    /// 只有 `except Exception`（HasCollection、ShowCollections）：一律 code 1、error_code 0。
    Generic,
}

#[allow(deprecated)]
pub(crate) fn success() -> common::Status {
    common::Status {
        error_code: 0,
        code: 0,
        ..Default::default()
    }
}

#[allow(deprecated)]
fn status(code: i32, error_code: i32, reason: String) -> common::Status {
    common::Status {
        error_code,
        code,
        reason,
        ..Default::default()
    }
}

/// 3.x 中未被识别为 MilvusLite 异常的错误（ValueError、numpy、pyarrow 等）。
pub(crate) fn unexpected(reason: String) -> common::Status {
    status(1, 0, reason)
}

/// Unsupported 映射为 gRPC UNIMPLEMENTED，其余映射为响应体中的 status。
pub(crate) fn from_engine(
    e: &EngineError,
    branch: Branch,
) -> Result<common::Status, tonic::Status> {
    if let EngineError::Unsupported { .. } = e {
        return Err(tonic::Status::unimplemented(format!(
            "MilvusLite does not support {e}"
        )));
    }
    if branch == Branch::Generic {
        return Ok(unexpected(e.to_string()));
    }
    let (code, error_code) = match e {
        EngineError::CollectionNotFound(_) | EngineError::AliasNotFound(_) => (100, 4),
        EngineError::CollectionAlreadyExists(_)
        | EngineError::AliasAlreadyExists(_)
        | EngineError::AliasConflictsWithCollection(_) => (1, 1),
        EngineError::PartitionNotFound(_) => (200, 200),
        EngineError::PartitionAlreadyExists(_) => (1, 1),
        EngineError::SchemaValidation(_) | EngineError::DatabaseNotFound(_) => (6, 6),
        EngineError::CollectionNotLoaded(_) => (101, 101),
        EngineError::IndexAlreadyExists(_) => (35, 35),
        EngineError::IndexNotFound(_) => (700, 11),
        EngineError::InvalidArgument(_)
        | EngineError::Closed
        | EngineError::DataDir(_)
        | EngineError::Storage(_)
        | EngineError::Unsupported { .. } => (1, 0),
    };
    Ok(status(code, error_code, e.to_string()))
}

/// 带 status 字段的响应：出错时只填 status，与 3.x 相同。
pub(crate) trait Reply: prost::Message + Default + Send + 'static {
    fn with_status(status: common::Status) -> Self;
}

impl Reply for common::Status {
    fn with_status(status: common::Status) -> Self {
        status
    }
}

macro_rules! reply_with_status_field {
    ($($t:ty),* $(,)?) => {$(
        impl Reply for $t {
            fn with_status(status: common::Status) -> Self {
                Self { status: Some(status), ..Default::default() }
            }
        }
    )*};
}

reply_with_status_field!(
    crate::pb::service::DescribeCollectionResponse,
    crate::pb::service::ShowCollectionsResponse,
    crate::pb::service::GetCollectionStatisticsResponse,
    crate::pb::service::BoolResponse,
    crate::pb::service::ShowPartitionsResponse,
    crate::pb::service::GetPartitionStatisticsResponse,
    crate::pb::service::DescribeIndexResponse,
    crate::pb::service::GetLoadingProgressResponse,
    crate::pb::service::GetLoadStateResponse,
    crate::pb::service::MutationResult,
    crate::pb::service::FlushResponse,
    crate::pb::service::GetFlushStateResponse,
    crate::pb::service::ManualCompactionResponse,
    crate::pb::service::GetCompactionStateResponse,
    crate::pb::service::QueryResults,
    crate::pb::service::SearchResults,
    crate::pb::service::DescribeAliasResponse,
    crate::pb::service::ListAliasesResponse,
);

// 只有 status 一个字段，宏中的 `..Default::default()` 无效。
impl Reply for crate::pb::service::TruncateCollectionResponse {
    fn with_status(status: common::Status) -> Self {
        Self {
            status: Some(status),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(deprecated)]
    fn codes(e: EngineError) -> (i32, i32) {
        let s = from_engine(&e, Branch::Engine).unwrap();
        (s.code, s.error_code)
    }

    #[test]
    fn codes_match_3x() {
        assert_eq!(codes(EngineError::CollectionNotFound("c".into())), (100, 4));
        assert_eq!(
            codes(EngineError::CollectionAlreadyExists("c".into())),
            (1, 1)
        );
        assert_eq!(codes(EngineError::AliasNotFound("a".into())), (100, 4));
        assert_eq!(codes(EngineError::AliasAlreadyExists("a".into())), (1, 1));
        assert_eq!(
            codes(EngineError::AliasConflictsWithCollection("a".into())),
            (1, 1)
        );
        assert_eq!(codes(EngineError::SchemaValidation("x".into())), (6, 6));
        assert_eq!(codes(EngineError::DatabaseNotFound("d".into())), (6, 6));
        assert_eq!(
            codes(EngineError::CollectionNotLoaded("c".into())),
            (101, 101)
        );
        assert_eq!(codes(EngineError::IndexAlreadyExists("v".into())), (35, 35));
        assert_eq!(codes(EngineError::IndexNotFound("x".into())), (700, 11));
        assert_eq!(codes(EngineError::InvalidArgument("x".into())), (1, 0));
        assert_eq!(codes(EngineError::Closed), (1, 0));
    }

    #[test]
    #[allow(deprecated)]
    fn generic_branch_and_unsupported() {
        let s = from_engine(&EngineError::DatabaseNotFound("d".into()), Branch::Generic).unwrap();
        assert_eq!((s.code, s.error_code), (1, 0));
        assert_eq!(s.reason, "database 'd' does not exist");
        let e = EngineError::Unsupported {
            feature: "partitions",
            reason: "why".into(),
        };
        let err = from_engine(&e, Branch::Engine).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unimplemented);
        assert_eq!(err.message(), "MilvusLite does not support partitions: why");
    }
}
