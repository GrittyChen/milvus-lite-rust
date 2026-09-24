//! `MilvusService` 的实现：只翻译，业务语义在引擎层。

use tonic::{Request, Response, Status};

use crate::pb::common;
use crate::pb::service::{ConnectRequest, ConnectResponse, GetVersionRequest, GetVersionResponse};

pub(crate) struct MilvusService {
    version: String,
}

impl MilvusService {
    pub(crate) fn new(version: String) -> Self {
        Self { version }
    }
}

fn success() -> Option<common::Status> {
    Some(common::Status::default())
}

#[tonic::async_trait]
impl crate::pb::service::milvus_service_server::MilvusService for MilvusService {
    async fn connect(
        &self,
        _request: Request<ConnectRequest>,
    ) -> Result<Response<ConnectResponse>, Status> {
        // 字段取值与 3.x 的 servicer.Connect 相同。
        Ok(Response::new(ConnectResponse {
            status: success(),
            server_info: Some(common::ServerInfo {
                build_tags: "milvus_lite".to_owned(),
                deploy_mode: "embedded".to_owned(),
                ..Default::default()
            }),
            identifier: 0,
        }))
    }

    async fn get_version(
        &self,
        _request: Request<GetVersionRequest>,
    ) -> Result<Response<GetVersionResponse>, Status> {
        // 格式与 3.x 相同：`milvus_lite-<版本>`，pymilvus 与参照实现的用例据此识别 Milvus Lite。
        Ok(Response::new(GetVersionResponse {
            status: success(),
            version: format!("milvus_lite-{}", self.version),
        }))
    }
}
