//! InsertRequest.fields_data → 引擎的写入批次，与 3.x `translators/records.py` 的 `fields_data_to_records` 一致：
//! 逐列按 FieldData 自带的 type 选择 proto 槽位（不按 schema 类型），检查按列的先后报告。
//! M1 不支持的列（空值、ARRAY 等类型）记下第一个，全部列解码完才报告，使 3.x 在其后各列上的
//! 解码错误照样优先（0004「决策」）。

use milvus_lite_core::data::{Validity, check_column_len};
use milvus_lite_core::error::py_repr;
use milvus_lite_core::timezone::TimezoneSource;
use milvus_lite_core::{EngineError, InputValues, InsertBatch, META_FIELD, Pk};

use crate::output::{pk_int, pk_string};
use crate::pb::schema::{self as pb, field_data, i_ds, scalar_field, vector_field};
use crate::schema::{milvus_type_name, unsupported};

/// 3.x `translators/records.py` 的 `_milvus_type_name`。
fn type_name(t: i32) -> String {
    milvus_type_name(t).map_or_else(|| format!("Unknown({t})"), str::to_owned)
}

/// `timestamptz_fields` 是 schema 中 TIMESTAMPTZ 字段的名字（type 21 的字符串只对它们按时间解析）；
/// `timezone` 提供无偏移字符串的有效时区（写入没有请求时区：集合 → 数据库）。
pub(crate) fn decode(
    fields_data: Vec<pb::FieldData>,
    num_rows: u32,
    timestamptz_fields: &[String],
    timezone: &TimezoneSource<'_>,
) -> Result<InsertBatch, EngineError> {
    let n = num_rows as usize;
    // 3.x：num_rows 为 0 时不看任何列。
    if n == 0 {
        return Ok(InsertBatch {
            num_rows: 0,
            columns: Vec::new(),
        });
    }
    let mut columns = Vec::with_capacity(fields_data.len());
    let mut first_unsupported = None;
    for fd in fields_data {
        let extracted = match fd.field {
            Some(field_data::Field::Scalars(sf)) => scalar(&fd.field_name, fd.r#type, sf),
            Some(field_data::Field::Vectors(vf)) => vector(&fd.field_name, fd.r#type, vf),
            None | Some(field_data::Field::StructArrays(_)) => {
                return Err(EngineError::SchemaValidation(format!(
                    "FieldData {} has neither scalars nor vectors",
                    py_repr(&fd.field_name)
                )));
            }
        };
        let checked = extracted
            .and_then(|values| check_valid_data(&fd.field_name, &fd.valid_data, values, n));
        let values = match checked {
            Ok(values) => values,
            Err(e @ EngineError::Unsupported { .. }) => {
                first_unsupported.get_or_insert(e);
                continue;
            }
            Err(e) => return Err(e),
        };
        check_column_len(&fd.field_name, &values, n)?;
        // 3.x 在这一列的位置换算时间字符串：type 26 的解析错误直接报告；type 21（VARCHAR 槽）的字符串由引擎
        // 按 schema 逐行解析，这里只做带时区的逐行替换，错误留给引擎（见 `timestamp_texts`）。
        let values = if fd.r#type == 26 {
            timestamps(values, timezone)?
        } else if fd.r#type == 21 && timestamptz_fields.contains(&fd.field_name) {
            timestamp_texts(values, timezone)?
        } else {
            values
        };
        // 3.x：is_dynamic 或名为 `$meta` 的列都承载动态字段。
        let name = if fd.is_dynamic {
            META_FIELD.to_owned()
        } else {
            fd.field_name
        };
        columns.push((name, values));
    }
    if let Some(e) = first_unsupported {
        return Err(e);
    }
    Ok(InsertBatch {
        num_rows: n,
        columns,
    })
}

