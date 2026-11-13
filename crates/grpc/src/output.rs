//! 读取结果 → FieldData 列，Query 与 Search 共用，与 3.x `records_to_fields_data` 一致：
//! 输出投影中的 schema 字段与主键（主键总在，位于它在 schema 中的位置），开启动态字段时最后是 `$meta` 列。
//! 没有结果时各列仍然设置对应的槽位。

use milvus_lite_core::timezone::{Components, TimezoneSource};
use milvus_lite_core::{
    CollectionSchema, ColumnData, DataType, EngineError, META_FIELD, Pk, Projection, Rows, pyjson,
};
use serde_json::{Map, Value};

use crate::pb::schema::{self as pb, field_data, scalar_field, vector_field};
use crate::schema::milvus_type;

/// 请求里的 `time_fields`：TIMESTAMPTZ 输出字段改为按有效时区取的各分量（`Array<Int64>`）。
/// 分量名与时区都只在真正编码一个非空时间戳值时才校验（3.x `extract_time_fields`）。
pub(crate) struct TimeFields<'a> {
    parts: Vec<String>,
    timezone: &'a TimezoneSource<'a>,
}

impl<'a> TimeFields<'a> {
    /// 3.x `_parse_time_fields`：逗号或空白分隔，空段忽略；没有任何分量时与未设置相同。
    pub(crate) fn new(raw: Option<&str>, timezone: &'a TimezoneSource<'a>) -> Option<Self> {
        let parts: Vec<String> = raw?
            .split(|c: char| c == ',' || is_python_space(c))
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect();
        (!parts.is_empty()).then_some(Self { parts, timezone })
    }

    fn values(&self, micros: i64) -> Result<Vec<i64>, EngineError> {
        milvus_lite_core::timestamptz::check_micros(micros)?;
        let local = self.timezone.for_time_fields()?.components(micros)?;
        self.parts
            .iter()
            .map(|part| component(&local, part))
            .collect()
    }
}

/// Python `str.isspace` / `re` 的 `\s`：Unicode 空白加上 `\x1c`–`\x1f`。
fn is_python_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

fn component(local: &Components, name: &str) -> Result<i64, EngineError> {
    // Python 的 casefold 还会把长 s（ſ）折叠为 s。
    Ok(match name.to_lowercase().replace('ſ', "s").as_str() {
        "year" => local.year,
        "month" => local.month,
        "day" => local.day,
        "hour" => local.hour,
        "minute" => local.minute,
        "second" => local.second,
        "microsecond" => local.microsecond,
        _ => {
            return Err(EngineError::SchemaValidation(format!(
                "unsupported time_fields component {}; supported: year, month, day, hour, minute, second, microsecond",
                milvus_lite_core::error::py_repr(name)
            )));
        }
    })
}

/// `Array<Int64>` 形式的时间分量列；空值行 `valid_data = false`（有空值时才带 `valid_data`）。
fn time_fields_data(
    name: &str,
    columns: &[&ColumnData],
    time_fields: &TimeFields<'_>,
) -> Result<pb::FieldData, EngineError> {
    let mut data = Vec::new();
    for column in columns {
        let ColumnData::Int64(values) = column.values() else {
            unreachable!("{MISMATCH}");
        };
        for (row, &micros) in values.iter().enumerate() {
            let null = column.is_null(row);
            data.push(pb::ScalarField {
                data: Some(scalar_field::Data::LongData(pb::LongArray {
                    data: if null {
                        Vec::new()
                    } else {
                        time_fields.values(micros)?
                    },
                })),
            });
        }
    }
    Ok(pb::FieldData {
        r#type: milvus_type(DataType::Array),
        field_name: name.to_owned(),
        valid_data: if columns.iter().any(|c| c.validity().is_some()) {
            columns
                .iter()
                .flat_map(|c| (0..c.len()).map(|row| !c.is_null(row)))
                .collect()
        } else {
            Vec::new()
        },
        field: Some(field_data::Field::Scalars(pb::ScalarField {
            data: Some(scalar_field::Data::ArrayData(pb::ArrayArray {
                data,
                element_type: milvus_type(DataType::Int64),
            })),
        })),
        ..Default::default()
    })
}

