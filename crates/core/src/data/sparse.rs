//! 稀疏向量的存储与值规则（[0030](../../../../docs/decisions/0030-sparse-vectors.md)）。
//!
//! 3.x 的数据目录可能含有违反值规则的历史行，而 3.x 打开、计数、按原字节输出都不解码，只在输出时检查长度、
//! 在建倒排时规范化（才报错）。所以存储层原样保存每行字节，解码与规范化留到使用的时机。

use crate::error::{EngineError, Result};

/// 每行的打包字节：偏移数组加字节数组，避免每行一次堆分配。
#[derive(Debug, Clone, PartialEq)]
pub struct SparseData {
    /// 长度为行数加 1，首元素为 0。
    offsets: Vec<usize>,
    bytes: Vec<u8>,
}

impl Default for SparseData {
    fn default() -> Self {
        Self {
            offsets: vec![0],
            bytes: Vec::new(),
        }
    }
}

impl SparseData {
    pub fn from_rows<I, R>(rows: I) -> Self
    where
        I: IntoIterator<Item = R>,
        R: AsRef<[u8]>,
    {
        let mut out = Self::default();
        for row in rows {
            out.push(row.as_ref());
        }
        out
    }

    /// `n` 个空行（`b""`），用作空值占位。
    pub(crate) fn empty_rows(n: usize) -> Self {
        Self {
            offsets: vec![0; n + 1],
            bytes: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn row(&self, row: usize) -> &[u8] {
        &self.bytes[self.offsets[row]..self.offsets[row + 1]]
    }

    pub fn push(&mut self, row: &[u8]) {
        self.bytes.extend_from_slice(row);
        self.offsets.push(self.bytes.len());
    }

    pub(crate) fn extend(&mut self, other: &SparseData) {
        let base = self.bytes.len();
        self.bytes.extend_from_slice(&other.bytes);
        self.offsets
            .extend(other.offsets[1..].iter().map(|offset| base + offset));
    }

    pub fn rows(&self) -> impl ExactSizeIterator<Item = &[u8]> {
        (0..self.len()).map(|row| self.row(row))
    }
}

/// 3.x `bytes_to_sparse`：按字节顺序解码成 dict。重复的维度保留第一次出现的位置、取最后的值；
/// 不做任何值校验。
pub fn decode(bytes: &[u8]) -> Result<Vec<(u32, f32)>> {
    if !bytes.len().is_multiple_of(8) {
        return Err(EngineError::InvalidArgument(format!(
            "sparse vector bytes length {} is not a multiple of 8",
            bytes.len()
        )));
    }
    let pairs: Vec<(u32, f32)> = bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|pair| {
            let dim = u32::from_le_bytes([pair[0], pair[1], pair[2], pair[3]]);
            let weight = f32::from_le_bytes([pair[4], pair[5], pair[6], pair[7]]);
            (dim, weight)
        })
        .collect();
    Ok(dict_pairs(pairs))
}

/// Python dict 的赋值语义：重复的维度保留第一次出现的位置、取最后的值。
pub fn dict_pairs(mut pairs: Vec<(u32, f32)>) -> Vec<(u32, f32)> {
    // 新写入与 3.x 写入的行都严格递增，只有手工构造的行需要去重。
    if pairs.windows(2).any(|w| w[0].0 >= w[1].0) {
        let mut position: std::collections::HashMap<u32, usize> =
            std::collections::HashMap::with_capacity(pairs.len());
        let mut unique: Vec<(u32, f32)> = Vec::with_capacity(pairs.len());
        for (dim, weight) in pairs {
            match position.entry(dim) {
                std::collections::hash_map::Entry::Occupied(entry) => {
                    unique[*entry.get()].1 = weight;
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(unique.len());
                    unique.push((dim, weight));
                }
            }
        }
        pairs = unique;
    }
    pairs
}

/// 3.x `normalize_sparse_vector`：维度小于 2^32-1，权重有限且非负；零权重（含 -0.0）丢弃，其余保持顺序。
/// 错误是 3.x 的 `ValueError` 文案，调用方按场景加上字段前缀或原样报出。
pub fn normalize(
    pairs: &[(u32, f32)],
    allow_empty: bool,
) -> std::result::Result<Vec<(u32, f32)>, String> {
    let mut out = Vec::with_capacity(pairs.len());
    for &(dim, weight) in pairs {
        if dim == u32::MAX {
            return Err(format!(
                "sparse dimension {dim} must be non-negative and less than 2^32-1"
            ));
        }
        if !weight.is_finite() || weight < 0.0 {
            return Err(format!(
                "sparse weight at dimension {dim} must be finite, non-negative float32"
            ));
        }
        if weight != 0.0 {
            out.push((dim, weight));
        }
    }
    if out.is_empty() && !allow_empty {
        return Err("sparse vector must contain at least one nonzero float32 weight".to_owned());
    }
    Ok(out)
}

/// 3.x `sparse_to_bytes`：按维度排序后打包。
pub fn encode(pairs: &[(u32, f32)]) -> Vec<u8> {
    let mut sorted = pairs.to_vec();
    sorted.sort_unstable_by_key(|&(dim, _)| dim);
    let mut out = Vec::with_capacity(sorted.len() * 8);
    for (dim, weight) in sorted {
        out.extend_from_slice(&dim.to_le_bytes());
        out.extend_from_slice(&weight.to_le_bytes());
    }
    out
}

/// 输出编码中 `SparseFloatArray.dim` 的一行贡献：解码出的最大维度号加 1（含零权重），空行为 0。
/// 3.x 为了算它对每个 bytes 行调用 `bytes_to_sparse`，所以长度错误在这里报出。
pub fn output_dim(bytes: &[u8]) -> Result<i64> {
    if !bytes.len().is_multiple_of(8) {
        return Err(EngineError::InvalidArgument(format!(
            "sparse vector bytes length {} is not a multiple of 8",
            bytes.len()
        )));
    }
    // 最大维度号与去重无关，单趟扫描即可。
    Ok(bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|pair| i64::from(u32::from_le_bytes([pair[0], pair[1], pair[2], pair[3]])) + 1)
        .max()
        .unwrap_or(0))
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

