//! M1 列式值到 3.x Arrow data / WAL schema 的转换；不负责写盘与提交。

use std::sync::Arc;

use arrow_array::{
    Array, ArrayRef, BooleanArray, FixedSizeListArray, Float32Array, Float64Array, Int8Array,
    Int16Array, Int32Array, Int64Array, ListArray, RecordBatch, StringArray, UInt64Array,
};
use arrow_schema::{DataType as ArrowType, Field, Schema};
use serde_json::{Map, Value};

use crate::data::{ArrayData, ColumnData, Pk, Validity};
use crate::pyjson;
use crate::schema::{CollectionSchema, DataType, FieldSchema};

#[derive(Debug, thiserror::Error)]
pub enum ArrowDataError {
    #[error("column count does not match schema")]
    ColumnCount,
    #[error("column {name:?} has {actual} rows, expected {expected}")]
    RowCount {
        name: String,
        actual: usize,
        expected: usize,
    },
    #[error("field {name:?} vector dimension is invalid")]
    VectorDim { name: String },
    #[error("field {name:?} contains invalid JSON text: {source}")]
    JsonText {
        name: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("field {name:?} contains a JSON number outside the finite float64 range: {value}")]
    NonFiniteJsonNumber { name: String, value: String },
    #[error("field {name:?} has an incompatible Arrow type or null value")]
    TypeMismatch { name: String },
    #[error("Arrow column {position} is {actual:?}, expected {expected:?}")]
    ColumnName {
        position: usize,
        expected: String,
        actual: String,
    },
    #[error("$meta is not a JSON object: {0}")]
    InvalidMeta(String),
    #[error("Arrow batch error: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedData {
    pub seqs: Vec<u64>,
    pub columns: Vec<ColumnData>,
    pub meta: Option<Vec<Map<String, Value>>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedDelta {
    pub pks: Vec<Pk>,
    pub seqs: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartitionLabels {
    Uniform(String),
    Rows(Vec<String>),
}

impl PartitionLabels {
    pub fn value(&self, row: usize) -> Option<&str> {
        match self {
            Self::Uniform(value) => Some(value),
            Self::Rows(values) => values.get(row).map(String::as_str),
        }
    }
}

fn partition_labels(batch: &RecordBatch, column: usize) -> Result<PartitionLabels, ArrowDataError> {
    let values = batch
        .column(column)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| ArrowDataError::TypeMismatch {
            name: "_partition".into(),
        })?;
    if values.null_count() != 0 {
        return Err(ArrowDataError::TypeMismatch {
            name: "_partition".into(),
        });
    }
    let first = if values.is_empty() {
        "_default"
    } else {
        values.value(0)
    };
    Ok(if (0..values.len()).all(|row| values.value(row) == first) {
        PartitionLabels::Uniform(first.to_owned())
    } else {
        PartitionLabels::Rows(
            (0..values.len())
                .map(|row| values.value(row).to_owned())
                .collect(),
        )
    })
}

fn json_strings(name: &str, values: &[Vec<u8>]) -> Result<Vec<String>, ArrowDataError> {
    #[cfg(test)]
    JSON_ENCODED_VALUES.with(|count| count.set(count.get() + values.len()));
    values
        .iter()
        .map(|raw| {
            let value = serde_json::from_slice::<Value>(raw).map_err(|source| {
                ArrowDataError::JsonText {
                    name: name.to_owned(),
                    source,
                }
            })?;
            check_finite_json_numbers(name, &value)?;
            Ok(pyjson::dumps_unicode(&value))
        })
        .collect()
}

#[cfg(test)]
thread_local! {
    static JSON_ENCODED_VALUES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn take_json_encoding_count() -> usize {
    JSON_ENCODED_VALUES.with(|count| count.replace(0))
}

/// 独占借用结束后只暴露只读列，WAL 复用期间不能替换已规范化的字节或 schema。
#[derive(Debug)]
pub(crate) struct NormalizedColumns<'a> {
    schema: &'a CollectionSchema,
    columns: &'a [ColumnData],
}

impl NormalizedColumns<'_> {
    pub(crate) fn columns(&self) -> &[ColumnData] {
        self.columns
    }

    pub(crate) fn schema(&self) -> &CollectionSchema {
        self.schema
    }

    pub(crate) fn wal_batch(
        &self,
        seqs: &[u64],
        partition: &str,
        meta: Option<&[Map<String, Value>]>,
    ) -> Result<RecordBatch, ArrowDataError> {
        build_batch(
            self.schema,
            seqs,
            Some(partition),
            self.columns,
            meta,
            JsonEncoding::Normalized,
        )
    }
}

