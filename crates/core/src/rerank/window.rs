//! 三条路径的候选窗口（0026 第 6 节）。公式彼此不同，也不同于 0024 的分组窗口（那里含 offset），
//! 分开写才不会被“看起来可以合并”误导。乘法在 i128 上饱和；结果不为正时该路径没有候选。

fn clamp(value: i128) -> i64 {
    i64::try_from(value).unwrap_or(if value < 0 { i64::MIN } else { i64::MAX })
}

/// Search + boost（引擎内）：`end = limit + offset`；有分组 `max(end · group_size · 3, end · 10)`，
/// 有范围参数再不小于 `end · 5`，最后不小于 `end · 10`。
pub fn search_boost(end: i64, group_size: Option<i64>, ranged: bool) -> usize {
    let end = i128::from(end);
    let tenfold = end.saturating_mul(10);
    let mut window = match group_size {
        Some(size) => end
            .saturating_mul(i128::from(size))
            .saturating_mul(3)
            .max(tenfold),
        None => end,
    };
    if ranged {
        window = window.max(end.saturating_mul(5));
    }
    usize::try_from(window.max(tenfold).max(0)).unwrap_or(usize::MAX)
}

/// Search + L2：交给引擎的 `top_k`（此时引擎的 offset 为 0、分组关闭，有 boost 时引擎再放大）。
/// `group_size` 只在有分组时给出，且已是 `group_size or 1`。
pub fn search_l2(limit: i64, offset: i64, group_size: Option<i64>) -> i64 {
    let limit = i128::from(limit);
    let mut window = limit.saturating_add(i128::from(offset));
    if let Some(size) = group_size {
        window = window.max(limit.saturating_mul(i128::from(size)).saturating_mul(3));
    }
    clamp(window.max(limit.saturating_mul(10)))
}

/// Hybrid 每一路交给引擎的 `top_k`：路由自己的 `topk`，有分组时不小于 `limit · group_size · 3`，
/// 有 boost 时不小于 `limit · 10`。这里的 `limit` 是 `rank_params` 的最终 limit。
pub fn hybrid_route(route_top_k: i64, limit: i64, group_size: Option<i64>, boost: bool) -> i64 {
    let limit = i128::from(limit);
    let mut window = i128::from(route_top_k);
    if let Some(size) = group_size {
        window = window.max(limit.saturating_mul(i128::from(size)).saturating_mul(3));
    }
    if boost {
        window = window.max(limit.saturating_mul(10));
    }
    clamp(window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_boost_matches_the_engine_formula() {
        // 探针：limit=1, offset=30 的窗口是 310。
        assert_eq!(search_boost(31, None, false), 310);
        assert_eq!(search_boost(5, None, true), 50);
        // 分组：max(end·gs·3, end·10)。
        assert_eq!(search_boost(3, Some(2), false), 30);
        assert_eq!(search_boost(3, Some(5), false), 45);
        assert_eq!(search_boost(3, Some(5), true), 45);
        // 负 group_size 不缩小窗口，负 end 没有候选。
        assert_eq!(search_boost(3, Some(-1), false), 30);
        assert_eq!(search_boost(0, None, false), 0);
        assert_eq!(search_boost(-1, Some(5), true), 0);
        assert_eq!(search_boost(i64::MAX, Some(i64::MAX), true), usize::MAX);
    }

    #[test]
    fn search_l2_keeps_limit_and_offset_apart() {
        // 探针：limit=5 时 offset=0 与 offset=3 的窗口都是 50。
        assert_eq!(search_l2(5, 0, None), 50);
        assert_eq!(search_l2(5, 3, None), 50);
        assert_eq!(search_l2(5, 100, None), 105);
        // 分组窗口不含 offset。
        assert_eq!(search_l2(5, 3, Some(4)), 60);
        assert_eq!(search_l2(5, 100, Some(4)), 105);
        // limit <= 0：limit=0, offset=5 有 5 个候选；limit=-1, offset=5 有 4 个；其余没有。
        assert_eq!(search_l2(0, 5, None), 5);
        assert_eq!(search_l2(-1, 5, None), 4);
        assert_eq!(search_l2(-3, 5, None), 2);
        assert_eq!(search_l2(0, 0, None), 0);
        assert_eq!(search_l2(-1, 0, Some(3)), -1);
        assert_eq!(search_l2(i64::MAX, i64::MAX, Some(i64::MAX)), i64::MAX);
        assert_eq!(search_l2(i64::MIN, i64::MIN, None), i64::MIN);
    }

    #[test]
    fn hybrid_route_only_grows_with_group_or_boost() {
        // 没有分组和 boost 时最终 limit 不放大候选。
        assert_eq!(hybrid_route(5, 100, None, false), 5);
        assert_eq!(hybrid_route(0, 100, None, false), 0);
        assert_eq!(hybrid_route(-1, 100, None, false), -1);
        assert_eq!(hybrid_route(5, 3, Some(2), false), 18);
        assert_eq!(hybrid_route(0, 3, Some(1), false), 9);
        assert_eq!(hybrid_route(5, 3, None, true), 30);
        assert_eq!(hybrid_route(0, 3, Some(5), true), 45);
        assert_eq!(hybrid_route(100, 3, Some(5), true), 100);
        assert_eq!(hybrid_route(5, -1, Some(5), true), 5);
    }
}
