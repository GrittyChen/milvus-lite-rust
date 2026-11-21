//! 3.x data / delta segment 的 Parquet 读写原语；文件名、manifest 提交与孤儿清理在上层。

use std::fs::{File, OpenOptions};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use arrow_array::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

#[derive(Debug, thiserror::Error)]
pub enum ParquetFileError {
    #[error("Parquet I/O error at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Parquet format error at {path:?}: {source}")]
    Format {
        path: PathBuf,
        #[source]
        source: parquet::errors::ParquetError,
    },
    #[error("Parquet reader panicked at {path:?}")]
    DecoderPanic { path: PathBuf },
}

#[cfg(feature = "fault-injection")]
#[derive(Clone, Copy, Debug)]
pub enum ParquetReadFault {
    MetadataPanic,
    BatchPanicAfter(usize),
    ReadIo,
}
#[cfg(feature = "fault-injection")]
thread_local! {
    static READ_FAULT: std::cell::Cell<Option<ParquetReadFault>> = const { std::cell::Cell::new(None) };
}
#[cfg(feature = "fault-injection")]
pub fn inject_read_failure_once(fault: ParquetReadFault) {
    READ_FAULT.with(|slot| slot.set(Some(fault)));
}

/// 行组上限：ArrowWriter 缓存整个行组的编码结果后才落盘，默认的 100 万行上限使一次 flush
/// 的整代数据都以编码形式留在内存里，成为写入期的内存峰值。行数上限约束窄行，字节上限约束
/// 宽向量。测量见 docs/testing/parquet-row-group-memory.md。
const ROW_GROUP_ROWS: usize = 64 * 1024;
const ROW_GROUP_BYTES: usize = 32 * 1024 * 1024;
/// 字节上限只在行组已有缓存行之后按平均行宽检查，整批一次写入时不起作用，所以分片写入；
/// 因而它是软上限，一个行组最多超出一个分片。
const WRITE_SLICE_ROWS: usize = 4096;

/// 只创建新文件；上层保证没有 manifest / snapshot 引用同名文件。
pub fn write_new(path: &Path, batch: &RecordBatch) -> Result<(), ParquetFileError> {
    write_limited(
        path,
        batch,
        ROW_GROUP_ROWS,
        ROW_GROUP_BYTES,
        WRITE_SLICE_ROWS,
    )
}

