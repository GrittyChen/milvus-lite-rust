//! rerank（0026）：boost、候选窗口，以及它们需要的 Python 值规则。
//!
//! 3.x 的 rerank 链把每个命中摊平成一个字典，同名字段会盖掉虚拟列，因此这里的比较与转换
//! 按 Python 的动态类型规则复现（包括抛出的异常文案），而不是按 schema 的静态类型。

pub mod boost;
pub mod chain;
pub mod pyval;
pub mod window;

use std::cmp::Ordering;

use crate::error::Result;

/// 稳定归并排序，比较可以失败：Python 的 `sort(key=cmp_to_key(...))` 在比较抛异常时中止。
/// 标准库的排序不能用于这里：比较出错时无法给出全序，而它们遇到非全序可能 panic。
pub(crate) fn try_sort_by<T>(
    items: Vec<T>,
    compare: &mut dyn FnMut(&T, &T) -> Result<Ordering>,
) -> Result<Vec<T>> {
    if items.len() < 2 {
        return Ok(items);
    }
    let mut left = items;
    let right = left.split_off(left.len() / 2);
    let left = try_sort_by(left, compare)?;
    let right = try_sort_by(right, compare)?;
    let mut merged = Vec::with_capacity(left.len() + right.len());
    let (mut left, mut right) = (left.into_iter().peekable(), right.into_iter().peekable());
    while let (Some(a), Some(b)) = (left.peek(), right.peek()) {
        let next = if compare(a, b)? == Ordering::Greater {
            right.next()
        } else {
            left.next()
        };
        merged.extend(next);
    }
    merged.extend(left);
    merged.extend(right);
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::EngineError;

    #[test]
    fn sort_is_stable_and_stops_at_the_first_failure() {
        let items = vec![(2, 'a'), (1, 'b'), (2, 'c'), (1, 'd'), (0, 'e')];
        let sorted = try_sort_by(items.clone(), &mut |a, b| Ok(a.0.cmp(&b.0))).unwrap();
        assert_eq!(sorted, [(0, 'e'), (1, 'b'), (1, 'd'), (2, 'a'), (2, 'c')]);
        let failed = try_sort_by(items, &mut |_, _| Err(EngineError::invalid("boom")));
        assert!(matches!(failed, Err(EngineError::InvalidArgument(m)) if m == "boom"));
        assert!(
            try_sort_by(Vec::<i32>::new(), &mut |_, _| unreachable!())
                .unwrap()
                .is_empty()
        );
    }
}
