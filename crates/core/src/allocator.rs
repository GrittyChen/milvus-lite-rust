//! 把分配器保留的空闲内存交还操作系统：Linux glibc 上调用 `malloc_trim`，其他平台什么都不做。
#![allow(unsafe_code)]

#[cfg(test)]
pub(crate) static RELEASES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 由后台维护在每轮结束后调用，不持有任何引擎锁。flush、合并与恢复释放的大块临时内存会留在
/// glibc 的 arena 里，不交还时百万行服务的 RSS 多出约三分之一（[分配器保留内存](../../../docs/testing/allocator-retention.md)）。
/// 作用于整个进程的堆，宿主 Python 的空闲块一并交还；trim 期间同一 arena 上的分配会短暂等待。
pub(crate) fn release_free_memory() {
    #[cfg(test)]
    RELEASES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim 只整理 glibc 自己的空闲块，与其他线程的 malloc / free 由 glibc 按 arena 加锁互斥；
    // 参数 0 表示堆顶不留余量。返回值只表示是否交还了内存，不是错误。
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn release_is_callable_from_any_thread() {
        let blocks: Vec<Vec<u8>> = (0..64).map(|_| vec![1u8; 64 * 1024]).collect();
        drop(blocks);
        std::thread::spawn(super::release_free_memory)
            .join()
            .unwrap();
        super::release_free_memory();
    }
}
