//! 服务实例：独占 tokio runtime、数据目录与监听端口，谁创建谁回收（见 0003「所有权」）。

use std::net::{SocketAddr, TcpListener};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use std::sync::Arc;

use milvus_lite_core::{Compute, ComputeError, DataDir, DataDirError, Db};
use tokio::runtime::Runtime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::server::TcpIncoming;

use crate::pb::service::milvus_service_server::MilvusServiceServer;
use crate::service::MilvusService;
use crate::{MAX_MESSAGE_SIZE, guard};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub data_dir: PathBuf,
    pub host: String,
    /// 0 表示由系统分配空闲端口。
    pub port: u16,
    /// `GetVersion` 返回 `milvus_lite-<version>`。
    pub version: String,
    pub max_blocking_threads: Option<NonZeroUsize>,
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("invalid server thread configuration: {0}")]
    ThreadConfig(String),
    #[error("storage shutdown failed; see storage diagnostics for details")]
    StorageShutdown {
        #[source]
        transport: Option<Box<ServerError>>,
    },
    #[error(transparent)]
    Engine(#[from] milvus_lite_core::EngineError),
    #[error(transparent)]
    DataDir(#[from] DataDirError),
    #[error(transparent)]
    Compute(#[from] ComputeError),
    #[error("failed to start runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("failed to bind {addr}: {source}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },
    #[error("gRPC server failed: {0}")]
    Serve(#[source] tonic::transport::Error),
    #[error("gRPC server task failed: {0}")]
    Task(#[source] tokio::task::JoinError),
    #[error("server was created by process {owner}, not the current process {current}")]
    NotOwner { owner: u32, current: u32 },
}

#[derive(Debug)]
pub struct Server {
    owner_pid: u32,
    port: u16,
    bound_address: SocketAddr,
    running: Option<Running>,
    #[cfg(test)]
    after_runtime_shutdown: Option<ShutdownPause>,
}

#[derive(Debug)]
struct Running {
    runtime: Runtime,
    shutdown: oneshot::Sender<()>,
    serve: JoinHandle<Result<(), tonic::transport::Error>>,
    /// 引擎；它持有数据目录的 LOCK，关闭时在所有在途任务结束后释放。
    db: Arc<Db>,
}

fn parse_io_threads(
    value: Result<String, std::env::VarError>,
) -> Result<NonZeroUsize, ServerError> {
    match value {
        Ok(value) => value.parse::<NonZeroUsize>().map_err(|error| {
            ServerError::ThreadConfig(format!(
                "TOKIO_WORKER_THREADS must be a positive usize: {error}"
            ))
        }),
        Err(std::env::VarError::NotPresent) => {
            Ok(std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN))
        }
        Err(std::env::VarError::NotUnicode(_)) => Err(ServerError::ThreadConfig(
            "TOKIO_WORKER_THREADS must be valid Unicode".into(),
        )),
    }
}

#[cfg(test)]
#[derive(Debug)]
struct ShutdownPause {
    entered: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
impl ShutdownPause {
    #[allow(clippy::expect_used)] // 受控测试必须报告观察者断开，不能把未执行的交错算作通过。
    fn wait(self) {
        self.entered.send(()).expect("shutdown observer is alive");
        self.resume
            .recv_timeout(Duration::from_secs(10))
            .expect("shutdown observer releases pause");
    }
}

impl Server {
    pub fn start(config: ServerConfig) -> Result<Self, ServerError> {
        #[cfg(test)]
        {
            Self::start_inner(config, crate::service::RpcTestHooks::default())
        }
        #[cfg(not(test))]
        {
            Self::start_inner(config)
        }
    }

    fn start_inner(
        config: ServerConfig,
        #[cfg(test)] hooks: crate::service::RpcTestHooks,
    ) -> Result<Self, ServerError> {
        let io_threads = parse_io_threads(std::env::var("TOKIO_WORKER_THREADS"))?;
        let blocking_threads = config.max_blocking_threads.map_or(512, NonZeroUsize::get);
        io_threads
            .get()
            .checked_add(blocking_threads)
            .ok_or_else(|| {
                ServerError::ThreadConfig("combined worker count exceeds usize".into())
            })?;
        let db = Db::new(DataDir::open(&config.data_dir)?, Compute::for_host()?)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("milvus-lite-rt")
            .worker_threads(io_threads.get())
            .max_blocking_threads(blocking_threads)
            .build()
            .map_err(ServerError::Runtime)?;

        let addr = format!("{}:{}", config.host, config.port);
        let bind_err = |source| ServerError::Bind {
            addr: addr.clone(),
            source,
        };
        let listener = TcpListener::bind((config.host.as_str(), config.port)).map_err(bind_err)?;
        listener.set_nonblocking(true).map_err(bind_err)?;
        let bound_address = listener.local_addr().map_err(bind_err)?;
        let port = bound_address.port();
        let listener = {
            let _guard = runtime.enter();
            tokio::net::TcpListener::from_std(listener).map_err(bind_err)?
        };
        // 自行 bind 时 tonic 的 tcp_nodelay 默认值不生效，这里显式开启，与 tonic 默认及 grpcio 一致，
        // 避免小响应受 Nagle 与延迟 ACK 叠加影响。
        let incoming = TcpIncoming::from(listener).with_nodelay(Some(true));

        let service = MilvusService::new(config.version, Arc::clone(&db));
        #[cfg(test)]
        let service = {
            let mut service = service;
            service.test_hooks = hooks;
            service
        };
        let service = MilvusServiceServer::new(service)
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE);
        let (shutdown, shutdown_rx) = oneshot::channel::<()>();
        let serve = runtime.spawn(
            tonic::transport::Server::builder()
                .add_routes(guard::routes(service))
                .serve_with_incoming_shutdown(incoming, async {
                    // 发送端被丢弃同样意味着该停止了。
                    match shutdown_rx.await {
                        Ok(()) | Err(_) => {}
                    }
                }),
        );

        Ok(Self {
            owner_pid: std::process::id(),
            port,
            bound_address,
            #[cfg(test)]
            after_runtime_shutdown: None,
            running: Some(Running {
                runtime,
                shutdown,
                serve,
                db,
            }),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn bound_address(&self) -> SocketAddr {
        self.bound_address
    }

    #[cfg(feature = "fault-injection")]
    pub fn pause_compaction(
        &self,
        point: milvus_lite_core::CompactionPausePoint,
        entered: std::sync::mpsc::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) -> Result<(), ServerError> {
        self.running
            .as_ref()
            .ok_or(milvus_lite_core::EngineError::Closed)?
            .db
            .pause_compaction_once(point, entered, resume);
        Ok(())
    }

    #[cfg(feature = "fault-injection")]
    pub fn inject_compaction_fault(
        &self,
        fault: milvus_lite_core::CompactionFault,
    ) -> Result<(), ServerError> {
        self.running
            .as_ref()
            .ok_or(milvus_lite_core::EngineError::Closed)?
            .db
            .inject_compaction_fault_once(fault);
        Ok(())
    }

    #[cfg(feature = "fault-injection")]
    pub fn wait_for_maintenance(
        &self,
        name: &str,
        timeout: Duration,
    ) -> Result<milvus_lite_core::MaintenanceReport, ServerError> {
        if std::process::id() != self.owner_pid {
            return Err(ServerError::NotOwner {
                owner: self.owner_pid,
                current: std::process::id(),
            });
        }
        self.running
            .as_ref()
            .ok_or(milvus_lite_core::EngineError::Closed)?
            .db
            .wait_for_maintenance("", name, timeout)
            .map_err(ServerError::Engine)
    }

    /// 按 0003「进程退出」的顺序停止：停止接受新请求，最多等 `grace` 让在途请求结束，超时后取消；
    /// 再关闭引擎，完成最后一次 flush 并排空已接受的维护；grace 不限制整个停服时长；最后释放 LOCK。
    /// 重复调用是空操作。只能由创建者进程调用（见 0003「fork」）。
    pub fn stop(&mut self, grace: Duration) -> Result<(), ServerError> {
        let current = std::process::id();
        if current != self.owner_pid {
            return Err(ServerError::NotOwner {
                owner: self.owner_pid,
                current,
            });
        }
        let Some(Running {
            runtime,
            shutdown,
            mut serve,
            db,
        }) = self.running.take()
        else {
            return Ok(());
        };
        // 接收端已不在说明服务任务已经结束，它的结果在下面取回。
        match shutdown.send(()) {
            Ok(()) | Err(()) => {}
        }
        let result = runtime.block_on(async {
            match tokio::time::timeout(grace, &mut serve).await {
                Ok(joined) => Some(joined),
                Err(_elapsed) => {
                    serve.abort();
                    None
                }
            }
        });
        // 宽限已在上面用完。tonic 为每条连接单独 spawn 任务，中止 serve 不会关闭已建立的连接，
        // 因此立即关闭 runtime：丢弃连接与请求的任务，客户端随即收到错误。已经开始的阻塞任务
        // （引擎调用）不可取消，会在阻塞线程上继续运行到结束。
        db.begin_shutdown();
        runtime.shutdown_timeout(Duration::ZERO);
        #[cfg(test)]
        if let Some(pause) = self.after_runtime_shutdown.take() {
            pause.wait();
        }
        // 关闭引擎：等上述引擎任务结束后才释放 LOCK；尚未开始的任务进入闸门时会被拒绝。
        db.close();
        let transport_result = match result {
            Some(Ok(Ok(()))) | None => Ok(()),
            Some(Ok(Err(e))) => Err(ServerError::Serve(e)),
            Some(Err(e)) => Err(ServerError::Task(e)),
        };
        if db.shutdown_failed() {
            // 原 transport 原因进入错误链，不能在报告收尾失败时丢掉它。
            return Err(ServerError::StorageShutdown {
                transport: transport_result.err().map(Box::new),
            });
        }
        transport_result
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        if std::process::id() != self.owner_pid {
            // fork 出的子进程：runtime 的线程并不存在，LOCK 与数据文件属于父进程。
            // 不 shutdown、不 join、不关 fd，直接泄漏（见 0003「所有权」）。
            std::mem::forget(running);
            return;
        }
        // 析构不得阻塞：通知停止后让 runtime 在后台关闭。正常路径会先显式调用 stop。
        match running.shutdown.send(()) {
            Ok(()) | Err(()) => {}
        }
        // 后台线程等在途任务结束、同步保留的 WAL，最后才释放 LOCK。
        running.db.close_nowait();
        running.runtime.shutdown_background();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use milvus_lite_core::{EngineError, LoadState};

    fn config(data_dir: PathBuf, port: u16) -> ServerConfig {
        ServerConfig {
            data_dir,
            host: "127.0.0.1".to_owned(),
            port,
            version: "test".to_owned(),
            max_blocking_threads: None,
        }
    }

    #[test]
    fn stop_is_idempotent_and_releases_lock() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("x.db");
        let mut server = Server::start(config(path.clone(), 0)).unwrap();
        assert_ne!(server.port(), 0);
        assert!(matches!(
            DataDir::open(&path),
            Err(DataDirError::Locked { .. })
        ));
        server.stop(Duration::from_secs(2)).unwrap();
        server.stop(Duration::from_secs(2)).unwrap();
        DataDir::open(&path).unwrap();
    }

    #[test]
    fn bind_failure_releases_lock() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("x.db");
        let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = occupied.local_addr().unwrap().port();
        let err = Server::start(config(path.clone(), port)).unwrap_err();
        assert!(matches!(err, ServerError::Bind { .. }), "{err}");
        DataDir::open(&path).unwrap();
    }

    #[test]
    fn existing_data_is_checked_when_a_collection_is_accessed() {
        let root = tempfile::tempdir().unwrap();
        let schema = root.path().join("collections/docs/schema.json");
        std::fs::create_dir_all(schema.parent().unwrap()).unwrap();
        std::fs::write(&schema, b"{").unwrap();
        let mut server = Server::start(config(root.path().to_path_buf(), 0)).unwrap();
        let db = &server.running.as_ref().unwrap().db;
        assert_eq!(
            db.list_collections("").unwrap(),
            [("docs".to_owned(), LoadState::Released)]
        );
        assert!(matches!(
            db.collection("", "docs"),
            Err(EngineError::Storage(_))
        ));
        assert_eq!(std::fs::read(&schema).unwrap(), b"{");
        server.stop(Duration::from_secs(2)).unwrap();
        DataDir::open(root.path()).unwrap();
    }

    #[test]
    fn drop_without_stop_releases_lock() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("x.db");
        drop(Server::start(config(path.clone(), 0)).unwrap());
        let start = std::time::Instant::now();
        loop {
            match DataDir::open(&path) {
                Ok(_) => break,
                Err(DataDirError::Locked { .. }) if start.elapsed() < Duration::from_secs(2) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("storage close did not release LOCK: {error}"),
            }
        }
    }
    fn io_client() -> Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    async fn load_state(
        channel: tonic::transport::Channel,
    ) -> Result<crate::pb::service::GetLoadStateResponse, tonic::Status> {
        let mut client = tonic::client::Grpc::new(channel);
        client.ready().await.unwrap();
        client
            .unary(
                tonic::Request::new(crate::pb::service::GetLoadStateRequest {
                    collection_name: "missing".into(),
                    ..Default::default()
                }),
                tonic::codegen::http::uri::PathAndQuery::from_static(
                    "/milvus.proto.milvus.MilvusService/GetLoadState",
                ),
                tonic_prost::ProstCodec::default(),
            )
            .await
            .map(tonic::Response::into_inner)
    }

    fn insertion(id: i64) -> crate::pb::service::InsertRequest {
        use crate::pb::schema::*;
        crate::pb::service::InsertRequest {
            collection_name: "c".into(),
            num_rows: 1,
            fields_data: vec![
                FieldData {
                    field_name: "id".into(),
                    r#type: 5,
                    field: Some(field_data::Field::Scalars(ScalarField {
                        data: Some(scalar_field::Data::LongData(LongArray { data: vec![id] })),
                    })),
                    ..Default::default()
                },
                FieldData {
                    field_name: "v".into(),
                    r#type: 101,
                    field: Some(field_data::Field::Vectors(VectorField {
                        dim: 2,
                        data: Some(vector_field::Data::FloatVector(FloatArray {
                            data: vec![1.0, 0.0],
                        })),
                    })),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    async fn insert_rpc(
        channel: tonic::transport::Channel,
        id: i64,
    ) -> Result<crate::pb::service::MutationResult, tonic::Status> {
        let mut client = tonic::client::Grpc::new(channel);
        client.ready().await.unwrap();
        client
            .unary(
                tonic::Request::new(insertion(id)),
                tonic::codegen::http::uri::PathAndQuery::from_static(
                    "/milvus.proto.milvus.MilvusService/Insert",
                ),
                tonic_prost::ProstCodec::default(),
            )
            .await
            .map(tonic::Response::into_inner)
    }

    fn schema() -> milvus_lite_core::CollectionSchema {
        use milvus_lite_core::{CollectionSchema, DataType, FieldSchema};
        let mut primary = FieldSchema::new("id", DataType::Int64);
        primary.is_primary = true;
        let mut vector = FieldSchema::new("v", DataType::FloatVector);
        vector.dim = Some(2);
        CollectionSchema {
            fields: vec![primary, vector],
            enable_dynamic_field: false,
            properties: vec![],
        }
    }

    #[test]
    fn thread_configuration_rejects_invalid_values_before_creating_resources() {
        for value in ["0", "-1", "bad", "", "184467440737095516160000"] {
            assert!(matches!(
                parse_io_threads(Ok(value.into())),
                Err(ServerError::ThreadConfig(_))
            ));
        }
        assert_eq!(parse_io_threads(Ok("3".into())).unwrap().get(), 3);
        assert!(
            parse_io_threads(Err(std::env::VarError::NotPresent))
                .unwrap()
                .get()
                > 0
        );
        assert!(parse_io_threads(Err(std::env::VarError::NotUnicode("invalid".into()))).is_err());
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("invalid.db");
        let mut cfg = config(path.clone(), 0);
        cfg.max_blocking_threads = NonZeroUsize::new(usize::MAX);
        assert!(matches!(
            Server::start(cfg),
            Err(ServerError::ThreadConfig(_))
        ));
        assert!(!path.exists());
    }

    #[test]
    fn configured_workers_bound_actual_rpc_processing_without_changing_compute() {
        use std::sync::{Mutex, mpsc};
        for n in [1, 3] {
            let root = tempfile::tempdir().unwrap();
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (queued_tx, queued_rx) = mpsc::channel();
            let release = Mutex::new(release_rx);
            let hooks = crate::service::RpcTestHooks {
                after_attempt: Some(Arc::new(move |admitted| {
                    assert!(admitted);
                    entered_tx.send(()).unwrap();
                    release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                })),
                after_enqueue: Some(Arc::new(move || queued_tx.send(()).unwrap())),
            };
            let mut cfg = config(root.path().to_path_buf(), 0);
            cfg.max_blocking_threads = NonZeroUsize::new(n);
            let mut server = Server::start_inner(cfg, hooks).unwrap();
            assert_eq!(server.bound_address().port(), server.port());
            assert_eq!(
                server.running.as_ref().unwrap().db.compute().threads(),
                std::thread::available_parallelism().map_or(1, NonZeroUsize::get)
            );
            let clients = io_client();
            let channel = clients
                .block_on(
                    tonic::transport::Endpoint::from_shared(format!(
                        "http://{}",
                        server.bound_address()
                    ))
                    .unwrap()
                    .connect(),
                )
                .unwrap();
            let jobs: Vec<_> = (0..n + 2)
                .map(|_| clients.spawn(load_state(channel.clone())))
                .collect();
            for _ in 0..n + 2 {
                queued_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            for _ in 0..n {
                entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            assert!(matches!(
                entered_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            for _ in 0..n + 2 {
                release_tx.send(()).unwrap();
            }
            for job in jobs {
                assert_eq!(
                    clients.block_on(job).unwrap().unwrap().status.unwrap().code,
                    0
                );
            }
            server.stop(Duration::from_secs(2)).unwrap();
        }
    }

    #[test]
    fn stop_retains_lock_until_admitted_rpc_finishes_after_runtime_shutdown() {
        use std::sync::{Mutex, mpsc};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("inflight.db");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release = Mutex::new(release_rx);
        let hooks = crate::service::RpcTestHooks {
            after_attempt: Some(Arc::new(move |admitted| {
                assert!(admitted);
                entered_tx.send(()).unwrap();
                release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
            })),
            after_enqueue: None,
        };
        let mut server = Server::start_inner(config(path.clone(), 0), hooks).unwrap();
        let clients = io_client();
        let channel = clients
            .block_on(
                tonic::transport::Endpoint::from_shared(format!(
                    "http://{}",
                    server.bound_address()
                ))
                .unwrap()
                .connect(),
            )
            .unwrap();
        let rpc = clients.spawn(load_state(channel));
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let (gap_tx, gap_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        server.after_runtime_shutdown = Some(ShutdownPause {
            entered: gap_tx,
            resume: finish_rx,
        });
        let (closed_tx, closed_rx) = mpsc::channel();
        let closer = std::thread::spawn(move || {
            let result = server.stop(Duration::from_millis(30));
            closed_tx.send(()).unwrap();
            result
        });
        gap_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        finish_tx.send(()).unwrap();
        let waiting = matches!(
            closed_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        let locked = matches!(
            DataDir::open(&path),
            Err(milvus_lite_core::DataDirError::Locked { .. })
        );
        // 先放行并回收线程，再断言失败，避免错误实现使测试自己留下后台任务。
        release_tx.send(()).unwrap();
        closer.join().unwrap().unwrap();
        assert!(clients.block_on(rpc).unwrap().is_err());
        assert!(waiting);
        assert!(locked);
        DataDir::open(&path).unwrap();
    }

    #[test]
    fn shutdown_closes_admission_before_runtime_and_rejects_queued_writes() {
        use std::sync::{Mutex, mpsc};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.db");
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (queued_tx, queued_rx) = mpsc::channel();
        let release = Mutex::new(release_rx);
        let hooks = crate::service::RpcTestHooks {
            after_attempt: Some(Arc::new(move |admitted| {
                attempt_tx.send(admitted).unwrap();
                if admitted {
                    release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                }
            })),
            after_enqueue: Some(Arc::new(move || queued_tx.send(()).unwrap())),
        };
        let mut cfg = config(path.clone(), 0);
        cfg.max_blocking_threads = NonZeroUsize::new(1);
        let mut server = Server::start_inner(cfg, hooks).unwrap();
        let db = Arc::clone(&server.running.as_ref().unwrap().db);
        db.create_collection("", "c", schema()).unwrap();
        let clients = io_client();
        let channel = clients
            .block_on(
                tonic::transport::Endpoint::from_shared(format!(
                    "http://{}",
                    server.bound_address()
                ))
                .unwrap()
                .connect(),
            )
            .unwrap();
        let first = clients.spawn(insert_rpc(channel.clone(), 1));
        assert!(attempt_rx.recv_timeout(Duration::from_secs(10)).unwrap());
        queued_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let second = clients.spawn(insert_rpc(channel, 2));
        queued_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let (gap_tx, gap_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        server.after_runtime_shutdown = Some(ShutdownPause {
            entered: gap_tx,
            resume: finish_rx,
        });
        let closer = std::thread::spawn(move || server.stop(Duration::from_millis(30)));
        gap_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let closed = matches!(db.enter(), Err(EngineError::Closed));
        // 失败断言前也要释放受控线程，不能使测试从失败退化为永久挂起。
        release_tx.send(()).unwrap();
        let denied = attempt_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        if denied {
            release_tx.send(()).unwrap();
        }
        finish_tx.send(()).unwrap();
        closer.join().unwrap().unwrap();
        assert!(closed);
        assert!(!denied);
        assert!(clients.block_on(first).unwrap().is_err());
        assert!(clients.block_on(second).unwrap().is_err());
        assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
        DataDir::open(&path).unwrap();
        let reopened =
            Db::new(DataDir::open(&path).unwrap(), Compute::for_host().unwrap()).unwrap();
        let handle = reopened.collection("", "c").unwrap();
        let rows = handle
            .read()
            .get(
                &[
                    milvus_lite_core::Pk::Int64(1),
                    milvus_lite_core::Pk::Int64(2),
                ],
                &[],
            )
            .unwrap()
            .rows;
        assert_eq!(rows.pks, vec![milvus_lite_core::Pk::Int64(1)]);
        assert_eq!(
            std::fs::read_to_string(path.join("collections/c/manifest.json"))
                .unwrap()
                .parse::<serde_json::Value>()
                .unwrap()["current_seq"],
            1
        );
        reopened.close();
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn storage_close_failure_is_reported_after_releasing_lock_and_keeps_rows() {
        use milvus_lite_core::{InputValues, InsertBatch, Pk};
        for fault in 0..4 {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("failed.db");
            let mut server = Server::start(config(path.clone(), 0)).unwrap();
            let db = Arc::clone(&server.running.as_ref().unwrap().db);
            db.create_collection("", "c", schema()).unwrap();
            db.insert(
                "",
                "c",
                InsertBatch {
                    num_rows: 1,
                    columns: vec![
                        ("id".into(), InputValues::Int(vec![1])),
                        (
                            "v".into(),
                            InputValues::FloatVector {
                                dim: 2,
                                values: vec![1.0, 0.0],
                            },
                        ),
                    ],
                },
            )
            .unwrap();
            match fault {
                0 => milvus_lite_core::db::inject_close_finish_panic_once(),
                1 => milvus_lite_core::storage::runtime::inject_flush_prepare_panic_once(),
                2 => milvus_lite_core::storage::fs_commit::inject_manifest_dir_sync_failure_once(),
                3 => milvus_lite_core::storage::runtime::inject_retained_wal_sync_failure_once(
                    milvus_lite_core::storage::runtime::RetainedWalSyncFault::Data,
                ),
                _ => unreachable!(),
            }
            assert!(matches!(
                server.stop(Duration::from_secs(2)),
                Err(ServerError::StorageShutdown { .. })
            ));
            assert!(db.shutdown_failed());
            assert!(db.enter().is_err());
            assert!(server.stop(Duration::ZERO).is_ok());
            let reopened =
                Db::new(DataDir::open(&path).unwrap(), Compute::for_host().unwrap()).unwrap();
            assert_eq!(
                reopened
                    .collection("", "c")
                    .unwrap()
                    .read()
                    .get(&[Pk::Int64(1)], &[])
                    .unwrap()
                    .rows
                    .pks,
                vec![Pk::Int64(1)]
            );
            reopened.close();
        }
    }
}
