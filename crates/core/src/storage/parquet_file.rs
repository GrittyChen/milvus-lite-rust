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

/// 只创建新文件；上层保证没有 manifest / snapshot 引用同名文件。
pub fn write_new(path: &Path, batch: &RecordBatch) -> Result<(), ParquetFileError> {
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
        .build();
    let mut writer =
        ArrowWriter::try_new(file, batch.schema(), Some(properties)).map_err(|source| {
            ParquetFileError::Format {
                path: path.to_path_buf(),
                source,
            }
        })?;
    writer
        .write(batch)
        .map_err(|source| ParquetFileError::Format {
            path: path.to_path_buf(),
            source,
        })?;
    let file = writer
        .into_inner()
        .map_err(|source| ParquetFileError::Format {
            path: path.to_path_buf(),
            source,
        })?;
    file.sync_all().map_err(|source| ParquetFileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// 损坏的文件一律返回错误，collection 按 0006 第 11 项 fail-closed。
pub fn read_all(path: &Path) -> Result<Vec<RecordBatch>, ParquetFileError> {
    let file = File::open(path).map_err(|source| ParquetFileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let decoded = catch_unwind(AssertUnwindSafe(|| {
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
        let reader = builder.build()?;
        reader.collect::<Result<Vec<_>, _>>()
    }));
    match decoded {
        Ok(Ok(batches)) => Ok(batches),
        Ok(Err(source)) => Err(ParquetFileError::Format {
            path: path.to_path_buf(),
            source: source.into(),
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

    #[test]
    fn corrupted_parquet_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data_000001_000001.parquet");
        std::fs::write(&path, b"not parquet").unwrap();
        assert!(read_all(&path).is_err());
    }
}
