//! 每个 Db 的 HNSW 后台构建（0041 第 3 步）：独立线程、不占维护线程与计算池；任务只持弱引用，构建全程不持锁，
//! 发布交给 [`Db::publish_graph`]（许可 → flush 锁 → 状态读锁，与 compaction 相同）。失败按退避重新排队。

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::collection::{ChunkData, Collection};
use crate::data::ColumnData;
use crate::db::Db;
use crate::error::{EngineError, Result};
use crate::hnsw::{Graph, Params, Part, Vectors};
use crate::index::Metric;
use crate::lock_order::RwLock;

/// 行数低于此值的段不建图，走 FLAT（0041「据此确定的取值」：1 万行 FLAT 约 0.3 ms，建图约 1 s）。
pub(crate) const MIN_GRAPH_ROWS: usize = 10_000;

const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(60);

/// 为一个已提交段的一个 HNSW 字段建图所需的全部信息。块与 collection 都只持弱引用：排队期间段被 compaction
/// 替换或 collection 被释放时，内存随之释放，任务开始时发现取不到就跳过。
pub(crate) struct GraphTask {
    pub collection: Weak<RwLock<Collection, 5>>,
    /// 段内各块（段内起始行号，块数据），按起点升序；图挂在第一块上。
    pub chunks: Vec<(usize, Weak<ChunkData>)>,
    pub epoch: Arc<AtomicU64>,
    pub seen: u64,
    pub field_pos: usize,
    pub field_name: String,
    pub metric: Metric,
    pub dim: usize,
    pub norm_stride: usize,
    pub norm_offset: usize,
    pub params: Params,
    /// 段所在分区的 `indexes/` 目录与段数据文件。
    pub indexes_dir: PathBuf,
    pub segment_path: PathBuf,
    pub(crate) attempts: u32,
    pub(crate) not_before: Option<Instant>,
}

/// 队列的去重键：（collection 实例的代号计数器地址，段数据文件，字段，代号）。计数器随任务存活，地址不会被
/// 另一个 collection 实例复用；含代号，release 之后 load 排的新一轮任务不会被上一轮仍在排队的任务挡掉。
type TaskKey = (usize, PathBuf, usize, u64);

impl GraphTask {
    fn key(&self) -> TaskKey {
        (
            Arc::as_ptr(&self.epoch) as usize,
            self.segment_path.clone(),
            self.field_pos,
            self.seen,
        )
    }

    /// 段内全部块都还在时返回它们；任何一块已释放（段被替换）返回 None。
    pub(crate) fn upgrade(&self) -> Option<Vec<(usize, Arc<ChunkData>)>> {
        self.chunks
            .iter()
            .map(|(start, chunk)| chunk.upgrade().map(|c| (*start, c)))
            .collect()
    }

    pub(crate) fn rows(chunks: &[(usize, Arc<ChunkData>)]) -> Result<u32> {
        let rows = chunks.last().map_or(0, |(start, c)| start + c.seqs.len());
        u32::try_from(rows).map_err(|_| EngineError::Storage("segment rows exceed u32".into()))
    }

    pub(crate) fn segment_name(&self) -> Result<&str> {
        self.segment_path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| EngineError::Storage("segment file name is not UTF-8".into()))
    }

    /// `indexes/<stem>.<字段>.hnsw.mlrs`。
    pub(crate) fn graph_path(&self) -> Result<PathBuf> {
        let stem = self
            .segment_path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| EngineError::Storage("segment file stem is not UTF-8".into()))?;
        Ok(self
            .indexes_dir
            .join(format!("{stem}.{}{GRAPH_SUFFIX}", self.field_name)))
    }

    /// 层级种子由段文件名决定，同一段重开、重建得到同一张图。
    pub(crate) fn seed(&self) -> Result<u64> {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for b in self.segment_name()?.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        Ok(h)
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.epoch.load(Ordering::SeqCst) != self.seen
    }

    pub(crate) fn file_key(
        &self,
        chunks: &[(usize, Arc<ChunkData>)],
    ) -> Result<crate::hnsw::FileKey> {
        let rows = Self::rows(chunks)?;
        let mut seqs = 0xcbf2_9ce4_8422_2325u64;
        for (_, chunk) in chunks {
            for &seq in &chunk.seqs {
                seqs = (seqs.rotate_left(5) ^ seq).wrapping_mul(0x517c_c1b7_2722_0a95);
            }
        }
        let segment_len = std::fs::metadata(&self.segment_path)
            .map_err(|e| EngineError::Storage(format!("segment {:?}: {e}", self.segment_path)))?
            .len();
        Ok(crate::hnsw::FileKey {
            metric: self.metric,
            dim: u32::try_from(self.dim)
                .map_err(|_| EngineError::Storage("vector dim exceeds u32".into()))?,
            params: self.params,
            rows,
            segment: self.segment_name()?.to_owned(),
            segment_len,
            seqs,
        })
    }
}