fn scalar(name: &str, t: i32, sf: pb::ScalarField) -> Result<InputValues, EngineError> {
    use scalar_field::Data;
    if t == 24 {
        return Err(unsupported("GEOMETRY values"));
    }
    // 槽位与 type 不符时，3.x 读到的是空数组，随后报行数不符。
    Ok(match (t, sf.data) {
        (22, Some(Data::ArrayData(array))) => InputValues::Array(
            array
                .data
                .into_iter()
                .map(|row| {
                    Some(match row.data {
                        Some(Data::LongData(v)) => InputValues::Int(v.data),
                        Some(Data::IntData(v)) => {
                            InputValues::Int(v.data.into_iter().map(i64::from).collect())
                        }
                        Some(Data::FloatData(v)) => {
                            InputValues::Float(v.data.into_iter().map(f64::from).collect())
                        }
                        Some(Data::DoubleData(v)) => InputValues::Float(v.data),
                        Some(Data::StringData(v)) => InputValues::Str(v.data),
                        Some(Data::BoolData(v)) => InputValues::Bool(v.data),
                        _ => InputValues::Int(Vec::new()),
                    })
                })
                .collect(),
        ),
        (22, _) => InputValues::Array(Vec::new()),
        (1, Some(Data::BoolData(a))) => InputValues::Bool(a.data),
        (1, _) => InputValues::Bool(Vec::new()),
        (2..=4, Some(Data::IntData(a))) => {
            InputValues::Int(a.data.into_iter().map(i64::from).collect())
        }
        (5, Some(Data::LongData(a))) => InputValues::Int(a.data),
        (2..=5, _) => InputValues::Int(Vec::new()),
        (10, Some(Data::FloatData(a))) => {
            InputValues::Float(a.data.into_iter().map(f64::from).collect())
        }
        (11, Some(Data::DoubleData(a))) => InputValues::Float(a.data),
        (10 | 11, _) => InputValues::Float(Vec::new()),
        (21, Some(Data::StringData(a))) => InputValues::Str(a.data),
        (21, _) => InputValues::Str(Vec::new()),
        // JSON 的内容错误由引擎在该行主键、向量等业务检查之后报告。
        // 保留所有原始列，让同名后列覆盖时也不能隐藏前列的坏 JSON。
        (23, Some(Data::JsonData(a))) => InputValues::Json(a.data),
        (23, _) => InputValues::Json(Vec::new()),
        // TIMESTAMPTZ：pymilvus 发 string_data（ISO 字符串），原始 gRPC 也可发 timestamptz_data（UTC 微秒）；
        // long_data 槽 3.x 不读，视为空列。字符串在 valid_data 展开后再换算（见 `timestamps`）。
        (26, Some(Data::TimestamptzData(a))) => InputValues::Int(a.data),
        (26, Some(Data::StringData(a))) => InputValues::Str(a.data),
        (26, _) => InputValues::Int(Vec::new()),
        _ => {
            return Err(EngineError::SchemaValidation(format!(
                "FieldData {} uses scalar type {} which MilvusLite does not support",
                py_repr(name),
                type_name(t)
            )));
        }
    })
}

/// 写入的时区上下文：schema 里 TIMESTAMPTZ 字段的名字与有效时区的来源（写入没有请求时区）。
/// 只在读锁内复制集合属性后立即释放，数据库属性由闭包按需读取。
pub(crate) fn timezone_context<'a>(
    handle: &milvus_lite_core::CollectionHandle,
    database: &'a dyn Fn() -> Result<Option<String>, EngineError>,
) -> (Vec<String>, TimezoneSource<'a>) {
    let collection = handle.read();
    let schema = collection.schema();
    let fields = schema
        .fields
        .iter()
        .filter(|f| f.data_type == milvus_lite_core::DataType::Timestamptz)
        .map(|f| f.name.clone())
        .collect();
    (
        fields,
        TimezoneSource::new(None, schema.timezone().map(str::to_owned), database),
    )
}

/// 换算后恰好越界的占位：带偏移的合法字符串，引擎按 `date value out of range`（1）报告，且报告位置仍在引擎逐行校验的次序里。
const OUT_OF_RANGE_PLACEHOLDER: &str = "0001-01-01T00:00:00+23:59";

