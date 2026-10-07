//! 3.x 的 Arrow IPC stream WAL。一个文件只有一个 schema 消息，后续 batch 共享同一 writer 状态。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::RecordBatch;
use arrow_ipc::reader::StreamReader;
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{ArrowError, Schema, SchemaRef};

#[cfg(feature = "fault-injection")]
thread_local! {
    static FAIL_NEXT_APPEND_AFTER_BYTES: std::cell::Cell<Option<(usize, bool)>> = const { std::cell::Cell::new(None) };
    static AFTER_APPEND_IO: std::cell::RefCell<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>> = const { std::cell::RefCell::new(None) };
}

/// 观察真实 File 写出之后、append 尚未返回的窗口，供进程崩溃模型使用。
#[cfg(feature = "fault-injection")]
pub fn pause_append_after_io_once(
    entered: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
) {
    AFTER_APPEND_IO.with(|slot| *slot.borrow_mut() = Some((entered, resume)));
}
#[cfg(feature = "fault-injection")]
fn after_append_io() -> io::Result<()> {
    AFTER_APPEND_IO.with(|slot| {
        if let Some((entered, resume)) = slot.borrow_mut().take() {
            entered.send(()).map_err(io::Error::other)?;
            resume.recv().map_err(io::Error::other)?;
        }
        Ok(())
    })
}

/// 仅测试构建可用；模拟 File 写出一部分 batch 后报错。
#[cfg(feature = "fault-injection")]
pub fn inject_append_failure_after_bytes(bytes: usize) {
    FAIL_NEXT_APPEND_AFTER_BYTES.with(|cell| cell.set(Some((bytes, false))));
}

/// 在 File 边界注入部分写入后 ENOSPC；不要求测试填满宿主磁盘。
#[cfg(feature = "fault-injection")]
pub fn inject_storage_full_after_bytes(bytes: usize) {
    FAIL_NEXT_APPEND_AFTER_BYTES.with(|cell| cell.set(Some((bytes, true))));
}

