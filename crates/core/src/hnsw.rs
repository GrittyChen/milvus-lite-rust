//! HNSW 图（0041）：单个 segment 一张图，只存邻接表，向量读 segment 的列。
//!
//! 打分全部经 [`flat::Query::score`]，返回的分数与 FLAT 对同一行算出的值逐位相同；图内的比较用
//! [`order_key`]，与 [`flat::rank`] 的顺序一致（NaN 最差，平局按行号）。构建单线程、按行号顺序插入，
//! 层级由调用方给的种子决定；由于打分逐位确定（flat.rs 模块说明），同一输入在各平台建出的图相同。
//!
//! 选邻居用 hnswlib 的启发式（`getNeighborsByHeuristic2`）；第 0 层每点至多 2M 个邻居，上层至多 M 个。
//! 检索时遍历不受过滤限制，只有允许的行进入结果（与 faiss `IDSelector` 同义），因此过滤下也能返回
//! 足 k 条；代价是通过率低时会遍历大半张图，调用方用距离计算预算截断并改走 FLAT（0041「据此确定的取值」）。

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use crate::flat::Query;
use crate::index::Metric;

/// 3.x（faiss）的构建参数默认值。
pub(crate) const DEFAULT_M: usize = 16;
pub(crate) const DEFAULT_EF_CONSTRUCTION: usize = 200;

/// 层数上限：层级服从几何分布，M=2 时 2^16 个点才期望出现一次第 16 层，再高没有意义。
const MAX_LEVEL: u8 = 16;

/// 取消标志的检查间隔（插入次数）。
const CANCEL_CHECK_EVERY: u32 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Params {
    pub m: usize,
    pub ef_construction: usize,
}

/// 构建参数的上限：超出时不建图、走 FLAT，避免离谱的参数分配巨量内存。
const MAX_M: usize = 512;
const MAX_EF_CONSTRUCTION: usize = 100_000;

impl Params {
    /// 对应 3.x 的 `int(params.get("M", 16))` / `int(params.get("efConstruction", 200))`，但只接受常见
    /// 写法：整数、截断的有限浮点数、首尾可带空白的 ASCII 十进制整数字符串与布尔；Python `int()` 另外接受的
    /// 下划线分隔、非 ASCII 数字等写法，以及超出 [`MAX_M`] / [`MAX_EF_CONSTRUCTION`] 的值，都返回 None，调用方
    /// 不建图、走 FLAT。这只影响本实现是否建图，不改变定义的保存与报错（0014）。M 至少按 2 构建。
    pub(crate) fn from_spec(params: &serde_json::Map<String, serde_json::Value>) -> Option<Self> {
        let m = int_param(params.get("M"), DEFAULT_M)?;
        let ef_construction = int_param(params.get("efConstruction"), DEFAULT_EF_CONSTRUCTION)?;
        ((1..=MAX_M).contains(&m) && (1..=MAX_EF_CONSTRUCTION).contains(&ef_construction))
            .then_some(Self {
                m: m.max(2),
                ef_construction,
            })
    }
}

/// 检索参数中的 `ef`（3.x `int(params.get("ef", 64))`，接受的写法同 [`Params::from_spec`]）。无法解析时
/// 返回 None，调用方不用图、走 FLAT：这只影响本实现是否用图，不新增报错。
pub(crate) fn search_ef(params: &serde_json::Map<String, serde_json::Value>) -> Option<usize> {
    int_param(params.get("ef"), DEFAULT_EF).map(|ef| ef.max(1))
}

/// 3.x（faiss）检索参数 ef 的默认值。
pub(crate) const DEFAULT_EF: usize = 64;

thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::new(Scratch::new());
}

/// 计算池线程各自复用一份访问标记，避免每个叶子任务分配与段行数等长的数组。
pub(crate) fn with_scratch<R>(f: impl FnOnce(&mut Scratch) -> R) -> R {
    SCRATCH.with(|s| f(&mut s.borrow_mut()))
}

fn int_param(value: Option<&serde_json::Value>, default: usize) -> Option<usize> {
    use serde_json::Value;
    let n: i64 = match value {
        None => return Some(default),
        Some(Value::Bool(b)) => i64::from(*b),
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) => i,
            None => {
                let f = n.as_f64()?;
                if !f.is_finite() || f.abs() >= 1e18 {
                    return None;
                }
                f.trunc() as i64
            }
        },
        Some(Value::String(s)) => s.trim().parse().ok()?,
        Some(_) => return None,
    };
    usize::try_from(n).ok()
}

/// 一个 segment 在内存中的一块：从段内第 `start` 行起的向量（按行展开）与范数。
pub(crate) struct Part<'a> {
    pub start: usize,
    pub values: &'a [f32],
    pub norms: &'a [f32],
}

/// 一个 segment 的向量列。重开后一个段按 Parquet 行组分成多块，flush 刚写出时是一块；图的行号一律是
/// 段内行号（块起点 + 块内行号），两种布局建出、读入的是同一张图。范数（[`flat::norm`]，只有 COSINE 使用）
/// 沿用块上 `行号 * 向量列数 + 向量列序号` 的交错布局（[`flat::row_norms`]）。
#[derive(Clone, Copy)]
pub(crate) struct Vectors<'a> {
    parts: &'a [Part<'a>],
    dim: usize,
    norm_stride: usize,
    norm_offset: usize,
    /// 除最后一块外各块等长时为块长，按除法定位；否则为 0，按起点二分查找。
    uniform: usize,
    rows: usize,
}