fn check_finite_json_numbers(name: &str, value: &Value) -> Result<(), ArrowDataError> {
    match value {
        Value::Number(number) => {
            let raw = number.to_string();
            // 与 pyjson 的浮点分支使用同一判定；整数文本不能因超出 f64 而被拒绝。
            if pyjson::json_float_text(&raw).is_some_and(|v| v.is_infinite()) {
                return Err(ArrowDataError::NonFiniteJsonNumber {
                    name: name.to_owned(),
                    value: raw,
                });
            }
        }
        Value::Array(values) => {
            for value in values {
                check_finite_json_numbers(name, value)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                check_finite_json_numbers(name, value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// 持久化读取视图与写出的 Arrow 批采用同一 JSON 文本，发布不改变读取字节。
pub(crate) fn normalize_data_json<'a>(
    schema: &'a CollectionSchema,
    columns: &'a mut [ColumnData],
) -> Result<NormalizedColumns<'a>, ArrowDataError> {
    for (field, column) in schema.fields.iter().zip(columns.iter_mut()) {
        if let ColumnData::Json(values) = column.values_mut() {
            *values = json_strings(&field.name, values)?
                .into_iter()
                .map(String::into_bytes)
                .collect();
        }
    }
    Ok(NormalizedColumns { schema, columns })
}

pub fn data_schema(schema: &CollectionSchema) -> Result<Schema, ArrowDataError> {
    let mut fields = vec![Field::new("_seq", ArrowType::UInt64, false)];
    fields.extend(user_fields(schema)?);
    if schema.enable_dynamic_field {
        fields.push(Field::new("$meta", ArrowType::Utf8, true));
    }
    Ok(Schema::new(fields))
}

pub fn wal_data_schema(schema: &CollectionSchema) -> Result<Schema, ArrowDataError> {
    let mut fields = vec![
        Field::new("_seq", ArrowType::UInt64, false),
        Field::new("_partition", ArrowType::Utf8, false),
    ];
    fields.extend(user_fields(schema)?);
    if schema.enable_dynamic_field {
        fields.push(Field::new("$meta", ArrowType::Utf8, true));
    }
    Ok(Schema::new(fields))
}

pub fn delta_schema(schema: &CollectionSchema) -> Result<Schema, ArrowDataError> {
    let pk = schema
        .primary_position()
        .ok_or(ArrowDataError::ColumnCount)?;
    let field = &schema.fields[pk];
    let dtype = match field.data_type {
        DataType::Int64 => ArrowType::Int64,
        DataType::VarChar => ArrowType::Utf8,
        _ => {
            return Err(ArrowDataError::TypeMismatch {
                name: field.name.clone(),
            });
        }
    };
    Ok(Schema::new(vec![
        Field::new(&field.name, dtype, false),
        Field::new("_seq", ArrowType::UInt64, false),
    ]))
}

pub fn wal_delta_schema(schema: &CollectionSchema) -> Result<Schema, ArrowDataError> {
    let mut fields = delta_schema(schema)?
        .fields()
        .iter()
        .map(|field| field.as_ref().clone())
        .collect::<Vec<_>>();
    fields.push(Field::new("_partition", ArrowType::Utf8, false));
    Ok(Schema::new(fields))
}

pub fn delta_batch(
    schema: &CollectionSchema,
    pks: &[Pk],
    seqs: &[u64],
) -> Result<RecordBatch, ArrowDataError> {
    build_delta_batch(schema, pks, seqs, None)
}

pub fn wal_delta_batch(
    schema: &CollectionSchema,
    pks: &[Pk],
    seqs: &[u64],
    partition: &str,
) -> Result<RecordBatch, ArrowDataError> {
    build_delta_batch(schema, pks, seqs, Some(partition))
}

fn build_delta_batch(
    schema: &CollectionSchema,
    pks: &[Pk],
    seqs: &[u64],
    partition: Option<&str>,
) -> Result<RecordBatch, ArrowDataError> {
    if pks.len() != seqs.len() {
        return Err(ArrowDataError::RowCount {
            name: "_seq".to_owned(),
            actual: seqs.len(),
            expected: pks.len(),
        });
    }
    let pk_index = schema
        .primary_position()
        .ok_or(ArrowDataError::ColumnCount)?;
    let field = &schema.fields[pk_index];
    let pk_array: ArrayRef = match field.data_type {
        DataType::Int64 => Arc::new(Int64Array::from(
            pks.iter()
                .map(|pk| match pk {
                    Pk::Int64(value) => Ok(*value),
                    _ => Err(ArrowDataError::TypeMismatch {
                        name: field.name.clone(),
                    }),
                })
                .collect::<Result<Vec<_>, _>>()?,
        )),
        DataType::VarChar => Arc::new(StringArray::from_iter_values(
            pks.iter()
                .map(|pk| match pk {
                    Pk::VarChar(value) => Ok(value.as_str()),
                    _ => Err(ArrowDataError::TypeMismatch {
                        name: field.name.clone(),
                    }),
                })
                .collect::<Result<Vec<_>, _>>()?,
        )),
        _ => {
            return Err(ArrowDataError::TypeMismatch {
                name: field.name.clone(),
            });
        }
    };
    let mut arrays: Vec<ArrayRef> = vec![pk_array, Arc::new(UInt64Array::from(seqs.to_vec()))];
    if let Some(partition) = partition {
        arrays.push(Arc::new(StringArray::from_iter_values(
            std::iter::repeat_n(partition, pks.len()),
        )));
    }
    let arrow_schema = if partition.is_some() {
        wal_delta_schema(schema)?
    } else {
        delta_schema(schema)?
    };
    Ok(RecordBatch::try_new(Arc::new(arrow_schema), arrays)?)
}

pub fn decode_delta_batch(
    schema: &CollectionSchema,
    batch: &RecordBatch,
) -> Result<DecodedDelta, ArrowDataError> {
    decode_delta(schema, batch, false, false)
}

pub fn decode_wal_delta_batch(
    schema: &CollectionSchema,
    batch: &RecordBatch,
) -> Result<DecodedDelta, ArrowDataError> {
    decode_delta(schema, batch, true, false)
}

pub fn decode_partitioned_wal_delta(
    schema: &CollectionSchema,
    batch: &RecordBatch,
) -> Result<(DecodedDelta, PartitionLabels), ArrowDataError> {
    Ok((
        decode_delta(schema, batch, true, true)?,
        partition_labels(batch, 2)?,
    ))
}

fn decode_delta(
    schema: &CollectionSchema,
    batch: &RecordBatch,
    has_partition: bool,
    any_partition: bool,
) -> Result<DecodedDelta, ArrowDataError> {
    if batch.num_columns() != 2 + usize::from(has_partition) {
        return Err(ArrowDataError::ColumnCount);
    }
    let pk_index = schema
        .primary_position()
        .ok_or(ArrowDataError::ColumnCount)?;
    let field = &schema.fields[pk_index];
    require_name(batch, 0, &field.name)?;
    require_name(batch, 1, "_seq")?;
    if has_partition {
        require_name(batch, 2, "_partition")?;
    }
    let pk_array = batch.column(0);
    if pk_array.null_count() != 0 {
        return Err(ArrowDataError::TypeMismatch {
            name: field.name.clone(),
        });
    }
    let pks = match field.data_type {
        DataType::Int64 => {
            let values = pk_array
                .as_any()
                .downcast_ref::<Int64Array>()
                .ok_or_else(|| ArrowDataError::TypeMismatch {
                    name: field.name.clone(),
                })?;
            (0..values.len())
                .map(|i| Pk::Int64(values.value(i)))
                .collect()
        }
        DataType::VarChar => {
            let values = pk_array
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| ArrowDataError::TypeMismatch {
                    name: field.name.clone(),
                })?;
            (0..values.len())
                .map(|i| Pk::VarChar(values.value(i).to_owned()))
                .collect()
        }
        _ => {
            return Err(ArrowDataError::TypeMismatch {
                name: field.name.clone(),
            });
        }
    };
    let seqs = batch
        .column(1)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| ArrowDataError::TypeMismatch {
            name: "_seq".to_owned(),
        })?;
    if seqs.null_count() != 0 {
        return Err(ArrowDataError::TypeMismatch {
            name: "_seq".to_owned(),
        });
    }
    if has_partition {
        let parts = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| ArrowDataError::TypeMismatch {
                name: "_partition".to_owned(),
            })?;
        if parts.null_count() != 0
            || (!any_partition
                && (0..parts.len()).any(|i| !matches!(parts.value(i), "_default" | "_all")))
        {
            return Err(ArrowDataError::TypeMismatch {
                name: "_partition".to_owned(),
            });
        }
    }
    Ok(DecodedDelta {
        pks,
        seqs: seqs.values().to_vec(),
    })
}