/// TIMESTAMPTZ 的字符串列换算成 UTC 微秒；空值行不解析。3.x 在 `fields_data_to_records` 里逐列做这件事，
/// 因此解析错误出现在该列的位置，先于之后各列的错误与引擎的校验。没有偏移的字符串按集合、数据库的时区
/// 解释，数据库属性只在遇到这样的字符串时才读取。
fn timestamps(
    values: InputValues,
    timezone: &TimezoneSource<'_>,
) -> Result<InputValues, EngineError> {
    map_texts(values, |texts, validity| {
        texts
            .iter()
            .enumerate()
            .map(|(row, text)| {
                if validity.is_some_and(|valid| !valid.get(row)) {
                    Ok(0)
                } else {
                    timezone.parse_timestamp(text)
                }
            })
            .collect::<Result<_, _>>()
            .map(InputValues::Int)
    })
}

/// type 21（VARCHAR 槽）的字符串：3.x 在 adapter 不解析，由引擎按 schema 逐行解析（带集合、数据库的时区），
/// 所以这里逐行处理而不是整列回退，也不提前报错：能换算的行写成规范的 UTC 字符串，换算后越界的无偏移字符串
/// 换成越界占位，其余行（类型不符、带偏移越界）保留原文，三者都由引擎按行、按它自己的次序报告。
/// 数据库属性读取失败（3.x 里这个库根本打不开）原样向上传。
fn timestamp_texts(
    values: InputValues,
    timezone: &TimezoneSource<'_>,
) -> Result<InputValues, EngineError> {
    map_texts(values, |texts, validity| {
        texts
            .iter()
            .enumerate()
            .map(|(row, text)| {
                if validity.is_some_and(|valid| !valid.get(row)) {
                    return Ok(text.clone());
                }
                match timezone.parse_timestamp(text) {
                    Ok(micros) => milvus_lite_core::timestamptz::format_iso_z(micros),
                    Err(EngineError::InvalidArgument(_))
                        if milvus_lite_core::timestamptz::is_naive(text) =>
                    {
                        Ok(OUT_OF_RANGE_PLACEHOLDER.to_owned())
                    }
                    Err(EngineError::SchemaValidation(_) | EngineError::InvalidArgument(_)) => {
                        Ok(text.clone())
                    }
                    Err(error) => Err(error),
                }
            })
            .collect::<Result<_, _>>()
            .map(InputValues::Str)
    })
}

/// 对（可空的）字符串列整体替换；其他形态的列原样返回。
fn map_texts(
    values: InputValues,
    convert: impl Fn(&[String], Option<&Validity>) -> Result<InputValues, EngineError>,
) -> Result<InputValues, EngineError> {
    Ok(match values {
        InputValues::Str(v) => convert(&v, None)?,
        InputValues::Nullable { values, validity } => match *values {
            InputValues::Str(v) => InputValues::Nullable {
                values: Box::new(convert(&v, Some(&validity))?),
                validity,
            },
            other => InputValues::Nullable {
                values: Box::new(other),
                validity,
            },
        },
        other => other,
    })
}

fn vector(name: &str, t: i32, vf: pb::VectorField) -> Result<InputValues, EngineError> {
    if t == 104 {
        return Err(unsupported("SPARSE_FLOAT_VECTOR values"));
    }
    if t != 101 {
        return Err(EngineError::SchemaValidation(format!(
            "FieldData {} uses vector type {} which MilvusLite does not support \
             (supported: FloatVector, SparseFloatVector)",
            py_repr(name),
            type_name(t)
        )));
    }
    let Some(vector_field::Data::FloatVector(values)) = vf.data else {
        return Err(EngineError::SchemaValidation(format!(
            "FieldData {} declared FloatVector but no float_vector data is set",
            py_repr(name)
        )));
    };
    let dim = usize::try_from(vf.dim)
        .ok()
        .filter(|d| *d > 0)
        .ok_or_else(|| {
            EngineError::SchemaValidation(format!(
                "FieldData {} has invalid dim {}",
                py_repr(name),
                vf.dim
            ))
        })?;
    Ok(InputValues::FloatVector {
        dim,
        values: values.data,
    })
}