impl<'a> Vectors<'a> {
    /// `parts` 须按 `start` 升序、首块从 0 开始且首尾相接；不满足时返回 None。
    pub(crate) fn new(
        parts: &'a [Part<'a>],
        dim: usize,
        norm_stride: usize,
        norm_offset: usize,
    ) -> Option<Self> {
        if dim == 0 || norm_stride == 0 || norm_offset >= norm_stride {
            return None;
        }
        let mut next = 0usize;
        for part in parts {
            let len = part.values.len() / dim;
            if part.start != next
                || part.values.len() != len * dim
                || part.norms.len() < len * norm_stride
            {
                return None;
            }
            next += len;
        }
        let first = parts.first().map_or(0, |p| p.values.len() / dim);
        let uniform = if first > 0
            && parts.iter().enumerate().all(|(i, p)| {
                p.start == i * first && (i + 1 == parts.len() || p.values.len() / dim == first)
            }) {
            first
        } else {
            0
        };
        Some(Self {
            parts,
            dim,
            norm_stride,
            norm_offset,
            uniform,
            rows: next,
        })
    }

    pub(crate) fn rows(&self) -> usize {
        self.rows
    }

    fn locate(&self, i: u32) -> (&'a Part<'a>, usize) {
        let i = i as usize;
        let p = match i.checked_div(self.uniform) {
            Some(p) => p,
            None => self.parts.partition_point(|p| p.start <= i) - 1,
        };
        let part = &self.parts[p];
        (part, i - part.start)
    }

    fn row(&self, i: u32) -> &'a [f32] {
        let (part, local) = self.locate(i);
        &part.values[local * self.dim..(local + 1) * self.dim]
    }

    fn norm(&self, i: u32) -> f32 {
        let (part, local) = self.locate(i);
        part.norms[local * self.norm_stride + self.norm_offset]
    }
}

/// 与 [`flat::rank`] 一致的升序键：越相似越小，NaN 最大，-0 与 +0 相同。
fn order_key(metric: Metric, score: f32) -> f32 {
    if score.is_nan() {
        return f32::NAN;
    }
    let key = if metric.higher_is_better() {
        -score
    } else {
        score
    };
    // rank 用 partial_cmp，-0 与 +0 相等、按行号定先后；total_cmp 会区分二者，因此归一。
    if key == 0.0 { 0.0 } else { key }
}

/// 图内的候选：排序键、对外分数、行号。按（键，行号）全序比较。
#[derive(Clone, Copy, Debug)]
struct Cand {
    key: f32,
    score: f32,
    row: u32,
}

