//! 暴力检索的距离计算，口径与 3.x `search/distance.py` 及对外换算一致：
//! COSINE 返回相似度（上限 1，零向量为 0），IP 返回点积，L2 返回平方距离。
//!
//! 求和分 16 路累加，再两两归约，最后按顺序加上不足 16 个的尾部元素。求和顺序只由源码决定：
//! Rust 不会把乘加合并成 FMA，也不会重排浮点运算，所以同一输入在各平台、各指令集上结果逐位相同。
//! NaN 结果除外：Rust 不保证 NaN 的符号与 payload，传播哪个输入 NaN、还是产生平台默认 NaN，
//! 随平台与代码生成而变（x86_64 与 aarch64 的 release 构建上均可观察到）。因此同一查询单独打分
//! 与按 [`BATCH`] 条一组打分，可能得到位模式不同的 NaN，对外返回的 NaN 分数的符号与 payload
//! 不作保证。排序（[`rank`]）把所有 NaN 视为相同，结果顺序不受影响。
//! 顺序累加的误差在 dim 32768 时超出 0004「决策」的允许误差，分路累加后回到界内。
//! L2 必须逐元素相减再平方（0004「决策」），不能展开成 ‖q‖² + ‖v‖² − 2q·v。
//!
//! 累加函数单独成函数、不内联：与归约和尾部写在一起时，LLVM 在 aarch64 上只生成 2 路的向量代码，
//! 慢近一倍。改动这里的写法时要用基准确认没有退化。

use std::cmp::Ordering;

use crate::index::Metric;

const LANES: usize = 16;
type Lanes = [f32; LANES];

/// 同时打分的查询条数：候选向量读入一次，供这几条查询共用。
pub(crate) const BATCH: usize = 4;

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

    /// `candidate_norm` 是候选向量的范数（[`norm`]），只有 COSINE 使用。
    pub(crate) fn score(&self, candidate: &[f32], candidate_norm: f32) -> f32 {
        let raw = match self.metric {
            Metric::L2 => finish(
                diff_sq_lanes(self.vector, candidate),
                self.vector,
                candidate,
                diff_sq,
            ),
            Metric::Ip | Metric::Cosine => finish(
                product_lanes(self.vector, candidate),
                self.vector,
                candidate,
                product,
            ),
        };
        self.to_score(raw, candidate_norm)
    }

    /// L2 与 IP 原样返回；COSINE 由点积换算成相似度。
    fn to_score(&self, raw: f32, candidate_norm: f32) -> f32 {
        if self.metric != Metric::Cosine {
            return raw;
        }
        let sim = if candidate_norm == 0.0 {
            0.0
        } else {
            raw * self.inv_norm / candidate_norm
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

/// 排序：越相似越靠前，NaN 一律排在最后（与 3.x 相同）。是全序，可用于 sort_by。
pub(crate) fn rank(metric: Metric, a: f32, b: f32) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            // 两者都不是 NaN，partial_cmp 必有结果。
            let ord = a.partial_cmp(&b).unwrap_or(Ordering::Equal);
            if metric.higher_is_better() {
                ord.reverse()
            } else {
                ord
            }
        }
    }
}

/// [`BATCH`] 条同一度量的查询对同一个候选打分，结果与逐条调用 [`Query::score`] 逐位相同（NaN
/// 只保证仍是 NaN，见模块说明）：每条查询的每一路做同样顺序的运算，只是共用读入的候选向量。
pub(crate) fn score_batch(
    queries: &[Query; BATCH],
    candidate: &[f32],
    candidate_norm: f32,
) -> [f32; BATCH] {
    let vectors = queries.each_ref().map(|q| q.vector);
    let (lanes, op): (_, fn(f32, f32) -> f32) = match queries[0].metric {
        Metric::L2 => (diff_sq_lanes_batch(&vectors, candidate), diff_sq),
        Metric::Ip | Metric::Cosine => (product_lanes_batch(&vectors, candidate), product),
    };
    std::array::from_fn(|i| {
        let raw = finish(lanes[i], vectors[i], candidate, op);
        queries[i].to_score(raw, candidate_norm)
    })
}

/// 向量的范数，与 COSINE 打分中的点积同一算法。
pub(crate) fn norm(v: &[f32]) -> f32 {
    finish(product_lanes(v, v), v, v, product).sqrt()
}

fn diff_sq(a: f32, b: f32) -> f32 {
    (a - b) * (a - b)
}

fn product(a: f32, b: f32) -> f32 {
    a * b
}

/// 把 16 路两两归约，再按顺序加上尾部元素。
fn finish(lanes: Lanes, a: &[f32], b: &[f32], op: fn(f32, f32) -> f32) -> f32 {
    let mut width = LANES;
    let mut l = lanes;
    while width > 1 {
        width /= 2;
        for i in 0..width {
            l[i] += l[i + width];
        }
    }
    let body = a.len() / LANES * LANES;
    a[body..]
        .iter()
        .zip(&b[body..])
        .fold(l[0], |s, (x, y)| s + op(*x, *y))
}

