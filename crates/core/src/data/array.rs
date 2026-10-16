//! ARRAY 的偏移与有类型子列；元素转换遵循固定参照的 Arrow 15/25 口径。

use std::borrow::Cow;

use super::{ColumnData, InputValues};
use crate::collection::{py_type_name, py_value_repr};
use crate::error::{EngineError, Result};
use crate::schema::{DataType, FieldSchema};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct ArrayData {
    pub element_type: DataType,
    /// Arrow 的子字段名用于保留参照在 WAL / Parquet 上的诊断差异。
    pub element_name: String,
    pub offsets: Vec<usize>,
    pub values: Box<ColumnData>,
}

impl ArrayData {
    pub(crate) fn empty_rows(field: &FieldSchema, rows: usize) -> Self {
        let Some(element_type) = field.element_type else {
            unreachable!("validated ARRAY has element_type");
        };
        Self {
            element_type,
            element_name: "item".into(),
            offsets: vec![0; rows + 1],
            values: Box::new(ColumnData::empty(&FieldSchema::new("item", element_type))),
        }
    }
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn range(&self, row: usize) -> std::ops::Range<usize> {
        self.offsets[row]..self.offsets[row + 1]
    }
    pub(crate) fn empty_like(&self) -> Self {
        let mut field = FieldSchema::new("item", DataType::Array);
        field.element_type = Some(self.element_type);
        let mut empty = Self::empty_rows(&field, 0);
        empty.element_name.clone_from(&self.element_name);
        empty
    }
    pub(crate) fn push_row(&mut self, source: &Self, row: usize) {
        if self.is_empty() {
            self.element_name.clone_from(&source.element_name);
        }
        for item in source.range(row) {
            self.values.push_row(&source.values, item);
        }
        self.offsets.push(self.values.len());
    }
    pub(crate) fn valid(&self, element_type: DataType) -> bool {
        self.element_type == element_type
            && self.offsets.first() == Some(&0)
            && self.offsets.last() == Some(&self.values.len())
            && self.offsets.windows(2).all(|w| w[0] <= w[1])
            && self.values.len() <= i32::MAX as usize
            && self
                .values
                .validity()
                .is_none_or(|v| v.len() == self.values.len())
            && matches!(
                (element_type, self.values.values()),
                (DataType::Bool, ColumnData::Bool(_))
                    | (DataType::Int8, ColumnData::Int8(_))
                    | (DataType::Int16, ColumnData::Int16(_))
                    | (DataType::Int32, ColumnData::Int32(_))
                    | (DataType::Int64, ColumnData::Int64(_))
                    | (DataType::Float, ColumnData::Float(_))
                    | (DataType::Double, ColumnData::Double(_))
                    | (DataType::VarChar, ColumnData::VarChar(_))
            )
    }
    pub(crate) fn input(&self, row: usize) -> InputValues {
        // 保留子列的实际数值种类和有效位，不经 JSON 文本中转（尤其是 NaN/Inf）。
        let mut values = ColumnData::empty(&FieldSchema::new("item", self.element_type));
        for i in self.range(row) {
            values.push_row(&self.values, i);
        }
        super::super::collection::input_from_column_all(&values)
    }
}

pub(crate) fn row_len(values: &InputValues, row: usize) -> Option<usize> {
    match values.values() {
        InputValues::Array(rows) => rows[row].as_ref().map(InputValues::len),
        InputValues::JsonValues(rows) => rows[row].as_array().map(Vec::len),
        InputValues::Json(rows) => serde_json::from_slice::<Value>(&rows[row])
            .ok()?
            .as_array()
            .map(Vec::len),
        InputValues::FloatVector { dim, .. } => Some(*dim),
        _ => None,
    }
}

