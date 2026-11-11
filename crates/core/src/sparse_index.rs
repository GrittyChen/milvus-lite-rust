//! 稀疏向量的 IP 倒排（[0030](../../../docs/decisions/0030-sparse-vectors.md)），对应 3.x `SparseIpIndex`。
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
