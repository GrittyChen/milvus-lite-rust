//! HNSW 图（0041）：单个 segment 一张图，只存邻接表，向量读 segment 的列。
//!
//! 打分全部经 [`flat::Query::score`]，返回的分数与 FLAT 对同一行算出的值逐位相同；图内的比较用
//! [`order_key`]，与 [`flat::rank`] 的顺序一致（NaN 最差，平局按行号）。构建单线程、按行号顺序插入，
//! 层级由调用方给的种子决定；由于打分逐位确定（flat.rs 模块说明），同一输入在各平台建出的图相同。
//!
//! 选邻居用 hnswlib 的启发式（`getNeighborsByHeuristic2`）；第 0 层每点至多 2M 个邻居，上层至多 M 个。
//! 检索时遍历不受过滤限制，只有允许的行进入结果（与 faiss `IDSelector` 同义），因此过滤下也能返回
//! 足 k 条；代价是通过率低时会遍历大半张图，调用方用距离计算预算截断并改走 FLAT（0041「据此确定的取值」）。

// 第 4 步接入检索之前，只有单测使用。
#![cfg_attr(not(test), allow(dead_code))]

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

/// 一个 segment 的向量列：`values` 按行展开，`norms` 为每行范数（[`flat::norm`]，只有 COSINE 使用）。
#[derive(Clone, Copy)]
pub(crate) struct Vectors<'a> {
    pub values: &'a [f32],
    pub dim: usize,
    pub norms: &'a [f32],
}

impl<'a> Vectors<'a> {
    fn row(&self, i: u32) -> &'a [f32] {
        let start = i as usize * self.dim;
        &self.values[start..start + self.dim]
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
        let rows =
            u32::try_from(vectors.values.len().checked_div(vectors.dim).unwrap_or(0)).ok()?;
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
        let score = query.score(vectors.row(row), vectors.norms[row as usize]);
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

    fn params() -> Params {
        Params {
            m: DEFAULT_M,
            ef_construction: DEFAULT_EF_CONSTRUCTION,
        }
    }

    fn build(metric: Metric, values: &[f32], norms: &[f32]) -> Graph {
        let v = Vectors {
            values,
            dim: DIM,
            norms,
        };
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
            let v = Vectors {
                values: &values,
                dim: DIM,
                norms: &n,
            };
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
            let v = Vectors {
                values: &values,
                dim: DIM,
                norms: &n,
            };
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
        let v = Vectors {
            values: &values,
            dim: DIM,
            norms: &n,
        };
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
        let v = Vectors {
            values: &values,
            dim: DIM,
            norms: &n,
        };
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
        let v = Vectors {
            values: &values,
            dim: DIM,
            norms: &n,
        };
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
        let v = Vectors {
            values: &values,
            dim: DIM,
            norms: &n,
        };
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
        let v = Vectors {
            values: &values,
            dim: DIM,
            norms: &n,
        };
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
        let v = Vectors {
            values: &values,
            dim: DIM,
            norms: &n,
        };
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
        let empty = Vectors {
            values: &[],
            dim: DIM,
            norms: &[],
        };
        let g = Graph::build(Metric::L2, empty, params(), 1, &|_| true, &|| false).expect("built");
        assert_eq!(
            g.search(&query, empty, 10, 64, None, None, &mut scratch),
            Search::Hits(vec![])
        );
        let one = data(1, 19);
        let n = norms(&one, DIM);
        let g = build(Metric::L2, &one, &n);
        let v = Vectors {
            values: &one,
            dim: DIM,
            norms: &n,
        };
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
            let v = Vectors {
                values: &values,
                dim: DIM,
                norms: &n,
            };
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

    #[test]
    fn cancellation_stops_the_build() {
        let values = data(2000, 23);
        let n = norms(&values, DIM);
        let v = Vectors {
            values: &values,
            dim: DIM,
            norms: &n,
        };
        let calls = std::cell::Cell::new(0);
        let cancel = || {
            calls.set(calls.get() + 1);
            calls.get() > 2
        };
        assert!(Graph::build(Metric::L2, v, params(), 1, &|_| true, &cancel).is_none());
        assert_eq!(calls.get(), 3);
    }
}
