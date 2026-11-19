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
use crate::schema::milvus_type_name;

/// 3.x `translators/records.py` 的 `_milvus_type_name`。
fn type_name(t: i32) -> String {
    milvus_type_name(t).map_or_else(|| format!("Unknown({t})"), str::to_owned)
}

/// `timestamptz_fields` 是 schema 中 TIMESTAMPTZ 字段的名字（type 21 的字符串只对它们按时间解析）；
/// `timezone` 提供无偏移字符串的有效时区（写入没有请求时区：集合 → 数据库）。
/// `vector_dims` 是 schema 中稠密向量字段的名字、dim，以及缺列时是否按全为空补零（nullable 且不是 function 的
/// 输出）：含空值的列只按 schema 的 dim 展开（见 `check_valid_data`）。
pub(crate) fn decode(
    fields_data: Vec<pb::FieldData>,
    num_rows: u32,
    timestamptz_fields: &[String],
    vector_dims: &[(String, usize, bool)],
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
    for fd in fields_data {
        let extracted = match fd.field {
            Some(field_data::Field::Scalars(sf)) => scalar(&fd.field_name, fd.r#type, sf),
            Some(field_data::Field::Vectors(vf)) => {
                vector(&fd.field_name, fd.r#type, vf, n, &fd.valid_data)
            }
            None | Some(field_data::Field::StructArrays(_)) => {
                return Err(EngineError::SchemaValidation(format!(
                    "FieldData {} has neither scalars nor vectors",
                    py_repr(&fd.field_name)
                )));
            }
        };
        let schema_dim = vector_dims
            .iter()
            .find(|(name, _, _)| *name == fd.field_name)
            .map(|(_, dim, _)| *dim);
        let values = check_valid_data(&fd.field_name, &fd.valid_data, extracted?, n, schema_dim)?;
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
    // 缺列的 nullable 向量由引擎按全为空补 n × dim 个零；没有任何列时 n 只受 u32 约束，同样封顶。
    for (name, dim, fills) in vector_dims {
        if *fills && !columns.iter().any(|(column, _)| column == name) {
            expanded_len(name, n, *dim)?;
        }
    }
    Ok(InsertBatch {
        num_rows: n,
        columns,
    })
}

fn scalar(name: &str, t: i32, sf: pb::ScalarField) -> Result<InputValues, EngineError> {
    use scalar_field::Data;
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
        // GEOMETRY：3.x 先读 geometry_wkt_data，再读 string_data；WKB（geometry_data）与其余槽位视为空列。
        (24, Some(Data::GeometryWktData(a))) => InputValues::Str(a.data),
        (24, Some(Data::StringData(a))) => InputValues::Str(a.data),
        (24, _) => InputValues::Str(Vec::new()),
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

/// 写入的解码上下文：schema 里 TIMESTAMPTZ 字段的名字、稠密向量字段的 dim 与有效时区的来源（写入没有请求时区）。
/// 只在读锁内复制集合属性后立即释放，数据库属性由闭包按需读取。
pub(crate) fn write_context<'a>(
    handle: &milvus_lite_core::CollectionHandle,
    database: &'a dyn Fn() -> Result<Option<String>, EngineError>,
) -> (Vec<String>, Vec<(String, usize, bool)>, TimezoneSource<'a>) {
    let collection = handle.read();
    let schema = collection.schema();
    let fields = schema
        .fields
        .iter()
        .filter(|f| f.data_type == milvus_lite_core::DataType::Timestamptz)
        .map(|f| f.name.clone())
        .collect();
    let vector_dims = schema
        .fields
        .iter()
        .filter(|f| f.data_type == milvus_lite_core::DataType::FloatVector)
        .filter_map(|f| {
            let fills = f.nullable && !f.is_function_output;
            Some((f.name.clone(), usize::try_from(f.dim?).ok()?, fills))
        })
        .collect();
    (
        fields,
        vector_dims,
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
                // 配置的时区本身不可用（手改的属性文件）：3.x 打开集合时就报错，不属于逐行的解析错误。
                if milvus_lite_core::timestamptz::is_naive(text) {
                    timezone.effective()?;
                }
                match timezone.parse_timestamp(text) {
                    Ok(micros) => milvus_lite_core::timestamptz::format_iso_z(micros),
                    // 只有换算越界才换成占位；时区名的路径格式错误（同为 InvalidArgument）保留原文，
                    // 由引擎再次解析时报出 3.x 的 zoneinfo 文案。
                    Err(EngineError::InvalidArgument(message))
                        if message == "date value out of range"
                            && milvus_lite_core::timestamptz::is_naive(text) =>
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

fn vector(
    name: &str,
    t: i32,
    vf: pb::VectorField,
    n: usize,
    valid: &[bool],
) -> Result<InputValues, EngineError> {
    if t == 104 {
        return sparse(name, vf, n);
    }
    if t != 101 {
        return Err(EngineError::SchemaValidation(format!(
            "FieldData {} uses vector type {} which MilvusLite does not support \
             (supported: FloatVector, SparseFloatVector)",
            py_repr(name),
            type_name(t)
        )));
    }
    // 整批都是空值时，pymilvus 两条线都不设置 float_vector，只发 dim 与全 False 的 valid_data；
    // 3.x 对此报错，本实现按全部为空写入（0038 决策 2）。其余缺少数据的情形照旧报 3.x 的原文。
    let values = match vf.data {
        Some(vector_field::Data::FloatVector(values)) => values.data,
        None if !valid.is_empty() && valid.iter().all(|v| !v) => Vec::new(),
        _ => {
            return Err(EngineError::SchemaValidation(format!(
                "FieldData {} declared FloatVector but no float_vector data is set",
                py_repr(name)
            )));
        }
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
    Ok(InputValues::FloatVector { dim, values })
}

/// 3.x `_extract_sparse_vector_column`：逐行 `bytes_to_sparse`（长度错误是 code 1），再检查行数；不看 valid_data，
/// 所以经 gRPC 写不进空值（空字节解码成空 dict，由引擎按「至少一个非零权重」拒绝）。
fn sparse(name: &str, vf: pb::VectorField, n: usize) -> Result<InputValues, EngineError> {
    let Some(vector_field::Data::SparseFloatVector(array)) = vf.data else {
        return Err(EngineError::SchemaValidation(format!(
            "FieldData {} declared SparseFloatVector but no sparse_float_vector data is set",
            py_repr(name)
        )));
    };
    let rows = array
        .contents
        .iter()
        .map(|bytes| milvus_lite_core::data::sparse::decode(bytes))
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() != n {
        return Err(EngineError::SchemaValidation(format!(
            "FieldData {} sparse_float_vector has {} rows, expected {n}",
            py_repr(name),
            rows.len()
        )));
    }
    Ok(InputValues::SparseFloatVector(rows))
}

/// 3.x 用 valid_data 表示 nullable 字段的空值：标量列可以只带有效值（紧凑）或带全部行；3.x 的长度检查先于
/// 空值报告。稠密向量按标量的规则处理（0038 决策 3）：valid_data 是空值的唯一依据，完整形态中空行的占位值
/// 丢弃（3.x 对向量忽略 valid_data，把占位值当作向量写入）。字段是否 nullable 由引擎检查。
fn check_valid_data(
    name: &str,
    valid: &[bool],
    values: InputValues,
    n: usize,
    schema_dim: Option<usize>,
) -> Result<InputValues, EngineError> {
    if valid.is_empty() || matches!(values, InputValues::SparseFloatVector(_)) {
        return Ok(values);
    }
    let n_valid = valid.iter().filter(|v| **v).count();
    if let InputValues::FloatVector {
        dim,
        values: vector,
    } = values
    {
        if valid.len() != n {
            return Err(EngineError::SchemaValidation(format!(
                "FieldData {} valid_data length {} != num_rows {n}",
                py_repr(name),
                valid.len()
            )));
        }
        let full = vector.len() == n.saturating_mul(dim);
        // 全部有效，或两种长度都不符（由随后的行数检查报 3.x 的错误）：原样返回。
        if n_valid == n || (!full && vector.len() != n_valid.saturating_mul(dim)) {
            return Ok(InputValues::FloatVector {
                dim,
                values: vector,
            });
        }
        // 空行占 schema dim 个零（3.x `_build_wal_data_batch` 同样按 schema 的 dim 补零）。dim 与 schema 不符时：
        // 全部为空的列按 schema 的 dim 写空值（3.x 不看空行的 dim）；含有效行的列按请求里的 dim 展开，由引擎
        // 报 3.x 的维度错误（pymilvus 发一行维度不对的向量加几行 None 就会走到这里）。原始 gRPC 可以给出任意大的
        // dim，按请求的 dim 展开后超过上限时不展开，原样返回，由行数检查报错；按 schema 的 dim 展开时超限报错。
        if schema_dim != Some(dim) {
            match schema_dim {
                Some(schema_dim) if n_valid == 0 => {
                    return Ok(InputValues::Nullable {
                        values: Box::new(InputValues::FloatVector {
                            dim: schema_dim,
                            values: zeros(name, n, schema_dim)?,
                        }),
                        validity: milvus_lite_core::data::Validity::from_bools(valid),
                    });
                }
                _ if n
                    .checked_mul(dim)
                    .is_none_or(|len| len > MAX_EXPANDED_FLOATS) =>
                {
                    return Ok(InputValues::FloatVector {
                        dim,
                        values: vector,
                    });
                }
                _ => {}
            }
        }
        let mut expanded = zeros(name, n, dim)?;
        let rows = valid.iter().enumerate().filter(|(_, v)| **v);
        for (next, (row, _)) in rows.enumerate() {
            let source = if full { row } else { next };
            expanded[row * dim..(row + 1) * dim]
                .copy_from_slice(&vector[source * dim..(source + 1) * dim]);
        }
        return Ok(InputValues::Nullable {
            values: Box::new(InputValues::FloatVector {
                dim,
                values: expanded,
            }),
            validity: milvus_lite_core::data::Validity::from_bools(valid),
        });
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

/// 空行按 dim 补零，一个请求里每个空行只占 valid_data 的一个字节，展开后却占 4 × dim 字节。展开后的向量数据
/// 不超过一个全部有效的请求所能携带的浮点数（0038「实施结果」），否则一个小请求就能让进程分配失败而 abort。
const MAX_EXPANDED_FLOATS: usize = crate::MAX_MESSAGE_SIZE / std::mem::size_of::<f32>();

fn expanded_len(name: &str, n: usize, dim: usize) -> Result<usize, EngineError> {
    n.checked_mul(dim)
        .filter(|len| *len <= MAX_EXPANDED_FLOATS)
        .ok_or_else(|| {
            EngineError::SchemaValidation(format!(
                "vector field {} has {n} rows of dim {dim}, more than the {MAX_EXPANDED_FLOATS} floats \
                 one insert request can carry; split the batch",
                py_repr(name)
            ))
        })
}

fn zeros(name: &str, n: usize, dim: usize) -> Result<Vec<f32>, EngineError> {
    Ok(vec![0.0; expanded_len(name, n, dim)?])
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(pairs: &[(u32, f32)]) -> Vec<u8> {
        pairs
            .iter()
            .flat_map(|(d, w)| d.to_le_bytes().into_iter().chain(w.to_le_bytes()))
            .collect()
    }

    fn sparse_column(contents: Vec<Vec<u8>>, valid: Vec<bool>) -> pb::FieldData {
        pb::FieldData {
            r#type: 104,
            field_name: "sp".into(),
            valid_data: valid,
            field: Some(field_data::Field::Vectors(pb::VectorField {
                dim: 0,
                data: Some(vector_field::Data::SparseFloatVector(
                    pb::SparseFloatArray { contents, dim: 0 },
                )),
            })),
            ..Default::default()
        }
    }

    fn decode_one(fd: pb::FieldData, n: u32) -> Result<InsertBatch, EngineError> {
        let database = || Ok(None);
        let timezone = TimezoneSource::new(None, None, &database);
        decode(vec![fd], n, &[], &[], &timezone)
    }

    /// 3.x `_extract_sparse_vector_column`：逐行按 dict 语义解码，不看 valid_data，再检查行数。
    #[test]
    fn sparse_contents_decode_like_3x_and_ignore_valid_data() {
        let batch = decode_one(
            sparse_column(
                vec![pack(&[(5, 1.0), (1, 2.0), (5, 3.0)]), Vec::new()],
                vec![true, false],
            ),
            2,
        )
        .unwrap();
        assert_eq!(
            batch.columns[0].1,
            InputValues::SparseFloatVector(vec![vec![(5, 3.0), (1, 2.0)], vec![]])
        );
        assert_eq!(
            decode_one(sparse_column(vec![pack(&[(1, 1.0)])], vec![false, true]), 2)
                .unwrap_err()
                .to_string(),
            "FieldData 'sp' sparse_float_vector has 1 rows, expected 2"
        );
        // 长度错误在逐行解码时报出，先于行数检查。
        let error = decode_one(sparse_column(vec![vec![0; 6]], Vec::new()), 2).unwrap_err();
        assert!(matches!(error, EngineError::InvalidArgument(_)));
        assert_eq!(
            error.to_string(),
            "sparse vector bytes length 6 is not a multiple of 8"
        );
        let mut empty = sparse_column(Vec::new(), Vec::new());
        empty.field = Some(field_data::Field::Vectors(pb::VectorField {
            dim: 0,
            data: None,
        }));
        assert_eq!(
            decode_one(empty, 1).unwrap_err().to_string(),
            "FieldData 'sp' declared SparseFloatVector but no sparse_float_vector data is set"
        );
    }

    fn dense_column(dim: i64, data: Option<Vec<f32>>, valid: Vec<bool>) -> pb::FieldData {
        pb::FieldData {
            r#type: 101,
            field_name: "v".into(),
            valid_data: valid,
            field: Some(field_data::Field::Vectors(pb::VectorField {
                dim,
                data: data.map(|data| vector_field::Data::FloatVector(pb::FloatArray { data })),
            })),
            ..Default::default()
        }
    }

    fn decode_dense(fd: pb::FieldData, n: u32) -> Result<InputValues, EngineError> {
        decode_with(vec![fd], n, 2, true).map(|mut batch| batch.columns.remove(0).1)
    }

    fn decode_with(
        fields: Vec<pb::FieldData>,
        n: u32,
        schema_dim: usize,
        fills: bool,
    ) -> Result<InsertBatch, EngineError> {
        let database = || Ok(None);
        let timezone = TimezoneSource::new(None, None, &database);
        decode(
            fields,
            n,
            &[],
            &[("v".into(), schema_dim, fills)],
            &timezone,
        )
    }

    fn nullable(values: Vec<f32>, valid: &[bool]) -> InputValues {
        InputValues::Nullable {
            values: Box::new(InputValues::FloatVector { dim: 2, values }),
            validity: Validity::from_bools(valid),
        }
    }

    /// valid_data 是空值的唯一依据（0038 决策 3）：紧凑与完整形态展开成同一列，完整形态中空行的占位值丢弃；
    /// 整批为空时 float_vector 未设置或为空都接受（决策 2）。
    #[test]
    fn dense_valid_data_expands_every_form_to_full_length() {
        let want = nullable(vec![1.0, 2.0, 0.0, 0.0, 3.0, 4.0], &[true, false, true]);
        let valid = vec![true, false, true];
        let compact = dense_column(2, Some(vec![1.0, 2.0, 3.0, 4.0]), valid.clone());
        assert_eq!(decode_dense(compact, 3).unwrap(), want);
        let full = dense_column(2, Some(vec![1.0, 2.0, 9.0, 9.0, 3.0, 4.0]), valid);
        assert_eq!(decode_dense(full, 3).unwrap(), want);

        let all_null = nullable(vec![0.0; 4], &[false, false]);
        assert_eq!(
            decode_dense(dense_column(2, None, vec![false; 2]), 2).unwrap(),
            all_null
        );
        assert_eq!(
            decode_dense(dense_column(2, Some(Vec::new()), vec![false; 2]), 2).unwrap(),
            all_null
        );
        // 全部为空时按 schema 的 dim 展开，不按请求里的 dim 分配。
        assert_eq!(
            decode_dense(dense_column(1 << 40, None, vec![false; 2]), 2).unwrap(),
            all_null
        );

        let plain = InputValues::FloatVector {
            dim: 2,
            values: vec![1.0, 2.0],
        };
        assert_eq!(
            decode_dense(dense_column(2, Some(vec![1.0, 2.0]), vec![true]), 1).unwrap(),
            plain
        );
        assert_eq!(
            decode_dense(dense_column(2, Some(vec![1.0, 2.0]), Vec::new()), 1).unwrap(),
            plain
        );
    }

    #[test]
    fn dense_valid_data_errors() {
        let message = |fd, n| decode_dense(fd, n).unwrap_err().to_string();
        assert_eq!(
            message(dense_column(2, None, Vec::new()), 1),
            "FieldData 'v' declared FloatVector but no float_vector data is set"
        );
        assert_eq!(
            message(dense_column(2, None, vec![false, true]), 2),
            "FieldData 'v' declared FloatVector but no float_vector data is set"
        );
        assert_eq!(
            message(
                dense_column(2, Some(vec![1.0, 2.0]), vec![true, false, false]),
                2
            ),
            "FieldData 'v' valid_data length 3 != num_rows 2"
        );
        assert_eq!(
            message(dense_column(2, Some(vec![1.0; 3]), vec![true, false]), 2),
            "FieldData 'v' float_vector data has 3 elements, expected num_rows(2) * dim(2) = 4"
        );
        // dim 与 schema 不符且含有效行：按请求的 dim 展开，由引擎报维度错误；展开超过一个请求的上限时
        // 不展开，由行数检查报错。
        assert_eq!(
            decode_dense(dense_column(3, Some(vec![1.0; 3]), vec![true, false]), 2).unwrap(),
            InputValues::Nullable {
                values: Box::new(InputValues::FloatVector {
                    dim: 3,
                    values: vec![1.0, 1.0, 1.0, 0.0, 0.0, 0.0],
                }),
                validity: Validity::from_bools(&[true, false]),
            }
        );
        let mut valid = vec![false; 1000];
        valid[0] = true;
        assert_eq!(
            message(dense_column(1 << 20, Some(vec![1.0; 1 << 20]), valid), 1000),
            "FieldData 'v' float_vector data has 1048576 elements, expected num_rows(1000) * dim(1048576) = 1048576000"
        );
    }

    /// 空行补零后的向量数据不超过一个请求能携带的浮点数：有列（按请求或 schema 的 dim 展开）与缺列
    /// （引擎按全为空补零）都封顶，不分配就报错。function 输出与非 nullable 字段缺列时不在这里检查。
    #[test]
    fn dense_null_expansion_is_capped_at_one_request() {
        assert_eq!(
            expanded_len("v", MAX_EXPANDED_FLOATS, 1).unwrap(),
            MAX_EXPANDED_FLOATS
        );
        let over = "vector field 'v' has 2 rows of dim 33554433, more than the 67108864 floats \
                    one insert request can carry; split the batch";
        assert_eq!(
            expanded_len("v", 2, MAX_EXPANDED_FLOATS / 2 + 1)
                .unwrap_err()
                .to_string(),
            over
        );
        assert!(expanded_len("v", usize::MAX, 2).is_err());

        let dim = MAX_EXPANDED_FLOATS / 2 + 1;
        let message = |fields, schema_dim| {
            decode_with(fields, 2, schema_dim, true)
                .unwrap_err()
                .to_string()
        };
        let request_dim = i64::try_from(dim).unwrap();
        assert_eq!(
            message(vec![dense_column(request_dim, None, vec![false; 2])], dim),
            over
        );
        assert_eq!(
            message(vec![dense_column(2, None, vec![false; 2])], dim),
            over
        );
        assert_eq!(message(Vec::new(), dim), over);

        let rows = decode_with(Vec::new(), u32::MAX, 128, true)
            .unwrap_err()
            .to_string();
        assert!(
            rows.starts_with("vector field 'v' has 4294967295 rows of dim 128"),
            "{rows}"
        );
        let unchecked = decode_with(Vec::new(), u32::MAX, 128, false).unwrap();
        assert!(unchecked.columns.is_empty());
    }
}
