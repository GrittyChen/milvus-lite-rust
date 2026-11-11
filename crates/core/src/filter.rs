//! 3.x 完整过滤语法与三路线求值；范围与拒绝边界见 0006、0009 和 0014。

mod columns;
mod dynamic;
mod eval;
mod parser;
mod python;
mod python_json;
pub(crate) mod temporal;
mod tree;
#[cfg(test)]
mod tree_tests;
pub(crate) mod unicode;
pub(crate) use dynamic::DynamicRow;
pub(crate) use eval::Predicate;
pub(crate) use python::{int_cmp, int_float_cmp};
pub use python_json::{AnalyzerParams, classify_analyzer_params};

/// 错误字符串与词法必须使用同一版 Unicode 属性，避免未分配码位的 repr 漂移。
pub(crate) fn printable_char(c: char) -> bool {
    unicode::printable(c)
}

/// 默认值的协议描述复用过滤器固定的 CPython 十进制数字表，避免两套 Unicode 分类。
pub fn python_decimal_digit(c: char) -> Option<u32> {
    unicode::decimal(c)
}

use crate::error::Result;

/// 与 schema 无关的解析结果；适配层可在取状态锁之前解析，再保留原来的校验错误优先级。
pub struct ParsedFilter {
    source: String,
    ast: Option<parser::Ast>,
}
impl ParsedFilter {
    pub(crate) fn is_empty(&self) -> bool {
        self.ast.is_none()
    }
    /// 没有任何时区配置的解析：无偏移的 `ISO` 字面量报 3.x 的「需要时区」错误。
    pub fn parse(source: &str) -> Result<Self> {
        Self::parse_with(source, &crate::timezone::TimezoneSource::none())
    }
    /// 表达式非空时，先校验请求时区（3.x 的过滤编译处），再解析；无偏移的 `ISO` 字面量按有效时区换算，
    /// 数据库时区只在遇到这样的字面量时才读取。
    pub fn parse_with(
        source: &str,
        timezone: &crate::timezone::TimezoneSource<'_>,
    ) -> Result<Self> {
        let ast = if source.is_empty() {
            None
        } else {
            timezone.validate_request()?;
            Some(parser::parse(source, &|local| {
                timezone.for_naive()?.local_to_utc_micros(local)
            })?)
        };
        Ok(Self {
            source: source.into(),
            ast,
        })
    }
    /// 非主键快路径仍须绑定 schema 并做语义检查；不得把无法识别当成无过滤。
    pub fn primary_keys(&self, pk: &str) -> Option<Vec<PkLiteral>> {
        use parser::{Kind, Literal};
        let ast = self.ast.as_ref()?;
        let Kind::In {
            field,
            list,
            negate: false,
        } = ast.nodes[ast.root].kind
        else {
            return None;
        };
        if !matches!(&ast.nodes[field].kind,Kind::Field(name) if name==pk) {
            return None;
        }
        let Kind::List(elements) = &ast.nodes[list].kind else {
            return None;
        };
        let mut out = Vec::with_capacity(elements.len());
        for &id in elements {
            match &ast.nodes[id].kind {
                Kind::Lit(Literal::Int(s)) => {
                    out.push(s.parse().map_or(PkLiteral::IntOutOfRange, PkLiteral::Int))
                }
                Kind::Lit(Literal::String(s)) => out.push(PkLiteral::Str(s.clone())),
                _ => return None,
            }
        }
        Some(out)
    }
}

/// boost 过滤（0026）：3.x 用 `compile_filter` + `_eval_row` 逐行求值，不走普通过滤按整条表达式选路线的规则，
/// 所以一律构造布尔树、一律走 Python 路线。行里只有 schema 字段，动态字段不可见（`meta = None`）。
///
/// 3.x 求值前令 `row[pk] = row.get(pk, row.get("$id"))`，同名字段盖掉 `$id` 后，过滤看到的主键是那个值，
/// 类型也不再是主键列的类型。这里把主键操作数改绑到值的真实来源，不经任何中间表示：NaN、`datetime`
/// 这些值换一种表示就会改变 `is null`、比较与 `in` 的结果。语义检查发生在改绑之前，仍按主键的 schema 类型。
pub(crate) struct RowFilter {
    tree: tree::Tree,
    /// 动态键 `$id` 盖掉主键的行用这棵树：主键读这个键，其余动态字段读不到。
    /// 只有表达式引用了主键且 `$id` 键在投影内时才有。
    dynamic_id: Option<tree::Tree>,
}

