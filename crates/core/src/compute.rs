//! 服务的计算池（[0005](../../../docs/decisions/0005-search-thread-model.md)）：检索在不可变读快照上
//! 把计算拆成叶子任务，在这里并行执行。依赖的是 rayon-core（rayon 的线程池部分，不含并行迭代器）；
//! 它的调用只出现在本 module，其余提交任务的接口由 clippy 禁止。
//!
//! 叶子任务不拿锁、不阻塞，只写自己独占的数据；[`Compute::for_each`] 只能从计算池之外的线程调用
//! （RPC 所在的 tokio 阻塞线程）。这样调用线程在 `in_place_scope` 中只等待、不偷其他请求的任务，
//! 也不会接走一个需要锁或阻塞的业务任务。

use std::num::NonZeroUsize;

#[derive(Debug, thiserror::Error)]
#[error("failed to start the compute pool: {0}")]
pub struct ComputeError(#[source] rayon_core::ThreadPoolBuildError);

#[derive(Debug)]
pub struct Compute {
    pool: rayon_core::ThreadPool,
}

impl Compute {
    pub fn new(threads: NonZeroUsize) -> Result<Self, ComputeError> {
        let pool = rayon_core::ThreadPoolBuilder::new()
            .num_threads(threads.get())
            .thread_name(|i| format!("milvus-lite-cpu-{i}"))
            .build()
            .map_err(ComputeError)?;
        Ok(Self { pool })
    }

    /// 线程数取 `available_parallelism`，取不到时为 1（与 rayon 的默认相同）。
    pub fn for_host() -> Result<Self, ComputeError> {
        Self::new(std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN))
    }

    pub fn threads(&self) -> usize {
        self.pool.current_num_threads()
    }

    /// 对每个元素执行一个叶子任务，全部结束后返回；叶子任务的 panic 在其余任务结束后传回调用方。
    /// 只有一个元素时直接在调用线程上执行。
    pub(crate) fn for_each<T, F>(&self, items: &mut [T], f: F)
    where
        T: Send,
        F: Fn(&mut T) + Sync,
    {
        // 从某个计算池的工作线程调用时，等待期间会偷该池的任务，正是 0005 要避免的优先级反转。
        // current_thread_index 只读线程局部量，不会初始化全局池。
        debug_assert!(
            rayon_core::current_thread_index().is_none(),
            "Compute::for_each must be called from outside any compute pool"
        );
        if let [item] = items {
            f(item);
            return;
        }
        let f = &f;
        self.pool.in_place_scope(|scope| {
            for item in items {
                scope.spawn(move |_| f(item));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compute(threads: usize) -> Compute {
        Compute::new(NonZeroUsize::new(threads).unwrap()).unwrap()
    }

    #[test]
    fn runs_every_item_once() {
        let c = compute(3);
        let mut items: Vec<u32> = (0..100).collect();
        c.for_each(&mut items, |x| *x += 1000);
        assert_eq!(items, (1000..1100).collect::<Vec<_>>());
        let mut one = [7];
        c.for_each(&mut one, |x| *x *= 2);
        assert_eq!(one, [14]);
    }

    #[test]
    fn a_panic_in_a_leaf_reaches_the_caller_and_the_pool_stays_usable() {
        let c = compute(2);
        let mut items: Vec<u32> = (0..50).collect();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            c.for_each(&mut items, |x| {
                if *x == 17 {
                    panic!("leaf 17");
                }
                *x += 1;
            })
        }));
        let payload = r.unwrap_err();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"leaf 17"));
        let mut again = [1, 2, 3];
        c.for_each(&mut again, |x| *x += 1);
        assert_eq!(again, [2, 3, 4]);
    }
}
