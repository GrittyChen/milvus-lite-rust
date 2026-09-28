// 测试辅助函数不带 #[test]，失败时仍应立即终止用例。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::sync::Arc;

use arrow_array::{FixedSizeListArray, Float32Array, Int64Array, RecordBatch, UInt64Array};
use arrow_schema::{DataType as ArrowType, Field, Schema};
use milvus_lite_core::storage::{
    arrow_data,
    manifest::Manifest,
    parquet_file,
    recovery::{self, RecoveryError},
    schema_file::SchemaFile,
    snapshot, wal,
};
use milvus_lite_core::{CollectionSchema, ColumnData, DataType, FieldSchema};

#[test]
fn parses_3x_schema_and_manifest_files() {
    let schema = SchemaFile::parse(include_bytes!("fixtures/schema_3x.json")).unwrap();
    assert_eq!(schema.collection_name, "c");
    assert_eq!(schema.fields.len(), 2);
    assert_eq!(schema.fields[0].dtype, "int64");
    assert_eq!(schema.fields[1].dtype, "float_vector");
    assert_eq!(schema.fields[1].dim, Some(2));

    let manifest = Manifest::parse(include_bytes!("fixtures/manifest_3x.json")).unwrap();
    assert_eq!(manifest.version, 1);
    assert_eq!(manifest.current_seq, 0);
    assert!(manifest.partitions["_default"].data_files.is_empty());
}

#[test]
fn recovers_two_batches_written_by_3x() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("wal_data_000001.arrow");
    fs::write(&path, include_bytes!("fixtures/wal_data_3x.arrow")).unwrap();
    let recovered = wal::read_prefix(&path).unwrap();
    assert!(recovered.stopped_at.is_none());
    assert_eq!(recovered.batches.len(), 2);
    for (batch, (seq, id)) in recovered.batches.iter().zip([(1, 10), (2, 20)]) {
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(
            batch
                .column(0)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap()
                .value(0),
            seq
        );
        assert_eq!(
            batch
                .column(2)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .value(0),
            id
        );
        let vector = batch
            .column(3)
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        let vector_values = vector.value(0);
        let vector_values = vector_values
            .as_any()
            .downcast_ref::<Float32Array>()
            .unwrap();
        assert_eq!([vector_values.value(0), vector_values.value(1)], [1.0, 2.0]);
        let schema = SchemaFile::parse(include_bytes!("fixtures/schema_3x.json"))
            .unwrap()
            .to_core()
            .unwrap();
        let decoded = arrow_data::decode_wal_data_batch(&schema, batch).unwrap();
        assert_eq!(decoded.seqs, [seq]);
        assert_eq!(
            decoded.columns[0],
            milvus_lite_core::ColumnData::Int64(vec![id])
        );
    }
}

#[test]
fn reads_data_parquet_written_by_3x() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("data_000001_000002.parquet");
    fs::write(&path, include_bytes!("fixtures/data_3x.parquet")).unwrap();
    let batches = parquet_file::read_all(&path).unwrap();
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 2);
    let ids: Vec<i64> = batches
        .iter()
        .flat_map(|batch| {
            let array = batch
                .column(1)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            (0..array.len())
                .map(|row| array.value(row))
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(ids, [10, 20]);
    let schema = SchemaFile::parse(include_bytes!("fixtures/schema_3x.json"))
        .unwrap()
        .to_core()
        .unwrap();
    let decoded = arrow_data::decode_data_batch(&schema, &batches[0]).unwrap();
    assert_eq!(decoded.seqs, [1, 2]);
    assert_eq!(
        decoded.columns[0],
        milvus_lite_core::ColumnData::Int64(vec![10, 20])
    );
    assert_eq!(
        decoded.columns[1],
        milvus_lite_core::ColumnData::FloatVector {
            dim: 2,
            values: vec![1.0, 2.0, 3.0, 4.0],
        }
    );
}

#[test]
fn reads_delta_wal_and_parquet_written_by_3x() {
    let root = tempfile::tempdir().unwrap();
    let wal_path = root.path().join("wal_delta_000003.arrow");
    fs::write(&wal_path, include_bytes!("fixtures/wal_delta_3x.arrow")).unwrap();
    let parquet_path = root.path().join("delta_000003_000003.parquet");
    fs::write(&parquet_path, include_bytes!("fixtures/delta_3x.parquet")).unwrap();
    let schema = SchemaFile::parse(include_bytes!("fixtures/schema_3x.json"))
        .unwrap()
        .to_core()
        .unwrap();

    let wal = wal::read_prefix(&wal_path).unwrap();
    assert!(wal.stopped_at.is_none());
    assert_eq!(wal.batches.len(), 1);
    let decoded = arrow_data::decode_wal_delta_batch(&schema, &wal.batches[0]).unwrap();
    assert_eq!(decoded.pks, [milvus_lite_core::Pk::Int64(10)]);
    assert_eq!(decoded.seqs, [3]);

    let parquet = parquet_file::read_all(&parquet_path).unwrap();
    assert_eq!(parquet.len(), 1);
    let decoded = arrow_data::decode_delta_batch(&schema, &parquet[0]).unwrap();
    assert_eq!(decoded.pks, [milvus_lite_core::Pk::Int64(10)]);
    assert_eq!(decoded.seqs, [3]);
}

#[test]
fn reads_3x_snapshot_pins() {
    let root = tempfile::tempdir().unwrap();
    let metadata = root.path().join("snapshots/metadata");
    fs::create_dir_all(&metadata).unwrap();
    fs::write(
        metadata.join("s1.json"),
        include_bytes!("fixtures/snapshot_3x.json"),
    )
    .unwrap();
    let pins = snapshot::load_pins(root.path()).unwrap();
    assert!(pins.data.contains(&(
        "_default".to_owned(),
        "data/data_000001_000002.parquet".to_owned()
    )));
    assert!(pins.delta.contains(&(
        "_default".to_owned(),
        "delta/delta_000003_000003.parquet".to_owned()
    )));
}

fn same_type_schema() -> CollectionSchema {
    let mut pk = FieldSchema::new("id", DataType::Int64);
    pk.is_primary = true;
    let mut vector = FieldSchema::new("vector", DataType::FloatVector);
    vector.dim = Some(2);
    CollectionSchema {
        fields: vec![pk, FieldSchema::new("score", DataType::Int64), vector],
        enable_dynamic_field: false,
        properties: Vec::new(),
    }
}

fn swapped(batch: &RecordBatch, left: usize, right: usize) -> RecordBatch {
    let mut fields = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.as_ref().clone())
        .collect::<Vec<_>>();
    fields.swap(left, right);
    let mut columns = batch.columns().to_vec();
    columns.swap(left, right);
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap()
}