impl PartialEq for Cand {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Cand {}
impl PartialOrd for Cand {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Cand {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key
            .total_cmp(&other.key)
            .then(self.row.cmp(&other.row))
    }
}

/// SplitMix64：只用于层级，输出由种子唯一决定。
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// (0, 1] 上的均匀分布，取 53 位。
    fn unit(&mut self) -> f64 {
        ((self.next() >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    }
}

/// 检索与构建共用的访问标记，按轮次复用，避免每次清零。
pub(crate) struct Scratch {
    tags: Vec<u32>,
    epoch: u32,
}

impl Scratch {
    pub(crate) fn new() -> Self {
        Self {
            tags: Vec::new(),
            epoch: 0,
        }
    }

    fn reset(&mut self, rows: usize) {
        if self.tags.len() < rows {
            self.tags.resize(rows, 0);
        }
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.tags.fill(0);
            self.epoch = 1;
        }
    }

    /// 首次访问返回 true。
    fn visit(&mut self, row: u32) -> bool {
        let tag = &mut self.tags[row as usize];
        if *tag == self.epoch {
            false
        } else {
            *tag = self.epoch;
            true
        }
    }
}

/// 检索结果：按 [`flat::rank`] 的顺序（平局按行号）排列的（对外分数，行号）。
#[derive(Debug, PartialEq)]
pub(crate) enum Search {
    Hits(Vec<(f32, u32)>),
    /// 距离计算次数超过预算，调用方改走 FLAT。
    OverBudget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Graph {
    metric: Metric,
    params: Params,
    rows: u32,
    /// 每行的层级；不在图中的行（空向量）为 `u8::MAX`。
    levels: Vec<u8>,
    entry: Option<u32>,
    max_level: u8,
    /// 第 0 层：每行 `2M + 1` 个槽，首槽为邻居数。
    layer0: Vec<u32>,
    /// 每行在 `upper` 中的起始块号，层级为 0 或不在图中时为 `u32::MAX`。
    upper_start: Vec<u32>,
    /// 上层：每行每层一块 `M + 1` 个槽，首槽为邻居数。
    upper: Vec<u32>,
}

const ABSENT: u8 = u8::MAX;

impl Graph {
    /// 按行号顺序插入 `present` 为真的行。`cancelled` 每插入 [`CANCEL_CHECK_EVERY`] 行检查一次，
    /// 返回 true 时放弃构建、返回 None；行数或上层块数超出 u32 时同样返回 None，调用方走 FLAT。
    pub(crate) fn build(
        metric: Metric,
        vectors: Vectors<'_>,
        params: Params,
        seed: u64,
        present: &dyn Fn(u32) -> bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<Graph> {
        let m = params.m.max(2);
        let params = Params {
            m,
            ef_construction: params.ef_construction.max(1),
        };
        let rows = u32::try_from(vectors.rows()).ok()?;
        let level_scale = 1.0 / (m as f64).ln();
        let mut rng = SplitMix(seed);
        let mut levels = Vec::with_capacity(rows as usize);
        let mut upper_start = Vec::with_capacity(rows as usize);
        let mut blocks = 0usize;
        for row in 0..rows {
            // 不在图中的行也消耗一次随机数，层级只由行号与种子决定。
            let level =
                ((-rng.unit().ln() * level_scale).floor() as u64).min(MAX_LEVEL as u64) as u8;
            if !present(row) {
                levels.push(ABSENT);
                upper_start.push(u32::MAX);
                continue;
            }
            levels.push(level);
            if level > 0 {
                upper_start.push(u32::try_from(blocks).ok()?);
                blocks += level as usize;
            } else {
                upper_start.push(u32::MAX);
            }
        }
        let mut graph = Graph {
            metric,
            params,
            rows,
            levels,
            entry: None,
            max_level: 0,
            layer0: vec![0; rows as usize * (2 * m + 1)],
            upper_start,
            upper: vec![0; blocks * (m + 1)],
        };
        let mut scratch = Scratch::new();
        for row in 0..rows {
            if row % CANCEL_CHECK_EVERY == 0 && cancelled() {
                return None;
            }
            if graph.levels[row as usize] != ABSENT {
                graph.insert(vectors, row, &mut scratch);
            }
        }
        Some(graph)
    }

    pub(crate) fn metric(&self) -> Metric {
        self.metric
    }

    pub(crate) fn rows(&self) -> u32 {
        self.rows
    }

    fn cap(&self, layer: u8) -> usize {
        if layer == 0 {
            2 * self.params.m
        } else {
            self.params.m
        }
    }

    fn slot(&self, row: u32, layer: u8) -> usize {
        if layer == 0 {
            row as usize * (2 * self.params.m + 1)
        } else {
            (self.upper_start[row as usize] as usize + layer as usize - 1) * (self.params.m + 1)
        }
    }

    fn neighbors(&self, row: u32, layer: u8) -> &[u32] {
        let start = self.slot(row, layer);
        let list = if layer == 0 {
            &self.layer0
        } else {
            &self.upper
        };
        let count = list[start] as usize;
        &list[start + 1..start + 1 + count]
    }

    fn set_neighbors(&mut self, row: u32, layer: u8, ids: &[u32]) {
        let start = self.slot(row, layer);
        let list = if layer == 0 {
            &mut self.layer0
        } else {
            &mut self.upper
        };
        list[start] = ids.len() as u32;
        list[start + 1..start + 1 + ids.len()].copy_from_slice(ids);
    }

    fn cand(&self, query: &Query<'_>, vectors: Vectors<'_>, row: u32) -> Cand {
        let score = query.score(vectors.row(row), vectors.norm(row));
        Cand {
            key: order_key(self.metric, score),
            score,
            row,
        }
    }

    fn insert(&mut self, vectors: Vectors<'_>, row: u32, scratch: &mut Scratch) {
        let level = self.levels[row as usize];
        let Some(entry) = self.entry else {
            self.entry = Some(row);
            self.max_level = level;
            return;
        };
        let query = Query::new(self.metric, vectors.row(row));
        let mut nearest = self.cand(&query, vectors, entry);
        let mut layer = self.max_level;
        while layer > level {
            nearest = self.greedy(&query, vectors, nearest, layer, &mut 0);
            layer -= 1;
        }
        let mut entries = vec![nearest];
        for layer in (0..=level.min(self.max_level)).rev() {
            let Some(found) = self.search_layer(
                &query,
                vectors,
                &entries,
                self.params.ef_construction,
                layer,
                None,
                scratch,
                &mut 0,
                u64::MAX,
            ) else {
                unreachable!("construction has no distance budget");
            };
            let selected = self.select(vectors, &found, self.params.m);
            let ids: Vec<u32> = selected.iter().map(|c| c.row).collect();
            self.set_neighbors(row, layer, &ids);
            for neighbor in &selected {
                self.link(vectors, neighbor.row, row, layer);
            }
            entries = found;
        }
        if level > self.max_level {
            self.max_level = level;
            self.entry = Some(row);
        }
    }

    /// 把 `new` 加入 `row` 的邻居；超出上限时按启发式重选。距离都从 `row` 一侧计算：COSINE 的两个方向
    /// 不逐位对称。
    fn link(&mut self, vectors: Vectors<'_>, row: u32, new: u32, layer: u8) {
        let mut ids = self.neighbors(row, layer).to_vec();
        if ids.len() < self.cap(layer) {
            ids.push(new);
            self.set_neighbors(row, layer, &ids);
            return;
        }
        let query = Query::new(self.metric, vectors.row(row));
        let mut cands: Vec<Cand> = ids
            .iter()
            .map(|&id| self.cand(&query, vectors, id))
            .collect();
        cands.push(self.cand(&query, vectors, new));
        cands.sort();
        let kept = self.select(vectors, &cands, self.cap(layer));
        let ids: Vec<u32> = kept.iter().map(|c| c.row).collect();
        self.set_neighbors(row, layer, &ids);
    }

    /// hnswlib 的启发式：按与目标的距离升序考察，候选比任何已选邻居更接近目标时才保留。输入须已升序。
    fn select(&self, vectors: Vectors<'_>, sorted: &[Cand], limit: usize) -> Vec<Cand> {
        if sorted.len() <= limit {
            return sorted.to_vec();
        }
        let mut kept: Vec<Cand> = Vec::with_capacity(limit);
        for &cand in sorted {
            if kept.len() >= limit {
                break;
            }
            let query = Query::new(self.metric, vectors.row(cand.row));
            let diverse = kept.iter().all(|s| {
                let between = self.cand(&query, vectors, s.row);
                between.key.total_cmp(&cand.key) != Ordering::Less
            });
            if diverse {
                kept.push(cand);
            }
        }
        kept
    }

    fn greedy(
        &self,
        query: &Query<'_>,
        vectors: Vectors<'_>,
        mut nearest: Cand,
        layer: u8,
        evals: &mut u64,
    ) -> Cand {
        loop {
            let mut moved = false;
            for &next in self.neighbors(nearest.row, layer) {
                let cand = self.cand(query, vectors, next);
                *evals += 1;
                if cand < nearest {
                    nearest = cand;
                    moved = true;
                }
            }
            if !moved {
                return nearest;
            }
        }
    }

    /// 束搜索，返回升序候选。`allowed` 为 None 时所有行都可进入结果；否则只限制结果，不限制遍历，
    /// 并且结果未满 ef 时不提前停止（hnswlib 带过滤时的停止条件）。距离计算累计（`evals`）超过 `limit`
    /// 时立即停止并返回 None：低通过率时遍历可能覆盖大半张图，必须在途中截断才能给出代价上界。
    #[allow(clippy::too_many_arguments)]
    fn search_layer(
        &self,
        query: &Query<'_>,
        vectors: Vectors<'_>,
        entries: &[Cand],
        ef: usize,
        layer: u8,
        allowed: Option<&dyn Fn(u32) -> bool>,
        scratch: &mut Scratch,
        evals: &mut u64,
        limit: u64,
    ) -> Option<Vec<Cand>> {
        scratch.reset(self.rows as usize);
        let admit = |row: u32| allowed.is_none_or(|f| f(row));
        let mut frontier: BinaryHeap<Reverse<Cand>> = BinaryHeap::new();
        let mut best: BinaryHeap<Cand> = BinaryHeap::new();
        for &entry in entries {
            if !scratch.visit(entry.row) {
                continue;
            }
            frontier.push(Reverse(entry));
            if admit(entry.row) {
                best.push(entry);
                if best.len() > ef {
                    best.pop();
                }
            }
        }
        while let Some(Reverse(current)) = frontier.pop() {
            if let Some(worst) = best.peek()
                && current > *worst
                && (best.len() >= ef || allowed.is_none())
            {
                break;
            }
            for &next in self.neighbors(current.row, layer) {
                if !scratch.visit(next) {
                    continue;
                }
                let cand = self.cand(query, vectors, next);
                *evals += 1;
                if *evals > limit {
                    return None;
                }
                if best.len() < ef || best.peek().is_some_and(|worst| cand < *worst) {
                    frontier.push(Reverse(cand));
                    if admit(next) {
                        best.push(cand);
                        if best.len() > ef {
                            best.pop();
                        }
                    }
                }
            }
        }
        let mut found = best.into_vec();
        found.sort();
        Some(found)
    }

    /// 取前 k 个允许的行。实际束宽为 `max(ef, k)`（0041「已确认的取舍」第 7 项，上界由调用方截到有效行数）。
    /// `budget` 为距离计算次数上限，超过时返回 [`Search::OverBudget`]。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn search(
        &self,
        query: &Query<'_>,
        vectors: Vectors<'_>,
        k: usize,
        ef: usize,
        allowed: Option<&dyn Fn(u32) -> bool>,
        budget: Option<u64>,
        scratch: &mut Scratch,
    ) -> Search {
        let Some(entry) = self.entry else {
            return Search::Hits(Vec::new());
        };
        if k == 0 {
            return Search::Hits(Vec::new());
        }
        let mut evals = 0u64;
        let mut nearest = self.cand(query, vectors, entry);
        let mut layer = self.max_level;
        while layer > 0 {
            nearest = self.greedy(query, vectors, nearest, layer, &mut evals);
            layer -= 1;
        }
        let Some(found) = self.search_layer(
            query,
            vectors,
            &[nearest],
            ef.max(k),
            0,
            allowed,
            scratch,
            &mut evals,
            budget.unwrap_or(u64::MAX),
        ) else {
            return Search::OverBudget;
        };
        Search::Hits(
            found
                .into_iter()
                .take(k)
                .map(|c| (c.score, c.row))
                .collect(),
        )
    }
}

/// 图文件（`indexes/<stem>.<字段>.hnsw.mlrs`，0041 第 3 步）的身份：读入时任何一项与当前段不符，
/// 文件就作废、重建。图文件是缓存，不是真值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileKey {
    pub metric: Metric,
    pub dim: u32,
    pub params: Params,
    pub rows: u32,
    /// 段数据文件名（不含目录）与字节数：段文件被替换时图随之作废。
    pub segment: String,
    pub segment_len: u64,
    /// 段内 `_seq` 按行序的摘要。3.x 的 compaction 只避开磁盘上仍存在的文件名，旧段被回收后同名可以
    /// 再次出现；`_seq` 逐行分配，同一序列意味着同样的行与行序，文件名与字节数相同也能区分内容。
    pub seqs: u64,
}

const MAGIC: &[u8; 8] = b"MLRSHNSW";
/// 图文件格式版本（规则 8，登记在 0002）。改动编码或图的含义时递增；旧版本文件读入时作废、重建。
const FORMAT_VERSION: u32 = 1;

fn metric_code(metric: Metric) -> u8 {
    match metric {
        Metric::L2 => 0,
        Metric::Ip => 1,
        Metric::Cosine => 2,
        Metric::Bm25 => 3,
    }
}

/// 非密码学校验和：按 8 字节一组乘法混合，尾部补零。只用来发现截断与损坏。
fn checksum(bytes: &[u8]) -> u64 {
    let mut h = 0x243F_6A88_85A3_08D3u64 ^ bytes.len() as u64;
    let (words, tail) = bytes.as_chunks::<8>();
    for w in words {
        h = (h ^ u64::from_le_bytes(*w))
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .rotate_left(29);
    }
    let mut last = [0u8; 8];
    last[..tail.len()].copy_from_slice(tail);
    h = (h ^ u64::from_le_bytes(last)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^ (h >> 32)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let out = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(out)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4)?.try_into().ok().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8)?.try_into().ok().map(u64::from_le_bytes)
    }
    fn words(&mut self) -> Option<Vec<u32>> {
        let len = usize::try_from(self.u64()?).ok()?;
        let raw = self.take(len.checked_mul(4)?)?;
        Some(
            raw.as_chunks::<4>()
                .0
                .iter()
                .map(|w| u32::from_le_bytes(*w))
                .collect(),
        )
    }
}