/// 本实现的图文件后缀。不以 `.idx` / `.sidx` 结尾，3.x 会跳过它（0002 约束 5）。
pub(crate) const GRAPH_SUFFIX: &str = ".hnsw.mlrs";
/// 构建线程写出的临时文件：`<正式名>.tmp-<序号>`。
pub(crate) const GRAPH_TMP_MARKER: &str = ".hnsw.mlrs.tmp-";

/// 文件名是否属于本实现的图（含临时文件）。
pub(crate) fn is_graph_file(name: &str) -> bool {
    name.ends_with(GRAPH_SUFFIX) || name.contains(GRAPH_TMP_MARKER)
}

/// 在段的各块上建图。空向量的行（nullable，0038）不进图。被取消返回 Ok(None)。
pub(crate) fn build(
    task: &GraphTask,
    chunks: &[(usize, Arc<ChunkData>)],
    stop: &AtomicBool,
) -> Result<Option<Graph>> {
    let mut parts = Vec::with_capacity(chunks.len());
    let mut columns = Vec::with_capacity(chunks.len());
    for (start, chunk) in chunks {
        let column = chunk
            .columns
            .get(task.field_pos)
            .ok_or_else(|| EngineError::Storage("graph field column missing".into()))?;
        let ColumnData::FloatVector { dim, values } = column.values() else {
            return Err(EngineError::Storage(
                "graph field is not a float vector".into(),
            ));
        };
        if *dim != task.dim {
            return Err(EngineError::Storage("graph field dim changed".into()));
        }
        parts.push(Part {
            start: *start,
            values,
            norms: &chunk.norms,
        });
        columns.push((*start, column));
    }
    let vectors = Vectors::new(&parts, task.dim, task.norm_stride, task.norm_offset)
        .ok_or_else(|| EngineError::Storage("segment chunks are not contiguous".into()))?;
    let present = |row: u32| {
        let row = row as usize;
        let i = columns.partition_point(|(start, _)| *start <= row) - 1;
        let (start, column) = columns[i];
        !column.is_null(row - start)
    };
    let cancelled = || stop.load(Ordering::Acquire) || task.cancelled();
    let graph = Graph::build(
        task.metric,
        vectors,
        task.params,
        task.seed()?,
        &present,
        &cancelled,
    );
    if graph.is_none() && !cancelled() {
        return Err(EngineError::Storage(
            "segment too large for an HNSW graph".into(),
        ));
    }
    Ok(graph)
}

/// 建图期间只持有块与 collection，不持有 Db；发布时才短暂升级 Db 的弱引用。
fn run_task(owner: &Weak<Db>, task: &GraphTask, stop: &AtomicBool) -> Result<()> {
    let (Some(chunks), Some(collection)) = (task.upgrade(), task.collection.upgrade()) else {
        return Ok(());
    };
    // 同一段可能被维护与 load / create_index 各排一次：已有图就不再重复构建。
    let filled = chunks
        .first()
        .is_some_and(|(_, first)| first.graphs.get(task.field_pos).is_some());
    if filled || task.cancelled() || stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let Some(graph) = build(task, &chunks, stop)? else {
        return Ok(());
    };
    let db = owner.upgrade().ok_or(EngineError::Closed)?;
    db.publish_graph(&collection, &chunks, task, graph)
}

#[derive(Default)]
struct State {
    queue: VecDeque<GraphTask>,
    keys: HashSet<TaskKey>,
    running: usize,
    stopped: bool,
}

pub(crate) struct GraphBuilder {
    state: Mutex<State>,
    wake: Condvar,
    idle: Condvar,
    stop: AtomicBool,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for GraphBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GraphBuilder")
    }
}