fn sample_columns() -> [ColumnData; 3] {
    [
        ColumnData::Int64(vec![1]),
        ColumnData::Int64(vec![99]),
        ColumnData::FloatVector {
            dim: 2,
            values: vec![1.0, 2.0],
        },
    ]
}

#[test]
fn swapped_same_type_fields_in_parquet_fail_recovery() {
    let root = tempfile::tempdir().unwrap();
    let collection = root.path().join("c");
    let data_dir = collection.join("partitions/_default/data");
    fs::create_dir_all(&data_dir).unwrap();
    let schema = same_type_schema();
    fs::write(
        collection.join("schema.json"),
        SchemaFile::from_core("c", &schema).to_bytes().unwrap(),
    )
    .unwrap();
    let correct = arrow_data::data_batch(&schema, &[1], &sample_columns(), None).unwrap();
    let wrong = swapped(&correct, 1, 2);
    parquet_file::write_new(&data_dir.join("data_000001_000001.parquet"), &wrong).unwrap();
    let mut manifest = Manifest {
        current_seq: 1,
        ..Manifest::default()
    };
    manifest
        .partitions
        .get_mut("_default")
        .unwrap()
        .data_files
        .push("data/data_000001_000001.parquet".to_owned());
    fs::write(
        collection.join("manifest.json"),
        manifest.to_bytes().unwrap(),
    )
    .unwrap();

    assert!(matches!(
        recovery::load_committed(&collection),
        Err(RecoveryError::Arrow(
            arrow_data::ArrowDataError::ColumnName { position: 1, .. }
        ))
    ));
    assert!(data_dir.join("data_000001_000001.parquet").exists());
}

#[test]
fn swapped_same_type_fields_in_wal_fail_recovery() {
    let root = tempfile::tempdir().unwrap();
    let collection = root.path().join("c");
    let wal_dir = collection.join("wal");
    fs::create_dir_all(&wal_dir).unwrap();
    let schema = same_type_schema();
    fs::write(
        collection.join("schema.json"),
        SchemaFile::from_core("c", &schema).to_bytes().unwrap(),
    )
    .unwrap();
    let correct =
        arrow_data::wal_data_batch(&schema, &[1], "_default", &sample_columns(), None).unwrap();
    let wrong = swapped(&correct, 2, 3);
    let mut writer =
        wal::WalWriter::create(&wal_dir, wal::WalKind::Data, 1, &wrong.schema()).unwrap();
    writer.append(&wrong).unwrap();
    writer.into_file().sync_all().unwrap();

    assert!(matches!(
        recovery::load_committed(&collection),
        Err(RecoveryError::Arrow(
            arrow_data::ArrowDataError::ColumnName { position: 2, .. }
        ))
    ));
    assert!(wal_dir.join("wal_data_000001.arrow").exists());
}

#[test]
fn renamed_primary_key_in_delta_parquet_fails_recovery() {
    let root = tempfile::tempdir().unwrap();
    let collection = root.path().join("c");
    let delta_dir = collection.join("partitions/_default/delta");
    fs::create_dir_all(&delta_dir).unwrap();
    let schema = SchemaFile::parse(include_bytes!("fixtures/schema_3x.json"))
        .unwrap()
        .to_core()
        .unwrap();
    fs::write(
        collection.join("schema.json"),
        include_bytes!("fixtures/schema_3x.json"),
    )
    .unwrap();
    let correct =
        arrow_data::delta_batch(&schema, &[milvus_lite_core::Pk::Int64(1)], &[1]).unwrap();
    let wrong_schema = Schema::new(vec![
        Field::new("score", ArrowType::Int64, false),
        Field::new("_seq", ArrowType::UInt64, false),
    ]);
    let wrong = RecordBatch::try_new(Arc::new(wrong_schema), correct.columns().to_vec()).unwrap();
    parquet_file::write_new(&delta_dir.join("delta_000001_000001.parquet"), &wrong).unwrap();
    let mut manifest = Manifest {
        current_seq: 1,
        ..Manifest::default()
    };
    manifest
        .partitions
        .get_mut("_default")
        .unwrap()
        .delta_files
        .push("delta/delta_000001_000001.parquet".to_owned());
    fs::write(
        collection.join("manifest.json"),
        manifest.to_bytes().unwrap(),
    )
    .unwrap();

    assert!(matches!(
        recovery::load_committed(&collection),
        Err(RecoveryError::Arrow(
            arrow_data::ArrowDataError::ColumnName { position: 0, .. }
        ))
    ));
    assert!(delta_dir.join("delta_000001_000001.parquet").exists());
}
