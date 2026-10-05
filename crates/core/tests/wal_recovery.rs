//! WAL 边界矩阵：只接受完整批前缀，不越过中间的损坏重新同步。

#![allow(clippy::unwrap_used)]

use arrow_array::RecordBatch;
use milvus_lite_core::storage::{
    arrow_data,
    wal::{self, WalKind, WalWriter},
};
use milvus_lite_core::{CollectionSchema, ColumnData, DataType, FieldSchema, Pk};

fn schema() -> CollectionSchema {
    let mut pk = FieldSchema::new("id", DataType::Int64);
    pk.is_primary = true;
    let mut vector = FieldSchema::new("v", DataType::FloatVector);
    vector.dim = Some(2);
    CollectionSchema {
        fields: vec![
            pk,
            vector,
            FieldSchema::new("label", DataType::VarChar),
            FieldSchema::new("j", DataType::Json),
        ],
        enable_dynamic_field: true,
        properties: Vec::new(),
    }
}

fn batch(schema: &CollectionSchema, kind: WalKind, n: i64) -> RecordBatch {
    let ids = vec![2 * n + 1, 2 * n + 2];
    if kind == WalKind::Delta {
        return arrow_data::wal_delta_batch(
            schema,
            &ids.iter().map(|id| Pk::Int64(*id)).collect::<Vec<_>>(),
            &[n as u64 + 1; 2],
            "_all",
        )
        .unwrap();
    }
    let columns = vec![
        ColumnData::Int64(ids),
        ColumnData::FloatVector {
            dim: 2,
            values: vec![1.25, -2.5, 0.0, 3.75],
        },
        ColumnData::VarChar(vec!["中文".into(), "longer text".into()]),
        ColumnData::Json(vec![br#"{"x":1}"#.to_vec(), br#"{"x":[2,3]}"#.to_vec()]),
    ];
    let meta =
        vec![
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(r#"{"extra":1}"#)
                .unwrap();
            2
        ];
    arrow_data::wal_data_batch(
        schema,
        &[2 * n as u64 + 1, 2 * n as u64 + 2],
        "_default",
        &columns,
        Some(&meta),
    )
    .unwrap()
}

fn fixture(dir: &std::path::Path, kind: WalKind) -> (Vec<u8>, Vec<usize>, Vec<RecordBatch>) {
    let schema = schema();
    let batches: Vec<_> = (0..3).map(|n| batch(&schema, kind, n)).collect();
    let mut writer = WalWriter::create(dir, kind, 1, &batches[0].schema()).unwrap();
    let mut ends = Vec::new();
    for batch in &batches {
        writer.append(batch).unwrap();
        ends.push(std::fs::metadata(writer.path()).unwrap().len() as usize);
    }
    let bytes = std::fs::read(writer.path()).unwrap();
    drop(writer.into_file());
    (bytes, ends, batches)
}

#[test]
fn every_truncated_byte_recovers_exact_complete_data_and_delta_prefix() {
    for kind in [WalKind::Data, WalKind::Delta] {
        let root = tempfile::tempdir().unwrap();
        let (bytes, ends, expected) = fixture(root.path(), kind);
        let cut = root.path().join("cut.arrow");
        for length in 0..=bytes.len() {
            std::fs::write(&cut, &bytes[..length]).unwrap();
            let got = wal::read_prefix(&cut).unwrap();
            let count = ends.iter().filter(|end| **end <= length).count();
            assert_eq!(
                got.batches,
                expected[..count],
                "{kind:?}, cut={length}, ends={ends:?}"
            );
            assert_eq!(std::fs::read(&cut).unwrap(), bytes[..length]);
        }
    }
}

#[test]
fn corrupt_and_zero_middle_message_never_resynchronizes_to_later_valid_batch() {
    for kind in [WalKind::Data, WalKind::Delta] {
        let root = tempfile::tempdir().unwrap();
        let (original, ends, expected) = fixture(root.path(), kind);
        for zero in [false, true] {
            let mut bytes = original.clone();
            if zero {
                bytes[ends[0]..ends[1]].fill(0);
            } else {
                bytes[ends[0] + 4..ends[0] + 8].fill(0xff);
            }
            let path = root.path().join("corrupt.arrow");
            std::fs::write(&path, &bytes).unwrap();
            let got = wal::read_prefix(&path).unwrap();
            assert_eq!(got.batches, expected[..1], "{kind:?}, zero={zero}");
            if zero {
                // 零长度消息按 EOS 结束而非格式错误，诊断语义见 [0002](../../../docs/decisions/0002-data-dir-compat.md#继承的-3x-已知风险)。
                assert!(got.stopped_at.is_none());
            } else {
                assert!(got.stopped_at.is_some());
            }
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn every_enospc_partial_write_poisoned_writer_recovers_only_prior_complete_batches() {
    use wal::{WalError, inject_storage_full_after_bytes};
    for kind in [WalKind::Data, WalKind::Delta] {
        for first in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let (_, ends, batches) = fixture(root.path(), kind);
            let encoded_len = if first { ends[0] } else { ends[1] - ends[0] };
            for cut in 0..encoded_len {
                let mut writer =
                    WalWriter::create(root.path(), kind, cut as u64 + 10, &batches[0].schema())
                        .unwrap();
                if !first {
                    writer.append(&batches[0]).unwrap();
                }
                inject_storage_full_after_bytes(cut);
                let error = writer.append(&batches[usize::from(!first)]).unwrap_err();
                match error {
                    WalError::Io { source, .. } => {
                        assert_eq!(source.kind(), std::io::ErrorKind::StorageFull);
                        #[cfg(unix)]
                        assert_eq!(source.raw_os_error(), Some(28));
                    }
                    other => panic!("unexpected failure: {other}"),
                }
                let path = writer.path().to_path_buf();
                let bytes = std::fs::read(&path).unwrap();
                assert!(matches!(
                    writer.append(&batches[2]),
                    Err(WalError::Poisoned { .. })
                ));
                drop(writer.into_file());
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
                let recovered = wal::read_prefix(&path).unwrap();
                assert_eq!(
                    recovered.batches,
                    batches[..usize::from(!first)],
                    "{kind:?}, first={first}, cut={cut}"
                );
            }
        }
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn decode_panics_return_validated_prefix_and_real_io_errors_are_fatal() {
    use wal::{WalReadFault, inject_read_fault_once};
    for kind in [WalKind::Data, WalKind::Delta] {
        let root = tempfile::tempdir().unwrap();
        let (bytes, _, expected) = fixture(root.path(), kind);
        let path = root.path().join(kind.file_name(1));
        inject_read_fault_once(WalReadFault::SchemaPanic);
        let got = wal::read_prefix(&path).unwrap();
        assert!(got.schema.is_none());
        assert!(got.batches.is_empty());
        assert_eq!(
            got.stopped_at.as_deref(),
            Some("Arrow IPC schema decoder panicked")
        );
        inject_read_fault_once(WalReadFault::BatchPanicAfter(1));
        let got = wal::read_prefix(&path).unwrap();
        assert_eq!(got.batches, expected[..1]);
        assert_eq!(
            got.stopped_at.as_deref(),
            Some("Arrow IPC record batch decoder panicked")
        );
        inject_read_fault_once(WalReadFault::Io);
        assert!(matches!(
            wal::read_prefix(&path),
            Err(wal::WalError::Io { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(wal::read_prefix(&path).unwrap().batches, expected);
        inject_read_fault_once(WalReadFault::BatchPanicAfter(10));
        for _ in 0..4 {
            assert_eq!(wal::read_prefix(&path).unwrap().batches, expected);
        }
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn oversized_append_fault_never_writes_a_complete_failed_batch() {
    use wal::{inject_append_failure_after_bytes, inject_storage_full_after_bytes};
    for kind in [WalKind::Data, WalKind::Delta] {
        for first in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let (_, ends, batches) = fixture(root.path(), kind);
            let encoded_len = if first { ends[0] } else { ends[1] - ends[0] };
            let mut number = 10;
            for requested in [encoded_len, encoded_len + 1, usize::MAX] {
                for storage_full in [false, true] {
                    let mut writer =
                        WalWriter::create(root.path(), kind, number, &batches[0].schema()).unwrap();
                    number += 1;
                    if !first {
                        writer.append(&batches[0]).unwrap();
                    }
                    let before = std::fs::metadata(writer.path()).unwrap().len() as usize;
                    if storage_full {
                        inject_storage_full_after_bytes(requested);
                    } else {
                        inject_append_failure_after_bytes(requested);
                    }
                    assert!(writer.append(&batches[usize::from(!first)]).is_err());
                    let path = writer.path().to_path_buf();
                    drop(writer.into_file());
                    let length = std::fs::metadata(&path).unwrap().len() as usize;
                    assert!(
                        length < before + encoded_len,
                        "{kind:?}, first={first}, requested={requested}, storage_full={storage_full}"
                    );
                    assert_eq!(
                        wal::read_prefix(&path).unwrap().batches,
                        batches[..usize::from(!first)]
                    );
                }
            }
        }
    }
}
