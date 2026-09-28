//! M2 存储切片的引擎级闭环；格式期望与参照实现 v3.2.1（43d1257）对照。

// 测试辅助函数不带 #[test]，失败时仍应立即终止用例。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use arrow_array::Int64Array;
use milvus_lite_core::storage::{
    arrow_data::{self, DecodedData},
    flush::flush_generation,
    fs_commit,
    manifest::Manifest,
    memtable::MemTable,
    parquet_file,
    recovery::load_committed,
    schema_file::SchemaFile,
    visibility::{SourceId, Visibility},
    wal::{WalKind, WalWriter},
};
use milvus_lite_core::{ColumnData, Pk};

fn data(ids: &[i64], seqs: &[u64]) -> DecodedData {
    DecodedData {
        seqs: seqs.to_vec(),
        columns: vec![
            ColumnData::Int64(ids.to_vec()),
            ColumnData::FloatVector {
                dim: 2,
                values: [1.0, 2.0].repeat(ids.len()),
            },
        ],
        meta: None,
    }
}

#[test]
fn wal_memtable_flush_bitmap_and_reopen_keep_dict_order() {
    let root = tempfile::tempdir().unwrap();
    let collection_dir = root.path().join("demo.db/collections/c");
    fs_commit::create_dir_synced(&collection_dir).unwrap();
    fs_commit::write_new_schema(
        &collection_dir.join("schema.json"),
        include_bytes!("fixtures/schema_3x.json"),
    )
    .unwrap();
    let schema = SchemaFile::read(&collection_dir.join("schema.json"))
        .unwrap()
        .to_core()
        .unwrap();
    let wal_dir = collection_dir.join("wal");
    fs_commit::create_dir_synced(&wal_dir).unwrap();
    let mut data_wal = WalWriter::create(
        &wal_dir,
        WalKind::Data,
        1,
        &arrow_data::wal_data_schema(&schema).unwrap(),
    )
    .unwrap();
    let mut delta_wal = WalWriter::create(
        &wal_dir,
        WalKind::Delta,
        1,
        &arrow_data::wal_delta_schema(&schema).unwrap(),
    )
    .unwrap();
    let mut visibility = Visibility::default();
    visibility.add_source(SourceId(1), 0).unwrap();
    let mut memtable = MemTable::new(SourceId(1));

    for input in [data(&[1, 2, 3], &[1, 2, 3]), data(&[1], &[4])] {
        let batch =
            arrow_data::wal_data_batch(&schema, &input.seqs, "_default", &input.columns, None)
                .unwrap();
        data_wal.append(&batch).unwrap();
        memtable
            .apply_data(&schema, input, &mut visibility)
            .unwrap();
    }
    let deleted = [Pk::Int64(2)];
    let delta = arrow_data::wal_delta_batch(&schema, &deleted, &[5], "_all").unwrap();
    delta_wal.append(&delta).unwrap();
    memtable.apply_delete(&deleted, 5, &mut visibility).unwrap();
    let input = data(&[2], &[6]);
    data_wal
        .append(
            &arrow_data::wal_data_batch(&schema, &input.seqs, "_default", &input.columns, None)
                .unwrap(),
        )
        .unwrap();
    memtable
        .apply_data(&schema, input, &mut visibility)
        .unwrap();
    let data_file = data_wal.into_file();
    let delta_file = delta_wal.into_file();
    data_file.sync_all().unwrap();
    delta_file.sync_all().unwrap();
    drop(data_file);
    drop(delta_file);

    let frozen = memtable.freeze(&visibility);
    let materialized = frozen.materialize(&schema).unwrap();
    let data = materialized.data.as_ref().unwrap();
    let ids = data
        .column(1)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    assert_eq!(
        (0..ids.len()).map(|row| ids.value(row)).collect::<Vec<_>>(),
        [1, 3, 2]
    );
    assert!(materialized.delta.is_none());
    let manifest = flush_generation(
        &collection_dir,
        &Manifest::default(),
        1,
        frozen.max_seq(),
        materialized.data.as_ref(),
        materialized.delta.as_ref(),
    )
    .unwrap();
    visibility
        .replace_source(
            SourceId(1),
            SourceId(2),
            data.num_rows(),
            &materialized.mapping,
        )
        .unwrap();
    assert_eq!(visibility.live_count(), 3);
    assert_eq!(manifest.current_seq, 6);
    assert!(!wal_dir.join("wal_data_000001.arrow").exists());
    assert!(!wal_dir.join("wal_delta_000001.arrow").exists());

    let reopened = load_committed(&collection_dir).unwrap();
    assert_eq!(reopened.visibility.live_count(), 3);
    assert_eq!(reopened.next_seq, 7);
    let path = collection_dir
        .join("partitions/_default")
        .join(&reopened.manifest.partitions["_default"].data_files[0]);
    let parquet = parquet_file::read_all(&path).unwrap();
    let decoded = arrow_data::decode_data_batch(&schema, &parquet[0]).unwrap();
    assert_eq!(decoded.columns[0], ColumnData::Int64(vec![1, 3, 2]));
    assert_eq!(decoded.seqs, [4, 3, 6]);
}

