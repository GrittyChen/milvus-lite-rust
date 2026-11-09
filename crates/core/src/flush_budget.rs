//! 触发自动 flush 的字节预算，与 3.x `constants.py` 的 `MEMTABLE_MEMORY_LIMIT`、`WAL_SIZE_LIMIT` 对应（0027）。

use std::sync::OnceLock;

/// 每个 collection 独立计算；不是数据库或进程级的内存上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushBudgets {
    pub memtable_bytes: u64,
    pub wal_bytes: u64,
}

impl Default for FlushBudgets {
    fn default() -> Self {
        Self {
            memtable_bytes: 128 * 1024 * 1024,
            wal_bytes: 256 * 1024 * 1024,
        }
    }
}

static PROCESS: OnceLock<FlushBudgets> = OnceLock::new();

/// 绑定层在导入时调用一次（3.x 也只在导入时读取环境变量），之后的调用不生效。
pub fn configure_process(budgets: FlushBudgets) {
    PROCESS.get_or_init(|| budgets);
}

pub fn process() -> FlushBudgets {
    PROCESS.get().copied().unwrap_or_default()
}
