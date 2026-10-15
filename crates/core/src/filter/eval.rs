//! S1 先绑定 schema，再按列遍历已发布的有效位；不在读取时重新判定主键版本。
use super::parser::{self, Ast, Kind, Literal};
use crate::data::ColumnData;
use crate::error::{EngineError, Result, py_repr, py_repr_list};
use crate::schema::{CollectionSchema, DataType};
use crate::storage::visibility::SourceBits;

#[derive(Debug, Clone)]
pub(super) enum Operand {
    Column(usize),
    Dynamic(String),
    Literal(Literal),
}
#[derive(Debug, Clone)]
pub(super) enum Term {
    Compare {
        op: Comparison,
        left: Operand,
        right: Operand,
    },
    In {
        operand: Operand,
        values: Vec<Literal>,
    },
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Comparison {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

impl Comparison {
    pub(super) fn parse(op: &str) -> Result<Self> {
        match op {
            "==" => Ok(Self::Equal),
            "!=" => Ok(Self::NotEqual),
            "<" => Ok(Self::Less),
            "<=" => Ok(Self::LessEqual),
            ">" => Ok(Self::Greater),
            ">=" => Ok(Self::GreaterEqual),
            _ => Err(unsupported()),
        }
    }

    fn reversed(self) -> Self {
        match self {
            Self::Less => Self::Greater,
            Self::LessEqual => Self::GreaterEqual,
            Self::Greater => Self::Less,
            Self::GreaterEqual => Self::LessEqual,
            other => other,
        }
    }
}
#[derive(Debug)]
pub(crate) struct Predicate {
    pub(super) terms: Vec<Term>,
    pub(super) field_names: Option<Vec<String>>,
    pub(super) tree: Option<super::tree::Tree>,
}

pub(super) fn unsupported() -> EngineError {
    EngineError::unsupported(
        "filter expressions",
        "arithmetic, temporal literals, text_match, and geometry predicates are not supported",
    )
}
pub(super) fn compatible(a: &str, b: &str) -> bool {
    a == b
        || a == "dynamic"
        || b == "dynamic"
        || matches!((a, b), ("int", "float") | ("float", "int"))
}
pub(super) fn sem(dtype: DataType) -> Option<&'static str> {
    match dtype {
        DataType::Bool => Some("bool"),
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => Some("int"),
        DataType::Float | DataType::Double => Some("float"),
        DataType::VarChar => Some("string"),
        _ => None,
    }
}

// difflib.SequenceMatcher 的最长连续匹配块分割；字段名很短，无需引入通用字符串相似度依赖。
fn ratio(a: &str, b: &str) -> f64 {
    let (alen, blen) = (a.chars().count(), b.chars().count());
    if alen + blen > 0 && 2.0 * alen.min(blen) as f64 / ((alen + blen) as f64) < 0.6 {
        return 0.0;
    }
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut matched = 0;
    let mut frequencies = std::collections::HashMap::new();
    for &c in &b {
        *frequencies.entry(c).or_insert(0usize) += 1;
    }
    let popular =
        |c: char| b.len() >= 200 && frequencies.get(&c).is_some_and(|n| *n > b.len() / 100 + 1);
    let mut stack = vec![(0, a.len(), 0, b.len())];
    while let Some((alo, ahi, blo, bhi)) = stack.pop() {
        let (mut ai, mut bi, mut size) = (alo, blo, 0);
        let mut previous = vec![0; bhi - blo + 1];
        for (i, value) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut next = vec![0; bhi - blo + 1];
            for j in blo..bhi {
                if *value == b[j] && !popular(b[j]) {
                    let n = previous[j - blo] + 1;
                    next[j - blo + 1] = n;
                    if n > size {
                        (ai, bi, size) = (i + 1 - n, j + 1 - n, n);
                    }
                }
            }
            previous = next;
        }
        while ai > alo && bi > blo && a[ai - 1] == b[bi - 1] {
            ai -= 1;
            bi -= 1;
            size += 1;
        }
        while ai + size < ahi && bi + size < bhi && a[ai + size] == b[bi + size] {
            size += 1;
        }
        if size > 0 {
            matched += size;
            if alo < ai && blo < bi {
                stack.push((alo, ai, blo, bi));
            }
            if ai + size < ahi && bi + size < bhi {
                stack.push((ai + size, ahi, bi + size, bhi));
            }
        }
    }
    2.0 * matched as f64 / (a.len() + b.len()) as f64
}
pub(super) fn field_error(
    name: &str,
    pos: usize,
    source: &str,
    schema: &CollectionSchema,
) -> EngineError {
    let reserved = matches!(name, "_seq" | "_partition" | "$meta");
    let message = if reserved {
        format!(
            "reserved field {} cannot be used in filter expressions",
            py_repr(name)
        )
    } else {
        format!("unknown field {}", py_repr(name))
    };
    field_diagnostic(&message, name, pos, source, schema)
}
pub(super) fn field_diagnostic(
    message: &str,
    name: &str,
    pos: usize,
    source: &str,
    schema: &CollectionSchema,
) -> EngineError {
    let mut names: Vec<&str> = schema.fields.iter().map(|f| f.name.as_str()).collect();
    names.sort();
    let mut tail = format!("available fields: {}", py_repr_list(names.iter().copied()));
    let best = names
        .iter()
        .map(|n| (ratio(n, name), *n))
        .filter(|(r, _)| *r >= 0.6)
        .max_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(b.1)));
    if let Some((_, best)) = best {
        tail.push_str(&format!("\ndid you mean {}?", py_repr(best)));
    }
    parser::diagnostic(message, source, pos, name.chars().count(), Some(&tail))
}
pub(super) fn describe(ast: &Ast, id: usize, t: &str) -> String {
    match &ast.nodes[id].kind {
        Kind::Field(name) => format!("{t} (field {})", py_repr(name)),
        _ => t.to_owned(),
    }
}

