//! 存储互通测试的提交点故障进程；正式 wheel 不启用此 feature。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use milvus_lite_core::storage::fs_commit::{CommitPhase, inject_manifest_panic_once};
use milvus_lite_core::{Compute, DataDir, Db};
fn main() {
    let path = std::env::args_os()
        .nth(1)
        .expect("data directory is required");
    let db = Db::new(
        DataDir::open(std::path::Path::new(&path)).unwrap(),
        Compute::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap(),
    )
    .unwrap();
    let c = db.collection("", "c").unwrap();
    c.write().release();
    inject_manifest_panic_once(CommitPhase::BeforeReplace);
    let result = db.drop_index("", "c", "vector", "");
    assert!(result.is_err(), "expected manifest BeforeReplace failure");
    assert!(c.read().index("vector").is_some());
    println!(
        "{}",
        serde_json::json!({"error":result.unwrap_err().to_string(),"indexes":c.read().indexes().len()})
    );
    db.close();
}