fn user_fields(schema: &CollectionSchema) -> Result<Vec<Field>, ArrowDataError> {
    schema
        .fields
        .iter()
        .map(|field| {
            let dtype = match field.data_type {
                DataType::Bool => ArrowType::Boolean,
                DataType::Int8 => ArrowType::Int8,
                DataType::Int16 => ArrowType::Int16,
                DataType::Int32 => ArrowType::Int32,
                DataType::Int64 => ArrowType::Int64,
                DataType::Float => ArrowType::Float32,
                DataType::Double => ArrowType::Float64,
                DataType::VarChar | DataType::Json => ArrowType::Utf8,
                DataType::Array => {
                    let element =
                        field
                            .element_type
                            .ok_or_else(|| ArrowDataError::TypeMismatch {
                                name: field.name.clone(),
                            })?;
                    let child = user_fields(&CollectionSchema {
                        fields: vec![FieldSchema {
                            nullable: true,
                            ..FieldSchema::new("item", element)
                        }],
                        ..Default::default()
                    })?;
                    ArrowType::List(Arc::new(child[0].clone()))
                }
                DataType::FloatVector => {
                    let dim = field
                        .dim
                        .and_then(|dim| i32::try_from(dim).ok())
                        .filter(|dim| *dim > 0)
                        .ok_or_else(|| ArrowDataError::VectorDim {
                            name: field.name.clone(),
                        })?;
                    ArrowType::FixedSizeList(
                        Arc::new(Field::new("item", ArrowType::Float32, true)),
                        dim,
                    )
                }
            };
            Ok(Field::new(&field.name, dtype, field.nullable))
        })
        .collect()
}

pub fn data_batch(
    schema: &CollectionSchema,
    seqs: &[u64],
    columns: &[ColumnData],
    meta: Option<&[Map<String, Value>]>,
) -> Result<RecordBatch, ArrowDataError> {
    build_batch(
        schema,
        seqs,
        None,
        columns,
        meta,
        JsonEncoding::Canonicalize,
    )
}

