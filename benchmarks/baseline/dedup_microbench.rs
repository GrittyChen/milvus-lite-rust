//! 估算「每次查询逐行重算跨源去重」在 Rust 下的代价，对应 3.x 的
//! `_update_max_seqs` + `_build_local_mask`（不含删除判定）。
//!
//! 编译与运行（无第三方依赖）：
//!   rustc -O -C target-cpu=native benchmarks/baseline/dedup_microbench.rs -o <work_dir>/dedup_microbench
//!   <work_dir>/dedup_microbench [N ...]      # 默认 N = 100000 1000000
//!
//! 数据：pk 为 0..N 的随机排列（i64），`_seq` 递增（u64），单线程，每个 N 取 5 次中的最好值。
//! 分别用标准库 SipHash 与 Fx 风格乘法哈希，区间两端即二者的结果。

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, BuildHasherDefault, Hasher};
use std::time::Instant;

#[derive(Default)]
struct FxHasher(u64);

impl Hasher for FxHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(b as u64);
        }
    }
    fn write_u64(&mut self, x: u64) {
        self.0 = (self.0.rotate_left(5) ^ x).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    fn write_i64(&mut self, x: i64) {
        self.write_u64(x as u64);
    }
}

fn per_query_dedup_ms<S: BuildHasher + Default>(pks: &[i64], seqs: &[u64]) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        // 与 3.x 每次查询相同：先求每个 pk 的全局最大 seq，再逐行判断该行是否为最新版本
        let mut max_seq: HashMap<i64, u64, S> = HashMap::with_capacity_and_hasher(pks.len(), S::default());
        for (pk, seq) in pks.iter().zip(seqs) {
            let e = max_seq.entry(*pk).or_insert(0);
            if *seq > *e {
                *e = *seq;
            }
        }
        let mask: Vec<bool> = pks.iter().zip(seqs).map(|(pk, seq)| max_seq[pk] == *seq).collect();
        std::hint::black_box(&mask);
        best = best.min(t.elapsed().as_secs_f64() * 1000.0);
    }
    best
}

fn shuffled(n: usize) -> Vec<i64> {
    let mut pks: Vec<i64> = (0..n as i64).collect();
    // 固定种子的 xorshift + Fisher-Yates，保证多次运行数据一致
    let mut x = 88_172_645_463_325_252u64;
    for i in (1..n).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        pks.swap(i, (x % (i as u64 + 1)) as usize);
    }
    pks
}

fn main() {
    let ns: Vec<usize> = std::env::args().skip(1).map(|a| a.parse().expect("N 须为正整数")).collect();
    let ns = if ns.is_empty() { vec![100_000, 1_000_000] } else { ns };
    for n in ns {
        let pks = shuffled(n);
        let seqs: Vec<u64> = (0..n as u64).collect();
        println!(
            "N={n}: std SipHash {:.1} ms, Fx 风格 {:.1} ms",
            per_query_dedup_ms::<RandomState>(&pks, &seqs),
            per_query_dedup_ms::<BuildHasherDefault<FxHasher>>(&pks, &seqs),
        );
    }
}
