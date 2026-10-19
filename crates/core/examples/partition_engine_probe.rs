//! 在实际 Collection 列、PkMap 和读取管线上诊断分区位图，不包含协议或新数据格式。

use milvus_lite_core::storage::visibility::partitions::{
    PartitionRow, PartitionVisibility, SourceKind,
};
use milvus_lite_core::storage::visibility::{RowLocation, SourceId};
use milvus_lite_core::{
    Collection, CollectionSchema, DEFAULT_DATABASE, DataType, Db, FieldSchema, InputValues,
    InsertBatch, Pk,
};
use std::io::{self, Write};
use std::time::Instant;

fn barrier() -> Result<(), Box<dyn std::error::Error>> {
    io::stdout().flush()?;
    let mut command = String::new();
    io::stdin().read_line(&mut command)?;
    if command.trim() != "continue" {
        return Err("continue required".into());
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let [mode, path, rows, dim] = args.as_slice()
        && mode == "seed"
    {
        let path = std::path::Path::new(path);
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|path| path.parent())
            .ok_or("repository path missing")?;
        let parent = path.parent().ok_or("parent missing")?.canonicalize()?;
        if parent.starts_with(repo) || path.exists() {
            return Err("seed requires new outside directory".into());
        }
        let rows: usize = rows.parse()?;
        let dim: usize = dim.parse()?;
        let mut pk = FieldSchema::new("id", DataType::Int64);
        pk.is_primary = true;
        let mut vector = FieldSchema::new("vector", DataType::FloatVector);
        vector.dim = Some(i64::try_from(dim)?);
        let db = Db::new(
            milvus_lite_core::DataDir::open(path)?,
            milvus_lite_core::Compute::new(
                std::num::NonZeroUsize::new(2).ok_or("worker count must be positive")?,
            )?,
        )?;
        db.create_collection(
            DEFAULT_DATABASE,
            "c",
            CollectionSchema {
                fields: vec![pk, vector],
                enable_dynamic_field: false,
                properties: Vec::new(),
            },
        )?;
        for start in (0..rows).step_by(10000) {
            let n = (rows - start).min(10000);
            db.insert(
                DEFAULT_DATABASE,
                "c",
                InsertBatch {
                    num_rows: n,
                    columns: vec![
                        (
                            "id".into(),
                            InputValues::Int((start..start + n).map(|id| id as i64).collect()),
                        ),
                        (
                            "vector".into(),
                            InputValues::FloatVector {
                                dim,
                                values: vec![1.; n.checked_mul(dim).ok_or("size overflow")?],
                            },
                        ),
                    ],
                },
            )?;
        }
        db.flush_collection(DEFAULT_DATABASE, "c")?;
        db.close();
        return Ok(());
    }
    if let [mode, path] = args.as_slice()
        && mode == "open"
    {
        println!("{}", serde_json::json!({"phase":"empty"}));
        barrier()?;
        let db = Db::new(
            milvus_lite_core::DataDir::open(std::path::Path::new(path))?,
            milvus_lite_core::Compute::new(
                std::num::NonZeroUsize::new(2).ok_or("worker count must be positive")?,
            )?,
        )?;
        let handle = db.collection(DEFAULT_DATABASE, "c")?;
        let rows = handle.read().count();
        println!("{}", serde_json::json!({"phase":"columns","rows":rows}));
        barrier()?;
        let start = Instant::now();
        handle.write().enable_partition_visibility()?;
        println!(
            "{}",
            serde_json::json!({"phase":"visibility","build_ms":start.elapsed().as_secs_f64()*1000.})
        );
        barrier()?;
        let snapshot = handle.read().read_partition_snapshot(Some(&[0]))?;
        if snapshot.count_filtered("id >= 0")? != rows {
            return Err("recovered column count differs".into());
        }
        let dim = handle
            .read()
            .schema()
            .vector_field()
            .and_then(|field| field.dim)
            .ok_or("vector dimension missing")? as usize;
        db.insert(
            DEFAULT_DATABASE,
            "c",
            InsertBatch {
                num_rows: 1,
                columns: vec![
                    ("id".into(), InputValues::Int(vec![rows as i64])),
                    (
                        "vector".into(),
                        InputValues::FloatVector {
                            dim,
                            values: vec![1.; dim],
                        },
                    ),
                ],
            },
        )?;
        if snapshot.count_filtered("id >= 0")? != rows
            || handle
                .read()
                .read_partition_snapshot(None)?
                .count_filtered("id >= 0")?
                != rows + 1
        {
            return Err("recovered old snapshot or new insert differs".into());
        }
        drop(snapshot);
        println!("{}", serde_json::json!({"phase":"queried","expected":rows}));
        barrier()?;
        db.close();
        return Ok(());
    }
    if args.len() != 3 {
        return Err("rows dim partitions required".into());
    }
    let rows: usize = args[0].parse()?;
    let dim: usize = args[1].parse()?;
    let parts: u32 = args[2].parse()?;
    if rows == 0 || dim == 0 || parts == 0 {
        return Err("positive sizes required".into());
    }
    let mut pk = FieldSchema::new("id", DataType::Int64);
    pk.is_primary = true;
    let mut vector = FieldSchema::new("vector", DataType::FloatVector);
    vector.dim = Some(i64::try_from(dim)?);
    let schema = CollectionSchema {
        fields: vec![pk, vector],
        enable_dynamic_field: false,
        properties: Vec::new(),
    };
    let mut collection = Collection::new("c", schema)?;
    println!("{}", serde_json::json!({"phase":"empty"}));
    barrier()?;
    collection.insert(InsertBatch {
        num_rows: rows,
        columns: vec![
            (
                "id".into(),
                InputValues::Int((0..rows).map(|id| id as i64).collect()),
            ),
            (
                "vector".into(),
                InputValues::FloatVector {
                    dim,
                    values: vec![1.; rows.checked_mul(dim).ok_or("size overflow")?],
                },
            ),
        ],
    })?;
    println!(
        "{}",
        serde_json::json!({"phase":"columns", "rows": collection.count()})
    );
    barrier()?;
    let mut visibility = PartitionVisibility::default();
    visibility.reserve(DataType::Int64, rows);
    visibility.register_source(SourceId(1), rows, SourceKind::Active)?;
    let start = Instant::now();
    for row in 0..rows {
        visibility.insert(
            Pk::Int64(row as i64),
            PartitionRow {
                partition: (row % parts as usize) as u32,
                location: RowLocation {
                    source: SourceId(1),
                    row,
                    seq: row as u64 + 1,
                },
            },
        )?;
    }
    println!(
        "{}",
        serde_json::json!({"phase":"visibility", "build_ms": start.elapsed().as_secs_f64()*1000., "keys": visibility.stats().keys})
    );
    barrier()?;
    let selected = [0];
    let view = visibility.snapshot(Some(&selected));
    let snapshot = collection.read_snapshot_with_visibility(&view)?;
    let expected = rows.div_ceil(parts as usize);
    let actual = snapshot.count_filtered("id >= 0")?;
    if actual != expected {
        return Err(format!("bad actual column count {actual} != {expected}").into());
    }
    let old = snapshot.query("id >= 0", &["id".into()], Some(3), 0)?;
    if old.rows.pks
        != (0..expected.min(3))
            .map(|i| Pk::Int64((i * parts as usize) as i64))
            .collect::<Vec<_>>()
    {
        return Err("bad projected primary keys".into());
    }
    let start = Instant::now();
    for _ in 0..100 {
        let bits = visibility.snapshot(Some(&selected));
        if collection
            .read_snapshot_with_visibility(&bits)?
            .count_filtered("id >= 0")?
            != expected
        {
            return Err("unstable column count".into());
        }
    }
    println!(
        "{}",
        serde_json::json!({"phase":"queried", "expected": expected, "read_us": start.elapsed().as_secs_f64()*10000.})
    );
    barrier()?;
    Ok(())
}