    #[test]
    fn decode_keeps_byte_order_and_python_dict_duplicates() {
        assert_eq!(decode(b"").unwrap(), vec![]);
        assert_eq!(
            decode(&pack(&[(5, 1.0), (1, 2.0), (5, 3.0)])).unwrap(),
            vec![(5, 3.0), (1, 2.0)]
        );
        assert_eq!(
            decode(&pack(&[(9, 1.0), (2, 2.0)])).unwrap(),
            vec![(9, 1.0), (2, 2.0)]
        );
        assert_eq!(
            decode(&pack(&[(1, 1.0)])[..7]).unwrap_err().to_string(),
            "sparse vector bytes length 7 is not a multiple of 8"
        );
    }

    #[test]
    fn normalize_matches_python_rules() {
        assert_eq!(
            normalize(&[(1, -0.0), (3, 1.0), (2, 0.0)], false).unwrap(),
            vec![(3, 1.0)]
        );
        assert_eq!(
            normalize(&[(u32::MAX, 1.0)], true).unwrap_err(),
            "sparse dimension 4294967295 must be non-negative and less than 2^32-1"
        );
        for bad in [-1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                normalize(&[(1, bad)], true).unwrap_err(),
                "sparse weight at dimension 1 must be finite, non-negative float32"
            );
        }
        assert_eq!(normalize(&[(1, 0.0)], true).unwrap(), vec![]);
        assert_eq!(
            normalize(&[], false).unwrap_err(),
            "sparse vector must contain at least one nonzero float32 weight"
        );
        // 维度错误先于同一对的权重错误，与 3.x 的检查顺序一致。
        assert!(
            normalize(&[(u32::MAX, f32::NAN)], true)
                .unwrap_err()
                .starts_with("sparse dimension")
        );
    }

    #[test]
    fn encode_sorts_and_output_dim_counts_zero_weights() {
        assert_eq!(encode(&[(9, 1.0), (2, 2.0)]), pack(&[(2, 2.0), (9, 1.0)]));
        assert_eq!(output_dim(&pack(&[(1, 1.0), (7, 0.0)])).unwrap(), 8);
        assert_eq!(output_dim(b"").unwrap(), 0);
        assert_eq!(output_dim(&pack(&[(u32::MAX, 1.0)])).unwrap(), 1 << 32);
    }

    #[test]
    fn storage_rows_round_trip() {
        let mut data = SparseData::from_rows([pack(&[(1, 1.0)]), Vec::new()]);
        let other = SparseData::from_rows([pack(&[(2, 2.0), (3, 3.0)])]);
        data.extend(&other);
        assert_eq!(data.len(), 3);
        assert_eq!(data.row(1), b"");
        assert_eq!(data.row(2), pack(&[(2, 2.0), (3, 3.0)]).as_slice());
        let blanks = SparseData::empty_rows(2);
        assert_eq!(blanks.rows().collect::<Vec<_>>(), vec![b"" as &[u8], b""]);
    }
}