pub(crate) fn row_input(values: &InputValues, row: usize) -> Result<Option<Cow<'_, InputValues>>> {
    if values.is_null(row) {
        return Ok(None);
    }
    let input = match values.values() {
        InputValues::Array(rows) => return Ok(rows[row].as_ref().map(Cow::Borrowed)),
        InputValues::JsonValues(rows) => {
            let items = rows[row]
                .as_array()
                .ok_or_else(|| EngineError::schema("validated ARRAY lost list value"))?;
            InputValues::JsonValues(items.clone())
        }
        InputValues::Json(rows) => {
            let value: Value = serde_json::from_slice(&rows[row])
                .map_err(|e| EngineError::schema(e.to_string()))?;
            let Value::Array(items) = value else {
                return Err(EngineError::schema("validated ARRAY lost list value"));
            };
            InputValues::JsonValues(items)
        }
        InputValues::FloatVector { dim, values } => InputValues::Float(
            values[row * dim..(row + 1) * dim]
                .iter()
                .map(|v| f64::from(*v))
                .collect(),
        ),
        _ => return Err(EngineError::schema("validated ARRAY lost list input")),
    };
    Ok(Some(Cow::Owned(input)))
}

pub(crate) fn convert(field: &FieldSchema, input: &InputValues) -> Result<ColumnData> {
    let mut array = ArrayData::empty_rows(field, 0);
    let mut valid = Vec::with_capacity(input.len());
    let child = FieldSchema {
        nullable: true,
        ..FieldSchema::new("item", array.element_type)
    };
    for row in 0..input.len() {
        if let Some(items) = row_input(input, row)? {
            if array
                .values
                .len()
                .checked_add(items.len())
                .is_none_or(|n| n > i32::MAX as usize)
            {
                return Err(EngineError::invalid(
                    "ARRAY child length exceeds Arrow list offset range",
                ));
            }
            for i in 0..items.len() {
                array.values.push_row(&element(&child, &items, i)?, 0);
            }
            valid.push(true);
        } else {
            valid.push(false);
        }
        array.offsets.push(array.values.len());
    }
    Ok(ColumnData::Array(array).with_validity(&valid))
}

