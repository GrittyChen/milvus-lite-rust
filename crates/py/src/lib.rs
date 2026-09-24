//! Python 扩展 `milvus_lite._native`：只做胶水，启动与停止 Rust gRPC 服务。
//! 路径表、atexit 与 fork 钩子在 `milvus_lite/server_manager.py`（见 0003）。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use milvus_lite_grpc::{Server, ServerConfig};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

#[pyclass(frozen, module = "milvus_lite._native")]
struct NativeServer {
    server: Mutex<Server>,
    port: u16,
}

#[pymethods]
impl NativeServer {
    /// 打开数据目录并在 `host:port` 上启动服务（`port=0` 由系统分配）。失败抛 `RuntimeError`。
    #[staticmethod]
    fn start(
        py: Python<'_>,
        data_dir: PathBuf,
        host: String,
        port: u16,
        version: String,
    ) -> PyResult<Self> {
        let config = ServerConfig {
            data_dir,
            host,
            port,
            version,
        };
        let server = py
            .detach(|| Server::start(config))
            .map_err(|e| PyRuntimeError::new_err(error_chain(&e)))?;
        let port = server.port();
        Ok(Self {
            server: Mutex::new(server),
            port,
        })
    }

    #[getter]
    fn port(&self) -> u16 {
        self.port
    }

    /// 停止服务并释放 LOCK，最多等 `grace` 秒让在途请求结束。重复调用是空操作。
    fn stop(&self, py: Python<'_>, grace: f64) -> PyResult<()> {
        let grace = Duration::try_from_secs_f64(grace)
            .map_err(|e| PyValueError::new_err(format!("invalid grace {grace}: {e}")))?;
        py.detach(|| {
            let mut server = self
                .server
                .lock()
                .map_err(|_| PyRuntimeError::new_err("server state is poisoned"))?;
            server
                .stop(grace)
                .map_err(|e| PyRuntimeError::new_err(error_chain(&e)))
        })
    }
}

fn error_chain(err: &dyn std::error::Error) -> String {
    let mut msg = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        let s = e.to_string();
        if !msg.contains(&s) {
            msg.push_str(": ");
            msg.push_str(&s);
        }
        source = e.source();
    }
    msg
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<NativeServer>()?;
    Ok(())
}