impl GraphBuilder {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            idle: Condvar::new(),
            stop: AtomicBool::new(false),
            threads: Mutex::new(Vec::new()),
        })
    }

    /// 线程数为可用核数的 1/8，限制在 1 到 4（0041 第 3 步设计第 2 条）。
    pub(crate) fn start(self: &Arc<Self>, owner: Weak<Db>) -> Result<()> {
        let cores = std::thread::available_parallelism().map_or(1, usize::from);
        let count = (cores / 8).clamp(1, 4);
        let mut threads = self.threads.lock();
        for i in 0..count {
            let builder = Arc::clone(self);
            let owner = owner.clone();
            let handle = std::thread::Builder::new()
                .name(format!("milvus-lite-graph-{i}"))
                .spawn(move || builder.run(&owner))
                .map_err(|e| EngineError::Storage(format!("cannot start graph builder: {e}")))?;
            threads.push(handle);
        }
        Ok(())
    }

    /// 已在队列中的（块、字段）不重复排队。停止后不再接受。
    pub(crate) fn enqueue(&self, tasks: Vec<GraphTask>) {
        if tasks.is_empty() {
            return;
        }
        let mut state = self.state.lock();
        if state.stopped {
            return;
        }
        for task in tasks {
            if state.keys.insert(task.key()) {
                state.queue.push_back(task);
            }
        }
        self.wake.notify_all();
    }

    /// 等队列排空、在途构建结束；超时报错。供测试与基准使用（0041 第 3 步设计第 10 条）。
    pub(crate) fn wait_idle(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock();
        while !(state.queue.is_empty() && state.running == 0) {
            if self.idle.wait_until(&mut state, deadline).timed_out() {
                return Err(EngineError::Storage(
                    "graph builds did not finish in time".into(),
                ));
            }
        }
        Ok(())
    }

    /// 置取消标志并等构建线程退出。由 Db 关闭在释放 LOCK 之前调用；可重复调用。
    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        {
            let mut state = self.state.lock();
            state.stopped = true;
            state.queue.clear();
            state.keys.clear();
        }
        self.wake.notify_all();
        self.idle.notify_all();
        let threads = std::mem::take(&mut *self.threads.lock());
        let me = std::thread::current().id();
        for thread in threads {
            // Db 的最后一个强引用可能恰好在构建线程上释放，析构在本线程调用 stop：不能 join 自己。
            if thread.thread().id() == me {
                continue;
            }
            if thread.join().is_err() {
                eprintln!("graph builder thread panicked");
            }
        }
    }

    fn next(&self) -> Option<GraphTask> {
        let mut state = self.state.lock();
        loop {
            if state.stopped {
                return None;
            }
            let now = Instant::now();
            if let Some(i) = state
                .queue
                .iter()
                .position(|t| t.not_before.is_none_or(|at| at <= now))
            {
                let task = state.queue.remove(i)?;
                state.keys.remove(&task.key());
                state.running += 1;
                return Some(task);
            }
            match state.queue.iter().filter_map(|t| t.not_before).min() {
                Some(at) => {
                    self.wake.wait_until(&mut state, at);
                }
                None => self.wake.wait(&mut state),
            }
        }
    }

    fn finish(&self, retry: Option<GraphTask>) {
        let mut state = self.state.lock();
        state.running -= 1;
        if let Some(task) = retry
            && !state.stopped
            && state.keys.insert(task.key())
        {
            state.queue.push_back(task);
            self.wake.notify_all();
        }
        if state.queue.is_empty() && state.running == 0 {
            self.idle.notify_all();
        }
    }

    fn run(&self, owner: &Weak<Db>) {
        while let Some(mut task) = self.next() {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_task(owner, &task, &self.stop)
            }))
            .unwrap_or_else(|payload| {
                Err(EngineError::Storage(format!(
                    "graph builder panicked: {}",
                    crate::error::panic_description(payload.as_ref())
                )))
            });
            let retry = match outcome {
                Ok(()) | Err(EngineError::Closed) => None,
                Err(_) if self.stop.load(Ordering::Acquire) || task.cancelled() => None,
                Err(error) => {
                    eprintln!(
                        "HNSW graph build for {:?} failed, falling back to FLAT until retry: {error}",
                        task.segment_path
                    );
                    let delay = FIRST_RETRY
                        .saturating_mul(1u32 << task.attempts.min(6))
                        .min(MAX_RETRY);
                    task.attempts += 1;
                    task.not_before = Some(Instant::now() + delay);
                    Some(task)
                }
            };
            self.finish(retry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(epoch: &Arc<AtomicU64>, seen: u64, segment: &str) -> GraphTask {
        GraphTask {
            collection: Weak::new(),
            chunks: Vec::new(),
            epoch: Arc::clone(epoch),
            seen,
            field_pos: 1,
            field_name: "v".into(),
            metric: Metric::L2,
            dim: 2,
            norm_stride: 1,
            norm_offset: 0,
            params: Params {
                m: 16,
                ef_construction: 200,
            },
            indexes_dir: PathBuf::from("indexes"),
            segment_path: PathBuf::from(segment),
            attempts: 0,
            not_before: None,
        }
    }

    /// 同一轮的同一段只排一次；release 之后 load 排的新一轮、以及另一个 collection 实例的同名段都要排上，
    /// 不能被上一轮还在排队的任务挡掉。
    #[test]
    fn a_new_round_or_another_collection_is_not_deduplicated_away() {
        let builder = GraphBuilder::new();
        let epoch = Arc::new(AtomicU64::new(0));
        builder.enqueue(vec![
            task(&epoch, 0, "a"),
            task(&epoch, 0, "a"),
            task(&epoch, 0, "b"),
        ]);
        assert_eq!(builder.state.lock().queue.len(), 2);
        epoch.store(2, Ordering::SeqCst);
        builder.enqueue(vec![task(&epoch, 2, "a"), task(&epoch, 2, "b")]);
        assert_eq!(builder.state.lock().queue.len(), 4);
        let other = Arc::new(AtomicU64::new(2));
        builder.enqueue(vec![task(&other, 2, "a")]);
        assert_eq!(builder.state.lock().queue.len(), 5);
        builder.stop();
    }
}
