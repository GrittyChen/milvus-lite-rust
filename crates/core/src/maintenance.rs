//! 每个 Db 的维护队列；一次触发消费一轮，线程不持有常驻 Db 强引用。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::db::{CollectionHandle, Db};
use crate::error::{EngineError, Result};
use crate::lock_order::Mutex as OrderedMutex;

#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionPausePoint {
    BeforePrepare,
    BeforeCommit,
    AfterReplace,
}

#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, Copy)]
pub enum CompactionFault {
    PreparePanic,
    BeforeReplacePanic,
    AfterReplacePanic,
    DirectorySyncIo,
    PublishPanic,
    CleanupPanic,
    ExitBeforeReplace,
    ExitAfterReplace,
}

#[cfg(feature = "fault-injection")]
thread_local! { static FAIL_START: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(feature = "fault-injection")]
pub fn inject_start_failure_once() {
    FAIL_START.with(|v| v.set(true));
}

#[cfg(feature = "fault-injection")]
type PauseHook = (
    CompactionPausePoint,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

#[derive(Debug, Clone, Default)]
pub struct MaintenanceReport {
    #[cfg(feature = "fault-injection")]
    pub accepted: u64,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub last_input_rows: usize,
    pub max_round_seconds: f64,
    pub last_failure: Option<String>,
}

#[derive(Debug, Default)]
struct Progress {
    report: MaintenanceReport,
    #[cfg(feature = "fault-injection")]
    waiters: std::collections::BTreeMap<u64, (u64, Option<MaintenanceReport>)>,
    #[cfg(feature = "fault-injection")]
    next_waiter: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Status {
    accepted: AtomicU64,
    report: Mutex<Progress>,
    finished: Condvar,
}

impl Status {
    fn complete(&self, result: Result<usize>, elapsed: Duration) {
        let mut progress = self.report.lock();
        let report = &mut progress.report;
        report.completed += 1;
        match result {
            Ok(rows) if rows > 0 => {
                report.last_input_rows = rows;
                report.max_round_seconds = report.max_round_seconds.max(elapsed.as_secs_f64());
            }
            Ok(_) => {}
            Err(EngineError::Closed) => report.cancelled += 1,
            Err(error) => {
                report.failed += 1;
                report.last_failure = Some(error.to_string());
            }
        }
        #[cfg(feature = "fault-injection")]
        if !progress.waiters.is_empty() {
            let snapshot = progress.report.clone();
            for (boundary, done) in progress.waiters.values_mut() {
                if *boundary == snapshot.completed {
                    let mut at_boundary = snapshot.clone();
                    at_boundary.accepted = *boundary;
                    *done = Some(at_boundary);
                }
            }
        }
        self.finished.notify_all();
    }

    #[cfg(feature = "fault-injection")]
    pub(crate) fn wait(&self, timeout: Duration) -> Result<MaintenanceReport> {
        let start = Instant::now();
        let mut progress = self.report.lock();
        let target = self.accepted.load(Ordering::Acquire);
        if progress.report.completed >= target {
            let mut result = progress.report.clone();
            result.accepted = target;
            return Ok(result);
        }
        let id = progress.next_waiter;
        progress.next_waiter = id
            .checked_add(1)
            .ok_or_else(|| EngineError::Storage("maintenance waiter count exhausted".into()))?;
        progress.waiters.insert(id, (target, None));
        loop {
            if let Some((_, Some(result))) = progress.waiters.get(&id) {
                let result = result.clone();
                progress.waiters.remove(&id);
                return Ok(result);
            }
            let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
                progress.waiters.remove(&id);
                return Err(EngineError::Storage("maintenance wait timed out".into()));
            };
            self.finished.wait_for(&mut progress, remaining);
        }
    }
}

#[derive(Debug, Default)]
struct Queue {
    order: VecDeque<usize>,
    entries: HashMap<usize, (CollectionHandle, u64)>,
    stopped: bool,
}

#[derive(Debug)]
pub(crate) struct Maintenance {
    queue: OrderedMutex<Queue, 6>,
    thread: OnceLock<Thread>,
    join: Mutex<Option<JoinHandle<()>>>,
    pub abort: AtomicBool,
    pub diagnostics: bool,
    #[cfg(feature = "fault-injection")]
    pub pause: Mutex<Option<PauseHook>>,
    #[cfg(feature = "fault-injection")]
    pub fault: Mutex<Option<CompactionFault>>,
    #[cfg(feature = "fault-injection")]
    pub trace: Mutex<Option<std::sync::mpsc::Sender<Vec<crate::storage::fs_commit::FsEvent>>>>,
}

impl Maintenance {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: OrderedMutex::new(Queue::default()),
            thread: OnceLock::new(),
            join: Mutex::new(None),
            abort: AtomicBool::new(false),
            diagnostics: std::env::var_os("MILVUS_LITE_COMPACTION_DIAGNOSTICS")
                .is_some_and(|value| value == "1"),
            #[cfg(feature = "fault-injection")]
            pause: Mutex::new(None),
            #[cfg(feature = "fault-injection")]
            fault: Mutex::new(None),
            #[cfg(feature = "fault-injection")]
            trace: Mutex::new(None),
        })
    }

    #[allow(clippy::expect_used)] // order 与 entries 在同一叶子锁内一起插入/回收。
    pub fn start(self: &Arc<Self>, owner: Weak<Db>) -> Result<()> {
        #[cfg(feature = "fault-injection")]
        if FAIL_START.with(|v| v.replace(false)) {
            return Err(EngineError::Storage(
                "injected maintenance start failure".into(),
            ));
        }
        let maintenance = Arc::clone(self);
        // macOS 的 std 通道等待复用调用线程的 libdispatch parker，fork 子进程可能继承失效对象。
        let ready = Arc::new((Mutex::new(false), Condvar::new()));
        let started = Arc::clone(&ready);
        let worker = std::thread::Builder::new()
            .name("milvus-lite-maintenance".into())
            .spawn(move || {
                *ready.0.lock() = true;
                ready.1.notify_all();
                loop {
                    let next = {
                        let mut queue = maintenance.queue.lock();
                        let key = queue.order.pop_front();
                        if let Some(key) = key {
                            let (handle, remaining) = queue
                                .entries
                                .get_mut(&key)
                                .expect("queued maintenance entry exists");
                            let handle = Arc::clone(handle);
                            *remaining -= 1;
                            if *remaining == 0 {
                                queue.entries.remove(&key);
                            } else {
                                queue.order.push_back(key);
                            }
                            Some(handle)
                        } else if queue.stopped {
                            break;
                        } else {
                            None
                        }
                    };
                    let Some(handle) = next else {
                        std::thread::park();
                        continue;
                    };
                    let status = handle
                        .read()
                        .persistent()
                        .map(|p| Arc::clone(&p.maintenance_status));
                    let start = Instant::now();
                    #[cfg(feature = "fault-injection")]
                    let trace = maintenance.trace.lock().take();
                    #[cfg(feature = "fault-injection")]
                    if trace.is_some() {
                        crate::storage::fs_commit::begin_fs_trace();
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let db = owner.upgrade().ok_or(EngineError::Closed)?;
                        if maintenance.abort.load(Ordering::Acquire) {
                            return Err(EngineError::Closed);
                        }
                        db.compact_handle(&handle)
                    }))
                    .unwrap_or_else(|payload| {
                        Err(EngineError::Storage(format!(
                            "maintenance worker panicked: {}",
                            crate::error::panic_description(payload.as_ref())
                        )))
                    });
                    if let Err(error) = &result
                        && !matches!(error, EngineError::Closed)
                    {
                        eprintln!("background maintenance failed: {error}");
                    }
                    let elapsed = start.elapsed();
                    #[cfg(feature = "fault-injection")]
                    if let Some(trace) = trace
                        && trace
                            .send(crate::storage::fs_commit::finish_fs_trace())
                            .is_err()
                    {
                        eprintln!("maintenance trace receiver disconnected");
                    }
                    if maintenance.diagnostics
                        && let Ok(rows) = &result
                        && *rows > 0
                    {
                        eprintln!(
                            "compaction completed: input_rows={rows} elapsed_seconds={:.6}",
                            elapsed.as_secs_f64()
                        );
                    }
                    if let Some(status) = status {
                        status.complete(result, elapsed);
                    }
                }
            })
            .map_err(|e| EngineError::Storage(format!("cannot start maintenance worker: {e}")))?;
        self.thread
            .set(worker.thread().clone())
            .map_err(|_| EngineError::Storage("maintenance worker already started".into()))?;
        *self.join.lock() = Some(worker);
        let mut awake = started.0.lock();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !*awake {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    EngineError::Storage("maintenance worker readiness timed out".into())
                })?;
            started.1.wait_for(&mut awake, remaining);
        }
        Ok(())
    }

    pub fn enqueue(&self, handle: &CollectionHandle) -> Result<()> {
        let status = handle
            .read()
            .persistent()
            .map(|p| Arc::clone(&p.maintenance_status))
            .ok_or_else(|| EngineError::Storage("persistent state missing".into()))?;
        let mut queue = self.queue.lock();
        if queue.stopped {
            return Err(EngineError::Closed);
        }
        let key = Arc::as_ptr(handle) as usize;
        let old = queue.entries.get(&key).map_or(0, |(_, pending)| *pending);
        let pending = old
            .checked_add(1)
            .ok_or_else(|| EngineError::Storage("maintenance trigger count exhausted".into()))?;
        status
            .accepted
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| EngineError::Storage("maintenance trigger count exhausted".into()))?;
        if old == 0 {
            queue.order.push_back(key);
        }
        queue.entries.insert(key, (Arc::clone(handle), pending));
        drop(queue);
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
        Ok(())
    }

    pub fn stop(&self, drain: bool) {
        if !drain {
            self.abort.store(true, Ordering::Release);
        }
        self.queue.lock().stopped = true;
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
        let worker = self.join.lock().take();
        if let Some(worker) = worker
            && worker.thread().id() != std::thread::current().id()
            && let Err(payload) = worker.join()
        {
            eprintln!(
                "maintenance thread terminated: {}",
                crate::error::panic_description(payload.as_ref())
            );
        }
    }
}