pub(crate) fn fields_data(
    schema: &CollectionSchema,
    projection: &Projection,
    rows: &[&Rows],
    time_fields: Option<&TimeFields<'_>>,
) -> Result<Vec<pb::FieldData>, EngineError> {
    let mut out = Vec::new();
    for (i, field) in schema.fields.iter().enumerate() {
        if field.is_primary && !projection.emits_primary(i) {
            continue;
        }
        let column = if field.is_primary {
            Column::Pk(rows.iter().flat_map(|r| r.pks.iter()).collect())
        } else if let Some(pos) = projection.schema_fields.iter().position(|&f| f == i) {
            Column::Data(rows.iter().map(|r| &r.fields[pos].1).collect())
        } else {
            continue;
        };
        if let (DataType::Timestamptz, Column::Data(columns), Some(time_fields)) =
            (field.data_type, &column, time_fields)
        {
            out.push(time_fields_data(&field.name, columns, time_fields)?);
            continue;
        }
        out.push(field_data(
            &field.name,
            field.data_type,
            field.dim,
            field.element_type,
            &column,
        )?);
    }
    if schema.enable_dynamic_field {
        let mut json = Vec::new();
        for r in rows {
            for (i, meta) in r.meta.iter().flatten().enumerate() {
                let float = r
                    .meta_float
                    .as_ref()
                    .map(|(key, values)| (key.as_str(), values[i]));
                json.push(meta_json(projection, meta, float).into_bytes());
            }
        }
        out.push(pb::FieldData {
            r#type: milvus_type(DataType::Json),
            field_name: META_FIELD.to_owned(),
            is_dynamic: true,
            field: Some(field_data::Field::Scalars(pb::ScalarField {
                data: Some(scalar_field::Data::JsonData(pb::JsonArray { data: json })),
            })),
            ..Default::default()
        });
    }
    Ok(out)
}

/// 分组检索的 `group_by_field_value`：各查询的分组值按命中顺序拼接，没有命中时不出现。
/// 空值以占位值加 `valid_data` 表示，与普通输出列的编码相同。
pub(crate) fn group_by_field_data(
    schema: &CollectionSchema,
    name: &str,
    columns: &[&ColumnData],
) -> Result<Option<pb::FieldData>, EngineError> {
    if columns.iter().all(|c| c.is_empty()) {
        return Ok(None);
    }
    let field = schema
        .fields
        .iter()
        .find(|f| f.name == name)
        .ok_or_else(|| EngineError::Storage(MISMATCH.into()))?;
    field_data(
        &field.name,
        field.data_type,
        field.dim,
        field.element_type,
        &Column::Data(columns.to_vec()),
    )
    .map(Some)
}

/// 3.x 以 `json.dumps` 输出每行的动态字段：显式请求的 key 按请求顺序，否则按 dict 的顺序（引擎已按
/// 3.x 读回的顺序排好顶层 key，见 `ReadSnapshot::gather`）；嵌套对象排序（3.x 保存时 `sort_keys=True`）。
/// `float` 是 rerank 链算出的浮点 key 与这一行的值（见 [`Rows::meta_float`]）。
fn meta_json(
    projection: &Projection,
    meta: &Map<String, Value>,
    float: Option<(&str, f64)>,
) -> String {
    let entries: Vec<(&str, &Value)> = if projection.all_dynamic {
        meta.iter().map(|(k, v)| (k.as_str(), v)).collect()
    } else {
        projection
            .dynamic_keys
            .iter()
            .filter_map(|k| meta.get(k).map(|v| (k.as_str(), v)))
            .collect()
    };
    match float {
        Some(float) => pyjson::dumps_object_with_float(entries, float),
        None => pyjson::dumps_object(entries),
    }
}

/// 引擎按 schema 字段的类型产生列与主键，二者必然一致。
const MISMATCH: &str = "column type must match its schema field";

fn pk_str(p: &Pk) -> &str {
    match p {
        Pk::VarChar(s) => s,
        Pk::Int64(_) => unreachable!("{MISMATCH}"),
    }
}

pub(crate) fn pk_int(p: &Pk) -> i64 {
    match p {
        Pk::Int64(i) => *i,
        Pk::VarChar(_) => unreachable!("{MISMATCH}"),
    }
}

pub(crate) fn pk_string(p: &Pk) -> String {
    pk_str(p).to_owned()
}