impl RowFilter {
    /// 表达式为空时返回 None（3.x 的 `if filt:`）。范围外构造优先 UNIMPLEMENTED，与普通过滤相同。
    /// `id_key_visible`：动态键 `$id` 是否在这次检索的投影内。
    pub(crate) fn compile(
        parsed: &ParsedFilter,
        schema: &crate::schema::CollectionSchema,
        id_key_visible: bool,
    ) -> Result<Option<Self>> {
        use crate::rerank::boost::ID_FIELD;
        use crate::schema::DataType;
        use eval::Operand;
        let Some(ast) = &parsed.ast else {
            return Ok(None);
        };
        if eval::general_arithmetic(ast, schema) {
            return Err(eval::unsupported());
        }
        let pk = schema.fields.iter().position(|f| f.is_primary);
        let is_pk = |op: &Operand| matches!(op, Operand::Column(i) if Some(*i) == pk);
        let mut tree = tree::Tree::compile(parsed, schema)?;
        // 同名的 schema 字段每行都在，直接改绑到这一列。
        let id_field = schema
            .fields
            .iter()
            .position(|f| f.name == ID_FIELD && !f.is_primary);
        if let Some(pos) = id_field {
            let bound = if schema.fields[pos].data_type == DataType::Timestamptz {
                Operand::Timestamp(pos)
            } else {
                Operand::Column(pos)
            };
            for op in tree.operands.iter_mut().flatten() {
                if is_pk(op) {
                    *op = bound.clone();
                }
            }
            return Ok(Some(Self {
                tree,
                dynamic_id: None,
            }));
        }
        let dynamic_id = if id_key_visible && tree.operands.iter().flatten().any(is_pk) {
            let mut rebound = tree::Tree::compile(parsed, schema)?;
            for op in rebound.operands.iter_mut().flatten() {
                if matches!(op, Operand::Dynamic(_)) {
                    *op = Operand::Absent;
                } else if is_pk(op) {
                    *op = Operand::Dynamic(ID_FIELD.to_owned());
                }
            }
            Some(rebound)
        } else {
            None
        };
        Ok(Some(Self { tree, dynamic_id }))
    }

    /// `meta` 是这一行完整的动态字段，只用来读 `$id`；求值结果为空值时不命中（3.x `_row_matches_filter`）。
    pub(crate) fn matches(
        &self,
        columns: &[crate::data::ColumnData],
        row: usize,
        meta: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<bool> {
        let (tree, meta) = match (&self.dynamic_id, meta) {
            (Some(tree), Some(meta)) if meta.contains_key(crate::rerank::boost::ID_FIELD) => {
                (tree, Some(meta))
            }
            _ => (&self.tree, None),
        };
        let bits = python::evaluate_tree(
            tree,
            &[DynamicRow {
                columns,
                meta,
                row,
                sequence: None,
            }],
        )?;
        Ok(bits.get(0))
    }
}

/// 3.x 稀疏检索在 MemTable 上对每条记录调用 `_eval_row`（记录含 `$meta`，动态字段可见），不按整条表达式
/// 选择路线（0030）；与 boost 的 [`RowFilter`] 不同，主键不改绑、动态字段照常可见。
pub(crate) struct RecordFilter {
    tree: tree::Tree,
}

impl RecordFilter {
    /// 表达式为空时返回 None。调用方已用 [`Predicate::from_parsed`] 报出编译与范围外错误。
    pub(crate) fn compile(
        parsed: &ParsedFilter,
        schema: &crate::schema::CollectionSchema,
    ) -> Result<Option<Self>> {
        if parsed.ast.is_none() {
            return Ok(None);
        }
        Ok(Some(Self {
            tree: tree::Tree::compile(parsed, schema)?,
        }))
    }

