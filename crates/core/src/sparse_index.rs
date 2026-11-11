//! 稀疏向量的 IP 倒排（[0030](../../../docs/decisions/0030-sparse-vectors.md)），对应 3.x `SparseIpIndex`；
//! BM25 输出字段的 tf 倒排与块统计（[0031](../../../docs/decisions/0031-bm25.md)），对应 3.x `SparseInvertedIndex`。
//!
//! 3.x 的打分：查询按维度的 dict 顺序遍历，每个维度按行号顺序遍历倒排，`scores[row] += q * w`（double）；
//! 同一数据源内 `heapq.nlargest` 按 double 分数取前 k，并列时保持插入 dict 的先后，即先比首个命中的
//! 查询维度序号、再比行号。这里保留 double 分数与首个命中的维度序号，交给检索的全序（见 `RankedRow`）。

use crate::data::sparse;
use crate::error::{EngineError, Result};

/// 一个不可变数据块（或一次检索的 MemTable 行集）的倒排：维度升序，每个维度的行号升序。
#[derive(Debug, Default)]
pub(crate) struct Postings {
    dims: Vec<u32>,
    /// 长度为 `dims.len() + 1`。
    offsets: Vec<usize>,
    rows: Vec<u32>,
    weights: Vec<f32>,
    /// BM25 倒排的块统计；IP 倒排为 None。
    bm25: Option<Bm25Block>,
}

/// BM25 的块统计。倒排只含 tf、doc_len、行数与 df，不含 k1 / b：它们在打分时读当前的索引定义。
#[derive(Debug, Default)]
struct Bm25Block {
    /// 计入 N 的行数：已提交块是全部物理行，MemTable 是存活且通过过滤的行。
    count: usize,
    /// 3.x `doc_len = sum(sv.values())` 的累加（float64，未舍入），按行号顺序。
    total: f64,
    /// 每行的 doc_len，3.x 存在 float32 数组里；不计入 N 的行为 0。
    doc_len: Vec<f32>,
}

impl Postings {
    /// `rows` 按行号升序给出规范化后的 `(维度, 权重)`；被排除的行不给出。
    pub(crate) fn build<'a>(rows: impl IntoIterator<Item = (u32, &'a [(u32, f32)])>) -> Self {
        let mut triples: Vec<(u32, u32, f32)> = Vec::new();
        for (row, pairs) in rows {
            triples.extend(pairs.iter().map(|&(dim, weight)| (dim, row, weight)));
        }
        // 稳定排序：同一维度内保持行号升序。
        triples.sort_by_key(|&(dim, _, _)| dim);
        let mut out = Self {
            offsets: vec![0],
            ..Self::default()
        };
        // 3.x 的倒排列表按行号追加，维度内保持行号升序。
        for (dim, row, weight) in triples {
            if out.dims.last() != Some(&dim) {
                if !out.dims.is_empty() {
                    out.offsets.push(out.rows.len());
                }
                out.dims.push(dim);
            }
            out.rows.push(row);
            out.weights.push(weight);
        }
        if !out.dims.is_empty() {
            out.offsets.push(out.rows.len());
        }
        out
    }

    fn list(&self, dim: u32) -> (&[u32], &[f32]) {
        match self.dims.binary_search(&dim) {
            Ok(i) => {
                let range = self.offsets[i]..self.offsets[i + 1];
                (&self.rows[range.clone()], &self.weights[range])
            }
            Err(_) => (&[], &[]),
        }
    }
}

/// 3.x 为每个 segment 建倒排：解码并规范化全部物理行（含已删除、旧版本与空值行），任何一行违反值规则都报错
/// （`ValueError`，code 1，文案不带字段名）。空值行（`valid` 为假）与空行不建倒排。
pub(crate) fn build_segment(
    data: &sparse::SparseData,
    valid: impl Fn(usize) -> bool,
) -> Result<Postings> {
    let mut normalized = Vec::with_capacity(data.len());
    for (row, bytes) in data.rows().enumerate() {
        if !valid(row) {
            normalized.push(Vec::new());
            continue;
        }
        let pairs = sparse::decode(bytes)?;
        normalized.push(sparse::normalize(&pairs, true).map_err(EngineError::InvalidArgument)?);
    }
    Ok(Postings::build(
        normalized
            .iter()
            .enumerate()
            .map(|(row, pairs)| (row as u32, pairs.as_slice())),
    ))
}