fn element(field: &FieldSchema, values: &InputValues, row: usize) -> Result<ColumnData> {
    if values.is_null(row)
        || matches!(values.values(), InputValues::JsonValues(v) if v[row].is_null())
    {
        return Ok(ColumnData::nulls(field, 1));
    }
    let repr = || py_value_repr(values, row);
    let kind = py_type_name(values, row);
    let integer = match values.values() {
        InputValues::Int(v) => Some(Ok(v[row])),
        InputValues::JsonValues(v) if kind == "int" => Some(v[row].as_i64().ok_or(())),
        _ => None,
    };
    let float = match values.values() {
        InputValues::Float(v) => Some(v[row]),
        InputValues::JsonValues(v) if kind == "float" => v[row].as_f64(),
        _ => None,
    };
    let boolean = match values.values() {
        InputValues::Bool(v) => Some(v[row]),
        InputValues::JsonValues(v) => v[row].as_bool(),
        _ => None,
    };
    let text = match values.values() {
        InputValues::Str(v) => Some(v[row].as_str()),
        InputValues::JsonValues(v) => v[row].as_str(),
        _ => None,
    };
    let target = match field.data_type {
        DataType::Bool => "boolean",
        DataType::Float => "float32",
        DataType::Double => "double",
        _ => "",
    };
    let mismatch = || {
        EngineError::invalid(format!(
            "Could not convert {} with type {kind}: tried to convert to {}",
            repr(),
            if target.is_empty() {
                field.data_type.name().to_ascii_lowercase()
            } else {
                target.into()
            }
        ))
    };
    Ok(match field.data_type {
        DataType::Bool => ColumnData::Bool(vec![boolean.ok_or_else(mismatch)?]),
        DataType::VarChar => ColumnData::VarChar(vec![
            text.ok_or_else(|| {
                EngineError::invalid(format!("Expected bytes, got a '{kind}' object"))
            })?
            .to_owned(),
        ]),
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => {
            if boolean.is_some() {
                return Err(EngineError::invalid("Expected integer, got bool"));
            }
            let bits = match field.data_type {
                DataType::Int8 => 8,
                DataType::Int16 => 16,
                DataType::Int32 => 32,
                _ => 64,
            };
            let value = if let Some(integer) = integer {
                let value = integer.map_err(|()| {
                    EngineError::invalid("Python int too large to convert to C long")
                })?;
                if bits != 64 && !(-(1_i64 << (bits - 1))..(1_i64 << (bits - 1))).contains(&value) {
                    return Err(EngineError::invalid(format!(
                        "Value {value} too large to fit in C integer type"
                    )));
                }
                value
            } else if let Some(float) = float {
                let value = float.trunc();
                let limit = 2_f64.powi(bits - 1);
                if !value.is_finite() || !(-limit..limit).contains(&value) {
                    return Err(mismatch());
                }
                value as i64
            } else {
                return Err(mismatch());
            };
            match field.data_type {
                DataType::Int8 => ColumnData::Int8(vec![value as i8]),
                DataType::Int16 => ColumnData::Int16(vec![value as i16]),
                DataType::Int32 => ColumnData::Int32(vec![value as i32]),
                _ => ColumnData::Int64(vec![value]),
            }
        }
        DataType::Float | DataType::Double => {
            let value = if let Some(integer) = integer {
                let value = integer
                    .map_err(|()| EngineError::invalid("PyLong is too large to fit int64"))?;
                crate::collection::exact_int(
                    value,
                    if field.data_type == DataType::Float {
                        1 << 24
                    } else {
                        1 << 53
                    },
                    if field.data_type == DataType::Float {
                        "single"
                    } else {
                        "double"
                    },
                )? as f64
            } else if let Some(value) = float {
                value
            } else if let Some(value) = boolean {
                f64::from(u8::from(value))
            } else {
                return Err(mismatch());
            };
            if field.data_type == DataType::Float {
                ColumnData::Float(vec![value as f32])
            } else {
                ColumnData::Double(vec![value])
            }
        }
        _ => {
            return Err(EngineError::schema(
                "unsupported ARRAY element type reached conversion",
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn element_conversion_matches_arrow_15_and_25() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/array_v321.json")).unwrap();
        let mut failures = Vec::new();
        for case in fixture["conversions"].as_array().unwrap() {
            let dtype = match case["type"].as_str().unwrap() {
                "bool" => DataType::Bool,
                "int8" => DataType::Int8,
                "int16" => DataType::Int16,
                "int32" => DataType::Int32,
                "int64" => DataType::Int64,
                "float" => DataType::Float,
                "double" => DataType::Double,
                "varchar" => DataType::VarChar,
                _ => panic!("type"),
            };
            let f = FieldSchema {
                element_type: Some(dtype),
                ..FieldSchema::new("x", DataType::Array)
            };
            let result = convert(
                &f,
                &InputValues::Array(vec![Some(InputValues::JsonValues(vec![
                    case["value"].clone(),
                ]))]),
            );
            if let Some(expected) = case.get("error") {
                match result {
                    Err(e) if e.to_string() == expected.as_str().unwrap() => {}
                    actual => failures.push(format!("{case}: {actual:?}")),
                }
            } else {
                let column = match result {
                    Ok(c) => c,
                    Err(e) => {
                        failures.push(format!("{case}: {e}"));
                        continue;
                    }
                };
                let ColumnData::Array(a) = column else {
                    panic!("array");
                };
                let actual = if a.values.is_null(0) {
                    Value::Null
                } else {
                    match a.values.values() {
                        ColumnData::Bool(v) => Value::from(v[0]),
                        ColumnData::Int8(v) => Value::from(v[0]),
                        ColumnData::Int16(v) => Value::from(v[0]),
                        ColumnData::Int32(v) => Value::from(v[0]),
                        ColumnData::Int64(v) => Value::from(v[0]),
                        ColumnData::Float(v) => Value::from(f64::from(v[0])),
                        ColumnData::Double(v) => Value::from(v[0]),
                        ColumnData::VarChar(v) => Value::from(v[0].clone()),
                        _ => panic!("child"),
                    }
                };
                let expected = &case["result"][0][0];
                if !(actual == *expected
                    || (actual.is_number()
                        && expected.is_number()
                        && actual.as_f64() == expected.as_f64()))
                {
                    failures.push(format!("{case}: {actual}"));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