    /// 按给出的先后逐行求值：第一个出错的行决定报出的错误。
    pub(crate) fn evaluate(
        &self,
        rows: &[DynamicRow<'_>],
    ) -> Result<crate::storage::visibility::SourceBits> {
        python::evaluate_tree(&self.tree, rows)
    }
}

/// 列表中的一个字面量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkLiteral {
    Int(i64),
    /// 超出 int64 的整数。3.x 不报错，只是查不到。
    IntOutOfRange,
    Str(String),
}

#[cfg(test)]
const MAX_INT_DIGITS: usize = 4300;

#[cfg(test)]
mod tests {
    use super::*;
    use PkLiteral::{Int, IntOutOfRange as Oor, Str};

    fn p(expr: &str) -> Option<Vec<PkLiteral>> {
        ParsedFilter::parse(expr)
            .ok()
            .and_then(|parsed| parsed.primary_keys("id"))
    }

    fn s(v: &str) -> PkLiteral {
        Str(v.to_owned())
    }

    // 期望值取自 3.x（v3.2.1）的 `_extract_pks_from_expr` 实测。
    #[test]
    fn accepted_forms() {
        let cases: &[(&str, Vec<PkLiteral>)] = &[
            ("id in [1]", vec![Int(1)]),
            (
                "id in [3, 1, 3, 2, 1]",
                vec![Int(3), Int(1), Int(3), Int(2), Int(1)],
            ),
            ("id IN [1]", vec![Int(1)]),
            ("id In [1]", vec![Int(1)]),
            ("id iN [1]", vec![Int(1)]),
            ("id in[1]", vec![Int(1)]),
            ("id\tin\n[1]", vec![Int(1)]),
            ("(id)in[1]", vec![Int(1)]),
            ("id in []", vec![]),
            ("id in [ ]", vec![]),
            ("id in [1,]", vec![Int(1)]),
            ("id in [1 , 2 ,\n 3\t,\r\n]", vec![Int(1), Int(2), Int(3)]),
            ("  id in [1]  ", vec![Int(1)]),
            ("\n\tid in [1]\r\n", vec![Int(1)]),
            ("id in [-5, - 5, -\n5]", vec![Int(-5), Int(-5), Int(-5)]),
            ("id in [-0, 00, 007]", vec![Int(0), Int(0), Int(7)]),
            (
                "id in [9223372036854775807, -9223372036854775808]",
                vec![Int(i64::MAX), Int(i64::MIN)],
            ),
            (
                "id in [9223372036854775808, 1, -9223372036854775809]",
                vec![Oor, Int(1), Oor],
            ),
            ("id in ['a', \"a\"]", vec![s("a"), s("a")]),
            ("id in ['a\"b', \"a\\\"b\"]", vec![s("a\"b"), s("a\"b")]),
            ("id in [\"a'b\", 'a\\'b']", vec![s("a'b"), s("a'b")]),
            ("id in ['a\\\\b']", vec![s("a\\b")]),
            (
                "id in ['a\\nb', 'a\\rb', 'a\\tb']",
                vec![s("a\nb"), s("a\rb"), s("a\tb")],
            ),
            (
                "id in ['a\tb', 'a\u{0}b', '\u{2028}', '中文😀', '']",
                vec![s("a\tb"), s("a\u{0}b"), s("\u{2028}"), s("中文😀"), s("")],
            ),
            ("id in ['a','b',]", vec![s("a"), s("b")]),
            ("id in [1, 'a']", vec![Int(1), s("a")]),
            ("id in [٣, １２, 1٣]", vec![Int(3), Int(12), Int(13)]),
            ("(id in [1])", vec![Int(1)]),
            ("((id in [1]))", vec![Int(1)]),
            ("( id in [1] )", vec![Int(1)]),
            ("(id) in [1]", vec![Int(1)]),
            ("((id)) in [1]", vec![Int(1)]),
            ("((id) in [1])", vec![Int(1)]),
            ("( ( id ) in [ 1 ] )", vec![Int(1)]),
        ];
        for (expr, want) in cases {
            assert_eq!(p(expr).as_ref(), Some(want), "{expr:?}");
        }
        let big = "1".repeat(MAX_INT_DIGITS);
        assert_eq!(p(&format!("id in [{big}]")), Some(vec![Oor]));
    }