#[inline(never)]
fn diff_sq_lanes(a: &[f32], b: &[f32]) -> Lanes {
    let mut acc = [0.0; LANES];
    for (x, y) in a
        .as_chunks::<LANES>()
        .0
        .iter()
        .zip(b.as_chunks::<LANES>().0)
    {
        for i in 0..LANES {
            let d = x[i] - y[i];
            acc[i] += d * d;
        }
    }
    acc
}

#[inline(never)]
fn product_lanes(a: &[f32], b: &[f32]) -> Lanes {
    let mut acc = [0.0; LANES];
    for (x, y) in a
        .as_chunks::<LANES>()
        .0
        .iter()
        .zip(b.as_chunks::<LANES>().0)
    {
        for i in 0..LANES {
            acc[i] += x[i] * y[i];
        }
    }
    acc
}

// 显式复用同一候选元素；每条查询的 16 路累加顺序和原归约保持不变。
#[inline(never)]
fn diff_sq_lanes_batch(qs: &[&[f32]; BATCH], b: &[f32]) -> [Lanes; BATCH] {
    let [q0, q1, q2, q3] = qs.map(|q| q.as_chunks::<LANES>().0);
    let (mut a0, mut a1, mut a2, mut a3) = ([0.0; LANES], [0.0; LANES], [0.0; LANES], [0.0; LANES]);
    for (k, y) in b.as_chunks::<LANES>().0.iter().enumerate() {
        let (x0, x1, x2, x3) = (&q0[k], &q1[k], &q2[k], &q3[k]);
        for i in 0..LANES {
            let d0 = x0[i] - y[i];
            let d1 = x1[i] - y[i];
            let d2 = x2[i] - y[i];
            let d3 = x3[i] - y[i];
            a0[i] += d0 * d0;
            a1[i] += d1 * d1;
            a2[i] += d2 * d2;
            a3[i] += d3 * d3;
        }
    }
    [a0, a1, a2, a3]
}

