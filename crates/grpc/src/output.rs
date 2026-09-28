//! 读取结果 → FieldData 列，Query 与 Search 共用，与 3.x `records_to_fields_data` 一致：
//! 输出投影中的 schema 字段与主键（主键总在，位于它在 schema 中的位置），开启动态字段时最后是 `$meta` 列。
//! 没有结果时各列仍然设置对应的槽位。

use milvus_lite_core::{
    CollectionSchema, ColumnData, DataType, META_FIELD, Pk, Projection, Rows, pyjson,
};
use serde_json::{Map, Value};

use crate::pb::schema::{self as pb, field_data, scalar_field, vector_field};
use crate::schema::milvus_type;

pub(crate) fn fields_data(
    schema: &CollectionSchema,
    projection: &Projection,
    rows: &[&Rows],
) -> Vec<pb::FieldData> {
    let mut out = Vec::new();
    for (i, field) in schema.fields.iter().enumerate() {
        let column = if field.is_primary {
            Column::Pk(rows.iter().flat_map(|r| r.pks.iter()).collect())
        } else if let Some(pos) = projection.schema_fields.iter().position(|&f| f == i) {
            Column::Data(rows.iter().map(|r| &r.fields[pos].1).collect())
        } else {
            continue;
        };
        out.push(field_data(&field.name, field.data_type, field.dim, &column));
    }
    if schema.enable_dynamic_field {
        let json = rows
            .iter()
            .flat_map(|r| r.meta.iter().flatten())
            .map(|m| meta_json(projection, m).into_bytes())
            .collect();
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
    out
}

/// 3.x 以 `json.dumps` 输出每行的动态字段：显式请求的 key 按请求顺序，否则按 key 排序
/// （3.x 保存 `$meta` 时 `sort_keys=True`）。
fn meta_json(projection: &Projection, meta: &Map<String, Value>) -> String {
    if projection.all_dynamic {
        pyjson::dumps(&Value::Object(meta.clone()))
    } else {
        pyjson::dumps_object(
            projection
                .dynamic_keys
                .iter()
                .filter_map(|k| meta.get(k).map(|v| (k.as_str(), v))),
        )
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
    column: &Column<'_>,
) -> pb::FieldData {
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
                    .flat_map(|c| match c {
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
            } else {
                field_data::Field::Scalars(pb::ScalarField {
                    data: Some(scalar(data_type, parts)),
                })
            }
        }
    };
    pb::FieldData {
        r#type: milvus_type(data_type),
        field_name: name.to_owned(),
        field: Some(field),
        ..Default::default()
    }
}

fn scalar(data_type: DataType, parts: &[&ColumnData]) -> scalar_field::Data {
    use scalar_field::Data;
    macro_rules! collect {
        ($variant:ident, $conv:expr) => {
            parts
                .iter()
                .flat_map(|c| match c {
                    ColumnData::$variant(v) => v.iter().map($conv).collect::<Vec<_>>(),
                    _ => unreachable!("{MISMATCH}"),
                })
                .collect()
        };
    }
    match data_type {
        DataType::Bool => Data::BoolData(pb::BoolArray {
            data: collect!(Bool, |v| *v),
        }),
        DataType::Int8 => Data::IntData(pb::IntArray {
            data: collect!(Int8, |v| i32::from(*v)),
        }),
        DataType::Int16 => Data::IntData(pb::IntArray {
            data: collect!(Int16, |v| i32::from(*v)),
        }),
        DataType::Int32 => Data::IntData(pb::IntArray {
            data: collect!(Int32, |v| *v),
        }),
        DataType::Int64 => Data::LongData(pb::LongArray {
            data: collect!(Int64, |v| *v),
        }),
        DataType::Float => Data::FloatData(pb::FloatArray {
            data: collect!(Float, |v| *v),
        }),
        DataType::Double => Data::DoubleData(pb::DoubleArray {
            data: collect!(Double, |v| *v),
        }),
        DataType::VarChar => Data::StringData(pb::StringArray {
            data: collect!(VarChar, Clone::clone),
        }),
        DataType::Json => Data::JsonData(pb::JsonArray {
            data: collect!(Json, Clone::clone),
        }),
        // 向量列在 field_data 中单独处理。
        DataType::FloatVector => Data::FloatData(pb::FloatArray { data: Vec::new() }),
    }
}