/// 3.x `SparseInvertedIndex.build`：`rows` 按行号升序给出计入 N 的行（dict 语义解码后的原值，不规范化、
/// 不校验，空行也计入），`slots` 是行号的上界。doc_len 是 CPython 3.12 `sum()` 的结果，存为 float32；
/// 总长度累加未舍入的值。
pub(crate) fn build_bm25<'a>(
    rows: impl IntoIterator<Item = (u32, &'a [(u32, f32)])>,
    slots: usize,
) -> Postings {
    let mut block = Bm25Block {
        doc_len: vec![0.0; slots],
        ..Bm25Block::default()
    };
    let mut kept = Vec::new();
    for (row, pairs) in rows {
        block.count += 1;
        let len = crate::rerank::pyval::python_sum(pairs.iter().map(|&(_, w)| f64::from(w)));
        block.doc_len[row as usize] = len as f32;
        block.total += len;
        kept.push((row, pairs));
    }
    let mut postings = Postings::build(kept);
    postings.bm25 = Some(block);
    postings
}

/// 3.x 为 BM25 字段的每个 segment 建倒排：全部物理行（含已删除、旧版本与空值行）都计入 N，空值行是空 dict。
/// 只解码，字节长度错误照样报出。
pub(crate) fn build_bm25_segment(
    data: &sparse::SparseData,
    valid: impl Fn(usize) -> bool,
) -> Result<Postings> {
    let mut decoded = Vec::with_capacity(data.len());
    for (row, bytes) in data.rows().enumerate() {
        decoded.push(if valid(row) {
            sparse::decode(bytes)?
        } else {
            Vec::new()
        });
    }
    Ok(build_bm25(
        decoded
            .iter()
            .enumerate()
            .map(|(row, pairs)| (row as u32, pairs.as_slice())),
        decoded.len(),
    ))
}

/// 一个数据源（3.x 的一个 segment 或 MemTable）的 BM25 统计：由它的各块汇总。3.x 对一个 segment 只建一个
/// 倒排，本实现的一个 segment 可以有多个块。
pub(crate) struct Bm25Source {
    count: usize,
    k1: f64,
    /// 下面几项是 3.x 运算里转成 float32 的 Python 浮点（NumPy 2 / NEP 50）。
    k1_f32: f32,
    b_f32: f32,
    one_minus_b_f32: f32,
    avgdl_f32: f32,
}

impl Bm25Source {
    /// `blocks` 是同一数据源的各块；没有计入 N 的行时返回 None（3.x `doc_count == 0` 时不打分）。
    /// 块的总长度按块序以 float64 相加：整数 tf（写入时生成的都是）的和是精确的，与 3.x 逐行相加相同。
    pub(crate) fn new(blocks: &[&Postings], k1: f64, b: f64) -> Option<Self> {
        let (count, total) = blocks
            .iter()
            .filter_map(|p| p.bm25.as_ref())
            .fold((0usize, 0.0f64), |(n, t), block| {
                (n + block.count, t + block.total)
            });
        if count == 0 {
            return None;
        }
        let avgdl = total / count as f64;
        let avgdl = if avgdl > 0.0 { avgdl } else { 1.0 };
        Some(Self {
            count,
            k1,
            k1_f32: k1 as f32,
            b_f32: b as f32,
            one_minus_b_f32: (1.0 - b) as f32,
            avgdl_f32: avgdl as f32,
        })
    }

    /// 一条查询每个查询词的 IDF（转成 float32 后参与运算）；df 为 0 的词没有命中，为 None。
    /// 3.x：`math.log((N - df + 0.5) / (df + 0.5) + 1.0)`，N 与 df 是整数，df 是各块倒排列表长度之和。
    pub(crate) fn idf(&self, blocks: &[&Postings], query: &[(u32, f32)]) -> Vec<Option<f32>> {
        query
            .iter()
            .map(|&(dim, _)| {
                let df: usize = blocks.iter().map(|p| p.list(dim).0.len()).sum();
                (df > 0).then(|| {
                    (((self.count - df) as f64 + 0.5) / (df as f64 + 0.5) + 1.0).ln() as f32
                })
            })
            .collect()
    }
}