pub fn wal_data_batch(
    schema: &CollectionSchema,
    seqs: &[u64],
    partition: &str,
    columns: &[ColumnData],
    meta: Option<&[Map<String, Value>]>,
) -> Result<RecordBatch, ArrowDataError> {
    build_batch(
        schema,
        seqs,
        Some(partition),
        columns,
        meta,
        JsonEncoding::Canonicalize,
    )
}

#[derive(Clone, Copy)]
enum JsonEncoding {
    Canonicalize,
    Normalized,
}

fn json_array(
    name: &str,
    values: &[Vec<u8>],
    validity: Option<&Validity>,
    encoding: JsonEncoding,
) -> Result<ArrayRef, ArrowDataError> {
    let owned;
    let strings: Vec<&str> = match encoding {
        JsonEncoding::Canonicalize => {
            owned = json_strings(name, values)?;
            owned.iter().map(String::as_str).collect()
        }
        JsonEncoding::Normalized => values
            .iter()
            .map(|raw| {
                std::str::from_utf8(raw)
                    .map_err(|_| ArrowDataError::TypeMismatch { name: name.into() })
            })
            .collect::<Result<_, _>>()?,
    };
    Ok(Arc::new(StringArray::from_iter(
        strings
            .into_iter()
            .enumerate()
            .map(|(row, value)| validity.is_none_or(|v| v.get(row)).then_some(value)),
    )))
}

fn build_batch(
    schema: &CollectionSchema,
    seqs: &[u64],
    partition: Option<&str>,
    columns: &[ColumnData],
    meta: Option<&[Map<String, Value>]>,
    json_encoding: JsonEncoding,
) -> Result<RecordBatch, ArrowDataError> {
    if columns.len() != schema.fields.len() {
        return Err(ArrowDataError::ColumnCount);
    }
    let n = seqs.len();
    let mut arrays: Vec<ArrayRef> = vec![Arc::new(UInt64Array::from(seqs.to_vec()))];
    if let Some(partition) = partition {
        arrays.push(Arc::new(StringArray::from_iter_values(
            std::iter::repeat_n(partition, n),
        )));
    }
    for (field, column) in schema.fields.iter().zip(columns) {
        if column.len() != n {
            return Err(ArrowDataError::RowCount {
                name: field.name.clone(),
                actual: column.len(),
                expected: n,
            });
        }
        let array: ArrayRef = match column {
            ColumnData::Array(array) => encode_array(array, None)?,
            ColumnData::Nullable { values, validity } => {
                nullable_array(&field.name, values, validity, json_encoding)?
            }
            ColumnData::Bool(v) => Arc::new(BooleanArray::from(v.clone())),
            ColumnData::Int8(v) => Arc::new(Int8Array::from(v.clone())),
            ColumnData::Int16(v) => Arc::new(Int16Array::from(v.clone())),
            ColumnData::Int32(v) => Arc::new(Int32Array::from(v.clone())),
            ColumnData::Int64(v) => Arc::new(Int64Array::from(v.clone())),
            ColumnData::Float(v) => Arc::new(Float32Array::from(v.clone())),
            ColumnData::Double(v) => Arc::new(Float64Array::from(v.clone())),
            ColumnData::VarChar(v) => Arc::new(StringArray::from_iter_values(v)),
            ColumnData::Json(v) => json_array(&field.name, v, None, json_encoding)?,
            ColumnData::FloatVector { dim, values } => {
                let dim = i32::try_from(*dim)
                    .ok()
                    .filter(|dim| *dim > 0)
                    .ok_or_else(|| ArrowDataError::VectorDim {
                        name: field.name.clone(),
                    })?;
                Arc::new(FixedSizeListArray::try_new(
                    Arc::new(Field::new("item", ArrowType::Float32, true)),
                    dim,
                    Arc::new(Float32Array::from(values.clone())),
                    None,
                )?)
            }
        };
        arrays.push(array);
    }
    if schema.enable_dynamic_field {
        let values = meta.ok_or(ArrowDataError::ColumnCount)?;
        if values.len() != n {
            return Err(ArrowDataError::RowCount {
                name: "$meta".to_owned(),
                actual: values.len(),
                expected: n,
            });
        }
        let json: Vec<String> = values
            .iter()
            .map(|map| pyjson::dumps(&Value::Object(map.clone())))
            .collect();
        arrays.push(Arc::new(StringArray::from(json)));
    }
    let arrow_schema = if partition.is_some() {
        wal_data_schema(schema)?
    } else {
        data_schema(schema)?
    };
    Ok(RecordBatch::try_new(Arc::new(arrow_schema), arrays)?)
}