fn write_limited(
    path: &Path,
    batch: &RecordBatch,
    row_group_rows: usize,
    row_group_bytes: usize,
    slice_rows: usize,
) -> Result<(), ParquetFileError> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| ParquetFileError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    // pyarrow 的默认压缩是 SNAPPY；coerce_types 避免 Arrow logical type 与物理类型不符。
    let properties = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .set_coerce_types(true)
        .set_max_row_group_row_count(Some(row_group_rows))
        .set_max_row_group_bytes(Some(row_group_bytes))
        .build();
    let mut writer =
        ArrowWriter::try_new(file, batch.schema(), Some(properties)).map_err(|source| {
            ParquetFileError::Format {
                path: path.to_path_buf(),
                source,
            }
        })?;
    let mut offset = 0;
    loop {
        let len = slice_rows.min(batch.num_rows() - offset);
        writer
            .write(&batch.slice(offset, len))
            .map_err(|source| ParquetFileError::Format {
                path: path.to_path_buf(),
                source,
            })?;
        offset += len;
        if offset == batch.num_rows() {
            break;
        }
    }
    let file = writer
        .into_inner()
        .map_err(|source| ParquetFileError::Format {
            path: path.to_path_buf(),
            source,
        })?;
    crate::storage::fs_commit::sync_file(&file, path).map_err(|source| ParquetFileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// 损坏的文件一律返回错误，collection 按 0006 第 11 项 fail-closed。
pub fn read_all(path: &Path) -> Result<Vec<RecordBatch>, ParquetFileError> {
    #[cfg(feature = "fault-injection")]
    let mut fault = READ_FAULT.with(|slot| slot.take());
    #[cfg(feature = "fault-injection")]
    if matches!(fault, Some(ParquetReadFault::ReadIo)) {
        return Err(ParquetFileError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other("injected Parquet read I/O failure"),
        });
    }
    let file = File::open(path).map_err(|source| ParquetFileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let decoded = catch_unwind(AssertUnwindSafe(|| {
        #[cfg(feature = "fault-injection")]
        if matches!(fault, Some(ParquetReadFault::MetadataPanic)) {
            panic!("injected Parquet metadata panic");
        }
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
        let mut reader = builder.build()?;
        let mut batches = Vec::new();
        loop {
            #[cfg(feature = "fault-injection")]
            if let Some(ParquetReadFault::BatchPanicAfter(left)) = &mut fault {
                if *left == 0 {
                    panic!("injected Parquet batch panic");
                }
                *left -= 1;
            }
            let Some(batch) = reader.next() else { break };
            let batch = batch?;
            // 和 WAL 相同：列边界必须完整校验后才能交给类型转换与位图构建。
            for column in batch.columns() {
                column.to_data().validate_full()?;
            }
            batches.push(batch);
        }
        Ok::<_, parquet::errors::ParquetError>(batches)
    }));
    match decoded {
        Ok(Ok(batches)) => Ok(batches),
        Ok(Err(source)) => Err(ParquetFileError::Format {
            path: path.to_path_buf(),
            source,
        }),
        Err(_) => Err(ParquetFileError::DecoderPanic {
            path: path.to_path_buf(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{Int64Array, UInt64Array};
    use arrow_schema::{DataType, Field, Schema};

    use super::*;

    #[test]
    fn writes_readable_snappy_parquet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data_000001_000002.parquet");
        let schema = Arc::new(Schema::new(vec![
            Field::new("_seq", DataType::UInt64, false),
            Field::new("id", DataType::Int64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(UInt64Array::from(vec![1, 2])),
                Arc::new(Int64Array::from(vec![10, 20])),
            ],
        )
        .unwrap();
        write_new(&path, &batch).unwrap();
        let batches = read_all(&path).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], batch);
        assert!(matches!(
            write_new(&path, &batch),
            Err(ParquetFileError::Io { .. })
        ));
    }

    fn row_groups(path: &Path) -> Vec<i64> {
        let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap()).unwrap();
        builder
            .metadata()
            .row_groups()
            .iter()
            .map(|group| group.num_rows())
            .collect()
    }

    #[test]
    fn row_group_limits_bound_each_group_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let schema = Arc::new(Schema::new(vec![
            Field::new("_seq", DataType::UInt64, false),
            Field::new("payload", DataType::Binary, false),
        ]));
        let rows = 10u64;
        let payload: Vec<Vec<u8>> = (0..rows).map(|i| vec![i as u8; 1000]).collect();
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(UInt64Array::from((1..=rows).collect::<Vec<_>>())),
                Arc::new(arrow_array::BinaryArray::from_iter_values(&payload)),
            ],
        )
        .unwrap();
        let same = |path: &Path| {
            let mut offset = 0;
            for read in read_all(path).unwrap() {
                assert_eq!(read, batch.slice(offset, read.num_rows()));
                offset += read.num_rows();
            }
            offset == batch.num_rows()
        };

        let by_rows = dir.path().join("rows.parquet");
        write_limited(&by_rows, &batch, 4, usize::MAX, 3).unwrap();
        assert_eq!(row_groups(&by_rows), [4, 4, 2]);
        assert!(same(&by_rows));

        // 每行约 1 KB：字节上限在约 2.5 行处切分，分片写入使它在第一个行组内也生效。
        let by_bytes = dir.path().join("bytes.parquet");
        write_limited(&by_bytes, &batch, usize::MAX, 2500, 1).unwrap();
        let groups = row_groups(&by_bytes);
        assert!(groups.len() >= 3, "{groups:?}");
        assert!(groups.iter().all(|&n| n <= 3), "{groups:?}");
        assert_eq!(groups.iter().sum::<i64>(), rows as i64);
        assert!(same(&by_bytes));

        // 生产常量：窄行由行数上限切分。
        let narrow_rows = 70_000u64;
        let narrow = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("_seq", DataType::UInt64, false),
                Field::new("id", DataType::Int64, false),
            ])),
            vec![
                Arc::new(UInt64Array::from((1..=narrow_rows).collect::<Vec<_>>())),
                Arc::new(Int64Array::from(
                    (0..narrow_rows as i64).collect::<Vec<_>>(),
                )),
            ],
        )
        .unwrap();
        let production = dir.path().join("production.parquet");
        write_new(&production, &narrow).unwrap();
        assert_eq!(row_groups(&production), [65_536, 4_464]);
        let mut offset = 0;
        for read in read_all(&production).unwrap() {
            assert_eq!(read, narrow.slice(offset, read.num_rows()));
            offset += read.num_rows();
        }
        assert_eq!(offset, narrow.num_rows());

        let empty = dir.path().join("empty.parquet");
        write_new(&empty, &batch.slice(0, 0)).unwrap();
        assert!(row_groups(&empty).is_empty());
        assert!(read_all(&empty).unwrap().iter().all(|b| b.num_rows() == 0));
    }

    #[test]
    fn corrupted_parquet_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data_000001_000001.parquet");
        std::fs::write(&path, b"not parquet").unwrap();
        assert!(read_all(&path).is_err());
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn decode_panics_never_return_a_partial_file_and_real_io_remains_distinct() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data.parquet");
        std::fs::write(
            &path,
            include_bytes!("../../tests/fixtures/data_3x.parquet"),
        )
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        for fault in [
            ParquetReadFault::MetadataPanic,
            ParquetReadFault::BatchPanicAfter(0),
            ParquetReadFault::BatchPanicAfter(1),
        ] {
            inject_read_failure_once(fault);
            assert!(
                matches!(read_all(&path), Err(ParquetFileError::DecoderPanic { .. })),
                "{fault:?}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert_eq!(
                read_all(&path)
                    .unwrap()
                    .iter()
                    .map(RecordBatch::num_rows)
                    .sum::<usize>(),
                2
            );
        }
        inject_read_failure_once(ParquetReadFault::ReadIo);
        assert!(matches!(read_all(&path), Err(ParquetFileError::Io { .. })));
        assert!(read_all(&path).is_ok());
    }
}
