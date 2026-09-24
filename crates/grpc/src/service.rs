//! `MilvusService` 的实现：只翻译，业务语义在引擎层。各 RPC 的请求解析、检查顺序与响应字段都与 3.x 的
//! `servicer.py` 一致；与 3.x 不同之处见 0004「决策」。

use std::sync::Arc;

use milvus_lite_core::{Db, EngineError, LoadState};
use tonic::{Request, Response, Status};

use crate::pb::common::{self, LoadState as PbLoadState};
use crate::pb::service::{self as svc, milvus_service_server::MilvusService as Service};
use crate::status::{self as st, Branch, Reply};
use crate::{MAX_MESSAGE_SIZE, index, insert, query, schema, search};

pub(crate) struct MilvusService {
    version: String,
    db: Arc<Db>,
}

impl MilvusService {
    pub(crate) fn new(version: String, db: Arc<Db>) -> Self {
        Self { version, db }
    }

    /// 在阻塞线程池中执行一个 RPC 的处理体（解码、引擎调用、编码都是 CPU 密集或可能等锁），
    /// 期间持有引擎的在途任务，保证停止服务时 LOCK 在所有任务结束后才释放（0003「进程退出」）。
    async fn run<R, F>(&self, branch: Branch, f: F) -> Result<Response<R>, Status>
    where
        R: Reply,
        F: FnOnce(&Db) -> Result<R, EngineError> + Send + 'static,
    {
        let db = Arc::clone(&self.db);
        let joined = tokio::task::spawn_blocking(move || {
            let _task = db.enter()?;
            f(&db)
        })
        .await;
        let reply = match joined {
            Ok(Ok(reply)) => reply,
            Ok(Err(e)) => R::with_status(st::from_engine(&e, branch)?),
            // 对应 3.x servicer 对未预期异常的处理：code 1、error_code 0。
            Err(e) if e.is_panic() => R::with_status(st::unexpected(format!(
                "internal error: {}",
                panic_message(e.into_panic())
            ))),
            Err(_) => return Err(Status::cancelled("the server is shutting down")),
        };
        // 3.x（grpcio）对超过上限的响应返回 RESOURCE_EXHAUSTED；tonic 会报 OUT_OF_RANGE，因此在编码前检查。
        let len = reply.encoded_len();
        if len > MAX_MESSAGE_SIZE {
            return Err(Status::resource_exhausted(format!(
                "SERVER: Sent message larger than max ({len} vs. {MAX_MESSAGE_SIZE})"
            )));
        }
        Ok(Response::new(reply))
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(s) => *s,
        Err(payload) => payload
            .downcast_ref::<&str>()
            .map_or_else(|| "panic".to_owned(), |s| (*s).to_owned()),
    }
}

/// 3.x `_database_name`：请求中的 db_name 优先，其次是 metadata 中第一个非空的 `dbname`。
fn database<T>(request: &Request<T>, db_name: impl Fn(&T) -> &str) -> String {
    let field = db_name(request.get_ref());
    if !field.is_empty() {
        return field.to_owned();
    }
    request
        .metadata()
        .get_all("dbname")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| !v.is_empty())
        .unwrap_or_default()
        .to_owned()
}

fn success() -> common::Status {
    st::success()
}

#[tonic::async_trait]
impl Service for MilvusService {
    async fn connect(
        &self,
        _request: Request<svc::ConnectRequest>,
    ) -> Result<Response<svc::ConnectResponse>, Status> {
        // 字段取值与 3.x 的 servicer.Connect 相同。
        Ok(Response::new(svc::ConnectResponse {
            status: Some(success()),
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
        _request: Request<svc::GetVersionRequest>,
    ) -> Result<Response<svc::GetVersionResponse>, Status> {
        // 格式与 3.x 相同：`milvus_lite-<版本>`，pymilvus 与参照实现的用例据此识别 Milvus Lite。
        Ok(Response::new(svc::GetVersionResponse {
            status: Some(success()),
            version: format!("milvus_lite-{}", self.version),
        }))
    }

    async fn create_collection(
        &self,
        request: Request<svc::CreateCollectionRequest>,
    ) -> Result<Response<common::Status>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            // 3.x 先翻译 schema，再依次检查数据库、名字、schema 与重名。
            let mut schema = schema::decode(&req.schema)?;
            schema.properties = schema::properties(&req.properties);
            db.create_collection(&database, &req.collection_name, schema)?;
            Ok(success())
        })
        .await
    }