#[cfg(feature = "fault-injection")]
#[derive(Clone, Copy, Debug)]
pub enum WalReadFault {
    Io,
    SchemaPanic,
    BatchPanicAfter(usize),
}
#[cfg(feature = "fault-injection")]
thread_local! { static READ_FAULT: std::cell::Cell<Option<WalReadFault>> = const { std::cell::Cell::new(None) }; }
#[cfg(feature = "fault-injection")]
pub fn inject_read_fault_once(fault: WalReadFault) {
    READ_FAULT.with(|cell| cell.set(Some(fault)));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalKind {
    Data,
    Delta,
}

impl WalKind {
    pub fn file_name(self, number: u64) -> String {
        let kind = match self {
            Self::Data => "data",
            Self::Delta => "delta",
        };
        format!("wal_{kind}_{number:06}.arrow")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error("WAL I/O error at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("WAL IPC encoding failed at {path:?}: {source}")]
    Encode {
        path: PathBuf,
        #[source]
        source: ArrowError,
    },
    #[error("WAL writer at {path:?} cannot be reused after an append failure")]
    Poisoned { path: PathBuf },
}

/// 每次 append 把该 batch 的全部新字节交给 OS；失败后不能再往该文件追加。
pub struct WalWriter {
    path: PathBuf,
    file: File,
    stream: StreamWriter<Vec<u8>>,
    poisoned: bool,
}

impl WalWriter {
    /// `wal_dir` 由持久化层先创建并同步；这里只负责独占地创建一个 WAL 文件。
    pub fn create(
        wal_dir: &Path,
        kind: WalKind,
        number: u64,
        schema: &Schema,
    ) -> Result<Self, WalError> {
        let path = wal_dir.join(kind.file_name(number));
        // schema 先编码到内存。首次 append 把它与首个 batch 一起写盘；空 WAL 不留下仅有 schema 的文件。
        let stream =
            StreamWriter::try_new(Vec::new(), schema).map_err(|source| WalError::Encode {
                path: path.clone(),
                source,
            })?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| WalError::Io {
                path: path.clone(),
                source,
            })?;
        Ok(Self {
            path,
            file,
            stream,
            poisoned: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&mut self, batch: &RecordBatch) -> Result<(), WalError> {
        if self.poisoned {
            return Err(WalError::Poisoned {
                path: self.path.clone(),
            });
        }
        if let Err(source) = self.stream.write(batch) {
            self.poisoned = true;
            return Err(WalError::Encode {
                path: self.path.clone(),
                source,
            });
        }
        // 只取走字节，保留 StreamWriter 的 dictionary/schema 状态。每个 batch 新建 writer
        // 会拼接多个 stream，3.x 的 open_stream 在第二个 schema 处报错。
        let encoded = std::mem::take(self.stream.get_mut());
        #[cfg(feature = "fault-injection")]
        if let Some((bytes, storage_full)) = FAIL_NEXT_APPEND_AFTER_BYTES.with(|cell| cell.take()) {
            // 真实 write_all 失败只发生在写入尚未完成时，不能伪造「全批写完后仍失败」。
            let prefix = bytes.min(encoded.len().saturating_sub(1));
            if let Err(source) = self.file.write_all(&encoded[..prefix]) {
                self.poisoned = true;
                return Err(WalError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
            self.poisoned = true;
            if let Err(error) = after_append_io() {
                panic!("WAL pause interrupted after partial write: {error}");
            }
            return Err(WalError::Io {
                path: self.path.clone(),
                source: if storage_full {
                    #[cfg(unix)]
                    {
                        io::Error::from_raw_os_error(28)
                    }
                    #[cfg(windows)]
                    {
                        io::Error::from_raw_os_error(112)
                    }
                    #[cfg(not(any(unix, windows)))]
                    {
                        io::Error::from(io::ErrorKind::StorageFull)
                    }
                } else {
                    io::Error::other("injected partial WAL append failure")
                },
            });
        }
        if let Err(source) = self.file.write_all(&encoded) {
            self.poisoned = true;
            return Err(WalError::Io {
                path: self.path.clone(),
                source,
            });
        }
        #[cfg(feature = "fault-injection")]
        if let Err(error) = after_append_io() {
            // 已完整写出后不能伪造普通 I/O 失败；断连属于测试中断，走 panic 协议。
            self.poisoned = true;
            panic!("WAL pause interrupted after complete write: {error}");
        }
        Ok(())
    }

    /// 冻结这一代时不写 EOS；3.x 在运行中的 WAL 也不带 EOS。调用方须在关闭返回的 File 前
    /// 完成 fsync，或者一直持有它直到提交后的 WAL 清理阶段（不变量 10）。
    pub fn into_file(self) -> File {
        self.file
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        crate::storage::fs_commit::sync_file(&self.file, &self.path)
    }
}

#[derive(Debug)]
pub struct RecoveredWal {
    pub schema: Option<SchemaRef>,
    pub batches: Vec<RecordBatch>,
    /// 截断或损坏时只取已通过完整校验的前缀；I/O 读错误由 `read_prefix` 返回。
    pub stopped_at: Option<String>,
}

pub fn read_prefix(path: &Path) -> Result<RecoveredWal, WalError> {
    // 文件先完整读进内存，之后解码产生的 UnexpectedEof 才能确定为格式/截断错误。
    #[cfg(feature = "fault-injection")]
    let mut read_fault = READ_FAULT.with(|cell| cell.take());
    #[cfg(feature = "fault-injection")]
    if matches!(read_fault, Some(WalReadFault::Io)) {
        return Err(WalError::Io {
            path: path.to_path_buf(),
            source: io::Error::other("injected WAL read I/O failure"),
        });
    }
    let bytes = fs::read(path).map_err(|source| WalError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let reader = catch_unwind(AssertUnwindSafe(|| {
        #[cfg(feature = "fault-injection")]
        if matches!(read_fault, Some(WalReadFault::SchemaPanic)) {
            panic!("injected WAL schema decode panic");
        }
        StreamReader::try_new(Cursor::new(bytes), None)
    }));
    let mut reader = match reader {
        Ok(Ok(reader)) => reader,
        Ok(Err(source)) => {
            return Ok(RecoveredWal {
                schema: None,
                batches: Vec::new(),
                stopped_at: Some(source.to_string()),
            });
        }
        Err(_) => {
            return Ok(RecoveredWal {
                schema: None,
                batches: Vec::new(),
                stopped_at: Some("Arrow IPC schema decoder panicked".to_owned()),
            });
        }
    };
    let schema = Arc::clone(&reader.schema());
    let mut batches = Vec::new();
    loop {
        let next = catch_unwind(AssertUnwindSafe(|| {
            #[cfg(feature = "fault-injection")]
            if let Some(WalReadFault::BatchPanicAfter(left)) = &mut read_fault {
                if *left == 0 {
                    panic!("injected WAL batch decode panic");
                }
                *left -= 1;
            }
            reader.next().map(|r| {
                r.and_then(|batch| {
                    for column in batch.columns() {
                        column.to_data().validate_full()?;
                    }
                    Ok(batch)
                })
            })
        }));
        match next {
            Ok(Some(Ok(batch))) => batches.push(batch),
            Ok(None) => {
                return Ok(RecoveredWal {
                    schema: Some(schema),
                    batches,
                    stopped_at: None,
                });
            }
            Ok(Some(Err(source))) => {
                return Ok(RecoveredWal {
                    schema: Some(schema),
                    batches,
                    stopped_at: Some(source.to_string()),
                });
            }
            Err(_) => {
                return Ok(RecoveredWal {
                    schema: Some(schema),
                    batches,
                    stopped_at: Some("Arrow IPC record batch decoder panicked".to_owned()),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use arrow_array::{Int64Array, StringArray, UInt64Array};
    use arrow_schema::{DataType, Field};

    use super::*;

    fn schema() -> Schema {
        Schema::new(vec![
            Field::new("_seq", DataType::UInt64, false),
            Field::new("_partition", DataType::Utf8, false),
            Field::new("id", DataType::Int64, false),
        ])
    }

    fn batch(schema: &Schema, seq: u64, id: i64) -> RecordBatch {
        RecordBatch::try_new(
            Arc::new(schema.clone()),
            vec![
                Arc::new(UInt64Array::from(vec![seq])),
                Arc::new(StringArray::from(vec!["_default"])),
                Arc::new(Int64Array::from(vec![id])),
            ],
        )
        .unwrap()
    }

    #[test]
    fn multiple_batches_share_one_stream() {
        let dir = tempfile::tempdir().unwrap();
        let schema = schema();
        let mut writer = WalWriter::create(dir.path(), WalKind::Data, 1, &schema).unwrap();
        writer.append(&batch(&schema, 1, 10)).unwrap();
        writer.append(&batch(&schema, 2, 20)).unwrap();
        let path = writer.path().to_path_buf();
        drop(writer.into_file());

        let recovered = read_prefix(&path).unwrap();
        assert_eq!(recovered.schema.as_deref(), Some(&schema));
        assert_eq!(recovered.batches.len(), 2);
        assert!(recovered.stopped_at.is_none());
        let ids: Vec<i64> = recovered
            .batches
            .iter()
            .map(|batch| {
                batch
                    .column(2)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .value(0)
            })
            .collect();
        assert_eq!(ids, [10, 20]);
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn disconnected_append_pause_is_not_reported_as_a_partial_io_write() {
        let dir = tempfile::tempdir().unwrap();
        let schema = schema();
        let mut writer = WalWriter::create(dir.path(), WalKind::Data, 1, &schema).unwrap();
        let (entered, receiver) = std::sync::mpsc::channel();
        drop(receiver);
        let (_resume, waiting) = std::sync::mpsc::channel();
        pause_append_after_io_once(entered, waiting);
        assert!(catch_unwind(AssertUnwindSafe(|| writer.append(&batch(&schema, 1, 10)))).is_err());
        assert_eq!(read_prefix(writer.path()).unwrap().batches.len(), 1);
        let before = std::fs::read(writer.path()).unwrap();
        assert!(matches!(
            writer.append(&batch(&schema, 2, 20)),
            Err(WalError::Poisoned { .. })
        ));
        assert_eq!(std::fs::read(writer.path()).unwrap(), before);
        writer.sync().unwrap();
        drop(writer.into_file());
    }

    #[test]
    fn truncated_tail_preserves_validated_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let schema = schema();
        let mut writer = WalWriter::create(dir.path(), WalKind::Data, 12, &schema).unwrap();
        writer.append(&batch(&schema, 1, 10)).unwrap();
        let first_len = fs::metadata(writer.path()).unwrap().len();
        writer.append(&batch(&schema, 2, 20)).unwrap();
        let path = writer.path().to_path_buf();
        drop(writer.into_file());
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(first_len + 10)
            .unwrap();

        let recovered = read_prefix(&path).unwrap();
        assert_eq!(recovered.batches.len(), 1);
        assert!(recovered.stopped_at.is_some());
    }

    #[test]
    fn existing_wal_is_never_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let schema = schema();
        let path = dir.path().join(WalKind::Delta.file_name(7));
        fs::write(&path, b"existing").unwrap();
        let error = WalWriter::create(dir.path(), WalKind::Delta, 7, &schema)
            .err()
            .unwrap();
        assert!(matches!(error, WalError::Io { .. }));
        assert_eq!(fs::read(path).unwrap(), b"existing");
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn partial_append_poisoning_prevents_failed_batch_from_reappearing() {
        let dir = tempfile::tempdir().unwrap();
        let schema = schema();
        let mut writer = WalWriter::create(dir.path(), WalKind::Data, 1, &schema).unwrap();
        writer.append(&batch(&schema, 1, 10)).unwrap();
        inject_append_failure_after_bytes(10);
        assert!(writer.append(&batch(&schema, 2, 20)).is_err());
        let length_after_failure = fs::metadata(writer.path()).unwrap().len();
        assert!(matches!(
            writer.append(&batch(&schema, 3, 30)),
            Err(WalError::Poisoned { .. })
        ));
        let path = writer.path().to_path_buf();
        assert_eq!(fs::metadata(&path).unwrap().len(), length_after_failure);
        drop(writer.into_file());
        let recovered = read_prefix(&path).unwrap();
        assert_eq!(recovered.batches.len(), 1);
        assert!(recovered.stopped_at.is_some());
    }
}
