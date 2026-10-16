//! 绑定后的布尔树；路线属于整条表达式，不能由运行期访问的叶子改变。

use super::dynamic::DynamicRow;
use super::eval::{self, Operand};
use super::parser::{self, Ast, Kind};
use crate::error::{EngineError, Result, py_repr};
use crate::schema::{CollectionSchema, DataType};
use crate::storage::visibility::SourceBits;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Backend {
    Arrow,
    Hybrid,
    Python,
}

#[derive(Debug)]
pub(super) struct Tree {
    pub(super) ast: Ast,
    pub(super) operands: Vec<Option<Operand>>,
    pub(super) names: Vec<String>,
    pub(super) backend: Backend,
    pub(super) patterns: Vec<Option<super::python::LikePattern>>,
}

pub(super) fn legacy_shape(ast: &Ast, schema: &CollectionSchema) -> bool {
    let field = |id: usize| match &ast.nodes[id].kind {
        Kind::Field(name) => !schema.fields.iter().any(|f| {
            &f.name == name
                && matches!(
                    f.data_type,
                    DataType::Json | DataType::Array | DataType::FloatVector
                )
        }),
        Kind::Meta(_) => true,
        _ => false,
    };
    let mut work = vec![ast.root];
    while let Some(id) = work.pop() {
        match &ast.nodes[id].kind {
            Kind::And(ids) => work.extend(ids),
            Kind::Cmp(_, a, b)
                if (field(*a) && matches!(ast.nodes[*b].kind, Kind::Lit(_)))
                    || (field(*b) && matches!(ast.nodes[*a].kind, Kind::Lit(_))) => {}
            Kind::In {
                field: f,
                negate: false,
                ..
            } => {
                if !field(*f) {
                    return false;
                }
                if let Kind::Field(name) = &ast.nodes[*f].kind
                    && let Some(f) = schema.fields.iter().find(|f| &f.name == name)
                    && (!f.is_primary || id != ast.root)
                {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

impl Tree {
    pub(super) fn compile(parsed: &super::ParsedFilter, schema: &CollectionSchema) -> Result<Self> {
        let ast = parsed
            .ast
            .as_ref()
            .ok_or_else(|| EngineError::Storage("empty filter tree".into()))?;
        let source = &parsed.source;
        let mut types = vec![""; ast.nodes.len()];
        let mut work = vec![(ast.root, 0_u8, "")];
        while let Some((id, stage, boolean)) = work.pop() {
            let node = &ast.nodes[id];
            if stage == 2 {
                if types[id] != "bool" {
                    return Err(parser::diagnostic(
                        &format!("operands of '{boolean}' must be boolean, got {}", types[id]),
                        source,
                        node.pos,
                        1,
                        None,
                    ));
                }
                continue;
            }
            if stage == 0 {
                if let Kind::IsNull {
                    value,
                    exists: true,
                    ..
                } = &node.kind
                    && let Kind::Field(name) = &ast.nodes[*value].kind
                    && let Some(field) = schema.fields.iter().find(|f| &f.name == name)
                {
                    let message = if field.data_type == DataType::Json {
                        "'exists' on a whole JSON field is not supported, only on a key within it"
                            .into()
                    } else {
                        format!(
                            "'exists' is only supported on JSON keys and dynamic fields, but {} is {}",
                            py_repr(name),
                            field.data_type.name().to_ascii_lowercase()
                        )
                    };
                    return Err(parser::diagnostic(
                        &message,
                        source,
                        ast.nodes[*value].pos,
                        name.chars().count(),
                        None,
                    ));
                }
                let children = match &node.kind {
                    Kind::And(ids) | Kind::Or(ids) => {
                        let operator = if matches!(node.kind, Kind::And(_)) {
                            "and"
                        } else {
                            "or"
                        };
                        work.push((id, 1, ""));
                        for &child in ids.iter().rev() {
                            work.extend([(child, 2, operator), (child, 0, "")]);
                        }
                        continue;
                    }
                    Kind::Cmp(_, a, b) => vec![*a, *b],
                    Kind::In { field, list, .. } => vec![*field, *list],
                    Kind::List(ids) => ids.clone(),
                    Kind::Not(value)
                    | Kind::Length(value)
                    | Kind::Like { value, .. }
                    | Kind::IsNull { value, .. }
                    | Kind::Contains { value, .. } => vec![*value],
                    Kind::Path { base, .. } => vec![*base],
                    _ => Vec::new(),
                };
                if !children.is_empty() {
                    work.push((id, 1, ""));
                    work.extend(children.into_iter().rev().map(|id| (id, 0, "")));
                    continue;
                }
            }
            types[id] = match &node.kind {
                Kind::Lit(literal) => literal.sem(),
                Kind::Field(name) => {
                    if matches!(name.as_str(), "_seq" | "_partition" | "$meta") {
                        return Err(eval::field_error(name, node.pos, source, schema));
                    }
                    match schema.fields.iter().find(|f| &f.name == name) {
                        Some(f) if f.data_type == DataType::Json => "string",
                        Some(f) if f.data_type == DataType::Array => "dynamic",
                        Some(f) => eval::sem(f.data_type).ok_or_else(|| {
                            parser::diagnostic(
                                &format!(
                                    "field {} of type {} cannot be used in scalar filter expressions",
                                    py_repr(name),
                                    f.data_type.name().to_ascii_lowercase(),
                                ),
                                source,
                                node.pos,
                                name.chars().count(),
                                None,
                            )
                        })?,
                        None if schema.enable_dynamic_field => "dynamic",
                        None => return Err(eval::field_error(name, node.pos, source, schema)),
                    }
                }
                Kind::Meta(_) => {
                    if !schema.enable_dynamic_field {
                        return Err(eval::field_diagnostic(
                            "$meta access requires schema enable_dynamic_field=True",
                            "$meta",
                            node.pos,
                            source,
                            schema,
                        ));
                    }
                    "dynamic"
                }
                Kind::Cmp(op, left, right) => {
                    let (a, b) = (types[*left], types[*right]);
                    if !eval::compatible(a, b) {
                        return Err(parser::diagnostic(
                            &format!("comparison '{op}' between incompatible types"),
                            source,
                            node.pos,
                            op.len(),
                            Some(&format!(
                                "left side is {}, right side is {}",
                                eval::describe(ast, *left, a),
                                eval::describe(ast, *right, b)
                            )),
                        ));
                    }
                    "bool"
                }
                Kind::And(_) | Kind::Or(_) => "bool",
                Kind::Not(child) => {
                    if types[*child] != "bool" {
                        return Err(parser::diagnostic(
                            &format!("operand of 'not' must be boolean, got {}", types[*child]),
                            source,
                            node.pos,
                            1,
                            None,
                        ));
                    }
                    "bool"
                }
                Kind::List(ids) => {
                    let first = ids.first().map_or("empty", |id| types[*id]);
                    for &child in ids.iter().skip(1) {
                        if !eval::compatible(first, types[child]) {
                            return Err(parser::diagnostic(
                                &format!(
                                    "list elements must be of compatible types, got {first} and {}",
                                    types[child]
                                ),
                                source,
                                node.pos,
                                1,
                                None,
                            ));
                        }
                    }
                    first
                }
                Kind::In { field, list, .. } => {
                    let (a, b) = (types[*field], types[*list]);
                    if b != "empty" && !eval::compatible(a, b) {
                        let Kind::Field(name) = &ast.nodes[*field].kind else {
                            return Err(eval::unsupported());
                        };
                        return Err(parser::diagnostic(
                            &format!(
                                "'in' list elements ({b}) incompatible with field {} ({a})",
                                py_repr(name)
                            ),
                            source,
                            node.pos,
                            2,
                            None,
                        ));
                    }
                    "bool"
                }
                Kind::Like { value, .. } => {
                    if !matches!(types[*value], "string" | "dynamic") {
                        let description = eval::describe(ast, *value, types[*value]);
                        return Err(parser::diagnostic(
                            &format!("LIKE requires a string operand, got {description}"),
                            source,
                            node.pos,
                            1,
                            Some(&format!("operand is {description}")),
                        ));
                    }
                    "bool"
                }
                Kind::IsNull { .. } => "bool",
                Kind::Path { .. } => "dynamic",
                Kind::Contains { value, .. } | Kind::Length(value) => {
                    let is_json = matches!(&ast.nodes[*value].kind,Kind::Field(name) if schema.fields.iter().any(|f| &f.name==name && f.data_type==DataType::Json));
                    if types[*value] != "dynamic" && !is_json {
                        let op = if matches!(node.kind, Kind::Length(_)) {
                            "array_length"
                        } else {
                            "array_contains"
                        };
                        return Err(parser::diagnostic(
                            &format!(
                                "{op} requires an array or dynamic value, got {}",
                                types[*value]
                            ),
                            source,
                            node.pos,
                            1,
                            None,
                        ));
                    }
                    if matches!(node.kind, Kind::Length(_)) {
                        "int"
                    } else {
                        "bool"
                    }
                }
                Kind::Unsupported => return Err(eval::unsupported()),
            };
        }
        if types[ast.root] != "bool" {
            return Err(parser::diagnostic(
                &format!(
                    "top-level filter expression must evaluate to bool, got {}",
                    types[ast.root]
                ),
                source,
                ast.nodes[ast.root].pos,
                1,
                None,
            ));
        }
        let mut backend = Backend::Arrow;
        let operands = ast
            .nodes
            .iter()
            .map(|node| match &node.kind {
                Kind::Field(name) if matches!(name.as_str(), "_seq" | "_partition") => None,
                Kind::Field(name) => Some(
                    if let Some(i) = schema.fields.iter().position(|f| &f.name == name) {
                        Operand::Column(i)
                    } else {
                        if schema.enable_dynamic_field && backend == Backend::Arrow {
                            backend = Backend::Hybrid;
                        }
                        Operand::Dynamic(name.clone())
                    },
                ),
                Kind::Meta(key) => {
                    if backend == Backend::Arrow {
                        backend = Backend::Hybrid;
                    }
                    Some(Operand::Dynamic(key.clone()))
                }
                Kind::Lit(l) => Some(Operand::Literal(l.clone())),
                Kind::Path { .. } | Kind::Contains { .. } | Kind::Length(_) => {
                    backend = Backend::Python;
                    None
                }
                _ => None,
            })
            .collect();
        let patterns = ast
            .nodes
            .iter()
            .map(|node| match &node.kind {
                Kind::Like { pattern, .. } => Some(super::python::LikePattern::new(pattern)),
                _ => None,
            })
            .collect();
        Ok(Self {
            ast: ast.clone(),
            operands,
            names: schema.fields.iter().map(|f| f.name.clone()).collect(),
            backend,
            patterns,
        })
    }

    pub(super) fn evaluate_rows(&self, rows: &[DynamicRow<'_>]) -> Result<SourceBits> {
        if rows.is_empty() {
            return Ok(SourceBits::new(0));
        }
        match self.backend {
            Backend::Python => super::python::evaluate_tree(self, rows),
            Backend::Arrow | Backend::Hybrid => super::columns::evaluate_rows(self, rows),
        }
    }
}
