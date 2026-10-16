//! 存储互通测试用例的 Rust 端入口；所有输出目录由测试传入，仓库内不生成数据目录。

use std::error::Error;
use std::io;
use std::path::Path;

use milvus_lite_core::storage::{
    arrow_data::{self, DecodedData},
    flush::flush_generation,
    fs_commit,
    manifest::Manifest,
    memtable::MemTable,
    recovery::load_committed,
    schema_file::SchemaFile,
    visibility::{SourceId, Visibility},
    wal::{WalKind, WalWriter},
};
use milvus_lite_core::{CollectionSchema, ColumnData, DataType, FieldSchema, Pk};

fn schema() -> CollectionSchema {
    let mut pk = FieldSchema::new("id", DataType::Int64);
    pk.is_primary = true;
    let mut vector = FieldSchema::new("vector", DataType::FloatVector);
    vector.dim = Some(2);
    CollectionSchema {
        fields: vec![pk, vector],
        enable_dynamic_field: false,
        properties: Vec::new(),
    }
}

fn data() -> DecodedData {
    DecodedData {
        seqs: vec![1, 2],
        columns: vec![
            ColumnData::Int64(vec![1, 2]),
            ColumnData::FloatVector {
                dim: 2,
                values: vec![1.0, 2.0, 3.0, 4.0],
            },
        ],
        meta: None,
    }
}

fn write(root: &Path, flush: bool) -> Result<(), Box<dyn Error>> {
    let collection_dir = root.join("collections/c");
    fs_commit::create_dir_synced(&collection_dir)?;
    let schema = schema();
    let schema_file = SchemaFile::from_core("c", &schema);
    fs_commit::write_new_schema(
        &collection_dir.join("schema.json"),
        &schema_file.to_bytes()?,
    )?;
    let wal_dir = collection_dir.join("wal");
    fs_commit::create_dir_synced(&wal_dir)?;
    let input = data();
    let wal_data =
        arrow_data::wal_data_batch(&schema, &input.seqs, "_default", &input.columns, None)?;
    let mut data_writer = WalWriter::create(&wal_dir, WalKind::Data, 1, &wal_data.schema())?;
    data_writer.append(&wal_data)?;
    data_writer.into_file().sync_all()?;
    let pk = [Pk::Int64(1)];
    let wal_delta = arrow_data::wal_delta_batch(&schema, &pk, &[3], "_all")?;
    let mut delta_writer = WalWriter::create(&wal_dir, WalKind::Delta, 1, &wal_delta.schema())?;
    delta_writer.append(&wal_delta)?;
    delta_writer.into_file().sync_all()?;

    if flush {
        let mut visibility = Visibility::default();
        visibility.add_source(SourceId(1), 0)?;
        let mut memtable = MemTable::new(SourceId(1));
        memtable.apply_data(&schema, input, &mut visibility)?;
        memtable.apply_delete(&pk, 3, &mut visibility)?;
        let frozen = memtable.freeze(&visibility);
        let files = frozen.materialize(&schema)?;
        flush_generation(
            &collection_dir,
            &Manifest::default(),
            1,
            frozen.max_seq(),
            files.data.as_ref(),
            files.delta.as_ref(),
        )?;
    }
    Ok(())
}