fn frozen_generation_survives_later_unflushed_change(delete: bool) {
    let root = tempfile::tempdir().unwrap();
    let collection_dir = root.path().join("demo.db/collections/c");
    fs_commit::create_dir_synced(&collection_dir).unwrap();
    fs_commit::write_new_schema(
        &collection_dir.join("schema.json"),
        include_bytes!("fixtures/schema_3x.json"),
    )
    .unwrap();
    let schema = SchemaFile::read(&collection_dir.join("schema.json"))
        .unwrap()
        .to_core()
        .unwrap();
    let wal_dir = collection_dir.join("wal");
    fs_commit::create_dir_synced(&wal_dir).unwrap();
    let old = data(&[1], &[1]);
    let mut wal = WalWriter::create(
        &wal_dir,
        WalKind::Data,
        1,
        &arrow_data::wal_data_schema(&schema).unwrap(),
    )
    .unwrap();
    wal.append(
        &arrow_data::wal_data_batch(&schema, &old.seqs, "_default", &old.columns, None).unwrap(),
    )
    .unwrap();
    let file = wal.into_file();
    file.sync_all().unwrap();
    drop(file);
    let mut visibility = Visibility::default();
    visibility.add_source(SourceId(1), 0).unwrap();
    let mut old_memtable = MemTable::new(SourceId(1));
    old_memtable
        .apply_data(&schema, old, &mut visibility)
        .unwrap();
    let frozen = old_memtable.freeze(&visibility);

    visibility.add_source(SourceId(2), 0).unwrap();
    let mut new_memtable = MemTable::new(SourceId(2));
    let new_wal_path = if delete {
        let mut wal = WalWriter::create(
            &wal_dir,
            WalKind::Delta,
            2,
            &arrow_data::wal_delta_schema(&schema).unwrap(),
        )
        .unwrap();
        wal.append(&arrow_data::wal_delta_batch(&schema, &[Pk::Int64(1)], &[2], "_all").unwrap())
            .unwrap();
        let path = wal.path().to_path_buf();
        wal.into_file().sync_all().unwrap();
        new_memtable
            .apply_delete(&[Pk::Int64(1)], 2, &mut visibility)
            .unwrap();
        path
    } else {
        let new = data(&[1], &[2]);
        let mut wal = WalWriter::create(
            &wal_dir,
            WalKind::Data,
            2,
            &arrow_data::wal_data_schema(&schema).unwrap(),
        )
        .unwrap();
        wal.append(
            &arrow_data::wal_data_batch(&schema, &new.seqs, "_default", &new.columns, None)
                .unwrap(),
        )
        .unwrap();
        let path = wal.path().to_path_buf();
        wal.into_file().sync_all().unwrap();
        new_memtable
            .apply_data(&schema, new, &mut visibility)
            .unwrap();
        path
    };
    assert_eq!(visibility.is_live(SourceId(1), 0), Some(false));
    let materialized = frozen.materialize(&schema).unwrap();
    let old_data = materialized.data.as_ref().unwrap();
    let decoded = arrow_data::decode_data_batch(&schema, old_data).unwrap();
    assert_eq!(decoded.columns[0], ColumnData::Int64(vec![1]));
    assert_eq!(decoded.seqs, [1]);

    let manifest = flush_generation(
        &collection_dir,
        &Manifest::default(),
        1,
        frozen.max_seq(),
        materialized.data.as_ref(),
        materialized.delta.as_ref(),
    )
    .unwrap();
    assert_eq!(manifest.current_seq, 1);
    assert!(new_wal_path.exists());
    visibility
        .replace_source(
            SourceId(1),
            SourceId(3),
            old_data.num_rows(),
            &materialized.mapping,
        )
        .unwrap();
    assert_eq!(visibility.is_live(SourceId(3), 0), Some(false));

    // 模拟断电使新代 WAL 消失；旧代提交没有清理编号更大的 WAL，旧行仍由 segment 恢复。
    std::fs::remove_file(new_wal_path).unwrap();
    let reopened = load_committed(&collection_dir).unwrap();
    assert_eq!(reopened.visibility.live_count(), 1);
    assert_eq!(
        reopened.visibility.latest_data(&Pk::Int64(1)).unwrap().seq,
        1
    );
}

#[test]
fn frozen_rows_survive_later_overwrite_if_new_wal_is_lost() {
    frozen_generation_survives_later_unflushed_change(false);
}

#[test]
fn frozen_rows_survive_later_delete_if_new_wal_is_lost() {
    frozen_generation_survives_later_unflushed_change(true);
}