impl Predicate {
    pub(crate) fn compile(source: &str, schema: &CollectionSchema) -> Result<Self> {
        Self::from_parsed(&super::ParsedFilter::parse(source)?, schema)
    }

    pub(crate) fn from_parsed(
        parsed: &super::ParsedFilter,
        schema: &CollectionSchema,
    ) -> Result<Self> {
        let source = parsed.source.as_str();
        let Some(ast) = &parsed.ast else {
            return Ok(Self {
                terms: Vec::new(),
                field_names: None,
                tree: None,
            });
        };
        // 语法总是完整检查；范围外构造优先 UNIMPLEMENTED，不执行其语义检查（0006 第 1 项）。
        if ast
            .nodes
            .iter()
            .any(|node| matches!(node.kind, Kind::Unsupported | Kind::Lit(Literal::Temporal)))
        {
            return Err(unsupported());
        }
        if !super::tree::legacy_shape(ast, schema) {
            return Ok(Self {
                terms: Vec::new(),
                field_names: Some(schema.fields.iter().map(|f| f.name.clone()).collect()),
                tree: Some(super::tree::Tree::compile(parsed, schema)?),
            });
        }
        for node in &ast.nodes {
            if let Kind::And(ids) = &node.kind
                && ids.iter().any(|&id| match &ast.nodes[id].kind {
                    Kind::Lit(Literal::Bool(_)) => true,
                    Kind::Field(name) => schema
                        .fields
                        .iter()
                        .any(|f| &f.name == name && f.data_type == DataType::Bool),
                    _ => false,
                })
            {
                return Err(unsupported());
            }
            if let Kind::Cmp(_, a, b) = &node.kind
                && matches!(
                    (&ast.nodes[*a].kind, &ast.nodes[*b].kind),
                    (
                        Kind::Field(_) | Kind::Meta(_),
                        Kind::Field(_) | Kind::Meta(_)
                    ) | (Kind::Lit(_), Kind::Lit(_))
                )
            {
                return Err(unsupported());
            }
            if let Kind::Field(name) = &node.kind {
                if schema.enable_dynamic_field
                    && !matches!(name.as_str(), "_seq" | "_partition" | "$meta")
                    && !schema.fields.iter().any(|f| &f.name == name)
                {
                    continue;
                }
                if schema
                    .fields
                    .iter()
                    .find(|f| &f.name == name)
                    .is_some_and(|f| matches!(f.data_type, DataType::Json | DataType::FloatVector))
                {
                    return Err(unsupported());
                }
            }
            if let Kind::In { field, negate, .. } = &node.kind {
                let dynamic = matches!(&ast.nodes[*field].kind, Kind::Meta(_))
                    || matches!(&ast.nodes[*field].kind, Kind::Field(name) if !matches!(name.as_str(), "_seq" | "_partition" | "$meta") && !schema.fields.iter().any(|f| &f.name == name));
                if *negate
                    || (!dynamic
                        && (!matches!(&ast.nodes[*field].kind, Kind::Field(name) if schema.fields.iter().any(|f| f.is_primary && &f.name == name))
                            || !std::ptr::eq(node, &ast.nodes[ast.root])))
                {
                    return Err(unsupported());
                }
            }
        }
        let mut types = vec![""; ast.nodes.len()];
        // and 必须在每个子表达式检查结束后立刻检查 bool，再访问下一个；不能全局后序扫描，
        // 否则 `age and missing > 1` 会把右侧未知字段错误放在左侧类型错误之前。
        let mut work = vec![(ast.root, 0_u8)];
        while let Some((id, stage)) = work.pop() {
            let node = &ast.nodes[id];
            if stage == 2 {
                let t = types[id];
                if t != "bool" {
                    return Err(parser::diagnostic(
                        &format!("operands of 'and' must be boolean, got {t}"),
                        source,
                        node.pos,
                        1,
                        None,
                    ));
                }
                continue;
            }
            if stage == 0 {
                match &node.kind {
                    Kind::Cmp(_, a, b) => {
                        work.extend([(id, 1), (*b, 0), (*a, 0)]);
                        continue;
                    }
                    Kind::In { field, list, .. } => {
                        work.extend([(id, 1), (*list, 0), (*field, 0)]);
                        continue;
                    }
                    Kind::And(ids) => {
                        work.push((id, 1));
                        for &child in ids.iter().rev() {
                            work.extend([(child, 2), (child, 0)]);
                        }
                        continue;
                    }
                    Kind::List(ids) => {
                        work.push((id, 1));
                        work.extend(ids.iter().rev().map(|&child| (child, 0)));
                        continue;
                    }
                    _ => {}
                }
            }
            let t = match &node.kind {
                Kind::Lit(lit) => lit.sem(),
                Kind::Meta(_) => {
                    if !schema.enable_dynamic_field {
                        return Err(field_diagnostic(
                            "$meta access requires schema enable_dynamic_field=True",
                            "$meta",
                            node.pos,
                            source,
                            schema,
                        ));
                    }
                    "dynamic"
                }
                Kind::Field(name) => {
                    if matches!(name.as_str(), "_seq" | "_partition" | "$meta") {
                        return Err(field_error(name, node.pos, source, schema));
                    }
                    let field = schema.fields.iter().find(|f| &f.name == name);
                    match field {
                        Some(field) => sem(field.data_type).ok_or_else(unsupported)?,
                        None if schema.enable_dynamic_field => "dynamic",
                        None => return Err(field_error(name, node.pos, source, schema)),
                    }
                }
                Kind::Cmp(op, left, right) => {
                    let (a, b) = (types[*left], types[*right]);
                    if !compatible(a, b) {
                        return Err(parser::diagnostic(
                            &format!("comparison '{op}' between incompatible types"),
                            source,
                            node.pos,
                            op.len(),
                            Some(&format!(
                                "left side is {}, right side is {}",
                                describe(ast, *left, a),
                                describe(ast, *right, b)
                            )),
                        ));
                    }
                    "bool"
                }
                Kind::And(ids) => {
                    for &id in ids {
                        let t = types[id];
                        if t != "bool" {
                            return Err(parser::diagnostic(
                                &format!("operands of 'and' must be boolean, got {t}"),
                                source,
                                ast.nodes[id].pos,
                                1,
                                None,
                            ));
                        }
                    }
                    "bool"
                }
                Kind::List(ids) => {
                    let first = ids.first().map_or("empty", |id| types[*id]);
                    for &id in ids.iter().skip(1) {
                        if !compatible(first, types[id]) {
                            return Err(parser::diagnostic(
                                &format!(
                                    "list elements must be of compatible types, got {first} and {}",
                                    types[id]
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
                    if b != "empty" && !compatible(a, b) {
                        let Kind::Field(name) = &ast.nodes[*field].kind else {
                            return Err(unsupported());
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
                _ => return Err(unsupported()),
            };
            types[id] = t;
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
        let operand = |id: usize| -> Result<Operand> {
            match &ast.nodes[id].kind {
                Kind::Field(name) => Ok(match schema.fields.iter().position(|f| &f.name == name) {
                    Some(i) => Operand::Column(i),
                    None if schema.enable_dynamic_field => Operand::Dynamic(name.clone()),
                    None => return Err(unsupported()),
                }),
                Kind::Meta(key) => Ok(Operand::Dynamic(key.clone())),
                Kind::Lit(lit) => Ok(Operand::Literal(lit.clone())),
                _ => Err(unsupported()),
            }
        };
        let mut stack = vec![ast.root];
        let mut terms = Vec::new();
        while let Some(id) = stack.pop() {
            match &ast.nodes[id].kind {
                Kind::And(ids) => stack.extend(ids.iter().rev()),
                Kind::Cmp(op, a, b) => {
                    if !matches!(
                        (&ast.nodes[*a].kind, &ast.nodes[*b].kind),
                        (Kind::Field(_) | Kind::Meta(_), Kind::Lit(_))
                            | (Kind::Lit(_), Kind::Field(_) | Kind::Meta(_))
                    ) {
                        return Err(unsupported());
                    }
                    terms.push(Term::Compare {
                        op: Comparison::parse(op)?,
                        left: operand(*a)?,
                        right: operand(*b)?,
                    });
                }
                Kind::In { field, list, .. } => {
                    let operand = operand(*field)?;
                    let Kind::List(ids) = &ast.nodes[*list].kind else {
                        return Err(unsupported());
                    };
                    let values = ids
                        .iter()
                        .map(|&id| match &ast.nodes[id].kind {
                            Kind::Lit(l) => Ok(l.clone()),
                            _ => Err(unsupported()),
                        })
                        .collect::<Result<Vec<_>>>()?;
                    terms.push(Term::In { operand, values });
                }
                _ => return Err(unsupported()),
            }
        }
        let dynamic = terms.iter().any(|term| match term {
            Term::Compare { left, right, .. } => {
                matches!(left, Operand::Dynamic(_)) || matches!(right, Operand::Dynamic(_))
            }
            Term::In { operand, .. } => matches!(operand, Operand::Dynamic(_)),
        });
        Ok(Self {
            terms,
            field_names: dynamic.then(|| schema.fields.iter().map(|f| f.name.clone()).collect()),
            tree: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn evaluate_with_meta(
        &self,
        columns: &[ColumnData],
        meta: Option<&[serde_json::Map<String, serde_json::Value>]>,
        live: &SourceBits,
    ) -> Result<SourceBits> {
        if self.field_names.is_some() {
            let rows: Vec<_> = live
                .live_rows()
                .map(|row| super::dynamic::DynamicRow {
                    columns,
                    meta: meta.map(|rows| &rows[row]),
                    row,
                    sequence: None,
                })
                .collect();
            let matching = self.evaluate_source_rows(&rows)?;
            let mut out = live.clone();
            for (index, row) in live.live_rows().enumerate() {
                if !matching.get(index) {
                    out.set(row, false);
                }
            }
            Ok(out)
        } else {
            self.evaluate(columns, live)
        }
    }

    pub(crate) fn requires_source_rows(&self) -> bool {
        self.field_names.is_some()
    }

    pub(crate) fn evaluate_source_rows(
        &self,
        rows: &[super::dynamic::DynamicRow<'_>],
    ) -> Result<SourceBits> {
        if let Some(tree) = &self.tree {
            tree.evaluate_rows(rows)
        } else {
            super::dynamic::evaluate_rows(self, rows)
        }
    }

    pub(crate) fn evaluate(&self, columns: &[ColumnData], live: &SourceBits) -> Result<SourceBits> {
        self.evaluate_columns(columns, live)
    }

    pub(super) fn evaluate_columns<C: std::ops::Index<usize, Output = ColumnData> + ?Sized>(
        &self,
        columns: &C,
        live: &SourceBits,
    ) -> Result<SourceBits> {
        // MemTable 没有存活行时 3.x 不调用 Arrow；已提交旧行的差异按 0006 第 13 项登记。
        // 解析和 schema 检查已在 compile 完成，此处只跳过没有候选行的数据求值。
        if !live.any_live() {
            return Ok(live.clone());
        }
        let mut out = live.clone();
        for term in &self.terms {
            // 所有 and 操作数都求值，不通过先前掩码跳过转换错误；只有原有效位能屏蔽旧版本。
            match term {
                Term::Compare { op, left, right } => {
                    validate_literal(left)?;
                    validate_literal(right)?;
                    let float =
                        operand_precision(left, columns).max(operand_precision(right, columns));
                    let constant_left = if matches!(left, Operand::Literal(_)) {
                        Some(value(left, columns, 0, float)?)
                    } else {
                        None
                    };
                    let constant_right = if matches!(right, Operand::Literal(_)) {
                        Some(value(right, columns, 0, float)?)
                    } else {
                        None
                    };
                    let (column, constant, op) = match (left, right) {
                        (Operand::Column(i), Operand::Literal(_)) => (*i, constant_right, *op),
                        (Operand::Literal(_), Operand::Column(i)) => {
                            (*i, constant_left, op.reversed())
                        }
                        _ => return Err(unsupported()),
                    };
                    compare_column(
                        &columns[column],
                        constant.ok_or_else(unsupported)?,
                        op,
                        live,
                        &mut out,
                    )?;
                }
                Term::In { operand, values } => {
                    let Operand::Column(column) = operand else {
                        return Err(unsupported());
                    };
                    for lit in values {
                        validate_literal(&Operand::Literal(lit.clone()))?;
                    }
                    let float = if matches!(
                        columns[*column],
                        ColumnData::Float(_) | ColumnData::Double(_)
                    ) || values.iter().any(|l| matches!(l, Literal::Float(_)))
                    {
                        64
                    } else {
                        0
                    };
                    // Arrow 先尝试把整个 double value_set 无损转成列的 int64 类型。
                    // 只有该转换失败才把列转为 double；后一路的 set lookup 按位区分 ±0。
                    // 2^63 不在 int64 范围内：参照的 Arrow 在 arm64 上把它饱和成 i64::MAX 并通过
                    // 往返校验，x86_64 则判为截断。这里按截断处理，越界浮点数（包括 float(i64::MAX - 1)）
                    // 不能命中、更不能删除另一个整数；差异见 0006「与 3.x 的差异」。
                    let integer_set = float != 0
                        && matches!(columns[*column], ColumnData::Int64(_))
                        && values.iter().all(|literal| match literal {
                            Literal::Int(_) => true,
                            Literal::Float(v) => {
                                v.is_finite()
                                    && v.fract() == 0.0
                                    && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0)
                                        .contains(v)
                            }
                            _ => false,
                        });
                    let lookup_precision = if integer_set { 0 } else { float };
                    let operands: Vec<_> = values.iter().cloned().map(Operand::Literal).collect();
                    let mut set = std::collections::HashSet::new();
                    for operand in &operands {
                        // pa.array 的混合 int/float value_set 用独立的安全转换诊断，不是 pc.cast 的文案。
                        if float != 0
                            && let Operand::Literal(Literal::Int(text)) = operand
                        {
                            let integer = text.parse::<i64>().map_err(|_| {
                                EngineError::invalid("Python int too large to convert to C long")
                            })?;
                            if !(-9_007_199_254_740_992..=9_007_199_254_740_992).contains(&integer)
                            {
                                return Err(EngineError::invalid(format!(
                                    "Integer value {integer} is outside of the range exactly representable by a IEEE 754 double precision value"
                                )));
                            }
                        }
                        let scalar =
                            if integer_set && let Operand::Literal(Literal::Float(v)) = operand {
                                // 上面的范围检查保证这个整数值的 double 能精确转换。
                                Scalar::Int(*v as i64)
                            } else {
                                value(operand, columns, 0, lookup_precision)?
                            };
                        set.insert(scalar.key());
                    }
                    let column_operand = Operand::Column(*column);
                    for row in 0..live.len {
                        if live.get(row) {
                            let a = value(&column_operand, columns, row, lookup_precision)?;
                            if !set.contains(&a.key()) {
                                out.set(row, false);
                            }
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

#[derive(Clone, Copy)]
pub(super) enum Scalar<'a> {
    Int(i64),
    Float(f64),
    String(&'a str),
    Bool(bool),
}
#[derive(PartialEq, Eq, Hash)]
pub(super) enum ScalarKey<'a> {
    Int(i64),
    Float(u64),
    String(&'a str),
    Bool(bool),
}
impl<'a> Scalar<'a> {
    pub(super) fn key(&self) -> ScalarKey<'a> {
        match self {
            Self::Int(v) => ScalarKey::Int(*v),
            Self::Float(v) => ScalarKey::Float(v.to_bits()),
            Self::String(v) => ScalarKey::String(v),
            Self::Bool(v) => ScalarKey::Bool(*v),
        }
    }
}
pub(super) fn validate_literal(op: &Operand) -> Result<()> {
    if let Operand::Literal(Literal::Int(s)) = op {
        s.parse::<i64>()
            .map_err(|_| EngineError::invalid("Python int too large to convert to C long"))?;
    }
    Ok(())
}
pub(super) fn operand_precision<C: std::ops::Index<usize, Output = ColumnData> + ?Sized>(
    op: &Operand,
    columns: &C,
) -> u8 {
    match op {
        Operand::Literal(Literal::Float(_)) => 64,
        Operand::Column(i) => match columns[*i] {
            ColumnData::Float(_) => 32,
            ColumnData::Double(_) => 64,
            _ => 0,
        },
        _ => 0,
    }
}
pub(super) fn value<'a, C: std::ops::Index<usize, Output = ColumnData> + ?Sized>(
    op: &'a Operand,
    columns: &'a C,
    row: usize,
    float: u8,
) -> Result<Scalar<'a>> {
    let scalar = match op {
        Operand::Literal(Literal::Int(s)) => Scalar::Int(
            s.parse()
                .map_err(|_| EngineError::invalid("Python int too large to convert to C long"))?,
        ),
        Operand::Literal(Literal::Float(f)) => Scalar::Float(*f),
        Operand::Literal(Literal::String(s)) => Scalar::String(s),
        Operand::Literal(Literal::Bool(b)) => Scalar::Bool(*b),
        Operand::Column(i) => match &columns[*i] {
            ColumnData::Int8(v) => Scalar::Int(i64::from(v[row])),
            ColumnData::Int16(v) => Scalar::Int(i64::from(v[row])),
            ColumnData::Int32(v) => Scalar::Int(i64::from(v[row])),
            ColumnData::Int64(v) => Scalar::Int(v[row]),
            ColumnData::Float(v) => Scalar::Float(f64::from(v[row])),
            ColumnData::Double(v) => Scalar::Float(v[row]),
            ColumnData::Bool(v) => Scalar::Bool(v[row]),
            ColumnData::VarChar(v) => Scalar::String(&v[row]),
            _ => return Err(unsupported()),
        },
        _ => return Err(unsupported()),
    };
    if float != 0
        && let Scalar::Int(i) = scalar
    {
        let bound = if float == 32 {
            16_777_216
        } else {
            9_007_199_254_740_992
        };
        check_integer_range(i, bound)?;
        return Ok(Scalar::Float(if float == 32 {
            f64::from(i as f32)
        } else {
            i as f64
        }));
    }
    Ok(scalar)
}
// 列类型、比较操作与常量在逐行循环之外绑定；浮点安全转换仍检查原来的全部活行。
fn compare_column(
    column: &ColumnData,
    constant: Scalar<'_>,
    op: Comparison,
    live: &SourceBits,
    out: &mut SourceBits,
) -> Result<()> {
    macro_rules! integer {
        ($values:expr) => {{
            let values = $values;
            match constant {
                Scalar::Int(constant) => {
                    compare_values(values, constant, |v| i64::from(*v), op, live, out);
                }
                Scalar::Float(constant) => {
                    // S1 只比较字段与字面量；整数列只有配 float64 字面量才会提升。
                    for row in live.live_rows() {
                        check_integer_range(i64::from(values[row]), 9_007_199_254_740_992)?;
                    }
                    compare_values(values, constant, |v| i64::from(*v) as f64, op, live, out);
                }
                _ => return Err(unsupported()),
            }
        }};
    }
    match column {
        ColumnData::Int8(values) => integer!(values),
        ColumnData::Int16(values) => integer!(values),
        ColumnData::Int32(values) => integer!(values),
        ColumnData::Int64(values) => integer!(values),
        ColumnData::Float(values) => {
            let Scalar::Float(constant) = constant else {
                return Err(unsupported());
            };
            compare_values(values, constant, |v| f64::from(*v), op, live, out);
        }
        ColumnData::Double(values) => {
            let Scalar::Float(constant) = constant else {
                return Err(unsupported());
            };
            compare_values(values, constant, |v| *v, op, live, out);
        }
        ColumnData::Bool(values) => {
            let Scalar::Bool(constant) = constant else {
                return Err(unsupported());
            };
            compare_values(values, constant, |v| *v, op, live, out);
        }
        ColumnData::VarChar(values) => {
            let Scalar::String(constant) = constant else {
                return Err(unsupported());
            };
            compare_values(values, constant, |v| v.as_str(), op, live, out);
        }
        _ => return Err(unsupported()),
    }
    Ok(())
}

fn check_integer_range(value: i64, bound: i64) -> Result<()> {
    if !(-bound..=bound).contains(&value) {
        return Err(EngineError::invalid(format!(
            "Integer value {value} not in range: -{bound} to {bound}"
        )));
    }
    Ok(())
}

fn compare_values<'a, T, U: PartialOrd + Copy>(
    values: &'a [T],
    constant: U,
    convert: impl Fn(&'a T) -> U,
    op: Comparison,
    live: &SourceBits,
    out: &mut SourceBits,
) {
    match op {
        Comparison::Equal => out.intersect_matching(live, |row| convert(&values[row]) == constant),
        Comparison::NotEqual => {
            out.intersect_matching(live, |row| convert(&values[row]) != constant)
        }
        Comparison::Less => out.intersect_matching(live, |row| convert(&values[row]) < constant),
        Comparison::LessEqual => {
            out.intersect_matching(live, |row| convert(&values[row]) <= constant)
        }
        Comparison::Greater => out.intersect_matching(live, |row| convert(&values[row]) > constant),
        Comparison::GreaterEqual => {
            out.intersect_matching(live, |row| convert(&values[row]) >= constant)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::FieldSchema;

    // 独立保留优化前的逐行标量模型，防止列特化在浮点提升、反向比较或稀疏位图上偏移。
    fn scalar_model(
        predicate: &Predicate,
        columns: &[ColumnData],
        live: &SourceBits,
    ) -> Result<SourceBits> {
        let mut out = live.clone();
        if !live.any_live() {
            return Ok(out);
        }
        for term in &predicate.terms {
            let Term::Compare { op, left, right } = term else {
                return Err(unsupported());
            };
            validate_literal(left)?;
            validate_literal(right)?;
            let precision = operand_precision(left, columns).max(operand_precision(right, columns));
            let a = if matches!(left, Operand::Literal(_)) {
                Some(value(left, columns, 0, precision)?)
            } else {
                None
            };
            let b = if matches!(right, Operand::Literal(_)) {
                Some(value(right, columns, 0, precision)?)
            } else {
                None
            };
            for row in 0..live.len {
                if !live.get(row) {
                    continue;
                }
                let a = a
                    .map(Ok)
                    .unwrap_or_else(|| value(left, columns, row, precision))?;
                let b = b
                    .map(Ok)
                    .unwrap_or_else(|| value(right, columns, row, precision))?;
                let order = match (a, b) {
                    (Scalar::Int(a), Scalar::Int(b)) => Some(a.cmp(&b)),
                    (Scalar::Float(a), Scalar::Float(b)) => a.partial_cmp(&b),
                    (Scalar::String(a), Scalar::String(b)) => Some(a.cmp(b)),
                    (Scalar::Bool(a), Scalar::Bool(b)) => Some(a.cmp(&b)),
                    _ => None,
                };
                use std::cmp::Ordering::{Equal, Greater, Less};
                let pass = match op {
                    Comparison::Equal => order == Some(Equal),
                    Comparison::NotEqual => order != Some(Equal),
                    Comparison::Less => order == Some(Less),
                    Comparison::LessEqual => matches!(order, Some(Less | Equal)),
                    Comparison::Greater => order == Some(Greater),
                    Comparison::GreaterEqual => matches!(order, Some(Greater | Equal)),
                };
                if !pass {
                    out.set(row, false);
                }
            }
        }
        Ok(out)
    }

    #[test]
    fn typed_comparisons_match_scalar_model_for_all_types_operators_and_sparse_masks() {
        for len in [0_usize, 1, 63, 64, 65, 129] {
            let ints: Vec<_> = (0..len).map(|row| (row % 19) as i64 - 9).collect();
            let floats: Vec<_> = (0..len)
                .map(|row| match row % 13 {
                    0 => f64::NAN,
                    1 => -0.0,
                    2 => f64::INFINITY,
                    3 => f64::NEG_INFINITY,
                    _ => (row % 19) as f64 / 4.0 - 2.0,
                })
                .collect();
            let cases = [
                (
                    DataType::Int8,
                    ColumnData::Int8(ints.iter().map(|v| *v as i8).collect()),
                    vec!["-2", "2.5", "-0.0"],
                ),
                (
                    DataType::Int16,
                    ColumnData::Int16(ints.iter().map(|v| *v as i16).collect()),
                    vec!["-2", "2.5"],
                ),
                (
                    DataType::Int32,
                    ColumnData::Int32(ints.iter().map(|v| *v as i32).collect()),
                    vec!["-2", "2.5"],
                ),
                (
                    DataType::Int64,
                    ColumnData::Int64(ints),
                    vec!["-2", "2.5", "1e999", "9223372036854775808"],
                ),
                (
                    DataType::Float,
                    ColumnData::Float(floats.iter().map(|v| *v as f32).collect()),
                    vec!["-2", "2.5", "16777217", "-0.0"],
                ),
                (
                    DataType::Double,
                    ColumnData::Double(floats),
                    vec!["-2", "2.5", "9007199254740993", "-0.0"],
                ),
                (
                    DataType::Bool,
                    ColumnData::Bool((0..len).map(|row| row % 3 == 0).collect()),
                    vec!["false", "true"],
                ),
                (
                    DataType::VarChar,
                    ColumnData::VarChar((0..len).map(|row| format!("三{}", row % 7)).collect()),
                    vec!["'三3'", "''"],
                ),
            ];
            for (dtype, column, literals) in cases {
                let schema = CollectionSchema {
                    fields: vec![FieldSchema::new("x", dtype)],
                    enable_dynamic_field: false,
                    properties: vec![],
                };
                for stride in [1, 3, 70] {
                    let live = SourceBits::from_live(
                        &(0..len).map(|row| row % stride == 0).collect::<Vec<_>>(),
                    );
                    for literal in &literals {
                        for op in ["==", "!=", "<", "<=", ">", ">="] {
                            for expr in [format!("x {op} {literal}"), format!("{literal} {op} x")] {
                                let predicate = Predicate::compile(&expr, &schema).unwrap();
                                let columns = std::slice::from_ref(&column);
                                let actual = predicate
                                    .evaluate(columns, &live)
                                    .map(|bits| bits.live_rows().collect::<Vec<_>>())
                                    .map_err(|err| err.to_string());
                                let expected = scalar_model(&predicate, columns, &live)
                                    .map(|bits| bits.live_rows().collect::<Vec<_>>())
                                    .map_err(|err| err.to_string());
                                assert_eq!(actual, expected, "{dtype:?}, {len}, {stride}, {expr}");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn typed_cast_rejects_live_wide_integers_even_after_a_false_term() {
        let schema = CollectionSchema {
            fields: vec![FieldSchema::new("x", DataType::Int64)],
            enable_dynamic_field: false,
            properties: vec![],
        };
        let columns = [ColumnData::Int64(vec![
            0,
            9_007_199_254_740_992,
            9_007_199_254_740_993,
            i64::MIN,
            i64::MAX,
        ])];
        for active in [
            vec![true; 5],
            vec![true, true, false, false, false],
            vec![false; 5],
        ] {
            let live = SourceBits::from_live(&active);
            for expr in [
                "x > 1.0",
                "1.0 < x",
                "x < 0 and x > 1.0",
                "x > 9223372036854775808",
            ] {
                let predicate = Predicate::compile(expr, &schema).unwrap();
                let actual = predicate
                    .evaluate(&columns, &live)
                    .map(|b| b.live_rows().collect::<Vec<_>>())
                    .map_err(|e| e.to_string());
                let expected = scalar_model(&predicate, &columns, &live)
                    .map(|b| b.live_rows().collect::<Vec<_>>())
                    .map_err(|e| e.to_string());
                assert_eq!(actual, expected, "{expr}, {active:?}");
            }
        }
    }
}