enum Column<'a> {
    Pk(Vec<&'a Pk>),
    /// 同一字段在各批结果中的列，按顺序拼接。
    Data(Vec<&'a ColumnData>),
}

fn field_data(
    name: &str,
    data_type: DataType,
    dim: Option<i64>,
    element_type: Option<DataType>,
    column: &Column<'_>,
) -> Result<pb::FieldData, EngineError> {
    use scalar_field::Data;
    let field = match column {
        Column::Pk(pks) => {
            let data = match data_type {
                DataType::VarChar => Data::StringData(pb::StringArray {
                    data: pks.iter().map(|p| pk_str(p).to_owned()).collect(),
                }),
                _ => Data::LongData(pb::LongArray {
                    data: pks.iter().map(|p| pk_int(p)).collect(),
                }),
            };
            field_data::Field::Scalars(pb::ScalarField { data: Some(data) })
        }
        Column::Data(parts) => {
            if data_type == DataType::FloatVector {
                let values = parts
                    .iter()
                    .flat_map(|c| match c.values() {
                        ColumnData::FloatVector { values, .. } => values.as_slice(),
                        _ => unreachable!("{MISMATCH}"),
                    })
                    .copied()
                    .collect();
                field_data::Field::Vectors(pb::VectorField {
                    dim: dim.unwrap_or(0),
                    data: Some(vector_field::Data::FloatVector(pb::FloatArray {
                        data: values,
                    })),
                })
            } else if data_type == DataType::SparseFloatVector {
                field_data::Field::Vectors(sparse_vectors(parts)?)
            } else {
                field_data::Field::Scalars(pb::ScalarField {
                    data: Some(scalar(data_type, element_type, parts)?),
                })
            }
        }
    };
    Ok(pb::FieldData {
        r#type: milvus_type(data_type),
        field_name: name.to_owned(),
        field: Some(field),
        valid_data: match column {
            Column::Data(parts) if parts.iter().any(|c| c.validity().is_some()) => parts
                .iter()
                .flat_map(|c| (0..c.len()).map(|row| !c.is_null(row)))
                .collect(),
            _ => Vec::new(),
        },
        ..Default::default()
    })
}

/// 有效行的字节原样输出（3.x 对 bytes 行直接 append），`dim` 取解码出的最大维度号加 1，长度不对时报 3.x 的错误。
/// 有空值时 `contents` 只含有效行，与 `valid_data` 一起按 pymilvus 的紧凑约定解码；3.x 把空值行也写成 `b""`，
/// pymilvus 因此把有效行读错，这一差异见 0030。
fn sparse_vectors(parts: &[&ColumnData]) -> Result<pb::VectorField, EngineError> {
    let mut contents = Vec::new();
    let mut dim = 0;
    for column in parts {
        let ColumnData::SparseFloatVector(rows) = column.values() else {
            unreachable!("{MISMATCH}");
        };
        for (row, bytes) in rows.rows().enumerate() {
            if column.is_null(row) {
                continue;
            }
            dim = dim.max(milvus_lite_core::data::sparse::output_dim(bytes)?);
            contents.push(bytes.to_vec());
        }
    }
    Ok(pb::VectorField {
        dim: 0,
        data: Some(vector_field::Data::SparseFloatVector(
            pb::SparseFloatArray { contents, dim },
        )),
    })
}

fn scalar(
    data_type: DataType,
    element_type: Option<DataType>,
    parts: &[&ColumnData],
) -> Result<scalar_field::Data, EngineError> {
    use scalar_field::Data;
    macro_rules! collect {
        ($variant:ident, $conv:expr) => {
            parts
                .iter()
                .flat_map(|c| match c.values() {
                    ColumnData::$variant(v) => v
                        .iter()
                        .map($conv)
                        .enumerate()
                        .map(|(row, value)| {
                            if c.is_null(row) {
                                Default::default()
                            } else {
                                value
                            }
                        })
                        .collect::<Vec<_>>(),
                    _ => unreachable!("{MISMATCH}"),
                })
                .collect()
        };
    }
    Ok(match data_type {
        DataType::Array => {
            let element_type = element_type
                .ok_or_else(|| EngineError::Storage("ARRAY schema lost element_type".into()))?;
            let mut data = Vec::new();
            for column in parts {
                let ColumnData::Array(array) = column.values() else {
                    return Err(EngineError::Storage(MISMATCH.into()));
                };
                for row in 0..array.len() {
                    let range = if column.is_null(row) {
                        0..0
                    } else {
                        array.range(row)
                    };
                    if range.clone().any(|i| array.values.is_null(i)) {
                        let reason = match element_type {
                            DataType::Float | DataType::Double => {
                                "must be real number, not NoneType"
                            }
                            DataType::VarChar => "bad argument type for built-in operation",
                            _ => "'NoneType' object cannot be interpreted as an integer",
                        };
                        return Err(EngineError::InvalidArgument(reason.into()));
                    }
                    macro_rules! items {
                        ($values:expr,$map:expr) => {
                            range.clone().map(|i| ($map)(&$values[i])).collect()
                        };
                    }
                    let row_data = match array.values.values() {
                        ColumnData::Bool(v) => Data::BoolData(pb::BoolArray {
                            data: items!(v, |v: &bool| *v),
                        }),
                        ColumnData::Int8(v) => Data::IntData(pb::IntArray {
                            data: items!(v, |v: &i8| i32::from(*v)),
                        }),
                        ColumnData::Int16(v) => Data::IntData(pb::IntArray {
                            data: items!(v, |v: &i16| i32::from(*v)),
                        }),
                        ColumnData::Int32(v) => Data::IntData(pb::IntArray {
                            data: items!(v, |v: &i32| *v),
                        }),
                        ColumnData::Int64(v) => Data::LongData(pb::LongArray {
                            data: items!(v, |v: &i64| *v),
                        }),
                        ColumnData::Float(v) => Data::FloatData(pb::FloatArray {
                            data: items!(v, |v: &f32| *v),
                        }),
                        ColumnData::Double(v) => Data::DoubleData(pb::DoubleArray {
                            data: items!(v, |v: &f64| *v),
                        }),
                        ColumnData::VarChar(v) => Data::StringData(pb::StringArray {
                            data: items!(v, |v: &String| v.clone()),
                        }),
                        _ => return Err(EngineError::Storage(MISMATCH.into())),
                    };
                    data.push(pb::ScalarField {
                        data: Some(row_data),
                    });
                }
            }
            Data::ArrayData(pb::ArrayArray {
                data,
                element_type: milvus_type(element_type),
            })
        }
        DataType::Bool => Data::BoolData(pb::BoolArray {
            data: collect!(Bool, |v| *v),
        }),
        // rerank 链算出的值写进 INT8 / INT16 字段时由 int32 列承载（3.x 不按字段类型收窄），
        // 存储列仍是字段自己的宽度，两种都写进 protobuf 的 int32 槽。
        DataType::Int8 | DataType::Int16 | DataType::Int32 => Data::IntData(pb::IntArray {
            data: parts
                .iter()
                .flat_map(|c| {
                    let values: Vec<i32> = match c.values() {
                        ColumnData::Int8(v) => v.iter().map(|x| i32::from(*x)).collect(),
                        ColumnData::Int16(v) => v.iter().map(|x| i32::from(*x)).collect(),
                        ColumnData::Int32(v) => v.clone(),
                        _ => unreachable!("{MISMATCH}"),
                    };
                    values
                        .into_iter()
                        .enumerate()
                        .map(|(row, value)| if c.is_null(row) { 0 } else { value })
                        .collect::<Vec<_>>()
                })
                .collect(),
        }),
        DataType::Int64 => Data::LongData(pb::LongArray {
            data: collect!(Int64, |v| *v),
        }),
        // 3.x 以 UTC 的 ISO 字符串输出（`micros_to_iso_z`），空值行为空串；越界值只可能来自 3.x 写出的毒化数据。
        DataType::Timestamptz => {
            let mut text = Vec::new();
            for column in parts {
                let ColumnData::Int64(values) = column.values() else {
                    unreachable!("{MISMATCH}");
                };
                for (row, &micros) in values.iter().enumerate() {
                    text.push(if column.is_null(row) {
                        String::new()
                    } else {
                        milvus_lite_core::timestamptz::format_iso_z(micros)?
                    });
                }
            }
            Data::StringData(pb::StringArray { data: text })
        }
        DataType::Float => Data::FloatData(pb::FloatArray {
            data: collect!(Float, |v| *v),
        }),
        DataType::Double => Data::DoubleData(pb::DoubleArray {
            data: collect!(Double, |v| *v),
        }),
        DataType::VarChar => Data::StringData(pb::StringArray {
            data: collect!(VarChar, Clone::clone),
        }),
        DataType::Geometry => Data::GeometryWktData(pb::GeometryWktArray {
            data: collect!(VarChar, Clone::clone),
        }),
        DataType::Json => Data::JsonData(pb::JsonArray {
            data: parts
                .iter()
                .flat_map(|column| match column.values() {
                    ColumnData::Json(values) => values
                        .iter()
                        .enumerate()
                        .map(|(row, v)| {
                            if column.is_null(row) {
                                b"null".to_vec()
                            } else {
                                v.clone()
                            }
                        })
                        .collect::<Vec<_>>(),
                    _ => unreachable!("{MISMATCH}"),
                })
                .collect(),
        }),
        // 向量列在 field_data 中单独处理。
        DataType::FloatVector | DataType::SparseFloatVector => {
            Data::FloatData(pb::FloatArray { data: Vec::new() })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use milvus_lite_core::data::{SparseData, Validity};

    fn pack(pairs: &[(u32, f32)]) -> Vec<u8> {
        pairs
            .iter()
            .flat_map(|(d, w)| d.to_le_bytes().into_iter().chain(w.to_le_bytes()))
            .collect()
    }

    fn sparse_field(parts: &[&ColumnData]) -> Result<pb::FieldData, EngineError> {
        field_data(
            "sp",
            DataType::SparseFloatVector,
            None,
            None,
            &Column::Data(parts.to_vec()),
        )
    }

    fn contents(fd: &pb::FieldData) -> (&[Vec<u8>], i64, i64) {
        let Some(field_data::Field::Vectors(vectors)) = &fd.field else {
            panic!("sparse output is not a vector field");
        };
        let Some(vector_field::Data::SparseFloatVector(array)) = &vectors.data else {
            panic!("sparse output lost its slot");
        };
        (&array.contents, array.dim, vectors.dim)
    }

    /// 没有空值时与 3.x 相同：按行原样输出字节（乱序与零权重不改写），`dim` 含零权重维度。
    #[test]
    fn rows_pass_through_and_dim_counts_every_decoded_dimension() {
        let unsorted = pack(&[(9, 1.0), (2, 0.0)]);
        let column =
            ColumnData::SparseFloatVector(SparseData::from_rows([unsorted.clone(), Vec::new()]));
        let fd = sparse_field(&[&column]).unwrap();
        assert!(fd.valid_data.is_empty());
        assert_eq!(fd.r#type, 104);
        let (rows, dim, vector_dim) = contents(&fd);
        assert_eq!(rows, &[unsorted, Vec::new()]);
        assert_eq!((dim, vector_dim), (10, 0));
    }

    /// 有空值时 contents 只含有效行，与 valid_data 一起按 pymilvus 的紧凑约定解码；3.x 的补齐写法是登记的差异。
    #[test]
    fn nulls_use_compact_contents_and_valid_rows_only_for_dim() {
        let nullable = |rows: SparseData, valid: &[bool]| ColumnData::Nullable {
            values: Box::new(ColumnData::SparseFloatVector(rows)),
            validity: Validity::from_bools(valid),
        };
        let first = nullable(
            SparseData::from_rows([Vec::new(), pack(&[(3, 1.0)])]),
            &[false, true],
        );
        let second = ColumnData::SparseFloatVector(SparseData::from_rows([pack(&[(1, 2.0)])]));
        let fd = sparse_field(&[&first, &second]).unwrap();
        assert_eq!(fd.valid_data, vec![false, true, true]);
        let (rows, dim, _) = contents(&fd);
        assert_eq!(rows, &[pack(&[(3, 1.0)]), pack(&[(1, 2.0)])]);
        assert_eq!(dim, 4);

        let all_null = nullable(SparseData::from_rows([Vec::<u8>::new()]), &[false]);
        let fd = sparse_field(&[&all_null]).unwrap();
        assert_eq!(fd.valid_data, vec![false]);
        let (rows, dim, _) = contents(&fd);
        assert!(rows.is_empty());
        assert_eq!(dim, 0);
    }

    /// 3.x 为算 `dim` 解码每个 bytes 行，所以长度不对的历史行在输出时报 code 1。
    #[test]
    fn malformed_stored_row_fails_output_like_3x() {
        let column =
            ColumnData::SparseFloatVector(SparseData::from_rows([vec![1, 2, 3, 4, 5, 6, 7]]));
        let error = sparse_field(&[&column]).unwrap_err();
        assert!(matches!(error, EngineError::InvalidArgument(_)));
        assert_eq!(
            error.to_string(),
            "sparse vector bytes length 7 is not a multiple of 8"
        );
    }
}
