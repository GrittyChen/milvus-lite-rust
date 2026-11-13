//! 不经服务的过滤求值对照入口：在内存集合上执行 Query，输出命中的主键或错误。
//!
//! 每行输入是一个 JSON 对象：`{"table": {...}}` 用给定的列与 `$meta` 新建集合，之后的
//! `{"expression": "..."}` 都在这张表上求值。浮点列的值写成 Python `repr` 文本（含 `nan`、`inf`），
//! 其余列按 JSON 取值，`null` 表示空值；`$meta` 每行是一段 JSON 文本。
use milvus_lite_core::data::Validity;
use milvus_lite_core::{
    Collection, CollectionSchema, DataType, EngineError, FieldSchema, InputValues, InsertBatch,
    META_FIELD, Pk,
};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

type Error = Box<dyn std::error::Error>;

fn data_type(name: &str) -> Result<DataType, Error> {
    Ok(match name {
        "INT8" => DataType::Int8,
        "INT16" => DataType::Int16,
        "INT32" => DataType::Int32,
        "INT64" => DataType::Int64,
        "FLOAT" => DataType::Float,
        "DOUBLE" => DataType::Double,
        "BOOL" => DataType::Bool,
        "VARCHAR" => DataType::VarChar,
        "JSON" => DataType::Json,
        "ARRAY" => DataType::Array,
        other => return Err(format!("unsupported probe type {other}").into()),
    })
}

fn nullable(values: InputValues, valid: Vec<bool>) -> InputValues {
    if valid.contains(&false) {
        InputValues::Nullable {
            values: Box::new(values),
            validity: Validity::from_bools(&valid),
        }
    } else {
        values
    }
}

fn column(field: &FieldSchema, values: &[Value]) -> Result<InputValues, Error> {
    let valid: Vec<bool> = values.iter().map(|v| !v.is_null()).collect();
    let float = |v: &Value| -> Result<f64, Error> {
        match v {
            Value::Null => Ok(0.0),
            Value::String(text) => Ok(text.parse()?),
            _ => Err("float columns carry repr text".into()),
        }
    };
    let data = match field.data_type {
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => InputValues::Int(
            values
                .iter()
                .map(|v| {
                    if v.is_null() {
                        Ok(0)
                    } else {
                        v.as_i64().ok_or("int")
                    }
                })
                .collect::<Result<_, _>>()?,
        ),
        DataType::Float | DataType::Double => {
            InputValues::Float(values.iter().map(float).collect::<Result<_, _>>()?)
        }
        DataType::Bool => {
            InputValues::Bool(values.iter().map(|v| v.as_bool() == Some(true)).collect())
        }
        DataType::VarChar => InputValues::Str(
            values
                .iter()
                .map(|v| v.as_str().unwrap_or_default().to_owned())
                .collect(),
        ),
        DataType::Json => InputValues::Json(
            values
                .iter()
                .map(|v| v.as_str().unwrap_or("null").as_bytes().to_vec())
                .collect(),
        ),
        DataType::Array => {
            return Ok(InputValues::Array(
                values
                    .iter()
                    .map(|v| {
                        v.as_array()
                            .map(|items| InputValues::JsonValues(items.clone()))
                    })
                    .collect(),
            ));
        }
        _ => return Err("unsupported probe column".into()),
    };
    Ok(nullable(data, valid))
}

fn table(spec: &Value) -> Result<Collection, Error> {
    let mut id = FieldSchema::new("id", DataType::Int64);
    id.is_primary = true;
    let mut vector = FieldSchema::new("vector", DataType::FloatVector);
    vector.dim = Some(2);
    let rows = spec["rows"].as_u64().ok_or("rows")? as usize;
    let mut fields = vec![id, vector];
    let mut columns = vec![
        (
            "id".to_owned(),
            InputValues::Int((0..rows as i64).collect()),
        ),
        (
            "vector".to_owned(),
            InputValues::FloatVector {
                dim: 2,
                values: vec![1.0; rows * 2],
            },
        ),
    ];
    for field in spec["fields"].as_array().ok_or("fields")? {
        let name = field[0].as_str().ok_or("name")?;
        let mut f = FieldSchema::new(name, data_type(field[1].as_str().ok_or("type")?)?);
        f.nullable = field[2].as_bool().ok_or("nullable")?;
        if let Some(element) = field[3].as_str() {
            f.element_type = Some(data_type(element)?);
            f.max_capacity = Some(64);
        }
        if f.data_type == DataType::VarChar {
            f.max_length = Some(65535);
        }
        let values = spec["columns"][name].as_array().ok_or("column")?;
        columns.push((name.to_owned(), column(&f, values)?));
        fields.push(f);
    }
    let dynamic = spec["meta"].is_array();
    if dynamic {
        let meta = spec["meta"].as_array().ok_or("meta")?;
        columns.push((
            META_FIELD.to_owned(),
            InputValues::Json(
                meta.iter()
                    .map(|row| {
                        row.as_str()
                            .map(|s| s.as_bytes().to_vec())
                            .ok_or("meta row")
                    })
                    .collect::<Result<_, _>>()?,
            ),
        ));
    }
    let schema = CollectionSchema {
        functions: Vec::new(),
        fields,
        enable_dynamic_field: dynamic,
        properties: vec![],
    };
    let mut collection = Collection::new("probe", schema)?;
    collection.insert(InsertBatch {
        num_rows: rows,
        columns,
    })?;
    collection.load()?;
    Ok(collection)
}

fn evaluate(collection: &Collection, expression: &str) -> Value {
    match collection.read_snapshot().query(expression, &[], None, 0) {
        Ok(result) => {
            let mut ids: Vec<i64> = result
                .rows
                .pks
                .iter()
                .map(|pk| match pk {
                    Pk::Int64(v) => *v,
                    Pk::VarChar(_) => -1,
                })
                .collect();
            ids.sort_unstable();
            json!({ "ids": ids })
        }
        Err(EngineError::FilterResourceExhausted(_)) => json!({ "error": null, "resource": true }),
        Err(error) => json!({ "error": error.to_string() }),
    }
}

fn main() -> Result<(), Error> {
    let input = io::stdin().lock();
    let mut output = io::BufWriter::new(io::stdout().lock());
    let mut current = None;
    for line in input.lines() {
        let request = milvus_lite_core::json_text::value_str(&line?)?;
        if let Some(spec) = request.get("table") {
            current = Some(table(spec)?);
            continue;
        }
        let collection = current.as_ref().ok_or("expression before table")?;
        let expression = request["expression"].as_str().ok_or("expression")?;
        serde_json::to_writer(&mut output, &evaluate(collection, expression))?;
        writeln!(output)?;
    }
    output.flush()?;
    Ok(())
}