fn nullable_array(
    name: &str,
    values: &ColumnData,
    validity: &Validity,
    json_encoding: JsonEncoding,
) -> Result<ArrayRef, ArrowDataError> {
    if values.len() != validity.len() {
        return Err(ArrowDataError::RowCount {
            name: name.into(),
            actual: validity.len(),
            expected: values.len(),
        });
    }
    macro_rules! numbers {
        ($array:ident, $values:expr) => {
            Arc::new($array::from_iter(
                $values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| validity.get(i).then_some(*v)),
            )) as ArrayRef
        };
    }
    Ok(match values {
        ColumnData::Array(array) => encode_array(array, Some(validity))?,
        ColumnData::Bool(v) => numbers!(BooleanArray, v),
        ColumnData::Int8(v) => numbers!(Int8Array, v),
        ColumnData::Int16(v) => numbers!(Int16Array, v),
        ColumnData::Int32(v) => numbers!(Int32Array, v),
        ColumnData::Int64(v) => numbers!(Int64Array, v),
        ColumnData::Float(v) => numbers!(Float32Array, v),
        ColumnData::Double(v) => numbers!(Float64Array, v),
        ColumnData::VarChar(v) => Arc::new(StringArray::from_iter(
            v.iter()
                .enumerate()
                .map(|(i, v)| validity.get(i).then_some(v.as_str())),
        )),
        ColumnData::Json(v) => json_array(name, v, Some(validity), json_encoding)?,
        _ => return Err(ArrowDataError::TypeMismatch { name: name.into() }),
    })
}

fn encode_array(
    array: &ArrayData,
    validity: Option<&Validity>,
) -> Result<ArrayRef, ArrowDataError> {
    use arrow_array::builder::*;
    if !array.valid(array.element_type) {
        return Err(ArrowDataError::TypeMismatch {
            name: "ARRAY offset overflow".into(),
        });
    }
    macro_rules! list {
        ($builder:ident, $values:expr, $get:expr) => {{
            let values = $values;
            let mut builder = ListBuilder::new($builder::new());
            for row in 0..array.len() {
                for i in array.range(row) {
                    if array.values.is_null(i) {
                        builder.values().append_null();
                    } else {
                        builder.values().append_value(($get)(values, i));
                    }
                }
                builder.append(validity.is_none_or(|v| v.get(row)));
            }
            Arc::new(builder.finish()) as ArrayRef
        }};
    }
    Ok(match array.values.values() {
        ColumnData::Bool(v) => list!(BooleanBuilder, v, |v: &Vec<bool>, i| v[i]),
        ColumnData::Int8(v) => list!(Int8Builder, v, |v: &Vec<i8>, i| v[i]),
        ColumnData::Int16(v) => list!(Int16Builder, v, |v: &Vec<i16>, i| v[i]),
        ColumnData::Int32(v) => list!(Int32Builder, v, |v: &Vec<i32>, i| v[i]),
        ColumnData::Int64(v) => list!(Int64Builder, v, |v: &Vec<i64>, i| v[i]),
        ColumnData::Float(v) => list!(Float32Builder, v, |v: &Vec<f32>, i| v[i]),
        ColumnData::Double(v) => list!(Float64Builder, v, |v: &Vec<f64>, i| v[i]),
        ColumnData::VarChar(v) => list!(StringBuilder, v, |v: &Vec<String>, i: usize| v[i].clone()),
        _ => {
            return Err(ArrowDataError::TypeMismatch {
                name: "ARRAY child".into(),
            });
        }
    })
}

fn decode_column(field: &FieldSchema, array: &ArrayRef) -> Result<ColumnData, ArrowDataError> {
    let n = array.len();
    if array.null_count() != 0 && !field.nullable {
        return Err(ArrowDataError::TypeMismatch {
            name: field.name.clone(),
        });
    }
    let bad = || ArrowDataError::TypeMismatch {
        name: field.name.clone(),
    };
    let values = match field.data_type {
        DataType::Array => {
            let list = array.as_any().downcast_ref::<ListArray>().ok_or_else(bad)?;
            let element_type = field.element_type.ok_or_else(bad)?;
            let offsets = list
                .value_offsets()
                .iter()
                .map(|&i| usize::try_from(i).map_err(|_| bad()))
                .collect::<Result<Vec<_>, _>>()?;
            let start = *offsets.first().ok_or_else(bad)?;
            let end = *offsets.last().ok_or_else(bad)?;
            let length = end
                .checked_sub(start)
                .filter(|_| end <= list.values().len())
                .ok_or_else(bad)?;
            let child = FieldSchema {
                nullable: true,
                ..FieldSchema::new("item", element_type)
            };
            let values = decode_column(&child, &list.values().slice(start, length))?;
            let ArrowType::List(item) = array.data_type() else {
                return Err(bad());
            };
            ColumnData::Array(ArrayData {
                element_type,
                element_name: item.name().clone(),
                offsets: offsets.into_iter().map(|i| i - start).collect(),
                values: Box::new(values),
            })
        }
        DataType::Bool => ColumnData::Bool(
            array
                .as_any()
                .downcast_ref::<BooleanArray>()
                .ok_or_else(bad)?
                .values()
                .iter()
                .collect(),
        ),
        DataType::Int8 => ColumnData::Int8(
            array
                .as_any()
                .downcast_ref::<Int8Array>()
                .ok_or_else(bad)?
                .values()
                .to_vec(),
        ),
        DataType::Int16 => ColumnData::Int16(
            array
                .as_any()
                .downcast_ref::<Int16Array>()
                .ok_or_else(bad)?
                .values()
                .to_vec(),
        ),
        DataType::Int32 => ColumnData::Int32(
            array
                .as_any()
                .downcast_ref::<Int32Array>()
                .ok_or_else(bad)?
                .values()
                .to_vec(),
        ),
        DataType::Int64 => ColumnData::Int64(
            array
                .as_any()
                .downcast_ref::<Int64Array>()
                .ok_or_else(bad)?
                .values()
                .to_vec(),
        ),
        DataType::Float => ColumnData::Float(
            array
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or_else(bad)?
                .values()
                .to_vec(),
        ),
        DataType::Double => ColumnData::Double(
            array
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(bad)?
                .values()
                .to_vec(),
        ),
        DataType::VarChar => {
            let strings = array
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(bad)?;
            ColumnData::VarChar(
                (0..n)
                    .map(|row| {
                        if strings.is_null(row) {
                            String::new()
                        } else {
                            strings.value(row).to_owned()
                        }
                    })
                    .collect(),
            )
        }
        DataType::Json => {
            let strings = array
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(bad)?;
            ColumnData::Json(
                (0..n)
                    .map(|row| {
                        if strings.is_null(row) {
                            b"null".to_vec()
                        } else {
                            strings.value(row).as_bytes().to_vec()
                        }
                    })
                    .collect(),
            )
        }
        DataType::FloatVector => {
            let list = array
                .as_any()
                .downcast_ref::<FixedSizeListArray>()
                .ok_or_else(bad)?;
            let dim = usize::try_from(list.value_length()).map_err(|_| bad())?;
            if Some(dim as i64) != field.dim {
                return Err(bad());
            }
            // Parquet 读出的 list 子字段名可为 `element`，WAL 中为 `item`；只校验值的类型与维度。
            let floats = list.values();
            let floats = floats
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or_else(bad)?;
            if floats.null_count() != 0 {
                return Err(bad());
            }
            ColumnData::FloatVector {
                dim,
                values: floats.values().to_vec(),
            }
        }
    };
    Ok(if array.null_count() == 0 {
        values
    } else {
        values.with_validity(&(0..n).map(|row| array.is_valid(row)).collect::<Vec<_>>())
    })
}