    #[test]
    fn rejected_forms() {
        let over = format!("id in [{}]", "1".repeat(MAX_INT_DIGITS + 1));
        let zeros = format!("id in [{}1]", "0".repeat(MAX_INT_DIGITS));
        let cases = [
            "ID in [1]",
            "idx in [1]",
            "`id` in [1]",
            "$meta[\"id\"] in [1]",
            "id[0] in [1]",
            "id not in [1]",
            "id NOT IN [1]",
            "id !in [1]",
            "id ! in [1]",
            "not id in [1]",
            "!(id in [1])",
            "id in (1,2)",
            "id in 1",
            "id == 1",
            "idin [1]",
            "id inn [1]",
            "id in\u{c}[1]",
            "id\u{a0}in [1]",
            "id\u{b}in [1]",
            "id\u{3000}in [1]",
            "id in [,]",
            "id in [1,,]",
            "id in [,1]",
            "id in [[1]]",
            "id in [1 2]",
            "id in [1",
            "id in [1]]",
            "id in [--5]",
            "id in [-(5)]",
            "id in [+5]",
            "id in [0x10]",
            "id in [1_000]",
            "id in [1a]",
            "id in [1+1]",
            &over,
            &zeros,
            "id in [1e3]",
            "id in [1E3]",
            "id in [1.0]",
            "id in [1.]",
            "id in [.5]",
            "id in [1e]",
            "id in [true]",
            "id in [False]",
            "id in [tRuE]",
            "id in [null]",
            "id in [ISO '2025-01-01']",
            "id in [INTERVAL 'P1D']",
            "id in [x]",
            "id in [id]",
            "id in [²]",
            "id in [1²]",
            "id in [①]",
            "id in ['\\x41']",
            "id in ['\\u4e2d']",
            "id in ['\\0']",
            "id in ['\\b']",
            "id in ['\\/']",
            "id in ['\\q']",
            "id in ['a\\\nb']",
            "id in ['a\nb']",
            "id in ['a\rb']",
            "id in ['a",
            "id in ['a]",
            "id in ['a\\']",
            "id in ['a'b']",
            "id in ['a''b']",
            "id in ['a' 'b']",
            "(id in [1]",
            "id in [1])",
            "(id)) in [1]",
            "((id) in [1]))",
            "id in ([1])",
            "()",
            "id in [1] and true",
            "id in [1] or id in [2]",
            "id in [1] && true",
            "id in [1] == true",
            "id in [1] in [2]",
            "id in [1] # c",
            "id in [1];",
            "",
            "   ",
            "id in {ids}",
            "id in [1, 1.5]",
            "id in [1, true]",
            "id",
            "id in",
        ];
        for expr in cases {
            assert_eq!(p(expr), None, "{expr:?}");
        }
    }

    mod row_filter {
        use super::super::*;
        use crate::data::ColumnData;
        use crate::error::EngineError;
        use crate::schema::{CollectionSchema, DataType, FieldSchema};
        use crate::timezone::TimezoneSource;
        use serde_json::json;

        fn schema(dynamic: bool) -> CollectionSchema {
            let mut id = FieldSchema::new("id", DataType::Int64);
            id.is_primary = true;
            let mut v = FieldSchema::new("v", DataType::FloatVector);
            v.dim = Some(1);
            let mut opt = FieldSchema::new("opt", DataType::Int32);
            opt.nullable = true;
            CollectionSchema {
                fields: vec![
                    id,
                    v,
                    FieldSchema::new("age", DataType::Int32),
                    FieldSchema::new("name", DataType::VarChar),
                    FieldSchema::new("j", DataType::Json),
                    opt,
                    FieldSchema::new("ts", DataType::Timestamptz),
                ],
                enable_dynamic_field: dynamic,
                properties: Vec::new(),
            }
        }