fn inspect(root: &Path) -> Result<(), Box<dyn Error>> {
    let recovered = load_committed(&root.join("collections/c"))?;
    let ids = [1_i64, 2, 3]
        .into_iter()
        .filter(|id| {
            recovered
                .visibility
                .latest_data(&Pk::Int64(*id))
                .and_then(|location| recovered.visibility.is_live(location.source, location.row))
                == Some(true)
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::json!({
            "live_count": recovered.visibility.live_count(),
            "ids": ids,
            "next_seq": recovered.next_seq,
            "next_wal_number": recovered.next_wal_number,
        })
    );
    Ok(())
}

fn write_wal_matrix(root: &Path) -> Result<(), Box<dyn Error>> {
    fs_commit::create_dir_synced(root)?;
    let schema = schema();
    let mut ends = serde_json::Map::new();
    for kind in [WalKind::Data, WalKind::Delta] {
        let mut writer = None;
        let mut boundaries = Vec::new();
        for n in 0..3u64 {
            let batch = match kind {
                WalKind::Data => arrow_data::wal_data_batch(
                    &schema,
                    &[2 * n + 1, 2 * n + 2],
                    "_default",
                    &[
                        ColumnData::Int64(vec![(2 * n + 1) as i64, (2 * n + 2) as i64]),
                        ColumnData::FloatVector {
                            dim: 2,
                            values: vec![1.0, 2.0, 3.0, 4.0],
                        },
                    ],
                    None,
                )?,
                WalKind::Delta => arrow_data::wal_delta_batch(
                    &schema,
                    &[Pk::Int64((2 * n + 1) as i64), Pk::Int64((2 * n + 2) as i64)],
                    &[n + 1; 2],
                    "_all",
                )?,
            };
            if writer.is_none() {
                writer = Some(WalWriter::create(root, kind, 1, &batch.schema())?);
            }
            let current = writer
                .as_mut()
                .ok_or_else(|| io::Error::other("writer missing"))?;
            current.append(&batch)?;
            boundaries.push(std::fs::metadata(current.path())?.len());
        }
        ends.insert(
            if kind == WalKind::Data {
                "data"
            } else {
                "delta"
            }
            .into(),
            serde_json::json!(boundaries),
        );
    }
    std::fs::write(root.join("ends.json"), serde_json::to_vec(&ends)?)?;
    Ok(())
}

fn read_wal_matrix(root: &Path) -> Result<(), Box<dyn Error>> {
    let cases: Vec<(String, String)> =
        serde_json::from_slice(&std::fs::read(root.join("cases.json"))?)?;
    let mut result = Vec::new();
    let schema = schema();
    for (file, kind) in cases {
        let recovered = milvus_lite_core::storage::wal::read_prefix(&root.join(file))?;
        let mut batches = Vec::new();
        for batch in recovered.batches {
            let summary = match kind.as_str() {
                "data" => {
                    let data = arrow_data::decode_wal_data_batch(&schema, &batch)?;
                    let (ColumnData::Int64(ids), ColumnData::FloatVector { values, .. }) =
                        (&data.columns[0], &data.columns[1])
                    else {
                        return Err(io::Error::other("unexpected data columns").into());
                    };
                    serde_json::json!({ "seq": data.seqs, "ids": ids, "vectors": values })
                }
                "delta" => {
                    let delta = arrow_data::decode_wal_delta_batch(&schema, &batch)?;
                    let ids: Vec<_> = delta
                        .pks
                        .iter()
                        .map(|pk| match pk {
                            Pk::Int64(id) => Ok(*id),
                            _ => Err(io::Error::other("unexpected primary key type")),
                        })
                        .collect::<Result<_, _>>()?;
                    serde_json::json!({ "seq": delta.seqs, "ids": ids })
                }
                _ => return Err(io::Error::other("unknown WAL kind").into()),
            };
            batches.push(summary);
        }
        result.push(batches);
    }
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

fn array_directory(root: &Path, state: &str) -> Result<(), Box<dyn Error>> {
    use milvus_lite_core::{Compute, DataDir, Db, InputValues, InsertBatch};
    let db = Db::new(
        DataDir::open(root)?,
        Compute::new(std::num::NonZeroUsize::MIN)?,
    )?;
    if state == "inspect" {
        let handle = db.collection("", "c")?;
        handle.write().load()?;
        let read = handle.read().read_snapshot();
        let rows = read.query("", &["*".into()], None, 0)?.rows;
        let mut result = Vec::new();
        for row in 0..rows.pks.len() {
            let mut object = serde_json::Map::new();
            object.insert(
                "id".into(),
                match &rows.pks[row] {
                    Pk::Int64(v) => serde_json::json!(v),
                    _ => return Err(io::Error::other("primary type").into()),
                },
            );
            for (name, col) in &rows.fields {
                if let ColumnData::Array(a) = col.values() {
                    let value = if col.is_null(row) {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::Array(
                            a.range(row)
                                .map(|i| {
                                    if a.values.is_null(i) {
                                        return serde_json::Value::Null;
                                    }
                                    match a.values.values() {
                                        ColumnData::Bool(v) => serde_json::json!(v[i]),
                                        ColumnData::Int8(v) => serde_json::json!(v[i]),
                                        ColumnData::Int16(v) => serde_json::json!(v[i]),
                                        ColumnData::Int32(v) => serde_json::json!(v[i]),
                                        ColumnData::Int64(v) => serde_json::json!(v[i]),
                                        ColumnData::Float(v) => serde_json::json!(v[i]),
                                        ColumnData::Double(v) => serde_json::json!(v[i]),
                                        ColumnData::VarChar(v) => serde_json::json!(v[i]),
                                        _ => unreachable!(),
                                    }
                                })
                                .collect(),
                        )
                    };
                    object.insert(name.clone(), value);
                }
            }
            result.push(object);
        }
        println!("{}", serde_json::to_string(&result)?);
        db.close();
        return Ok(());
    }
    let mut s = schema();
    for (name, dt) in [
        ("b", DataType::Bool),
        ("i8", DataType::Int8),
        ("i16", DataType::Int16),
        ("i32", DataType::Int32),
        ("i64", DataType::Int64),
        ("f", DataType::Float),
        ("d", DataType::Double),
        ("s", DataType::VarChar),
    ] {
        let mut f = FieldSchema::new(name, DataType::Array);
        f.element_type = Some(dt);
        f.max_capacity = Some(4);
        f.nullable = true;
        s.fields.push(f);
    }
    let mut default = FieldSchema::new("default", DataType::Array);
    default.element_type = Some(DataType::Int64);
    default.default_value = Some(serde_json::json!([]));
    s.fields.push(default);
    db.create_collection("", "c", s)?;
    for part in 0..if state == "compacted" { 4 } else { 1 } {
        let mut columns = vec![
            (
                "id".into(),
                InputValues::Int(vec![part * 3 + 1, part * 3 + 2, part * 3 + 3]),
            ),
            (
                "vector".into(),
                InputValues::FloatVector {
                    dim: 2,
                    values: vec![1., 0., 1., 0., 1., 0.],
                },
            ),
        ];
        for (name, value) in [
            ("b", serde_json::json!(true)),
            ("i8", serde_json::json!(7)),
            ("i16", serde_json::json!(7)),
            ("i32", serde_json::json!(7)),
            ("i64", serde_json::json!(7)),
            ("f", serde_json::json!(1.5)),
            ("d", serde_json::json!(1.5)),
            ("s", serde_json::json!("abc")),
        ] {
            columns.push((
                name.into(),
                InputValues::Array(vec![
                    None,
                    Some(InputValues::Int(vec![])),
                    Some(InputValues::JsonValues(vec![
                        serde_json::Value::Null,
                        value,
                    ])),
                ]),
            ));
        }
        db.insert(
            "",
            "c",
            InsertBatch {
                num_rows: 3,
                columns,
            },
        )?;
        if state != "wal" {
            db.flush_collection("", "c")?;
        }
    }
    if state == "wal" {
        std::process::exit(0);
    }
    db.close();
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let operation = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing operation"))?;
    let root = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing output path"))?;
    if args.next().is_some() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "too many arguments").into());
    }
    let root = Path::new(&root);
    match operation.as_str() {
        "write-array-wal" => array_directory(root, "wal"),
        "write-array-flushed" => array_directory(root, "flushed"),
        "write-array-compacted" => array_directory(root, "compacted"),
        "inspect-array" => array_directory(root, "inspect"),
        "write-wal" => write(root, false),
        "write-flushed" => write(root, true),
        "inspect" => inspect(root),
        "write-wal-matrix" => write_wal_matrix(root),
        "read-wal-matrix" => read_wal_matrix(root),
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "unknown operation").into()),
    }
}
