//! `MilvusService` 的实现：只翻译，业务语义在引擎层。各 RPC 的请求解析、检查顺序与响应字段都与 3.x 的
//! `servicer.py` 一致；与 3.x 不同之处见 0004「决策」。

use std::sync::Arc;

use milvus_lite_core::{Db, EngineError, LoadState};
use tonic::{Request, Response, Status};

use crate::pb::common::{self, LoadState as PbLoadState};
use crate::pb::service::{self as svc, milvus_service_server::MilvusService as Service};
use crate::status::{self as st, Branch, Reply};
use crate::{MAX_MESSAGE_SIZE, index, insert, query, schema, search};

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct RpcTestHooks {
    pub after_attempt: Option<Arc<dyn Fn(bool) + Send + Sync>>,
    pub after_enqueue: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub(crate) struct MilvusService {
    version: String,
    db: Arc<Db>,
    #[cfg(test)]
    after_search_snapshot: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    pub(crate) test_hooks: RpcTestHooks,
}

impl MilvusService {
    pub(crate) fn new(version: String, db: Arc<Db>) -> Self {
        Self {
            version,
            db,
            #[cfg(test)]
            after_search_snapshot: None,
            #[cfg(test)]
            test_hooks: RpcTestHooks::default(),
        }
    }

    /// 在阻塞线程池中执行一个 RPC 的处理体（解码、引擎调用、编码都是 CPU 密集或可能等锁），
    /// 期间持有引擎的在途任务，保证停止服务时 LOCK 在所有任务结束后才释放（0003「进程退出」）。
    async fn run<R, F>(&self, branch: Branch, f: F) -> Result<Response<R>, Status>
    where
        R: Reply,
        F: FnOnce(&Db) -> Result<R, EngineError> + Send + 'static,
    {
        let db = Arc::clone(&self.db);
        #[cfg(test)]
        let after_attempt = self.test_hooks.after_attempt.clone();
        let task = tokio::task::spawn_blocking(move || {
            let admitted = db.enter();
            #[cfg(test)]
            if let Some(hook) = after_attempt {
                hook(admitted.is_ok());
            }
            let _task = admitted?;
            f(&db)
        });
        #[cfg(test)]
        if let Some(hook) = &self.test_hooks.after_enqueue {
            hook();
        }
        let joined = task.await;
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
    async fn manual_compaction(
        &self,
        _request: Request<svc::ManualCompactionRequest>,
    ) -> Result<Response<svc::ManualCompactionResponse>, Status> {
        self.run(Branch::Engine, |_| {
            Ok(svc::ManualCompactionResponse {
                status: Some(st::success()),
                ..Default::default()
            })
        })
        .await
    }

    async fn get_compaction_state(
        &self,
        _request: Request<svc::GetCompactionStateRequest>,
    ) -> Result<Response<svc::GetCompactionStateResponse>, Status> {
        self.run(Branch::Engine, |_| {
            Ok(svc::GetCompactionStateResponse {
                status: Some(st::success()),
                state: common::CompactionState::Completed as i32,
                ..Default::default()
            })
        })
        .await
    }
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
                schema: Some(schema::encode(c.name(), c.schema())?),
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
            let index_request = index::request(&req.extra_params)?;
            db.create_index(
                &database,
                &req.collection_name,
                &req.field_name,
                index_request,
            )?;
            Ok(success())
        })
        .await
    }

    async fn drop_index(
        &self,
        request: Request<svc::DropIndexRequest>,
    ) -> Result<Response<common::Status>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            db.drop_index(
                &database,
                &req.collection_name,
                &req.field_name,
                &req.index_name,
            )?;
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
                .load()?;
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
            let _handle = db.collection(&database, &req.collection_name)?;
            let batch = insert::decode(req.fields_data, req.num_rows)?;
            if batch.num_rows > 0 && !matches!(req.partition_name.as_str(), "" | "_default") {
                return Err(schema::unsupported("partitions other than '_default'"));
            }
            let outcome = db.insert_with_pk_type(&database, &req.collection_name, batch)?;
            let pks = outcome.pks;
            let n = u32::try_from(pks.len()).unwrap_or(u32::MAX);
            Ok(svc::MutationResult {
                status: Some(success()),
                i_ds: Some(insert::ids(&pks, outcome.varchar)),
                succ_index: (0..n).collect(),
                insert_cnt: i64::from(n),
                ..Default::default()
            })
        })
        .await
    }

    async fn delete(
        &self,
        request: Request<svc::DeleteRequest>,
    ) -> Result<Response<svc::MutationResult>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            let (pks, varchar) = db.delete_expr(
                &database,
                &req.collection_name,
                &req.expr,
                &req.partition_name,
            )?;
            Ok(svc::MutationResult {
                status: Some(success()),
                i_ds: Some(insert::ids(&pks, varchar)),
                delete_cnt: i64::try_from(pks.len()).unwrap_or(i64::MAX),
                ..Default::default()
            })
        })
        .await
    }

    async fn upsert(
        &self,
        request: Request<svc::UpsertRequest>,
    ) -> Result<Response<svc::MutationResult>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            let _handle = db.collection(&database, &req.collection_name)?;
            if !req.field_ops.is_empty() {
                return Err(schema::unsupported("field_ops"));
            }
            let batch = insert::decode(req.fields_data, req.num_rows)?;
            if batch.num_rows > 0 && !matches!(req.partition_name.as_str(), "" | "_default") {
                return Err(schema::unsupported("partitions other than '_default'"));
            }
            let outcome = db.upsert_with_pk_type(&database, &req.collection_name, batch)?;
            let pks = outcome.pks;
            let n = u32::try_from(pks.len()).unwrap_or(u32::MAX);
            Ok(svc::MutationResult {
                status: Some(success()),
                i_ds: Some(insert::ids(&pks, outcome.varchar)),
                succ_index: (0..n).collect(),
                upsert_cnt: i64::from(n),
                ..Default::default()
            })
        })
        .await
    }

    async fn flush(
        &self,
        request: Request<svc::FlushRequest>,
    ) -> Result<Response<svc::FlushResponse>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            for name in &req.collection_names {
                match db.flush_collection(&database, name) {
                    Err(EngineError::CollectionNotFound(_)) => continue,
                    result => result?,
                }
            }
            Ok(svc::FlushResponse {
                status: Some(success()),
                ..Default::default()
            })
        })
        .await
    }

    async fn get_flush_state(
        &self,
        _request: Request<svc::GetFlushStateRequest>,
    ) -> Result<Response<svc::GetFlushStateResponse>, Status> {
        Ok(Response::new(svc::GetFlushStateResponse {
            status: Some(success()),
            flushed: true,
        }))
    }

    async fn query(
        &self,
        request: Request<svc::QueryRequest>,
    ) -> Result<Response<svc::QueryResults>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        self.run(Branch::Engine, move |db| {
            let handle = db.collection(&database, &req.collection_name)?;
            query::query(&handle, req)
        })
        .await
    }

    async fn search(
        &self,
        request: Request<svc::SearchRequest>,
    ) -> Result<Response<svc::SearchResults>, Status> {
        let database = database(&request, |r| &r.db_name);
        let req = request.into_inner();
        #[cfg(test)]
        let after_snapshot = self.after_search_snapshot.clone();
        self.run(Branch::Engine, move |db| {
            let handle = db.collection(&database, &req.collection_name)?;
            // 解析（包括解码查询向量）不需要 collection 的状态，放在取读锁之前以缩短持锁时间；
            // collection 不存在的错误仍然最先报出（3.x 相同）。
            let parsed = search::parse(&req)?;
            let snapshot = {
                let guard = handle.read();
                guard.read_snapshot()
            };
            #[cfg(test)]
            if let Some(pause) = after_snapshot {
                pause();
            }
            search::search(&snapshot, db.compute(), parsed, req.output_fields)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use milvus_lite_core::{Compute, DataDir};

    use super::*;

    #[test]
    fn search_rpc_releases_state_lock_before_computing_and_encoding() {
        use milvus_lite_core::{CollectionSchema, DataType, FieldSchema, InputValues, InsertBatch};
        use prost::Message;
        use std::sync::mpsc;
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        let db = Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap(),
        )
        .unwrap();
        let mut id = FieldSchema::new("id", DataType::Int64);
        id.is_primary = true;
        let mut v = FieldSchema::new("v", DataType::FloatVector);
        v.dim = Some(2);
        db.create_collection(
            "",
            "c",
            CollectionSchema {
                fields: vec![id, v],
                enable_dynamic_field: false,
                properties: vec![],
            },
        )
        .unwrap();
        db.insert(
            "",
            "c",
            InsertBatch {
                num_rows: 2,
                columns: vec![
                    ("id".into(), InputValues::Int(vec![1, 2])),
                    (
                        "v".into(),
                        InputValues::FloatVector {
                            dim: 2,
                            values: vec![1.0, 0.0, 0.0, 1.0],
                        },
                    ),
                ],
            },
        )
        .unwrap();
        let mut service = MilvusService::new("test".into(), Arc::clone(&db));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let resume_rx = std::sync::Mutex::new(resume_rx);
        service.after_search_snapshot = Some(Arc::new(move || {
            ready_tx.send(()).unwrap();
            resume_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }));
        let reader = std::thread::spawn(move || {
            let req = svc::SearchRequest {
                collection_name: "c".into(),
                search_input: Some(svc::search_request::SearchInput::PlaceholderGroup(
                    common::PlaceholderGroup {
                        placeholders: vec![common::PlaceholderValue {
                            tag: "$0".into(),
                            r#type: 101,
                            element_level: false,
                            values: vec![
                                [1.0_f32, 0.0]
                                    .into_iter()
                                    .flat_map(f32::to_le_bytes)
                                    .collect(),
                            ],
                        }],
                    }
                    .encode_to_vec(),
                )),
                search_params: vec![common::KeyValuePair {
                    key: "topk".into(),
                    value: "2".into(),
                }],
                ..Default::default()
            };
            let reply = tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(service.search(Request::new(req)))
                .unwrap()
                .into_inner();
            assert_eq!(reply.status.unwrap().code, 0);
            let result = reply.results.unwrap();
            assert_eq!(result.scores, [1.0, 0.0]);
            assert_eq!(
                result.ids.unwrap().id_field,
                Some(crate::pb::schema::i_ds::IdField::IntId(
                    crate::pb::schema::LongArray { data: vec![1, 2] }
                ))
            );
        });
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let writer_db = Arc::clone(&db);
        let (done_tx, done_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            writer_db.drop_collection("", "c").unwrap();
            done_tx.send(()).unwrap();
        });
        let completed = done_rx.recv_timeout(Duration::from_secs(10));
        resume_tx.send(()).unwrap();
        reader.join().unwrap();
        writer.join().unwrap();
        completed.unwrap();
        db.close();
    }

    /// 0005「验证」叶子任务 panic 的后半段：处理体中的 panic（包括计算池从叶子任务传回的 panic，
    /// 见 core 的 compute 用例）映射成 code 1、error_code 0，之后服务照常可用。
    #[test]
    // error_code 在 proto 中已废弃，但 3.x 与 pymilvus 仍读取它（status.rs）。
    #[allow(deprecated)]
    fn a_panic_in_the_body_maps_to_code_1_and_the_service_keeps_working() {
        let root = tempfile::tempdir().unwrap();
        let compute = Compute::new(std::num::NonZeroUsize::MIN).unwrap();
        let db = Db::new(DataDir::open(root.path()).unwrap(), compute).unwrap();
        let service = MilvusService::new("test".to_owned(), db);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let reply = rt
            .block_on(service.run::<common::Status, _>(Branch::Engine, |_| panic!("leaf 17")))
            .unwrap()
            .into_inner();
        assert_eq!(
            (reply.code, reply.error_code, reply.reason.as_str()),
            (1, 0, "internal error: leaf 17")
        );
        let reply = rt
            .block_on(service.run(Branch::Engine, |_| Ok(success())))
            .unwrap()
            .into_inner();
        assert_eq!(reply.code, 0);
    }
}
