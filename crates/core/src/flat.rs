//! 暴力检索的距离计算，口径与 3.x `search/distance.py` 及对外换算一致：
//! COSINE 返回相似度（上限 1，零向量为 0），IP 返回点积，L2 返回平方距离。
//! 标量实现，同时作为后续 SIMD 版本的正确性基准。

use std::cmp::Ordering;

use crate::index::Metric;

pub(crate) struct Query<'a> {
    metric: Metric,
    vector: &'a [f32],
    /// COSINE 用：查询向量的范数倒数，零向量为 0。
    inv_norm: f32,
}

impl<'a> Query<'a> {
    pub(crate) fn new(metric: Metric, vector: &'a [f32]) -> Self {
        let norm = norm(vector);
        Self {
            metric,
            vector,
            inv_norm: if norm == 0.0 { 0.0 } else { 1.0 / norm },
        }
    }

    pub(crate) fn score(&self, candidate: &[f32]) -> f32 {
        match self.metric {
            Metric::Ip => dot(self.vector, candidate),
            Metric::L2 => self
                .vector
                .iter()
                .zip(candidate)
                .map(|(a, b)| (a - b) * (a - b))
                .sum(),
            Metric::Cosine => {
                let c_norm = norm(candidate);
                let sim = if c_norm == 0.0 {
                    0.0
                } else {
                    dot(self.vector, candidate) * self.inv_norm / c_norm
                };
                // 3.x：内部距离 np.maximum(1 - sim, 0)，对外 1 - 距离。np.maximum 传播 NaN，
                // 而 f32::max 会丢掉 NaN，因此单独处理。
                if sim.is_nan() {
                    sim
                } else {
                    1.0 - (1.0 - sim).max(0.0)
                }
            }
        }
    }

    /// 排序：越相似越靠前，NaN 一律排在最后（与 3.x 相同）。是全序，可用于 sort_by。
    pub(crate) fn rank(&self, a: f32, b: f32) -> Ordering {
        match (a.is_nan(), b.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                // 两者都不是 NaN，partial_cmp 必有结果。
                let ord = a.partial_cmp(&b).unwrap_or(Ordering::Equal);
                if self.metric.higher_is_better() {
                    ord.reverse()
                } else {
                    ord
                }
            }
        }
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn norm(v: &[f32]) -> f32 {
    dot(v, v).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_match_3x_conventions() {
        let q = [1.0, 2.0];
        let c = [3.0, -1.0];
        assert_eq!(Query::new(Metric::Ip, &q).score(&c), 1.0);
        assert_eq!(Query::new(Metric::L2, &q).score(&c), 13.0);
        let cos = Query::new(Metric::Cosine, &q).score(&c);
        assert!((cos - 1.0 / (5.0f32.sqrt() * 10.0f32.sqrt())).abs() < 1e-6);
    }

    #[test]
    fn cosine_edge_cases() {
        let q = Query::new(Metric::Cosine, &[1.0, 0.0]);
        assert_eq!(q.score(&[0.0, 0.0]), 0.0);
        assert!(q.score(&[2.0, 0.0]) <= 1.0);
        assert_eq!(
            Query::new(Metric::Cosine, &[0.0, 0.0]).score(&[1.0, 0.0]),
            0.0
        );
        assert!((q.score(&[-1.0, 0.0]) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn ordering_direction_and_nan_last() {
        let l2 = Query::new(Metric::L2, &[0.0]);
        let ip = Query::new(Metric::Ip, &[0.0]);
        assert_eq!(l2.rank(1.0, 2.0), Ordering::Less);
        assert_eq!(ip.rank(2.0, 1.0), Ordering::Less);
        assert_eq!(ip.rank(1.0, 1.0), Ordering::Equal);
        for q in [&l2, &ip] {
            assert_eq!(q.rank(f32::NAN, f32::INFINITY), Ordering::Greater);
            assert_eq!(q.rank(f32::NEG_INFINITY, -f32::NAN), Ordering::Less);
            assert_eq!(q.rank(f32::NAN, -f32::NAN), Ordering::Equal);
        }
        assert!(
            Query::new(Metric::Cosine, &[1.0, 0.0])
                .score(&[f32::NAN, 0.0])
                .is_nan()
        );
    }
}