pub fn decode_data_batch(
    schema: &CollectionSchema,
    batch: &RecordBatch,
) -> Result<DecodedData, ArrowDataError> {
    decode_batch(schema, batch, false, false)
}

pub fn decode_wal_data_batch(
    schema: &CollectionSchema,
    batch: &RecordBatch,
) -> Result<DecodedData, ArrowDataError> {
    decode_batch(schema, batch, true, false)
}

pub fn decode_partitioned_wal_data(
    schema: &CollectionSchema,
    batch: &RecordBatch,
) -> Result<(DecodedData, PartitionLabels), ArrowDataError> {
    Ok((
        decode_batch(schema, batch, true, true)?,
        partition_labels(batch, 1)?,
    ))
}

fn decode_batch(
    schema: &CollectionSchema,
    batch: &RecordBatch,
    has_partition: bool,
    any_partition: bool,
) -> Result<DecodedData, ArrowDataError> {
    let n = batch.num_rows();
    let expected = 1
        + usize::from(has_partition)
        + schema.fields.len()
        + usize::from(schema.enable_dynamic_field);
    if batch.num_columns() != expected {
        return Err(ArrowDataError::ColumnCount);
    }
    require_name(batch, 0, "_seq")?;
    if has_partition {
        require_name(batch, 1, "_partition")?;
    }
    let seq_array = batch
        .column(0)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| ArrowDataError::TypeMismatch {
            name: "_seq".to_owned(),
        })?;
    if seq_array.null_count() != 0 {
        return Err(ArrowDataError::TypeMismatch {
            name: "_seq".to_owned(),
        });
    }
    let seqs = seq_array.values().to_vec();
    let start = if has_partition {
        let partitions = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| ArrowDataError::TypeMismatch {
                name: "_partition".to_owned(),
            })?;
        if partitions.null_count() != 0
            || (!any_partition && (0..n).any(|row| partitions.value(row) != "_default"))
        {
            return Err(ArrowDataError::TypeMismatch {
                name: "_partition".to_owned(),
            });
        }
        2
    } else {
        1
    };
    let mut columns = Vec::with_capacity(schema.fields.len());
    for (offset, field) in schema.fields.iter().enumerate() {
        require_name(batch, start + offset, &field.name)?;
        let array = batch.column(start + offset);
        let mut column = decode_column(field, array)?;
        if !has_partition && let ColumnData::Array(array) = column.values_mut() {
            // PyArrow 15/25 的 Parquet 读取采用 compliant nested type 的子字段名。
            array.element_name = "element".into();
        }
        columns.push(column);
    }
    let meta = if schema.enable_dynamic_field {
        require_name(batch, start + schema.fields.len(), "$meta")?;
        let array = batch.column(start + schema.fields.len());
        let strings = array
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| ArrowDataError::TypeMismatch {
                name: "$meta".to_owned(),
            })?;
        let mut meta = Vec::with_capacity(n);
        for row in 0..n {
            if strings.is_null(row) {
                meta.push(Map::new());
                continue;
            }
            let value: Value = serde_json::from_str(strings.value(row))
                .map_err(|source| ArrowDataError::InvalidMeta(source.to_string()))?;
            let Value::Object(map) = value else {
                return Err(ArrowDataError::InvalidMeta(format!(
                    "row {row} is not an object"
                )));
            };
            meta.push(map);
        }
        Some(meta)
    } else {
        None
    };
    Ok(DecodedData {
        seqs,
        columns,
        meta,
    })
}

