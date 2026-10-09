//! Python 扩展 `milvus_lite._native`：只做胶水，启动与停止 Rust gRPC 服务。
//! 路径表、atexit 与 fork 钩子在 `milvus_lite/server_manager.py`（见 0003）。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use milvus_lite_grpc::{Server, ServerConfig};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

#[cfg(feature = "fault-injection")]
type PauseChannels = (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>);

#[pyclass(frozen, module = "milvus_lite._native")]
struct NativeServer {
    server: Mutex<Server>,
    port: u16,
    #[cfg(feature = "fault-injection")]
    compaction_pause: Mutex<Option<PauseChannels>>,
}

#[pymethods]
impl NativeServer {
    #[cfg(feature = "fault-injection")]
    fn _pause_compaction(&self, point: &str) -> PyResult<()> {
        let point = match point {
            "before_prepare" => milvus_lite_grpc::CompactionPausePoint::BeforePrepare,
            "before_commit" => milvus_lite_grpc::CompactionPausePoint::BeforeCommit,
            "after_replace" => milvus_lite_grpc::CompactionPausePoint::AfterReplace,
            _ => return Err(PyValueError::new_err("unknown compaction pause point")),
        };
        let (entered, arrived) = std::sync::mpsc::channel();
        let (resume, continued) = std::sync::mpsc::channel();
        let mut pause = self
            .compaction_pause
            .lock()
            .map_err(|_| PyRuntimeError::new_err("compaction pause state is poisoned"))?;
        if pause.is_some() {
            return Err(PyRuntimeError::new_err(
                "compaction pause already installed",
            ));
        }
        self.server
            .lock()
            .map_err(|_| PyRuntimeError::new_err("server state is poisoned"))?
            .pause_compaction(point, entered, continued)
            .map_err(|e| PyRuntimeError::new_err(error_chain(&e)))?;
        *pause = Some((arrived, resume));
        Ok(())
    }

    #[cfg(feature = "fault-injection")]
    fn _wait_compaction_pause(&self, py: Python<'_>, timeout: f64) -> PyResult<()> {
        let timeout = Duration::try_from_secs_f64(timeout)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        py.detach(|| {
            self.compaction_pause
                .lock()
                .map_err(|_| PyRuntimeError::new_err("compaction pause state is poisoned"))?
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("compaction pause not installed"))?
                .0
                .recv_timeout(timeout)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }

    #[cfg(feature = "fault-injection")]
    fn _resume_compaction(&self) -> PyResult<()> {
        self.compaction_pause
            .lock()
            .map_err(|_| PyRuntimeError::new_err("compaction pause state is poisoned"))?
            .take()
            .ok_or_else(|| PyRuntimeError::new_err("compaction pause not installed"))?
            .1
            .send(())
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }

    #[cfg(feature = "fault-injection")]
    fn _inject_compaction_fault(&self, fault: &str) -> PyResult<()> {
        use milvus_lite_grpc::CompactionFault as F;
        let fault = match fault {
            "prepare_panic" => F::PreparePanic,
            "before_replace_panic" => F::BeforeReplacePanic,
            "after_replace_panic" => F::AfterReplacePanic,
            "directory_sync_io" => F::DirectorySyncIo,
            "publish_panic" => F::PublishPanic,
            "cleanup_panic" => F::CleanupPanic,
            "exit_before_replace" => F::ExitBeforeReplace,
            "exit_after_replace" => F::ExitAfterReplace,
            _ => return Err(PyValueError::new_err("unknown compaction fault")),
        };
        self.server
            .lock()
            .map_err(|_| PyRuntimeError::new_err("server state is poisoned"))?
            .inject_compaction_fault(fault)
            .map_err(|e| PyRuntimeError::new_err(error_chain(&e)))
    }
    #[cfg(feature = "fault-injection")]
    fn _wait_for_maintenance<'py>(
        &self,
        py: Python<'py>,
        name: &str,
        timeout: f64,
    ) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
        use pyo3::types::PyDictMethods;
        let timeout = Duration::try_from_secs_f64(timeout)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let report = py.detach(|| {
            self.server
                .lock()
                .map_err(|_| PyRuntimeError::new_err("server state is poisoned"))?
                .wait_for_maintenance(name, timeout)
                .map_err(|e| PyRuntimeError::new_err(error_chain(&e)))
        })?;
        let result = pyo3::types::PyDict::new(py);
        result.set_item("accepted", report.accepted)?;
        result.set_item("completed", report.completed)?;
        result.set_item("failed", report.failed)?;
        result.set_item("cancelled", report.cancelled)?;
        result.set_item("last_input_rows", report.last_input_rows)?;
        result.set_item("max_round_seconds", report.max_round_seconds)?;
        result.set_item("last_failure", report.last_failure)?;
        Ok(result)
    }
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
            #[cfg(feature = "fault-injection")]
            compaction_pause: Mutex::new(None),
        })
    }

    #[getter]
    fn port(&self) -> u16 {
        self.port
    }

    /// grace 只限制 RPC 宽限；正常停服还会等待引擎与后台维护排空后释放 LOCK。
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