    async fn drop_collection(
        &self,
        request: Request<svc::DropCollectionRequest>,
    ) -> Result<Response<common::Status>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            db.drop_collection(&database, &req.collection_name)?;
            Ok(success())
        })
        .await
    }

    async fn describe_collection(
        &self,
        request: Request<svc::DescribeCollectionRequest>,
    ) -> Result<Response<svc::DescribeCollectionResponse>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            let handle = db.collection(&database, &req.collection_name)?;
            let c = handle.read();
            Ok(svc::DescribeCollectionResponse {
                status: Some(success()),
                schema: Some(schema::encode(c.name(), c.schema())),
                collection_name: c.name().to_owned(),
                shards_num: 1,
                num_partitions: 1,
                properties: c
                    .schema()
                    .properties
                    .iter()
                    .map(|(k, v)| schema::kv(k, v.clone()))
                    .collect(),
                ..Default::default()
            })
        })
        .await
    }

    async fn show_collections(
        &self,
        request: Request<svc::ShowCollectionsRequest>,
    ) -> Result<Response<svc::ShowCollectionsResponse>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        // 3.x 的 ShowCollections 只有 `except Exception` 分支。这两个字段已被 proto 标为废弃，但 3.x 仍在读写。
        #[allow(deprecated)]
        self.run(Branch::Generic, move |db| {
            let mut all = db.list_collections(&database)?;
            if !req.collection_names.is_empty() {
                all.retain(|(name, _)| req.collection_names.contains(name));
            }
            let percentages: Vec<i64> = all
                .iter()
                .map(|(_, state)| if *state == LoadState::Loaded { 100 } else { 0 })
                .collect();
            Ok(svc::ShowCollectionsResponse {
                status: Some(success()),
                query_service_available: percentages.iter().map(|p| *p == 100).collect(),
                shards_num: vec![1; all.len()],
                collection_names: all.into_iter().map(|(name, _)| name).collect(),
                in_memory_percentages: percentages,
                ..Default::default()
            })
        })
        .await
    }

    async fn get_collection_statistics(
        &self,
        request: Request<svc::GetCollectionStatisticsRequest>,
    ) -> Result<Response<svc::GetCollectionStatisticsResponse>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            let count = db
                .collection(&database, &req.collection_name)?
                .read()
                .count();
            Ok(svc::GetCollectionStatisticsResponse {
                status: Some(success()),
                stats: vec![schema::kv("row_count", count.to_string())],
            })
        })
        .await
    }

    async fn create_index(
        &self,
        request: Request<svc::CreateIndexRequest>,
    ) -> Result<Response<common::Status>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            let handle = db.collection(&database, &req.collection_name)?;
            let index_request = index::request(&req.extra_params)?;
            handle
                .write()
                .create_index(&req.field_name, index_request)?;
            Ok(success())
        })
        .await
    }

    async fn describe_index(
        &self,
        request: Request<svc::DescribeIndexRequest>,
    ) -> Result<Response<svc::DescribeIndexResponse>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            let handle = db.collection(&database, &req.collection_name)?;
            let c = handle.read();
            index::describe(&c, &req)
        })
        .await
    }

    async fn load_collection(
        &self,
        request: Request<svc::LoadCollectionRequest>,
    ) -> Result<Response<common::Status>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            db.collection(&database, &req.collection_name)?
                .write()
                .load();
            Ok(success())
        })
        .await
    }

    async fn release_collection(
        &self,
        request: Request<svc::ReleaseCollectionRequest>,
    ) -> Result<Response<common::Status>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            db.collection(&database, &req.collection_name)?
                .write()
                .release();
            Ok(success())
        })
        .await
    }

    async fn get_loading_progress(
        &self,
        request: Request<svc::GetLoadingProgressRequest>,
    ) -> Result<Response<svc::GetLoadingProgressResponse>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            let state = db
                .collection(&database, &req.collection_name)?
                .read()
                .load_state();
            Ok(svc::GetLoadingProgressResponse {
                status: Some(success()),
                progress: if state == LoadState::Loaded { 100 } else { 0 },
                ..Default::default()
            })
        })
        .await
    }

    async fn get_load_state(
        &self,
        request: Request<svc::GetLoadStateRequest>,
    ) -> Result<Response<svc::GetLoadStateResponse>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            // 3.x：collection 不存在时返回成功与 LoadStateNotExist。
            let state = match db.collection(&database, &req.collection_name) {
                Ok(handle) => match handle.read().load_state() {
                    LoadState::Loaded => PbLoadState::Loaded,
                    LoadState::Released => PbLoadState::NotLoad,
                },
                Err(EngineError::CollectionNotFound(_)) => PbLoadState::NotExist,
                Err(e) => return Err(e),
            };
            Ok(svc::GetLoadStateResponse {
                status: Some(success()),
                state: state as i32,
            })
        })
        .await
    }

    async fn insert(
        &self,
        request: Request<svc::InsertRequest>,
    ) -> Result<Response<svc::MutationResult>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            let handle = db.collection(&database, &req.collection_name)?;
            let batch = insert::decode(req.fields_data, req.num_rows)?;
            if batch.num_rows > 0 && !matches!(req.partition_name.as_str(), "" | "_default") {
                return Err(schema::unsupported("partitions other than '_default'"));
            }
            let mut c = handle.write();
            let varchar = c
                .schema()
                .fields
                .iter()
                .any(|f| f.is_primary && f.data_type == milvus_lite_core::DataType::VarChar);
            let pks = c.insert(batch)?;
            drop(c);
            let n = u32::try_from(pks.len()).unwrap_or(u32::MAX);
            Ok(svc::MutationResult {
                status: Some(success()),
                i_ds: Some(insert::ids(&pks, varchar)),
                succ_index: (0..n).collect(),
                insert_cnt: i64::from(n),
                ..Default::default()
            })
        })
        .await
    }

    async fn query(
        &self,
        request: Request<svc::QueryRequest>,
    ) -> Result<Response<svc::QueryResults>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            let handle = db.collection(&database, &req.collection_name)?;
            let c = handle.read();
            query::query(&c, req)
        })
        .await
    }

    async fn search(
        &self,
        request: Request<svc::SearchRequest>,
    ) -> Result<Response<svc::SearchResults>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            let handle = db.collection(&database, &req.collection_name)?;
            let c = handle.read();
            search::search(&c, req)
        })
        .await
    }
}