/// 3.x 用 valid_data 表示 nullable 字段的空值：标量列可以只带有效值（紧凑）或带全部行，向量列按完整长度
/// 发送时 3.x 忽略 valid_data。M1 没有 nullable 字段，含空值的列一律不支持（3.x 在不同位置报错或忽略，
/// 见 0004「决策」）；3.x 的长度检查先于空值报告。
fn check_valid_data(
    name: &str,
    valid: &[bool],
    values: InputValues,
    n: usize,
) -> Result<InputValues, EngineError> {
    if valid.is_empty() {
        return Ok(values);
    }
    let n_valid = valid.iter().filter(|v| **v).count();
    if let InputValues::FloatVector {
        dim,
        values: vector,
    } = &values
    {
        let len = vector.len();
        // 按完整长度发送时 3.x 忽略 valid_data；按紧凑长度发送才表示含空值。
        // 长度都不符时由随后的行数检查报 3.x 的错误。
        if len != n.saturating_mul(*dim) && len == n_valid.saturating_mul(*dim) {
            return Err(unsupported("null values"));
        }
        return Ok(values);
    }
    if valid.len() != n {
        return Err(EngineError::SchemaValidation(format!(
            "FieldData {} valid_data length {} != num_rows {n}",
            py_repr(name),
            valid.len()
        )));
    }
    if values.len() != n_valid && values.len() != n {
        return Err(EngineError::SchemaValidation(format!(
            "FieldData {} has {} values but valid_data expects {n_valid} non-null or {n} total",
            py_repr(name),
            values.len()
        )));
    }
    if n_valid == n {
        return Ok(values);
    }
    let full = values.len() == n;
    macro_rules! expand {
        ($variant:ident,$values:expr,$placeholder:expr) => {{
            let values = $values;
            if full {
                InputValues::$variant(values)
            } else {
                let mut index = 0;
                InputValues::$variant(
                    valid
                        .iter()
                        .map(|&valid| {
                            if valid {
                                let value = values[index].clone();
                                index += 1;
                                value
                            } else {
                                $placeholder
                            }
                        })
                        .collect(),
                )
            }
        }};
    }
    let expanded = match values {
        InputValues::Array(v) => expand!(Array, v, None),
        InputValues::Bool(v) => expand!(Bool, v, false),
        InputValues::Int(v) => expand!(Int, v, 0),
        InputValues::Float(v) => expand!(Float, v, 0.0),
        InputValues::Str(v) => expand!(Str, v, String::new()),
        InputValues::Json(v) => expand!(Json, v, b"null".to_vec()),
        _ => {
            return Err(EngineError::Storage(
                "non-scalar input reached scalar validity expansion".into(),
            ));
        }
    };
    Ok(InputValues::Nullable {
        values: Box::new(expanded),
        validity: milvus_lite_core::data::Validity::from_bools(valid),
    })
}

/// MutationResult.IDs：主键为 VARCHAR 时用 str_id，否则 int_id；没有行时不设置。
pub(crate) fn ids(pks: &[Pk], varchar: bool) -> pb::IDs {
    if pks.is_empty() {
        return pb::IDs { id_field: None };
    }
    pb_ids(pks, varchar)
}

/// 检索结果中的 IDs：即使没有命中也设置对应的槽位（3.x 相同）。
pub(crate) fn pb_ids(pks: &[Pk], varchar: bool) -> pb::IDs {
    let id_field = if varchar {
        i_ds::IdField::StrId(pb::StringArray {
            data: pks.iter().map(pk_string).collect(),
        })
    } else {
        i_ds::IdField::IntId(pb::LongArray {
            data: pks.iter().map(pk_int).collect(),
        })
    };
    pb::IDs {
        id_field: Some(id_field),
    }
}