/// 3.x `SparseInvertedIndex.search` 在一个块上的累加，按 NumPy 2 的类型提升逐步舍入：`tf·(k1+1)` 在
/// float64 中算出，其余每一步与累加都是 float32（Python 浮点在进入运算时转 float32）。分数从 `0.0 + 第一项`
/// 开始（与 Python 的 `scores.get(id, 0.0) + x` 相同，-0.0 变为 0.0），以 float64 存放 float32 的值。
pub(crate) fn score_bm25(
    postings: &Postings,
    source: &Bm25Source,
    idf: &[Option<f32>],
    query: &[(u32, f32)],
    live: impl Fn(u32) -> bool,
    scratch: &mut Scratch,
    mut emit: impl FnMut(u32, Score),
) {
    let Some(block) = &postings.bm25 else {
        return;
    };
    let k1_plus_1 = source.k1 + 1.0;
    for (index, (&(dim, query_weight), idf)) in query.iter().zip(idf).enumerate() {
        let Some(idf) = *idf else {
            continue;
        };
        let (rows, weights) = postings.list(dim);
        for (&row, &tf) in rows.iter().zip(weights) {
            if !live(row) {
                continue;
            }
            let slot = row as usize;
            let dl = block.doc_len[slot];
            let x = source.b_f32 * dl / source.avgdl_f32;
            let x = source.k1_f32 * (source.one_minus_b_f32 + x);
            let tf_norm = ((f64::from(tf) * k1_plus_1) as f32) / (tf + x);
            let term = idf * tf_norm * query_weight;
            if scratch.first_hit[slot] == UNTOUCHED {
                scratch.first_hit[slot] = index as u32;
                scratch.exact[slot] = 0.0;
                scratch.touched.push(row);
            }
            scratch.exact[slot] = f64::from(scratch.exact[slot] as f32 + term);
        }
    }
    for row in scratch.touched.drain(..) {
        let slot = row as usize;
        let first_hit = std::mem::replace(&mut scratch.first_hit[slot], UNTOUCHED);
        emit(
            row,
            Score {
                exact: scratch.exact[slot],
                first_hit,
            },
        );
    }
}

/// 一行的打分：double 分数与首个命中的查询维度序号。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Score {
    pub(crate) exact: f64,
    pub(crate) first_hit: u32,
}

/// 按 3.x 的累加顺序为 `query` 打分，只看 `live` 为真的行；`emit` 收到每个至少命中一个维度的行。
/// `scratch` 由调用方复用，长度不小于倒排的行数。
pub(crate) fn score(
    postings: &Postings,
    query: &[(u32, f32)],
    live: impl Fn(u32) -> bool,
    scratch: &mut Scratch,
    mut emit: impl FnMut(u32, Score),
) {
    for (index, &(dim, query_weight)) in query.iter().enumerate() {
        let (rows, weights) = postings.list(dim);
        for (&row, &weight) in rows.iter().zip(weights) {
            if !live(row) {
                continue;
            }
            let slot = row as usize;
            if scratch.first_hit[slot] == UNTOUCHED {
                scratch.first_hit[slot] = index as u32;
                scratch.exact[slot] = 0.0;
                scratch.touched.push(row);
            }
            scratch.exact[slot] += f64::from(query_weight) * f64::from(weight);
        }
    }
    for row in scratch.touched.drain(..) {
        let slot = row as usize;
        let first_hit = std::mem::replace(&mut scratch.first_hit[slot], UNTOUCHED);
        emit(
            row,
            Score {
                exact: scratch.exact[slot],
                first_hit,
            },
        );
    }
}

/// 未被当前查询触及的行。查询的维度数来自打包字节的长度，到不了 `u32::MAX`。
const UNTOUCHED: u32 = u32::MAX;

/// 打分的工作区：每行的 double 分数与首个命中的维度序号（每行 12 字节），以及被触及的行。
#[derive(Default)]
pub(crate) struct Scratch {
    exact: Vec<f64>,
    first_hit: Vec<u32>,
    touched: Vec<u32>,
}