impl Graph {
    /// 测试用：入口不可达的同一张图，检索返回空结果。
    #[cfg(test)]
    pub(crate) fn unreachable(&self) -> Graph {
        Graph {
            entry: None,
            ..self.clone()
        }
    }

    pub(crate) fn encode(&self, key: &FileKey) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            self.levels.len()
                + 4 * (self.layer0.len() + self.upper_start.len() + self.upper.len())
                + 128,
        );
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        Self::encode_key(&mut out, key);
        out.extend_from_slice(&self.entry.unwrap_or(u32::MAX).to_le_bytes());
        out.push(self.max_level);
        out.extend_from_slice(&self.levels);
        for words in [&self.layer0, &self.upper_start, &self.upper] {
            out.extend_from_slice(&(words.len() as u64).to_le_bytes());
            for w in words.iter() {
                out.extend_from_slice(&w.to_le_bytes());
            }
        }
        let sum = checksum(&out);
        out.extend_from_slice(&sum.to_le_bytes());
        out
    }

    fn encode_key(out: &mut Vec<u8>, key: &FileKey) {
        out.push(metric_code(key.metric));
        for v in [
            key.dim,
            key.params.m as u32,
            key.params.ef_construction as u32,
            key.rows,
            key.segment.len() as u32,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(key.segment.as_bytes());
        out.extend_from_slice(&key.segment_len.to_le_bytes());
        out.extend_from_slice(&key.seqs.to_le_bytes());
    }

    /// 校验和、身份与结构全部无误时返回图；否则 None（调用方重建）。结构检查保证解码出的图在
    /// 检索时不会越界：长度、邻居数上限、邻居行号范围与所指行在图中。
    pub(crate) fn decode(bytes: &[u8], key: &FileKey) -> Option<Graph> {
        let (body, sum) = bytes.split_at_checked(bytes.len().checked_sub(8)?)?;
        if checksum(body) != u64::from_le_bytes(sum.try_into().ok()?) {
            return None;
        }
        let mut r = Reader { bytes: body, at: 0 };
        if r.take(8)? != MAGIC || r.u32()? != FORMAT_VERSION {
            return None;
        }
        let mut expected = Vec::new();
        Self::encode_key(&mut expected, key);
        if r.take(expected.len())? != expected.as_slice() {
            return None;
        }
        let m = key.params.m;
        if m < 2 || key.params.ef_construction == 0 {
            return None;
        }
        let rows = key.rows;
        let entry = match r.u32()? {
            u32::MAX => None,
            e => Some(e),
        };
        let max_level = r.u8()?;
        let levels = r.take(rows as usize)?.to_vec();
        let layer0 = r.words()?;
        let upper_start = r.words()?;
        let upper = r.words()?;
        if r.at != body.len() {
            return None;
        }
        let graph = Graph {
            metric: key.metric,
            params: key.params,
            rows,
            levels,
            entry,
            max_level,
            layer0,
            upper_start,
            upper,
        };
        graph.is_well_formed().then_some(graph)
    }

    fn is_well_formed(&self) -> bool {
        let m = self.params.m;
        let rows = self.rows as usize;
        if self.layer0.len() != rows.saturating_mul(2 * m + 1)
            || self.upper_start.len() != rows
            || !self.upper.len().is_multiple_of(m + 1)
            || self.max_level > MAX_LEVEL
        {
            return false;
        }
        let blocks = self.upper.len() / (m + 1);
        let present = |r: u32| (r as usize) < rows && self.levels[r as usize] != ABSENT;
        match self.entry {
            None => {
                if self.levels.iter().any(|&l| l != ABSENT) {
                    return false;
                }
            }
            Some(e) => {
                if !present(e) || self.levels[e as usize] != self.max_level {
                    return false;
                }
            }
        }
        let mut used_blocks = 0usize;
        for row in 0..rows {
            let level = self.levels[row];
            let start = self.upper_start[row];
            if level == ABSENT || level == 0 {
                if start != u32::MAX {
                    return false;
                }
                if level == ABSENT && self.layer0[row * (2 * m + 1)] != 0 {
                    return false;
                }
                if level == ABSENT {
                    continue;
                }
            } else {
                if level > self.max_level
                    || start as usize != used_blocks
                    || used_blocks + level as usize > blocks
                {
                    return false;
                }
                used_blocks += level as usize;
            }
            for layer in 0..=level {
                let slot = self.slot(row as u32, layer);
                let list = if layer == 0 {
                    &self.layer0
                } else {
                    &self.upper
                };
                let count = list[slot] as usize;
                if count > self.cap(layer)
                    || list[slot + 1..slot + 1 + count]
                        .iter()
                        .any(|&n| !present(n) || self.levels[n as usize] < layer)
                {
                    return false;
                }
            }
        }
        used_blocks == blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flat;

    const DIM: usize = 16;

    fn data(rows: usize, seed: u64) -> Vec<f32> {
        let mut rng = SplitMix(seed);
        (0..rows * DIM)
            .map(|_| (rng.unit() as f32) * 2.0 - 1.0)
            .collect()
    }

    fn norms(values: &[f32], dim: usize) -> Vec<f32> {
        values.chunks(dim).map(flat::norm).collect()
    }

    fn vecs<'a>(values: &'a [f32], norms: &'a [f32]) -> Vectors<'a> {
        let parts = Box::leak(Box::new([Part {
            start: 0,
            values,
            norms,
        }]));
        Vectors::new(parts, DIM, 1, 0).expect("valid layout")
    }

    fn params() -> Params {
        Params {
            m: DEFAULT_M,
            ef_construction: DEFAULT_EF_CONSTRUCTION,
        }
    }

    fn build(metric: Metric, values: &[f32], norms: &[f32]) -> Graph {
        let v = vecs(values, norms);
        Graph::build(metric, v, params(), 7, &|_| true, &|| false).expect("not cancelled")
    }

    /// FLAT 真值：对 `allowed` 的行打分，按 rank 排序、平局按行号，取前 k。
    fn exact(
        metric: Metric,
        values: &[f32],
        norms: &[f32],
        q: &[f32],
        k: usize,
        allowed: &dyn Fn(u32) -> bool,
    ) -> Vec<(f32, u32)> {
        let query = Query::new(metric, q);
        let mut all: Vec<(f32, u32)> = (0..(values.len() / DIM) as u32)
            .filter(|&r| allowed(r))
            .map(|r| {
                let i = r as usize;
                (query.score(&values[i * DIM..(i + 1) * DIM], norms[i]), r)
            })
            .collect();
        all.sort_by(|a, b| flat::rank(metric, a.0, b.0).then(a.1.cmp(&b.1)));
        all.truncate(k);
        all
    }

    fn hits(s: Search) -> Vec<(f32, u32)> {
        match s {
            Search::Hits(h) => h,
            Search::OverBudget => panic!("over budget"),
        }
    }

    fn bits(v: &[(f32, u32)]) -> Vec<(u32, u32)> {
        v.iter().map(|(s, r)| (s.to_bits(), *r)).collect()
    }

    #[test]
    fn beam_covering_every_row_matches_flat_bit_for_bit() {
        let values = data(300, 1);
        let n = norms(&values, DIM);
        let queries = data(20, 2);
        for metric in [Metric::L2, Metric::Ip, Metric::Cosine] {
            let g = build(metric, &values, &n);
            let v = vecs(&values, &n);
            let mut scratch = Scratch::new();
            for q in queries.chunks(DIM) {
                let query = Query::new(metric, q);
                let got = hits(g.search(&query, v, 10, 300, None, None, &mut scratch));
                let want = exact(metric, &values, &n, q, 10, &|_| true);
                assert_eq!(bits(&got), bits(&want), "{metric:?}");
            }
        }
    }

    #[test]
    fn every_returned_score_is_the_flat_score_and_order_follows_rank() {
        let values = data(2000, 3);
        let n = norms(&values, DIM);
        let queries = data(30, 4);
        for metric in [Metric::L2, Metric::Ip, Metric::Cosine] {
            let g = build(metric, &values, &n);
            let v = vecs(&values, &n);
            let mut scratch = Scratch::new();
            for q in queries.chunks(DIM) {
                let query = Query::new(metric, q);
                let got = hits(g.search(&query, v, 10, 64, None, None, &mut scratch));
                assert_eq!(got.len(), 10);
                for (score, row) in &got {
                    let i = *row as usize;
                    let want = query.score(&values[i * DIM..(i + 1) * DIM], n[i]);
                    assert_eq!(score.to_bits(), want.to_bits());
                }
                assert!(got.windows(2).all(|w| {
                    flat::rank(metric, w[0].0, w[1].0).then(w[0].1.cmp(&w[1].1)) == Ordering::Less
                }));
            }
        }
    }

    #[test]
    fn recall_is_high_with_default_parameters() {
        let values = data(5000, 5);
        let n = norms(&values, DIM);
        let queries = data(50, 6);
        let g = build(Metric::L2, &values, &n);
        let v = vecs(&values, &n);
        let mut scratch = Scratch::new();
        let mut hit = 0;
        for q in queries.chunks(DIM) {
            let query = Query::new(Metric::L2, q);
            let got = hits(g.search(&query, v, 10, 64, None, None, &mut scratch));
            let want = exact(Metric::L2, &values, &n, q, 10, &|_| true);
            hit += got
                .iter()
                .filter(|h| want.iter().any(|w| w.1 == h.1))
                .count();
        }
        let recall = hit as f64 / 500.0;
        assert!(recall >= 0.95, "recall {recall}");
    }

    #[test]
    fn building_twice_gives_the_same_graph_and_the_seed_matters() {
        let values = data(1500, 7);
        let n = norms(&values, DIM);
        let v = vecs(&values, &n);
        let a = Graph::build(Metric::L2, v, params(), 11, &|_| true, &|| false);
        let b = Graph::build(Metric::L2, v, params(), 11, &|_| true, &|| false);
        let c = Graph::build(Metric::L2, v, params(), 12, &|_| true, &|| false);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    /// 图结构的固定摘要：x86_64 与 aarch64 上必须相同（0041 第 2 节「构建可重复」）。改动构建算法时
    /// 须同时更新这个常量，并在两种架构上确认。
    #[test]
    fn graph_is_identical_across_platforms() {
        let values = data(800, 9);
        let n = norms(&values, DIM);
        let mut digest = 0xcbf2_9ce4_8422_2325u64;
        for metric in [Metric::L2, Metric::Ip, Metric::Cosine] {
            let g = build(metric, &values, &n);
            let mut words: Vec<u32> = vec![g.entry.unwrap_or(u32::MAX), g.max_level.into()];
            words.extend(g.levels.iter().map(|&l| u32::from(l)));
            words.extend(&g.layer0);
            words.extend(&g.upper_start);
            words.extend(&g.upper);
            for w in words {
                for b in w.to_le_bytes() {
                    digest ^= u64::from(b);
                    digest = digest.wrapping_mul(0x0100_0000_01b3);
                }
            }
        }
        assert_eq!(
            digest, 0x235724962902a104,
            "update the pinned digest only after checking both architectures"
        );
    }

    #[test]
    fn filter_restricts_results_but_not_traversal() {
        let values = data(3000, 13);
        let n = norms(&values, DIM);
        let g = build(Metric::L2, &values, &n);
        let v = vecs(&values, &n);
        let mut scratch = Scratch::new();
        let q = &data(1, 14)[..];
        let query = Query::new(Metric::L2, q);
        let every_seventh = |r: u32| r.is_multiple_of(7);
        let got = hits(g.search(&query, v, 10, 64, Some(&every_seventh), None, &mut scratch));
        assert_eq!(got.len(), 10);
        assert!(got.iter().all(|(_, r)| r.is_multiple_of(7)));
        let want = exact(Metric::L2, &values, &n, q, 10, &every_seventh);
        assert_eq!(bits(&got), bits(&want));
        // 只允许一行：整张图都可能被遍历，但仍找得到它。
        let only = |r: u32| r == 2999;
        let got = hits(g.search(&query, v, 10, 64, Some(&only), None, &mut scratch));
        assert_eq!(got.iter().map(|h| h.1).collect::<Vec<_>>(), vec![2999]);
        let none = |_: u32| false;
        assert!(hits(g.search(&query, v, 10, 64, Some(&none), None, &mut scratch)).is_empty());
    }

    #[test]
    fn exceeding_the_distance_budget_reports_over_budget() {
        let values = data(3000, 15);
        let n = norms(&values, DIM);
        let g = build(Metric::L2, &values, &n);
        let v = vecs(&values, &n);
        let mut scratch = Scratch::new();
        let q = &data(1, 16)[..];
        let query = Query::new(Metric::L2, q);
        let rare = |r: u32| r == 5;
        assert_eq!(
            g.search(&query, v, 10, 64, Some(&rare), Some(100), &mut scratch),
            Search::OverBudget
        );
        assert!(matches!(
            g.search(&query, v, 10, 64, None, Some(1_000_000), &mut scratch),
            Search::Hits(h) if h.len() == 10
        ));
    }

    #[test]
    fn the_budget_stops_the_traversal_midway() {
        let values = data(3000, 25);
        let n = norms(&values, DIM);
        let g = build(Metric::L2, &values, &n);
        let v = vecs(&values, &n);
        let mut scratch = Scratch::new();
        let q = &data(1, 26)[..];
        let query = Query::new(Metric::L2, q);
        let entry = g.cand(&query, v, g.entry.expect("non-empty"));
        let none = |_: u32| false;
        let mut unbounded = 0;
        let found = g.search_layer(
            &query,
            v,
            &[entry],
            64,
            0,
            Some(&none),
            &mut scratch,
            &mut unbounded,
            u64::MAX,
        );
        assert_eq!(found, Some(vec![]));
        assert!(
            unbounded > 2000,
            "no admitted row: the whole component is traversed"
        );
        let mut bounded = 0;
        let found = g.search_layer(
            &query,
            v,
            &[entry],
            64,
            0,
            Some(&none),
            &mut scratch,
            &mut bounded,
            200,
        );
        assert_eq!(found, None);
        assert_eq!(bounded, 201);
    }

    #[test]
    fn rows_outside_the_graph_are_never_returned() {
        let values = data(500, 17);
        let n = norms(&values, DIM);
        let v = vecs(&values, &n);
        let odd = |r: u32| !r.is_multiple_of(2);
        let g = Graph::build(Metric::L2, v, params(), 3, &odd, &|| false).expect("built");
        let mut scratch = Scratch::new();
        for q in data(10, 18).chunks(DIM) {
            let query = Query::new(Metric::L2, q);
            let got = hits(g.search(&query, v, 20, 500, None, None, &mut scratch));
            assert!(got.iter().all(|(_, r)| !r.is_multiple_of(2)));
            assert_eq!(
                bits(&got),
                bits(&exact(Metric::L2, &values, &n, q, 20, &odd))
            );
        }
    }

    #[test]
    fn empty_single_row_and_k_beyond_rows() {
        let mut scratch = Scratch::new();
        let q = [0.5f32; DIM];
        let query = Query::new(Metric::L2, &q);
        let empty = vecs(&[], &[]);
        let g = Graph::build(Metric::L2, empty, params(), 1, &|_| true, &|| false).expect("built");
        assert_eq!(
            g.search(&query, empty, 10, 64, None, None, &mut scratch),
            Search::Hits(vec![])
        );
        let one = data(1, 19);
        let n = norms(&one, DIM);
        let g = build(Metric::L2, &one, &n);
        let v = vecs(&one, &n);
        assert_eq!(
            hits(g.search(&query, v, 10, 64, None, None, &mut scratch)).len(),
            1
        );
        assert!(hits(g.search(&query, v, 0, 64, None, None, &mut scratch)).is_empty());
    }

    #[test]
    fn nan_and_zero_vectors_do_not_break_build_or_order() {
        let mut values = data(400, 21);
        values[3 * DIM] = f32::NAN;
        values[10 * DIM..11 * DIM].fill(0.0);
        let n = norms(&values, DIM);
        for metric in [Metric::L2, Metric::Ip, Metric::Cosine] {
            let g = build(metric, &values, &n);
            let v = vecs(&values, &n);
            let mut scratch = Scratch::new();
            let q = &data(1, 22)[..];
            let query = Query::new(metric, q);
            let got = hits(g.search(&query, v, 400, 400, None, None, &mut scratch));
            assert_eq!(
                bits(&got),
                bits(&exact(metric, &values, &n, q, 400, &|_| true)),
                "{metric:?}"
            );
            assert!(got.last().is_some_and(|h| h.0.is_nan()));
        }
    }

    fn key(rows: u32) -> FileKey {
        FileKey {
            metric: Metric::Cosine,
            dim: DIM as u32,
            params: params(),
            rows,
            segment: "data_000001.parquet".into(),
            segment_len: 12345,
            seqs: 0x0123_4567_89ab_cdef,
        }
    }

    #[test]
    fn file_round_trip_and_every_mismatch_invalidates() {
        let mut values = data(700, 31);
        values[5 * DIM..6 * DIM].fill(0.0);
        let n = norms(&values, DIM);
        let v = vecs(&values, &n);
        let g =
            Graph::build(Metric::Cosine, v, params(), 4, &|r| r != 9, &|| false).expect("built");
        let k = key(700);
        let bytes = g.encode(&k);
        assert_eq!(Graph::decode(&bytes, &k).as_ref(), Some(&g));
        let mut keys = vec![
            FileKey {
                metric: Metric::Ip,
                ..k.clone()
            },
            FileKey {
                dim: 8,
                ..k.clone()
            },
            FileKey {
                rows: 699,
                ..k.clone()
            },
            FileKey {
                segment: "data_000002.parquet".into(),
                ..k.clone()
            },
            FileKey {
                segment_len: 12346,
                ..k.clone()
            },
            FileKey {
                seqs: 0x0123_4567_89ab_cdee,
                ..k.clone()
            },
        ];
        keys.push(FileKey {
            params: Params { m: 8, ..params() },
            ..k.clone()
        });
        keys.push(FileKey {
            params: Params {
                ef_construction: 100,
                ..params()
            },
            ..k.clone()
        });
        for other in &keys {
            assert_eq!(Graph::decode(&bytes, other), None, "{other:?}");
        }
        assert_eq!(Graph::decode(&bytes[..bytes.len() - 1], &k), None);
        assert_eq!(Graph::decode(&[], &k), None);
        for at in [0, 9, 20, bytes.len() / 2, bytes.len() - 1] {
            let mut bad = bytes.clone();
            bad[at] ^= 0x40;
            assert_eq!(Graph::decode(&bad, &k), None, "flip at {at}");
        }
        let mut old = bytes.clone();
        old[8..12].copy_from_slice(&0u32.to_le_bytes());
        let body = old.len() - 8;
        let sum = checksum(&old[..body]);
        old[body..].copy_from_slice(&sum.to_le_bytes());
        assert_eq!(Graph::decode(&old, &k), None, "other format version");
    }

    /// 校验和正确但结构错误的文件（例如另一实现写出的）也只作废，不 panic。
    #[test]
    fn structurally_invalid_files_with_valid_checksums_are_rejected() {
        let values = data(300, 33);
        let n = norms(&values, DIM);
        let g = build(Metric::L2, &values, &n);
        let k = FileKey {
            metric: Metric::L2,
            ..key(300)
        };
        // encode 只序列化、不校验，正好用来写出结构错误但校验和正确的文件。
        let reseal = |g: Graph| g.encode(&k);
        type Mutation = Box<dyn Fn(&mut Graph)>;
        let mutations: Vec<Mutation> = vec![
            Box::new(|g| g.layer0[1] = 300),
            Box::new(|g| g.layer0[0] = 33),
            Box::new(|g| g.entry = Some(299_999)),
            Box::new(|g| g.max_level = 40),
            Box::new(|g| {
                g.layer0.pop();
            }),
            Box::new(|g| g.upper.extend([0; 17])),
            Box::new(|g| {
                if let Some(row) = g.levels.iter().position(|&l| l > 0) {
                    g.upper_start[row] = u32::MAX;
                }
            }),
        ];
        for (i, mutate) in mutations.iter().enumerate() {
            let mut bad = g.clone();
            mutate(&mut bad);
            assert_ne!(bad, g, "mutation {i} changed nothing");
            let bytes = reseal(bad);
            assert_eq!(Graph::decode(&bytes, &k), None, "mutation {i}");
        }
    }

    #[test]
    fn interleaved_norms_select_the_right_column() {
        let values = data(400, 35);
        let n = norms(&values, DIM);
        let interleaved: Vec<f32> = n.iter().flat_map(|&x| [f32::NAN, x]).collect();
        let a = build(Metric::Cosine, &values, &n);
        let parts = [Part {
            start: 0,
            values: &values,
            norms: &interleaved,
        }];
        let v = Vectors::new(&parts, DIM, 2, 1).expect("valid layout");
        let b = Graph::build(Metric::Cosine, v, params(), 7, &|_| true, &|| false).expect("built");
        assert_eq!(a, b);
    }

    /// 重开后一个段分成多块：等长与不等长的切分都与整块建出同一张图，检索结果也相同。
    #[test]
    fn a_segment_split_into_parts_builds_the_same_graph() {
        let values = data(2500, 37);
        let n = norms(&values, DIM);
        let whole = build(Metric::Cosine, &values, &n);
        for cuts in [vec![0, 1024, 2048], vec![0, 7, 1500, 2499]] {
            let parts: Vec<Part> = cuts
                .iter()
                .enumerate()
                .map(|(i, &start)| {
                    let end = cuts.get(i + 1).copied().unwrap_or(2500);
                    Part {
                        start,
                        values: &values[start * DIM..end * DIM],
                        norms: &n[start..end],
                    }
                })
                .collect();
            let v = Vectors::new(&parts, DIM, 1, 0).expect("valid layout");
            let g =
                Graph::build(Metric::Cosine, v, params(), 7, &|_| true, &|| false).expect("built");
            assert_eq!(g, whole, "{cuts:?}");
            let q = &data(1, 38)[..];
            let query = Query::new(Metric::Cosine, q);
            let mut scratch = Scratch::new();
            assert_eq!(
                bits(&hits(g.search(&query, v, 10, 64, None, None, &mut scratch))),
                bits(&hits(whole.search(
                    &query,
                    vecs(&values, &n),
                    10,
                    64,
                    None,
                    None,
                    &mut scratch
                )))
            );
        }
        let gap = [
            Part {
                start: 0,
                values: &values[..DIM],
                norms: &n[..1],
            },
            Part {
                start: 2,
                values: &values[DIM..2 * DIM],
                norms: &n[1..2],
            },
        ];
        assert!(Vectors::new(&gap, DIM, 1, 0).is_none());
    }

    #[test]
    fn cancellation_stops_the_build() {
        let values = data(2000, 23);
        let n = norms(&values, DIM);
        let v = vecs(&values, &n);
        let calls = std::cell::Cell::new(0);
        let cancel = || {
            calls.set(calls.get() + 1);
            calls.get() > 2
        };
        assert!(Graph::build(Metric::L2, v, params(), 1, &|_| true, &cancel).is_none());
        assert_eq!(calls.get(), 3);
    }
}
