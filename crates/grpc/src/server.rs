//! 服务实例：独占 tokio runtime、数据目录与监听端口，谁创建谁回收（见 0003「所有权」）。

use std::net::TcpListener;
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
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
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
    running: Option<Running>,
}

#[derive(Debug)]
struct Running {
    runtime: Runtime,
    shutdown: oneshot::Sender<()>,
    serve: JoinHandle<Result<(), tonic::transport::Error>>,
    /// 引擎；它持有数据目录的 LOCK，关闭时在所有在途任务结束后释放。
    db: Arc<Db>,
}

impl Server {
    pub fn start(config: ServerConfig) -> Result<Self, ServerError> {
        let db = Db::new(DataDir::open(&config.data_dir)?, Compute::for_host()?)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("milvus-lite-rt")
            .build()
            .map_err(ServerError::Runtime)?;

        let addr = format!("{}:{}", config.host, config.port);
        let bind_err = |source| ServerError::Bind {
            addr: addr.clone(),
            source,
        };
        let listener = TcpListener::bind((config.host.as_str(), config.port)).map_err(bind_err)?;
        listener.set_nonblocking(true).map_err(bind_err)?;
        let port = listener.local_addr().map_err(bind_err)?.port();
        let listener = {
            let _guard = runtime.enter();
            tokio::net::TcpListener::from_std(listener).map_err(bind_err)?
        };
        // 自行 bind 时 tonic 的 tcp_nodelay 默认值不生效，这里显式开启，与 tonic 默认及 grpcio 一致，
        // 避免小响应受 Nagle 与延迟 ACK 叠加影响。
        let incoming = TcpIncoming::from(listener).with_nodelay(Some(true));

        let service = MilvusServiceServer::new(MilvusService::new(config.version, Arc::clone(&db)))
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
        runtime.shutdown_timeout(Duration::ZERO);
        // 关闭引擎：等上述引擎任务结束后才释放 LOCK；尚未开始的任务进入闸门时会被拒绝。
        db.close();
        match result {
            Some(Ok(Ok(()))) | None => Ok(()),
            Some(Ok(Err(e))) => Err(ServerError::Serve(e)),
            Some(Err(e)) => Err(ServerError::Task(e)),
        }
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
}