impl Scratch {
    pub(crate) fn with_rows(rows: usize) -> Self {
        Self {
            exact: vec![0.0; rows],
            first_hit: vec![UNTOUCHED; rows],
            touched: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(pairs: &[(u32, f32)]) -> Vec<u8> {
        pairs
            .iter()
            .flat_map(|(d, w)| d.to_le_bytes().into_iter().chain(w.to_le_bytes()))
            .collect()
    }

    fn scores(postings: &Postings, query: &[(u32, f32)], rows: usize) -> Vec<(u32, Score)> {
        let mut out = Vec::new();
        let mut scratch = Scratch::with_rows(rows);
        score(
            postings,
            query,
            |_| true,
            &mut scratch,
            |row, s| out.push((row, s)),
        );
        out.sort_by_key(|&(row, _)| row);
        out
    }

    #[test]
    fn scores_accumulate_in_query_order_and_record_first_hit() {
        let data = sparse::SparseData::from_rows([
            pack(&[(1, 1.0), (5, 2.0)]),
            pack(&[(5, 3.0)]),
            Vec::new(),
            pack(&[(9, 1.0)]),
        ]);
        let postings = build_segment(&data, |_| true).unwrap();
        let got = scores(&postings, &[(5, 1.0), (1, 0.5)], 4);
        assert_eq!(
            got,
            vec![
                (
                    0,
                    Score {
                        exact: 2.5,
                        first_hit: 0
                    }
                ),
                (
                    1,
                    Score {
                        exact: 3.0,
                        first_hit: 0
                    }
                ),
            ]
        );
        // 维度 1 先出现时，行 0 的首个命中维度序号为 0，行 1 为 1。
        let got = scores(&postings, &[(1, 1.0), (5, 1.0)], 4);
        assert_eq!(got[0].1.first_hit, 0);
        assert_eq!(got[1].1.first_hit, 1);
        // f32 下溢的乘积在 double 中仍为正，行照样返回。
        let tiny = sparse::SparseData::from_rows([pack(&[(0, 1e-30)])]);
        let got = scores(&build_segment(&tiny, |_| true).unwrap(), &[(0, 1e-30)], 1);
        assert!(got[0].1.exact > 0.0 && (got[0].1.exact as f32) == 0.0);
    }

    fn bm25_scores(
        blocks: &[&Postings],
        query: &[(u32, f32)],
        k1: f64,
        b: f64,
    ) -> Vec<Vec<(u32, Score)>> {
        let source = Bm25Source::new(blocks, k1, b).unwrap();
        let idf = source.idf(blocks, query);
        blocks
            .iter()
            .map(|block| {
                let mut out = Vec::new();
                let rows = block.bm25.as_ref().unwrap().doc_len.len();
                let mut scratch = Scratch::with_rows(rows);
                score_bm25(
                    block,
                    &source,
                    &idf,
                    query,
                    |_| true,
                    &mut scratch,
                    |row, s| out.push((row, s)),
                );
                out.sort_by_key(|&(row, _)| row);
                out
            })
            .collect()
    }

    /// 一个 segment 拆成多个块时，N、总长度与 df 按数据源汇总，分数与整块逐位相同（整数 tf）。
    #[test]
    fn bm25_blocks_of_one_source_score_like_a_single_block() {
        let rows: Vec<Vec<(u32, f32)>> = vec![
            vec![(1, 1.0), (2, 2.0)],
            vec![],
            vec![(2, 3.0), (7, 1.0)],
            vec![(1, 4.0)],
            vec![(7, 2.0), (1, 1.0), (9, 5.0)],
        ];
        let block = |range: std::ops::Range<usize>| {
            build_bm25(
                rows[range.clone()]
                    .iter()
                    .enumerate()
                    .map(|(row, pairs)| (row as u32, pairs.as_slice())),
                range.len(),
            )
        };
        let whole = block(0..5);
        let (left, right) = (block(0..2), block(2..5));
        let query = [(1, 1.0), (7, 2.0), (2, 1.0), (4, 1.0)];
        for (k1, b) in [(1.5, 0.75), (0.3, 0.0), (2.0, 1.0)] {
            let single = bm25_scores(&[&whole], &query, k1, b).remove(0);
            let split = bm25_scores(&[&left, &right], &query, k1, b);
            let shifted: Vec<(u32, Score)> = split[0]
                .iter()
                .copied()
                .chain(split[1].iter().map(|&(row, s)| (row + 2, s)))
                .collect();
            assert_eq!(single, shifted);
        }
        // 第 3 个查询词（维度 2）在行 0 与行 2 中，首个命中的序号来自查询词顺序。
        let single = bm25_scores(&[&whole], &query, 1.5, 0.75).remove(0);
        assert_eq!(
            single
                .iter()
                .map(|(row, s)| (*row, s.first_hit))
                .collect::<Vec<_>>(),
            vec![(0, 0), (2, 1), (3, 0), (4, 0)]
        );
        // 只有空行的数据源：N 不为 0，avgdl 取 1，没有命中；没有行时不打分。
        let empty = build_bm25([(0, [].as_slice())], 1);
        assert!(bm25_scores(&[&empty], &query, 1.5, 0.75)[0].is_empty());
        assert!(Bm25Source::new(&[&build_bm25([], 0)], 1.5, 0.75).is_none());
    }

    /// 块与数据源的总长度以 float64 逐行累加，avgdl 在 float64 中算出后才转 float32（3.x 的 `total_length` 是
    /// Python 浮点）；非整数 tf 时用 float32 累加会得到不同的 avgdl。
    #[test]
    fn bm25_total_length_accumulates_in_f64() {
        let rows: Vec<Vec<(u32, f32)>> = (0..1000).map(|_| vec![(1, 0.1)]).collect();
        let block = build_bm25(
            rows.iter()
                .enumerate()
                .map(|(row, pairs)| (row as u32, pairs.as_slice())),
            rows.len(),
        );
        let total = (0..1000).fold(0.0f64, |t, _| t + f64::from(0.1f32));
        let source = Bm25Source::new(&[&block], 1.5, 0.75).unwrap();
        assert_eq!(source.avgdl_f32, (total / 1000.0) as f32);
        assert_ne!(
            source.avgdl_f32,
            (0..1000).fold(0.0f32, |t, _| t + 0.1) / 1000.0,
            "数据须能区分 float32 累加"
        );
    }

    /// doc_len 是 CPython 3.12 `sum()`（Neumaier 补偿）的结果，再存为 float32。
    #[test]
    fn bm25_doc_len_uses_python_sum() {
        // 逐项相加得 0（1 被 1e30 吞掉），CPython 3.12 的 sum() 得 1。
        let pairs = [(1, 1e30_f32), (2, 1.0), (3, -1e30)];
        let block = build_bm25([(0, pairs.as_slice())], 1);
        let stats = block.bm25.as_ref().unwrap();
        assert_eq!(stats.doc_len, vec![1.0]);
        assert_eq!(stats.total, 1.0);
    }

    #[test]
    fn segment_build_rejects_bad_rows_like_3x_but_skips_nulls() {
        let bad = sparse::SparseData::from_rows([pack(&[(1, 1.0)]), pack(&[(1, -1.0)])]);
        let error = build_segment(&bad, |_| true).unwrap_err();
        assert!(matches!(error, EngineError::InvalidArgument(_)));
        assert_eq!(
            error.to_string(),
            "sparse weight at dimension 1 must be finite, non-negative float32"
        );
        assert!(build_segment(&bad, |row| row == 0).is_ok());
        let short = sparse::SparseData::from_rows([vec![0; 7]]);
        assert_eq!(
            build_segment(&short, |_| true).unwrap_err().to_string(),
            "sparse vector bytes length 7 is not a multiple of 8"
        );
        // 零权重与乱序、重复维度按 dict 语义规范化，不报错。
        let odd = sparse::SparseData::from_rows([pack(&[(5, 1.0), (1, 0.0), (5, 3.0)])]);
        let postings = build_segment(&odd, |_| true).unwrap();
        assert_eq!(scores(&postings, &[(5, 1.0)], 1)[0].1.exact, 3.0);
        assert!(scores(&postings, &[(1, 1.0)], 1).is_empty());
    }
}