#[inline(never)]
fn product_lanes_batch(qs: &[&[f32]; BATCH], b: &[f32]) -> [Lanes; BATCH] {
    let [q0, q1, q2, q3] = qs.map(|q| q.as_chunks::<LANES>().0);
    let (mut a0, mut a1, mut a2, mut a3) = ([0.0; LANES], [0.0; LANES], [0.0; LANES], [0.0; LANES]);
    for (k, y) in b.as_chunks::<LANES>().0.iter().enumerate() {
        let (x0, x1, x2, x3) = (&q0[k], &q1[k], &q2[k], &q3[k]);
        for i in 0..LANES {
            a0[i] += x0[i] * y[i];
            a1[i] += x1[i] * y[i];
            a2[i] += x2[i] * y[i];
            a3[i] += x3[i] * y[i];
        }
    }
    [a0, a1, a2, a3]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(metric: Metric, q: &[f32], c: &[f32]) -> f32 {
        Query::new(metric, q).score(c, norm(c))
    }

    #[test]
    fn scores_match_3x_conventions() {
        let q = [1.0, 2.0];
        let c = [3.0, -1.0];
        assert_eq!(score(Metric::Ip, &q, &c), 1.0);
        assert_eq!(score(Metric::L2, &q, &c), 13.0);
        let cos = score(Metric::Cosine, &q, &c);
        assert!((cos - 1.0 / (5.0f32.sqrt() * 10.0f32.sqrt())).abs() < 1e-6);
    }

    #[test]
    fn cosine_edge_cases() {
        let q = [1.0, 0.0];
        assert_eq!(score(Metric::Cosine, &q, &[0.0, 0.0]), 0.0);
        assert!(score(Metric::Cosine, &q, &[2.0, 0.0]) <= 1.0);
        assert_eq!(score(Metric::Cosine, &[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert!((score(Metric::Cosine, &q, &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn ordering_direction_and_nan_last() {
        assert_eq!(rank(Metric::L2, 1.0, 2.0), Ordering::Less);
        assert_eq!(rank(Metric::Ip, 2.0, 1.0), Ordering::Less);
        assert_eq!(rank(Metric::Ip, 1.0, 1.0), Ordering::Equal);
        for m in [Metric::L2, Metric::Ip] {
            assert_eq!(rank(m, f32::NAN, f32::INFINITY), Ordering::Greater);
            assert_eq!(rank(m, f32::NEG_INFINITY, -f32::NAN), Ordering::Less);
            assert_eq!(rank(m, f32::NAN, -f32::NAN), Ordering::Equal);
        }
        assert!(score(Metric::Cosine, &[1.0, 0.0], &[f32::NAN, 0.0]).is_nan());
    }

    /// 求和顺序决定结果的末位，是对外可观察的：第 i 个元素累加到第 i % 16 路，
    /// 16 路按 l[i] += l[i + w]（w = 8, 4, 2, 1）两两归约，最后按顺序加上尾部。
    #[test]
    fn summation_order_is_fixed() {
        fn reference(v: &[f32]) -> f32 {
            let body = v.len() / 16 * 16;
            let mut l = [0.0f32; 16];
            for (k, x) in v[..body].iter().enumerate() {
                l[k % 16] += x;
            }
            for w in [8, 4, 2, 1] {
                for i in 0..w {
                    l[i] += l[i + w];
                }
            }
            v[body..].iter().fold(l[0], |s, x| s + x)
        }
        // 第 0 路与第 8 路的大数先相互抵消，第 1 路的 1 才保得住：相邻两两配对或顺序累加时，
        // 1e8 + 1 先被舍入掉，结果是 0.875 而不是 1.875。
        let mut c = vec![0.0f32; 19];
        c[0] = 1e8;
        c[1] = 1.0;
        c[8] = -1e8;
        c[16..].copy_from_slice(&[0.5, 0.25, 0.125]);
        assert_eq!(score(Metric::Ip, &[1.0; 19], &c), 1.875);
        // 大数与小数交错的一般情形。
        let mixed: Vec<f32> = (0..70)
            .map(|i| {
                if i % 3 == 0 {
                    1e7 + i as f32
                } else {
                    0.37 * i as f32
                }
            })
            .collect();
        for v in [&c, &mixed] {
            let got = score(Metric::Ip, &vec![1.0; v.len()], v);
            assert_eq!(got.to_bits(), reference(v).to_bits());
            let sequential: f32 = v.iter().sum();
            assert_ne!(got.to_bits(), sequential.to_bits());
        }
    }

    #[test]
    fn batch_scores_are_bit_identical_to_single_scores() {
        let mut x = 0x2545_f491_u32;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        for dim in [1, 15, 16, 17, 33, 128, 257] {
            let qs: Vec<Vec<f32>> = (0..BATCH)
                .map(|_| (0..dim).map(|_| next()).collect())
                .collect();
            let c: Vec<f32> = (0..dim).map(|_| next()).collect();
            for metric in [Metric::L2, Metric::Ip, Metric::Cosine] {
                let queries: [Query; BATCH] = std::array::from_fn(|i| Query::new(metric, &qs[i]));
                let batch = score_batch(&queries, &c, norm(&c));
                for i in 0..BATCH {
                    assert_eq!(batch[i].to_bits(), queries[i].score(&c, norm(&c)).to_bits());
                }
            }
        }
    }

    #[test]
    fn expanded_batch_kernel_matches_original_loops_including_special_values_and_tails() {
        let mut seed = 0x1a93_7d81_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32 / u32::MAX as f32 - 0.5) * 2000.0
        };
        for dim in [1, 15, 16, 17, 33, 128, 257, 1024, 32768] {
            let mut vectors: [Vec<f32>; BATCH] =
                std::array::from_fn(|_| (0..dim).map(|_| next()).collect());
            let mut candidate: Vec<f32> = (0..dim).map(|_| next()).collect();
            for special in [false, true] {
                if special {
                    let values = [
                        0.0,
                        -0.0,
                        f32::INFINITY,
                        f32::NEG_INFINITY,
                        f32::from_bits(0x7fc0_0123),
                        1e8,
                        -1e8,
                    ];
                    for (i, value) in candidate.iter_mut().enumerate() {
                        *value = values[i % values.len()];
                    }
                    for (qi, vector) in vectors.iter_mut().enumerate() {
                        for (i, value) in vector.iter_mut().enumerate() {
                            *value = values[(i + qi) % values.len()];
                        }
                    }
                }
                for metric in [Metric::L2, Metric::Ip, Metric::Cosine] {
                    let queries = std::array::from_fn(|i| Query::new(metric, &vectors[i]));
                    let mut old = [[0.0; LANES]; BATCH];
                    for (k, row) in candidate.as_chunks::<LANES>().0.iter().enumerate() {
                        for qi in 0..BATCH {
                            let query = &vectors[qi].as_chunks::<LANES>().0[k];
                            for i in 0..LANES {
                                old[qi][i] += if metric == Metric::L2 {
                                    let d = query[i] - row[i];
                                    d * d
                                } else {
                                    query[i] * row[i]
                                };
                            }
                        }
                    }
                    let op = if metric == Metric::L2 {
                        diff_sq
                    } else {
                        product
                    };
                    let expected: [f32; BATCH] = std::array::from_fn(|i| {
                        queries[i].to_score(
                            finish(old[i], &vectors[i], &candidate, op),
                            norm(&candidate),
                        )
                    });
                    let got = score_batch(&queries, &candidate, norm(&candidate));
                    for i in 0..BATCH {
                        let case =
                            format!("dim={dim}, metric={metric:?}, special={special}, query={i}");
                        // NaN 的符号与 payload 随平台与代码生成而变（见模块说明），只要求两边都是 NaN。
                        if expected[i].is_nan() {
                            assert!(got[i].is_nan(), "{case}: got {}", got[i]);
                        } else {
                            assert_eq!(got[i].to_bits(), expected[i].to_bits(), "{case}");
                        }
                    }
                }
            }
        }
    }
}
