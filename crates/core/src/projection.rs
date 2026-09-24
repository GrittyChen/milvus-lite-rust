//! 输出字段的展开，规则与 3.x `engine/projection.py` 的 `build_projection_plan` 一致。

use std::collections::HashSet;

use crate::data::META_FIELD;
use crate::schema::CollectionSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKind {
    Query,
    Search,
}

/// 展开后的投影。gRPC 层据此编码 fields_data 与响应的 output_fields 列表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    /// 请求是否为空（3.x 的 `output_fields is None`）。
    pub defaulted: bool,
    /// 输出的 schema 字段下标，按 schema 顺序。
    pub schema_fields: Vec<usize>,
    /// 显式请求的动态字段 key，按请求顺序去重。
    pub dynamic_keys: Vec<String>,
    /// 是否输出完整的动态字段对象（`*`、`$meta`，或请求为空时）。
    pub all_dynamic: bool,
}

impl Projection {
    /// `output_fields` 为空等同于未指定。不存在的字段不报错：开启动态字段时视为动态 key，否则丢弃。
    pub fn new(schema: &CollectionSchema, output_fields: &[String], kind: ApiKind) -> Self {
        let dynamic = schema.enable_dynamic_field;
        if output_fields.is_empty() {
            let schema_fields = schema
                .fields
                .iter()
                .enumerate()
                .filter(|(_, f)| kind == ApiKind::Query || !f.is_primary)
                .map(|(i, _)| i)
                .collect();
            return Self {
                defaulted: true,
                schema_fields,
                dynamic_keys: Vec::new(),
                all_dynamic: dynamic,
            };
        }

        let mut seen = HashSet::new();
        let requested: Vec<&str> = output_fields
            .iter()
            .map(String::as_str)
            .filter(|f| seen.insert(*f))
            .collect();
        let star = requested.contains(&"*");
        let meta = requested.contains(&META_FIELD);
        let schema_fields = schema
            .fields
            .iter()
            .enumerate()
            .filter(|(_, f)| star || requested.contains(&f.name.as_str()))
            .map(|(i, _)| i)
            .collect();
        let dynamic_keys = if dynamic {
            requested
                .iter()
                .filter(|f| **f != "*" && **f != META_FIELD && schema.field(f).is_none())
                .map(|f| (*f).to_owned())
                .collect()
        } else {
            Vec::new()
        };
        Self {
            defaulted: false,
            schema_fields,
            dynamic_keys,
            all_dynamic: dynamic && (star || meta),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{DataType, FieldSchema};

    fn schema(dynamic: bool) -> CollectionSchema {
        CollectionSchema {
            fields: vec![
                FieldSchema {
                    is_primary: true,
                    ..FieldSchema::new("id", DataType::Int64)
                },
                FieldSchema {
                    dim: Some(2),
                    ..FieldSchema::new("v", DataType::FloatVector)
                },
                FieldSchema::new("t", DataType::VarChar),
            ],
            enable_dynamic_field: dynamic,
        }
    }

    fn p(dynamic: bool, fields: &[&str], kind: ApiKind) -> Projection {
        let fields: Vec<String> = fields.iter().map(|s| (*s).to_owned()).collect();
        Projection::new(&schema(dynamic), &fields, kind)
    }

    #[test]
    fn defaults_differ_between_query_and_search() {
        let q = p(true, &[], ApiKind::Query);
        assert_eq!(q.schema_fields, [0, 1, 2]);
        assert!(q.all_dynamic && q.defaulted);
        let s = p(true, &[], ApiKind::Search);
        assert_eq!(s.schema_fields, [1, 2]);
        assert!(s.all_dynamic);
        assert!(!p(false, &[], ApiKind::Search).all_dynamic);
    }

    #[test]
    fn explicit_fields_follow_schema_order_and_dynamic_keys_follow_request_order() {
        let x = p(true, &["zz", "t", "id", "a", "zz"], ApiKind::Search);
        assert_eq!(x.schema_fields, [0, 2]);
        assert_eq!(x.dynamic_keys, ["zz", "a"]);
        assert!(!x.all_dynamic);
    }

    #[test]
    fn star_and_meta() {
        let star = p(true, &["*"], ApiKind::Query);
        assert_eq!(star.schema_fields, [0, 1, 2]);
        assert!(star.all_dynamic && star.dynamic_keys.is_empty());
        let meta = p(true, &["$meta"], ApiKind::Query);
        assert!(meta.schema_fields.is_empty() && meta.all_dynamic);
    }

    #[test]
    fn unknown_fields_are_dropped_without_dynamic() {
        let x = p(false, &["zz", "$meta"], ApiKind::Query);
        assert!(x.schema_fields.is_empty() && x.dynamic_keys.is_empty() && !x.all_dynamic);
    }
}
