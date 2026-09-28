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
        "write-wal" => write(root, false),
        "write-flushed" => write(root, true),
        "inspect" => inspect(root),
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "unknown operation").into()),
    }
}