        fn columns() -> Vec<ColumnData> {
            vec![
                ColumnData::Int64(vec![1, 2, 3]),
                ColumnData::FloatVector {
                    dim: 1,
                    values: vec![0.0, 0.0, 0.0],
                },
                ColumnData::Int32(vec![1, 5, 9]),
                ColumnData::VarChar(vec!["ab".into(), "ac".into(), "b".into()]),
                ColumnData::Json(vec![
                    br#"{"k":1}"#.to_vec(),
                    br#"{"k":2}"#.to_vec(),
                    br#"{"x":1}"#.to_vec(),
                ]),
                ColumnData::Int32(vec![7, 0, 7]).with_validity(&[true, false, true]),
                // 2025-01-01T00:00:00Z 与它前后各一秒。
                ColumnData::Int64(vec![
                    1_735_689_599_000_000,
                    1_735_689_600_000_000,
                    1_735_689_601_000_000,
                ]),
            ]
        }

        fn filter(expr: &str, dynamic: bool) -> Result<Option<RowFilter>> {
            let parsed = ParsedFilter::parse_with(expr, &TimezoneSource::none())?;
            RowFilter::compile(&parsed, &schema(dynamic), dynamic)
        }

        fn hits(expr: &str, dynamic: bool) -> Vec<bool> {
            let f = filter(expr, dynamic).unwrap().unwrap();
            let columns = columns();
            (0..3)
                .map(|row| f.matches(&columns, row, None).unwrap())
                .collect()
        }

        #[test]
        fn evaluates_every_shape_row_by_row() {
            // 主键 in 与简单比较在普通过滤里走快路径，这里同样必须走 Python 路线。
            for (expr, want) in [
                ("age > 1", [false, true, true]),
                ("id in [1, 3]", [true, false, true]),
                ("id == 2 and age == 5", [false, true, false]),
                ("age == id", [true, false, false]),
                ("name like 'a%'", [true, true, false]),
                ("age not in [5]", [true, false, true]),
                ("j['k'] == 1", [true, false, false]),
                ("exists j['k']", [true, true, false]),
                ("opt is null", [false, true, false]),
                ("opt is not null", [true, false, true]),
                ("opt > 1", [true, false, true]),
                ("ts >= ISO '2025-01-01T00:00:00Z'", [false, true, true]),
            ] {
                assert_eq!(hits(expr, false), want, "{expr}");
                assert_eq!(hits(expr, true), want, "{expr}");
            }
        }

        #[test]
        fn dynamic_fields_are_invisible() {
            for expr in [
                "dyn > 5",
                "$meta['x'] > 1",
                "dyn is not null",
                "nosuch == 1",
            ] {
                assert_eq!(hits(expr, true), [false; 3], "{expr}");
            }
        }

        #[test]
        fn dynamic_disabled_rejects_unknown_fields() {
            let code6 = |expr: &str, want: &str| match filter(expr, false) {
                Err(EngineError::SchemaValidation(message)) => {
                    assert!(message.contains(want), "{expr}: {message}");
                }
                other => panic!("{expr}: {:?}", other.map(|f| f.is_some())),
            };
            code6("nosuch == 1", "unknown field 'nosuch'");
            code6(
                "$meta['x'] > 1",
                "$meta access requires schema enable_dynamic_field=True",
            );
        }

        #[test]
        fn out_of_scope_constructs_stay_unimplemented() {
            for dynamic in [false, true] {
                for expr in ["age + 1 > 5", "geometry_contains(name, 'POINT (0 0)')"] {
                    assert!(
                        matches!(filter(expr, dynamic), Err(EngineError::Unsupported { .. })),
                        "{expr}"
                    );
                }
            }
        }

        #[test]
        fn empty_expression_is_no_filter() {
            assert!(filter("", true).unwrap().is_none());
        }

        fn map(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
            value.as_object().unwrap().clone()
        }