fn require_name(
    batch: &RecordBatch,
    position: usize,
    expected: &str,
) -> Result<(), ArrowDataError> {
    let batch_schema = batch.schema();
    let actual = batch_schema.field(position).name();
    if actual != expected {
        return Err(ArrowDataError::ColumnName {
            position,
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::schema::FieldSchema;

    use super::*;

    #[test]
    fn normalized_wal_keeps_exact_json_bytes_and_validity_with_one_encoding() {
        let raw = [
            r#" {"z":1E2,"a":"中文\u0000\t\n\"\\","wide":184467440737095516160} "#,
            r#"{"negative_zero":-0.0,"exponent":1e-12,"max":1.7976931348623157e308,"min":5e-324}"#,
            r#"{"underflow":-1e-999,"nested":[true,null,{},[1,2.5]],"text":"\ud83d\ude00"}"#,
            " null\t\r\n",
            "[1,2,3]",
            "{}",
            "0.1",
        ];
        for mask in [
            None,
            Some(vec![true; raw.len()]),
            Some(vec![false; raw.len()]),
            Some((0..raw.len()).map(|i| i % 2 == 0).collect()),
        ] {
            let mut field = FieldSchema::new("j", DataType::Json);
            field.nullable = mask.is_some();
            let schema = CollectionSchema {
                fields: vec![field],
                enable_dynamic_field: true,
                properties: vec![],
            };
            let mut column = ColumnData::Json(raw.iter().map(|s| s.as_bytes().to_vec()).collect());
            if let Some(mask) = mask {
                column = column.with_validity(&mask);
            }
            let mut columns = vec![column];
            let seqs: Vec<_> = (1..=raw.len() as u64).collect();
            let meta = vec![
                Map::from_iter([("unicode".into(), Value::String("中文".into()))]);
                raw.len()
            ];
            let original = wal_data_batch(&schema, &seqs, "_pk_10", &columns, Some(&meta)).unwrap();
            take_json_encoding_count();
            let normalized = normalize_data_json(&schema, &mut columns).unwrap();
            assert_eq!(take_json_encoding_count(), raw.len());
            let repeated =
                wal_data_batch(&schema, &seqs, "_pk_10", normalized.columns(), Some(&meta))
                    .unwrap();
            assert_eq!(take_json_encoding_count(), raw.len());
            assert_eq!(original, repeated);
            let optimized = normalized.wal_batch(&seqs, "_pk_10", Some(&meta)).unwrap();
            assert_eq!(take_json_encoding_count(), 0);
            assert_eq!(original, optimized);
            let strings = optimized
                .column(2)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            for row in 0..raw.len() {
                let original_strings = original
                    .column(2)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap();
                assert_eq!(strings.is_valid(row), original_strings.is_valid(row));
                assert_eq!(
                    strings.value(row).as_bytes(),
                    original_strings.value(row).as_bytes()
                );
                if strings.is_valid(row) {
                    let ColumnData::Json(values) = columns[0].values() else {
                        panic!("JSON column")
                    };
                    assert_eq!(strings.value(row).as_bytes(), values[row]);
                }
            }
        }
    }

    #[test]
    fn normalized_wal_keeps_generic_rejection_including_masked_json() {
        let schema = CollectionSchema {
            fields: vec![FieldSchema {
                nullable: true,
                ..FieldSchema::new("j", DataType::Json)
            }],
            ..Default::default()
        };
        for raw in [b"{bad".as_slice(), b"{\"a\":1e999}", &[0xff, 0xfe]] {
            let mut columns = vec![ColumnData::Json(vec![raw.to_vec()]).with_validity(&[false])];
            assert!(data_batch(&schema, &[1], &columns, None).is_err());
            assert!(wal_data_batch(&schema, &[1], "_default", &columns, None).is_err());
            assert!(normalize_data_json(&schema, &mut columns).is_err());
        }
    }

    #[test]
    fn json_overflow_rejects_nested_floats_but_keeps_wide_integers_and_literal_strings() {
        for raw in [r#"{"a":1e999}"#, r#"[0,{"a":-1e999}]"#, "1E999"] {
            assert!(matches!(
                json_strings("j", &[raw.as_bytes().to_vec()]),
                Err(ArrowDataError::NonFiniteJsonNumber { .. })
            ));
        }
        let cases = [
            (r#"{"a":1e5}"#, r#"{"a": 100000.0}"#),
            (r#"{"a":1e-999}"#, r#"{"a": 0.0}"#),
            (
                r#"{"a":9223372036854775808}"#,
                r#"{"a": 9223372036854775808}"#,
            ),
            (
                r#"{"a":"Infinity","b":"三"}"#,
                r#"{"a": "Infinity", "b": "三"}"#,
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                json_strings("j", &[raw.as_bytes().to_vec()]).unwrap(),
                [expected]
            );
        }
        let huge_integer = "9".repeat(400);
        assert_eq!(
            json_strings("j", &[huge_integer.as_bytes().to_vec()]).unwrap(),
            [huge_integer]
        );
    }

    #[test]
    fn m1_dense_schema_matches_3x_wal_columns() {
        let mut pk = FieldSchema::new("id", DataType::Int64);
        pk.is_primary = true;
        let mut vector = FieldSchema::new("vector", DataType::FloatVector);
        vector.dim = Some(2);
        let schema = CollectionSchema {
            fields: vec![pk, vector],
            enable_dynamic_field: false,
            properties: Vec::new(),
        };
        let batch = wal_data_batch(
            &schema,
            &[1, 2],
            "_default",
            &[
                ColumnData::Int64(vec![10, 20]),
                ColumnData::FloatVector {
                    dim: 2,
                    values: vec![1.0, 2.0, 3.0, 4.0],
                },
            ],
            None,
        )
        .unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.schema().fields().len(), 4);
        assert_eq!(
            batch.schema().field(3).data_type(),
            &ArrowType::FixedSizeList(Arc::new(Field::new("item", ArrowType::Float32, true)), 2)
        );
    }

    #[test]
    fn json_field_preserves_key_order_and_meta_sorts_recursively() {
        let mut pk = FieldSchema::new("id", DataType::Int64);
        pk.is_primary = true;
        let mut vector = FieldSchema::new("vector", DataType::FloatVector);
        vector.dim = Some(2);
        let schema = CollectionSchema {
            fields: vec![pk, vector, FieldSchema::new("j", DataType::Json)],
            enable_dynamic_field: true,
            properties: Vec::new(),
        };
        let Value::Object(meta) =
            serde_json::from_str(r#"{"z":{"b":1,"a":2},"a":"中文"}"#).unwrap()
        else {
            panic!("object fixture");
        };
        let batch = data_batch(
            &schema,
            &[1],
            &[
                ColumnData::Int64(vec![1]),
                ColumnData::FloatVector {
                    dim: 2,
                    values: vec![1.0, 2.0],
                },
                ColumnData::Json(vec![
                    r#"{"z":1E2,"a":"中文","wide":184467440737095516160}"#
                        .as_bytes()
                        .to_vec(),
                ]),
            ],
            Some(&[meta]),
        )
        .unwrap();
        let json = batch
            .column(3)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(
            json.value(0),
            r#"{"z": 100.0, "a": "中文", "wide": 184467440737095516160}"#
        );
        let meta = batch
            .column(4)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(
            meta.value(0),
            r#"{"a": "\u4e2d\u6587", "z": {"a": 2, "b": 1}}"#
        );
    }

    fn two_int_schema() -> CollectionSchema {
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

    fn swapped_batch(batch: &RecordBatch, left: usize, right: usize) -> RecordBatch {
        let mut fields = batch
            .schema()
            .fields()
            .iter()
            .map(|field| field.as_ref().clone())
            .collect::<Vec<_>>();
        fields.swap(left, right);
        let mut columns = batch.columns().to_vec();
        columns.swap(left, right);
        RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap()
    }

    #[test]
    fn same_type_data_field_swap_is_rejected() {
        let schema = two_int_schema();
        let columns = [
            ColumnData::Int64(vec![1]),
            ColumnData::Int64(vec![99]),
            ColumnData::FloatVector {
                dim: 2,
                values: vec![1.0, 2.0],
            },
        ];
        let data = data_batch(&schema, &[1], &columns, None).unwrap();
        let swapped = swapped_batch(&data, 1, 2);
        assert!(matches!(
            decode_data_batch(&schema, &swapped),
            Err(ArrowDataError::ColumnName { position: 1, .. })
        ));
        let wal = wal_data_batch(&schema, &[1], "_default", &columns, None).unwrap();
        let swapped = swapped_batch(&wal, 2, 3);
        assert!(matches!(
            decode_wal_data_batch(&schema, &swapped),
            Err(ArrowDataError::ColumnName { position: 2, .. })
        ));
    }

    #[test]
    fn delta_names_are_checked_in_parquet_and_wal() {
        let schema = two_int_schema();
        let delta = delta_batch(&schema, &[Pk::Int64(1)], &[2]).unwrap();
        let wrong_schema = Schema::new(vec![
            Field::new("score", ArrowType::Int64, false),
            Field::new("_seq", ArrowType::UInt64, false),
        ]);
        let renamed =
            RecordBatch::try_new(Arc::new(wrong_schema), delta.columns().to_vec()).unwrap();
        assert!(matches!(
            decode_delta_batch(&schema, &renamed),
            Err(ArrowDataError::ColumnName { position: 0, .. })
        ));

        let mut varchar_pk = FieldSchema::new("id", DataType::VarChar);
        varchar_pk.is_primary = true;
        let mut vector = FieldSchema::new("vector", DataType::FloatVector);
        vector.dim = Some(2);
        let varchar_schema = CollectionSchema {
            fields: vec![varchar_pk, vector],
            enable_dynamic_field: false,
            properties: Vec::new(),
        };
        let wal = wal_delta_batch(
            &varchar_schema,
            &[Pk::VarChar("a".to_owned())],
            &[2],
            "_all",
        )
        .unwrap();
        let swapped = swapped_batch(&wal, 0, 2);
        assert!(matches!(
            decode_wal_delta_batch(&varchar_schema, &swapped),
            Err(ArrowDataError::ColumnName { position: 0, .. })
        ));
    }
}
