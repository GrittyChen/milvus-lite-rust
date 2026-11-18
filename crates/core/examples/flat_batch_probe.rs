//! 不经服务的 FLAT 批量检索计时：在内存集合上按 baseline 的写入批次建库，FLAT L2、top10，
//! 输出各 nq 的中位耗时与结果指纹（主键与分数位模式），用于比较距离核与任务切分的改动。
//!
//! 用法：`flat_batch_probe <rows> <dim> <threads> <reps> [nq...]`，nq 默认 1 与 100。
//! `flat_batch_probe <rows> <dim> <threads> <reps> fair <bg_nq>`：另一线程在同一计算池上不停执行
//! nq 为 `bg_nq` 的检索，同时计时 `reps` 次单条检索（每次间隔 2 ms），观察后到请求的排队等待。
use milvus_lite_core::collection::{QueryVectors, SearchRequest};
use milvus_lite_core::{
    Collection, CollectionSchema, Compute, DataType, FieldSchema, IndexRequest, InputValues,
    InsertBatch, Pk,
};
use std::num::NonZeroUsize;
use std::time::Instant;

type Error = Box<dyn std::error::Error>;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    let rows: usize = args.get(1).ok_or("rows")?.parse()?;
    let dim: usize = args.get(2).ok_or("dim")?.parse()?;
    let threads: usize = args.get(3).ok_or("threads")?.parse()?;
    let reps: usize = args.get(4).ok_or("reps")?.parse()?;
    let fair: Option<usize> = match args.get(5).map(String::as_str) {
        Some("fair") => Some(args.get(6).ok_or("bg_nq")?.parse()?),
        _ => None,
    };
    let nqs: Vec<usize> = if fair.is_some() {
        vec![1]
    } else if args.len() > 5 {
        args[5..]
            .iter()
            .map(|s| s.parse())
            .collect::<Result<_, _>>()?
    } else {
        vec![1, 100]
    };
    let schema = CollectionSchema {
        functions: Vec::new(),
        fields: vec![
            FieldSchema {
                is_primary: true,
                ..FieldSchema::new("id", DataType::Int64)
            },
            FieldSchema {
                dim: Some(dim as i64),
                ..FieldSchema::new("vec", DataType::FloatVector)
            },
        ],
        enable_dynamic_field: false,
        properties: vec![],
    };
    let mut collection = Collection::new("probe", schema)?;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let batch = 1000;
    for start in (0..rows).step_by(batch) {
        let n = batch.min(rows - start);
        collection.insert(InsertBatch {
            num_rows: n,
            columns: vec![
                (
                    "id".into(),
                    InputValues::Int((start as i64..(start + n) as i64).collect()),
                ),
                (
                    "vec".into(),
                    InputValues::FloatVector {
                        dim,
                        values: (0..n * dim).map(|_| rng.next()).collect(),
                    },
                ),
            ],
        })?;
    }
    collection.create_index(
        "vec",
        IndexRequest {
            index_type: "FLAT".into(),
            metric_type: Some("L2".into()),
            params: Default::default(),
            search_params: Default::default(),
        },
    )?;
    collection.load()?;
    let compute = Compute::new(NonZeroUsize::new(threads).ok_or("threads")?)?;
    let max_nq = nqs.iter().copied().chain(fair).max().unwrap_or(1);
    let queries: Vec<Vec<f32>> = (0..max_nq)
        .map(|_| (0..dim).map(|_| rng.next()).collect())
        .collect();
    let search = |nq: usize| {
        collection.search(
            SearchRequest {
                anns_field: None,
                vectors: QueryVectors::Dense(queries[..nq].to_vec()),
                metric_type: Some("L2".into()),
                limit: 10,
                offset: 0,
                output_fields: Vec::new(),
                params: Default::default(),
                negative_top_k: false,
            },
            &compute,
        )
    };
    if let Some(bg_nq) = fair {
        let stop = std::sync::atomic::AtomicBool::new(false);
        let (times, background) = std::thread::scope(|s| {
            let bg = s.spawn(|| {
                let mut done = 0usize;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    search(bg_nq).map_err(|e| e.to_string())?;
                    done += 1;
                }
                Ok::<_, String>(done)
            });
            std::thread::sleep(std::time::Duration::from_millis(200));
            let mut times = Vec::with_capacity(reps);
            for _ in 0..reps {
                let t = Instant::now();
                let r = search(1).map(|_| t.elapsed().as_secs_f64() * 1e3);
                times.push(r);
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            (times, bg.join())
        });
        let mut times = times.into_iter().collect::<Result<Vec<f64>, _>>()?;
        let background = background.map_err(|_| "background panicked")??;
        times.sort_by(f64::total_cmp);
        println!(
            "{{\"rows\":{rows},\"dim\":{dim},\"threads\":{threads},\"fair_bg_nq\":{bg_nq},\"reps\":{reps},\"background_searches\":{background},\"p50_ms\":{:.3},\"p99_ms\":{:.3},\"max_ms\":{:.3}}}",
            times[reps / 2],
            times[(reps * 99 / 100).min(reps - 1)],
            times[reps - 1]
        );
        return Ok(());
    }
    for nq in nqs {
        let run = || search(nq);
        let result = run()?;
        let mut fingerprint: u64 = 0xcbf2_9ce4_8422_2325;
        for hits in &result.hits {
            for (pk, score) in hits.rows.pks.iter().zip(&hits.scores) {
                let id = match pk {
                    Pk::Int64(v) => *v as u64,
                    Pk::VarChar(_) => u64::MAX,
                };
                for word in [id, score.to_bits() as u64] {
                    fingerprint = (fingerprint ^ word).wrapping_mul(0x100_0000_01b3);
                }
            }
        }
        let mut times = Vec::with_capacity(reps);
        for _ in 0..reps {
            let t = Instant::now();
            run()?;
            times.push(t.elapsed().as_secs_f64() * 1e3);
        }
        times.sort_by(f64::total_cmp);
        println!(
            "{{\"rows\":{rows},\"dim\":{dim},\"threads\":{threads},\"nq\":{nq},\"reps\":{reps},\"p50_ms\":{:.3},\"min_ms\":{:.3},\"max_ms\":{:.3},\"fingerprint\":\"{fingerprint:016x}\"}}",
            times[reps / 2],
            times[0],
            times[reps - 1]
        );
    }
    Ok(())
}
