//! 未实现的 RPC 一律返回 UNIMPLEMENTED 与明确原因，绝不静默成功（不变量 15）。
//!
//! 生成的服务只含已实现的方法；这里拦截其余方法（包括其他 service 与未知路径），
//! 原因的格式与 3.x 的 `_unimplemented` 一致：`MilvusLite does not support <RPC>: <说明>`。

use std::convert::Infallible;
use std::task::{Context, Poll};

use tonic::body::Body;
use tonic::codegen::{BoxFuture, Service, http};
use tonic::server::NamedService;
use tonic::service::Routes;
use tonic::{Code, Status};

include!(concat!(env!("OUT_DIR"), "/implemented_methods.rs"));

use crate::MAX_MESSAGE_SIZE;

const REASON: &str = "not implemented in this build of milvus-lite-rust yet";

/// 3.x 自身也不支持、并给出专门说明的 RPC：说明文字与 3.x 的 servicer 一致。
const REASONS_FROM_3X: &[(&str, &str)] = &[
    (
        "LoadPartitions",
        "partition-level load is not supported; use load_collection instead",
    ),
    (
        "ReleasePartitions",
        "partition-level release is not supported; use release_collection instead",
    ),
];

/// 包住生成的 `MilvusServiceServer`，只放行 `IMPLEMENTED_METHODS`。
#[derive(Clone)]
pub(crate) struct Guard<S> {
    inner: S,
}

impl<S: NamedService> NamedService for Guard<S> {
    const NAME: &'static str = S::NAME;
}

impl<S, B> Service<http::Request<B>> for Guard<S>
where
    S: Service<http::Request<B>, Response = http::Response<Body>, Error = Infallible>,
    S::Future: Send + 'static,
{
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let method = method_name(req.uri().path());
        if IMPLEMENTED_METHODS.contains(&method) {
            let call = self.inner.call(req);
            Box::pin(async move { call.await.map(rewrite_oversized_request) })
        } else {
            let resp = unimplemented(method);
            Box::pin(async move { Ok(resp) })
        }
    }
}

/// 路由：`MilvusService` 走 `Guard`，其余路径走同样格式的 fallback。
pub(crate) fn routes<S>(service: S) -> Routes
where
    S: Service<http::Request<Body>, Response = http::Response<Body>, Error = Infallible>
        + NamedService
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send + 'static,
{
    let mut routes = Routes::new(Guard { inner: service });
    let router = std::mem::take(routes.axum_router_mut());
    *routes.axum_router_mut() =
        router.fallback(|uri: http::Uri| async move { unimplemented(method_name(uri.path())) });
    routes
}

/// tonic 对超过上限的请求在读完消息头时就返回 OUT_OF_RANGE（trailers-only 响应，状态在响应头中）；
/// 3.x（grpcio）返回 RESOURCE_EXHAUSTED。pymilvus 不重试后者，却会重试前者。
/// 这里依赖 tonic 0.14 `codec/decode.rs` 的错误原文，升级 tonic 时由 256MB 的端到端用例把关。
const DECODE_TOO_LARGE: &str = "Error, decoded message length too large: found ";

fn rewrite_oversized_request(resp: http::Response<Body>) -> http::Response<Body> {
    let Some(status) = Status::from_header_map(resp.headers()) else {
        return resp;
    };
    if status.code() != Code::OutOfRange {
        return resp;
    }
    let Some(len) = status
        .message()
        .strip_prefix(DECODE_TOO_LARGE)
        .and_then(|rest| rest.split(' ').next())
    else {
        return resp;
    };
    Status::resource_exhausted(format!(
        "SERVER: Received message larger than max ({len} vs. {MAX_MESSAGE_SIZE})"
    ))
    .into_http()
}

fn method_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn unimplemented(method: &str) -> http::Response<Body> {
    let reason = REASONS_FROM_3X
        .iter()
        .find(|(m, _)| *m == method)
        .map_or(REASON, |(_, r)| r);
    Status::unimplemented(format!("MilvusLite does not support {method}: {reason}")).into_http()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_name_is_last_path_segment() {
        assert_eq!(
            method_name("/milvus.proto.milvus.MilvusService/LoadPartitions"),
            "LoadPartitions"
        );
        assert_eq!(method_name("/x"), "x");
        assert_eq!(method_name(""), "");
    }
}