        #[test]
        fn dynamic_id_key_overrides_the_primary_key_only() {
            let columns = columns();
            let meta = map(json!({"$id": 100, "pk": 5, "dyn": 9}));
            let f = filter("id == 100", true).unwrap().unwrap();
            assert!(!f.matches(&columns, 0, None).unwrap());
            assert!(
                !f.matches(&columns, 0, Some(&map(json!({"dyn": 9}))))
                    .unwrap()
            );
            assert!(f.matches(&columns, 0, Some(&meta)).unwrap());
            // 覆盖时其余动态字段（包括名字就叫 pk 的键与 `$meta['$id']`）仍然不可见。
            for expr in [
                "pk > 0",
                "id > 10 and pk > 0",
                "$meta['pk'] > 0",
                "$meta['$id'] > 0",
            ] {
                let f = filter(expr, true).unwrap().unwrap();
                assert!(!f.matches(&columns, 0, Some(&meta)).unwrap(), "{expr}");
            }
            // 字符串覆盖整数主键：语义检查仍按 INT64，求值时比较不成立。
            let f = filter("id == 1", true).unwrap().unwrap();
            assert!(
                !f.matches(&columns, 0, Some(&map(json!({"$id": "1"}))))
                    .unwrap()
            );
            // 值为 null 时键存在：主键读到空值。
            let f = filter("id is null", true).unwrap().unwrap();
            assert!(
                f.matches(&columns, 0, Some(&map(json!({"$id": null}))))
                    .unwrap()
            );
            assert!(!f.matches(&columns, 0, None).unwrap());
            // 键不在投影内时不覆盖。
            let parsed = ParsedFilter::parse_with("id == 100", &TimezoneSource::none()).unwrap();
            let f = RowFilter::compile(&parsed, &schema(true), false)
                .unwrap()
                .unwrap();
            assert!(!f.matches(&columns, 0, Some(&meta)).unwrap());
        }

        #[test]
        fn schema_id_field_overrides_with_its_native_value() {
            let schema_with = |data_type| {
                let mut s = schema(false);
                s.fields.push(FieldSchema::new("$id", data_type));
                s
            };
            let hits = |expr: &str, data_type, column: ColumnData| -> Vec<bool> {
                let parsed = ParsedFilter::parse_with(expr, &TimezoneSource::none()).unwrap();
                let f = RowFilter::compile(&parsed, &schema_with(data_type), false)
                    .unwrap()
                    .unwrap();
                let mut all = columns();
                all.push(column);
                (0..3)
                    .map(|row| f.matches(&all, row, None).unwrap())
                    .collect()
            };
            let doubles = || ColumnData::Double(vec![f64::NAN, f64::INFINITY, 3.0]);
            // NaN 与无穷不是空值。
            assert_eq!(hits("id is null", DataType::Double, doubles()), [false; 3]);
            assert_eq!(
                hits("id is not null", DataType::Double, doubles()),
                [true; 3]
            );
            assert_eq!(
                hits("id > 2", DataType::Double, doubles()),
                [false, true, true]
            );
            // 真正的空值。
            let nullable =
                ColumnData::Double(vec![0.0, 1.0, 2.0]).with_validity(&[false, true, true]);
            assert_eq!(
                hits("id is null", DataType::Double, nullable),
                [true, false, false]
            );
            // 时间戳是 datetime：与整数比较不成立。
            let times = || ColumnData::Int64(vec![1, 2, 3]);
            assert_eq!(hits("id == 2", DataType::Timestamptz, times()), [false; 3]);
            assert_eq!(
                hits("id is null", DataType::Timestamptz, times()),
                [false; 3]
            );
        }
    }

    #[test]
    fn primary_key_names() {
        for pk in [
            "ID",
            "_id",
            "text_match",
            "a1",
            "编号",
            "é",
            "ユーザー",
            "사용자",
            "ſ",
            "नम",
            "x²",
            "\u{9ffd}",
        ] {
            assert_eq!(
                ParsedFilter::parse(&format!("{pk} in [1]"))
                    .ok()
                    .and_then(|parsed| parsed.primary_keys(pk)),
                Some(vec![Int(1)]),
                "{pk}"
            );
        }
        // 关键字、布尔字面量（不区分大小写）与 3.x 切分不成单个标识符的名字。
        for pk in [
            "in",
            "and",
            "like",
            "LIKE",
            "Like",
            "liKe",
            "not",
            "is",
            "null",
            "exists",
            "or",
            "true",
            "True",
            "tRue",
            "iso",
            "INTERVAL",
            "नाम",
            "a-b",
            "a b",
            "1a",
            "my.id",
            "",
        ] {
            assert_eq!(
                ParsedFilter::parse(&format!("{pk} in [1]"))
                    .ok()
                    .and_then(|parsed| parsed.primary_keys(pk)),
                None,
                "{pk:?}"
            );
        }
    }
}
