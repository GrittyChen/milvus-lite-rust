//! 语法与诊断以参照实现 v3.2.1（43d1257）tokens.py / parser.py 为准（Apache-2.0）。
//! arena 保存节点，避免长左结合链在析构与语义检查时递归耗尽栈。

use super::unicode;
use crate::error::{EngineError, Result, py_repr};

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Literal {
    Int(String),
    Float(f64),
    String(String),
    Bool(bool),
    Temporal,
}
impl Literal {
    pub(super) fn negate(self) -> Self {
        match self {
            Self::Int(s) => Self::Int(if let Some(v) = s.strip_prefix('-') {
                v.to_owned()
            } else if s == "0" {
                s
            } else {
                format!("-{s}")
            }),
            Self::Float(f) => Self::Float(-f),
            other => other,
        }
    }
    pub(super) fn sem(&self) -> &'static str {
        match self {
            Self::Int(_) => "int",
            Self::Float(_) => "float",
            Self::String(_) => "string",
            Self::Bool(_) => "bool",
            Self::Temporal => "temporal",
        }
    }
}

#[derive(Debug)]
pub(super) enum Kind {
    Lit(Literal),
    Field(String),
    Meta(String),
    Cmp(String, usize, usize),
    And(Vec<usize>),
    In {
        field: usize,
        list: usize,
        negate: bool,
    },
    List(Vec<usize>),
    Path,
    Unsupported,
}
#[derive(Debug)]
pub(super) struct Node {
    pub kind: Kind,
    pub pos: usize,
}
pub(super) struct Ast {
    pub nodes: Vec<Node>,
    pub root: usize,
}

#[derive(Debug, Clone)]
struct Token {
    kind: String,
    text: String,
    pos: usize,
    lit: Option<Literal>,
}

pub(super) fn diagnostic(
    message: &str,
    source: &str,
    pos: usize,
    span: usize,
    tail: Option<&str>,
) -> EngineError {
    let mut message = format!("{message} at column {}", pos + 1);
    if !source.is_empty() {
        let line = source.trim_end_matches('\n');
        let width = span
            .max(1)
            .min(line.chars().count().saturating_sub(pos) + 1);
        message.push_str(&format!(
            "\n  {line}\n  {}{}",
            " ".repeat(pos),
            "^".repeat(width)
        ));
    }
    if let Some(tail) = tail {
        message.push('\n');
        message.push_str(tail);
    }
    EngineError::schema(message)
}

fn lex(source: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if matches!(c, ' ' | '\t' | '\r' | '\n') {
            i += 1;
            continue;
        }
        let start = i;
        let mut lit = None;
        let kind: String;
        if matches!(c, '(' | ')' | '[' | ']' | ',' | '-' | '+' | '*' | '/') {
            kind = c.to_string();
            i += 1;
        } else if matches!(c, '=' | '!' | '<' | '>' | '&' | '|') {
            let next = chars.get(i + 1).copied();
            let pair = format!("{c}{}", next.unwrap_or('\0'));
            if matches!(pair.as_str(), "==" | "!=" | "<=" | ">=" | "&&" | "||") {
                kind = match pair.as_str() {
                    "&&" => "and".into(),
                    "||" => "or".into(),
                    _ => pair,
                };
                i += 2;
            } else if c == '!' {
                kind = "not".into();
                i += 1;
            } else if c == '<' && next == Some('>') {
                return Err(diagnostic(
                    "unexpected token '<>'",
                    source,
                    i,
                    2,
                    Some("did you mean '!='?"),
                ));
            } else if matches!(c, '<' | '>') {
                kind = c.to_string();
                i += 1;
            } else {
                let hint = match c {
                    '=' => "did you mean '=='?",
                    '&' => "did you mean '&&'?",
                    _ => "did you mean '||'?",
                };
                return Err(diagnostic(
                    &format!("unexpected character {}", py_repr(&c.to_string())),
                    source,
                    i,
                    1,
                    Some(hint),
                ));
            }
        } else if matches!(c, '\'' | '"') {
            i += 1;
            let mut value = String::new();
            loop {
                let Some(&ch) = chars.get(i) else {
                    return Err(diagnostic(
                        "unterminated string literal",
                        source,
                        start,
                        1,
                        None,
                    ));
                };
                if ch == c {
                    i += 1;
                    break;
                }
                if ch == '\\' {
                    let Some(&esc) = chars.get(i + 1) else {
                        return Err(diagnostic(
                            "unterminated string literal",
                            source,
                            start,
                            1,
                            None,
                        ));
                    };
                    let mapped = match esc {
                        '"' | '\'' | '\\' => esc,
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        _ => {
                            return Err(diagnostic(
                                &format!("unknown escape sequence \\{esc}"),
                                source,
                                i,
                                2,
                                None,
                            ));
                        }
                    };
                    value.push(mapped);
                    i += 2;
                } else if matches!(ch, '\r' | '\n') {
                    return Err(diagnostic(
                        "unterminated string literal (newline)",
                        source,
                        start,
                        1,
                        None,
                    ));
                } else {
                    value.push(ch);
                    i += 1;
                }
            }
            kind = "string".into();
            lit = Some(Literal::String(value));
        } else if c == '$' {
            if chars[i..].starts_with(&['$', 'm', 'e', 't', 'a']) {
                i += 5;
                kind = "$meta".into();
            } else {
                return Err(diagnostic(
                    "unexpected character '$'",
                    source,
                    i,
                    1,
                    Some("only '$meta' is supported as a dynamic-field marker"),
                ));
            }
        } else if unicode::digit(c) {
            while chars.get(i).is_some_and(|c| unicode::digit(*c)) {
                i += 1;
            }
            let mut floating = false;
            if chars.get(i) == Some(&'.') {
                if !chars.get(i + 1).is_some_and(|c| unicode::digit(*c)) {
                    return Err(diagnostic(
                        "malformed float literal (digit expected after '.')",
                        source,
                        i,
                        1,
                        None,
                    ));
                }
                floating = true;
                i += 1;
                while chars.get(i).is_some_and(|c| unicode::digit(*c)) {
                    i += 1;
                }
            }
            if chars.get(i).is_some_and(|c| matches!(c, 'e' | 'E')) {
                floating = true;
                i += 1;
                if chars.get(i).is_some_and(|c| matches!(c, '+' | '-')) {
                    i += 1;
                }
                if !chars.get(i).is_some_and(|c| unicode::digit(*c)) {
                    return Err(diagnostic(
                        "malformed float literal (digit expected in exponent)",
                        source,
                        i,
                        1,
                        None,
                    ));
                }
                while chars.get(i).is_some_and(|c| unicode::digit(*c)) {
                    i += 1;
                }
            }
            let text: String = chars[start..i].iter().collect();
            let normalized: Option<String> = text
                .chars()
                .map(|c| {
                    if unicode::digit(c) {
                        unicode::decimal(c).and_then(|n| char::from_digit(n, 10))
                    } else {
                        Some(c)
                    }
                })
                .collect();
            if floating {
                let value = normalized
                    .as_deref()
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or_else(|| {
                        diagnostic(
                            &format!("invalid float literal {}", py_repr(&text)),
                            source,
                            start,
                            i - start,
                            None,
                        )
                    })?;
                lit = Some(Literal::Float(value));
                kind = "float".into();
            } else {
                let normalized = normalized.filter(|_| i - start <= 4300).ok_or_else(|| {
                    diagnostic(
                        &format!("invalid integer literal {}", py_repr(&text)),
                        source,
                        start,
                        i - start,
                        None,
                    )
                })?;
                let stripped = normalized.trim_start_matches('0');
                lit = Some(Literal::Int(if stripped.is_empty() {
                    "0".into()
                } else {
                    stripped.into()
                }));
                kind = "int".into();
            }
        } else if unicode::alpha(c) || c == '_' {
            i += 1;
            while chars
                .get(i)
                .is_some_and(|c| unicode::alnum(*c) || *c == '_')
            {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            let lower = text.to_lowercase();
            if matches!(
                text.as_str(),
                "true" | "True" | "TRUE" | "false" | "False" | "FALSE"
            ) {
                lit = Some(Literal::Bool(lower == "true"));
                kind = "bool".into();
            } else if matches!(lower.as_str(), "true" | "false") {
                let title = if lower == "true" { "True" } else { "False" };
                return Err(diagnostic(
                    &format!("unexpected identifier {}", py_repr(&text)),
                    source,
                    start,
                    i - start,
                    Some(&format!(
                        "did you mean {}, {}, or {}?",
                        py_repr(&lower),
                        py_repr(title),
                        py_repr(&lower.to_uppercase())
                    )),
                ));
            } else if matches!(
                lower.as_str(),
                "and"
                    | "or"
                    | "not"
                    | "in"
                    | "like"
                    | "is"
                    | "null"
                    | "exists"
                    | "iso"
                    | "interval"
            ) {
                kind = lower;
            } else {
                kind = "ident".into();
            }
        } else {
            return Err(diagnostic(
                &format!("unexpected character {}", py_repr(&c.to_string())),
                source,
                i,
                1,
                None,
            ));
        }
        tokens.push(Token {
            kind,
            text: chars[start..i].iter().collect(),
            pos: start,
            lit,
        });
    }
    tokens.push(Token {
        kind: "eof".into(),
        text: String::new(),
        pos: chars.len(),
        lit: None,
    });
    Ok(tokens)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    at: usize,
    source: &'a str,
    nodes: Vec<Node>,
    depth: usize,
}
impl Parser<'_> {
    fn peek(&self) -> &Token {
        &self.tokens[self.at]
    }
    fn has(&self, kind: &str) -> bool {
        self.peek().kind == kind
    }
    fn take(&mut self) -> Token {
        let token = self.peek().clone();
        self.at += 1;
        token
    }
    fn node(&mut self, kind: Kind, pos: usize) -> usize {
        let i = self.nodes.len();
        self.nodes.push(Node { kind, pos });
        i
    }
    fn error(
        &self,
        message: impl AsRef<str>,
        pos: usize,
        span: usize,
        hint: Option<&str>,
    ) -> EngineError {
        diagnostic(message.as_ref(), self.source, pos, span, hint)
    }
    fn required(&mut self, kind: &str, message: &str) -> Result<()> {
        if !self.has(kind) {
            return Err(self.error(message, self.peek().pos, 1, None));
        }
        self.take();
        Ok(())
    }
    fn boolean(&mut self, is_or: bool) -> Result<usize> {
        let first = if is_or {
            self.boolean(false)?
        } else {
            self.not()?
        };
        let mut ids = vec![first];
        let kind = if is_or { "or" } else { "and" };
        while self.has(kind) {
            self.take();
            ids.push(if is_or {
                self.boolean(false)?
            } else {
                self.not()?
            });
        }
        if ids.len() == 1 {
            Ok(first)
        } else {
            let pos = self.nodes[first].pos;
            Ok(self.node(
                if is_or {
                    Kind::Unsupported
                } else {
                    Kind::And(ids)
                },
                pos,
            ))
        }
    }
    fn not(&mut self) -> Result<usize> {
        if self.has("not") {
            let tok = self.take();
            self.enter()?;
            let inner = self.not();
            self.depth -= 1;
            inner?;
            Ok(self.node(Kind::Unsupported, tok.pos))
        } else {
            self.cmp()
        }
    }
    fn enter(&mut self) -> Result<()> {
        // 明确限制嵌套深度，服务输入不能通过递归使宿主栈溢出；长平面 and 链不受此限制。
        self.depth += 1;
        if self.depth > 128 {
            return Err(EngineError::invalid("maximum recursion depth exceeded"));
        }
        Ok(())
    }
    fn cmp(&mut self) -> Result<usize> {
        let mut left = self.arith(false)?;
        loop {
            if matches!(
                self.peek().kind.as_str(),
                "==" | "!=" | "<" | "<=" | ">" | ">="
            ) {
                let tok = self.take();
                let right = self.arith(false)?;
                left = self.node(Kind::Cmp(tok.kind, left, right), tok.pos);
                continue;
            }
            let negate =
                self.has("not") && self.tokens.get(self.at + 1).is_some_and(|t| t.kind == "in");
            if self.has("in") || negate {
                if negate {
                    self.take();
                }
                let tok = self.take();
                if !matches!(self.nodes[left].kind, Kind::Field(_)) {
                    return Err(self.error(
                        "left side of 'in' must be a field reference",
                        tok.pos,
                        1,
                        Some("example: field in [1, 2, 3]"),
                    ));
                }
                if !self.has("[") {
                    return Err(self.error(
                        format!(
                            "expected '[' after 'in', got {}",
                            py_repr(&self.peek().text)
                        ),
                        self.peek().pos,
                        1,
                        Some("'in' must be followed by a literal list, e.g. [1, 2, 3]"),
                    ));
                }
                let list = self.list()?;
                return Ok(self.node(
                    Kind::In {
                        field: left,
                        list,
                        negate,
                    },
                    tok.pos,
                ));
            }
            if self.has("like") {
                let tok = self.take();
                if !self.has("string") {
                    return Err(self.error(
                        "LIKE requires a string literal pattern",
                        self.peek().pos,
                        1,
                        Some("example: title like 'AI%'"),
                    ));
                }
                self.take();
                return Ok(self.node(Kind::Unsupported, tok.pos));
            }
            if self.has("is") {
                let tok = self.take();
                if !matches!(
                    self.nodes[left].kind,
                    Kind::Field(_) | Kind::Meta(_) | Kind::Path
                ) {
                    return Err(self.error(
                        "left side of 'is null' must be a field reference or JSON path",
                        tok.pos,
                        1,
                        Some("examples: title is null, metadata[\"key\"] is null"),
                    ));
                }
                let negate = self.has("not");
                if negate {
                    self.take();
                }
                if !self.has("null") {
                    return Err(self.error(
                        format!(
                            "expected {} after 'is', got {}",
                            if negate { "'not null'" } else { "'null'" },
                            py_repr(&self.peek().text)
                        ),
                        self.peek().pos,
                        1,
                        None,
                    ));
                }
                self.take();
                return Ok(self.node(Kind::Unsupported, tok.pos));
            }
            break;
        }
        Ok(left)
    }
    fn arith(&mut self, mul: bool) -> Result<usize> {
        let mut left = if mul {
            self.unary()?
        } else {
            self.arith(true)?
        };
        while if mul {
            matches!(self.peek().kind.as_str(), "*" | "/")
        } else {
            matches!(self.peek().kind.as_str(), "+" | "-")
        } {
            let tok = self.take();
            if mul {
                self.unary()?;
            } else {
                self.arith(true)?;
            }
            left = self.node(Kind::Unsupported, tok.pos);
        }
        Ok(left)
    }
    fn unary(&mut self) -> Result<usize> {
        if self.has("-") {
            let tok = self.take();
            self.enter()?;
            let inner = self.unary();
            self.depth -= 1;
            let id = inner?;
            let kind = match &self.nodes[id].kind {
                Kind::Lit(lit @ (Literal::Int(_) | Literal::Float(_))) => {
                    Kind::Lit(lit.clone().negate())
                }
                _ => Kind::Unsupported,
            };
            Ok(self.node(kind, tok.pos))
        } else if self.has("exists") {
            let tok = self.take();
            let id = self.primary()?;
            if !matches!(
                self.nodes[id].kind,
                Kind::Field(_) | Kind::Meta(_) | Kind::Path
            ) {
                return Err(self.error(
                    "'exists' must be followed by a field reference or JSON path",
                    tok.pos,
                    1,
                    Some("example: exists metadata[\"key\"]"),
                ));
            }
            Ok(self.node(Kind::Unsupported, tok.pos))
        } else {
            self.primary()
        }
    }
    fn primary(&mut self) -> Result<usize> {
        self.enter()?;
        let result = self.primary_inner();
        self.depth -= 1;
        result
    }
    fn primary_inner(&mut self) -> Result<usize> {
        let tok = self.peek().clone();
        if let Some(lit) = tok.lit {
            self.take();
            return Ok(self.node(Kind::Lit(lit), tok.pos));
        }
        if matches!(tok.kind.as_str(), "iso" | "interval") {
            return self.temporal();
        }
        if tok.kind == "ident" {
            self.take();
            if self.has("(") {
                return self.function(tok);
            }
            let id = self.node(Kind::Field(tok.text), tok.pos);
            if self.has("[") {
                self.path()?;
                return Ok(self.node(Kind::Path, tok.pos));
            }
            return Ok(id);
        }
        if tok.kind == "$meta" {
            self.take();
            if !self.has("[") {
                return Err(self.error(
                    format!(
                        "expected '[' after '$meta', got {}",
                        py_repr(&self.peek().text)
                    ),
                    self.peek().pos,
                    1,
                    Some("example: $meta[\"category\"]"),
                ));
            }
            self.take();
            if !self.has("string") {
                return Err(self.error(
                    format!(
                        "$meta key must be a string literal, got {}",
                        py_repr(&self.peek().text)
                    ),
                    self.peek().pos,
                    1,
                    Some("example: $meta[\"category\"]"),
                ));
            }
            let key = self.take();
            if !self.has("]") {
                return Err(self.error(
                    format!(
                        "expected ']' after $meta key, got {}",
                        py_repr(&self.peek().text)
                    ),
                    self.peek().pos,
                    1,
                    None,
                ));
            }
            self.take();
            let nested = self.has("[");
            self.path()?;
            return Ok(self.node(
                if nested {
                    Kind::Path
                } else {
                    Kind::Meta(match key.lit {
                        Some(Literal::String(key)) => key,
                        _ => {
                            return Err(self.error(
                                "$meta key must be a string literal",
                                key.pos,
                                1,
                                None,
                            ));
                        }
                    })
                },
                tok.pos,
            ));
        }
        if tok.kind == "(" {
            self.take();
            self.enter()?;
            let inner = self.boolean(true);
            self.depth -= 1;
            let inner = inner?;
            if !self.has(")") {
                return Err(self.error(
                    format!("expected ')' got {}", py_repr(&self.peek().text)),
                    self.peek().pos,
                    1,
                    None,
                ));
            }
            self.take();
            return Ok(inner);
        }
        if tok.kind == "eof" {
            return Err(self.error(
                "unexpected end of expression",
                tok.pos,
                1,
                Some("expression cannot be empty"),
            ));
        }
        Err(self.error(
            format!("unexpected token {}", py_repr(&tok.text)),
            tok.pos,
            tok.text.chars().count(),
            Some("expected literal, identifier, or '('"),
        ))
    }
    fn path(&mut self) -> Result<()> {
        while self.has("[") {
            self.take();
            if self.has("-") {
                return Err(self.error(
                    "array index must be non-negative",
                    self.peek().pos,
                    1,
                    None,
                ));
            }
            if !matches!(self.peek().kind.as_str(), "string" | "int") {
                return Err(self.error(
                    "field access: expected string key or non-negative integer index",
                    self.peek().pos,
                    1,
                    None,
                ));
            }
            self.take();
            self.required("]", "field access: expected ']'")?;
        }
        Ok(())
    }
    fn list(&mut self) -> Result<usize> {
        let tok = self.take();
        let mut elements = Vec::new();
        if !self.has("]") {
            loop {
                let next = self.peek().clone();
                let id = if self.has("-") {
                    self.take();
                    if !matches!(self.peek().kind.as_str(), "int" | "float") {
                        return Err(self.error(
                            "unary '-' in list must be followed by a numeric literal",
                            next.pos,
                            1,
                            None,
                        ));
                    }
                    let inner = self.take();
                    let lit = inner
                        .lit
                        .ok_or_else(|| EngineError::invalid("numeric token has no value"))?;
                    self.node(Kind::Lit(lit.negate()), next.pos)
                } else if matches!(
                    self.peek().kind.as_str(),
                    "int" | "float" | "string" | "bool" | "iso"
                ) {
                    self.primary()?
                } else {
                    return Err(self.error(
                        format!(
                            "list elements must be literals, got {}",
                            py_repr(&next.text)
                        ),
                        next.pos,
                        next.text.chars().count(),
                        None,
                    ));
                };
                elements.push(id);
                if !self.has(",") {
                    break;
                }
                self.take();
                if self.has("]") {
                    break;
                }
            }
        }
        if !self.has("]") {
            return Err(self.error(
                format!("expected ']' or ',' got {}", py_repr(&self.peek().text)),
                self.peek().pos,
                1,
                None,
            ));
        }
        self.take();
        Ok(self.node(Kind::List(elements), tok.pos))
    }
    fn temporal(&mut self) -> Result<usize> {
        let tok = self.take();
        let value = self.peek().clone();
        let Some(Literal::String(s)) = value.lit else {
            return Err(self.error(
                if tok.kind == "iso" {
                    "ISO timestamp literal requires a string"
                } else {
                    "INTERVAL literal requires a string"
                },
                value.pos,
                1,
                Some(if tok.kind == "iso" {
                    "example: ISO '2025-01-01T00:00:00Z'"
                } else {
                    "example: INTERVAL 'P1D'"
                }),
            ));
        };
        self.take();
        super::temporal::validate(&s, tok.kind == "iso")
            .map_err(|m| self.error(m, value.pos, 1, None))?;
        Ok(self.node(Kind::Lit(Literal::Temporal), tok.pos))
    }
    fn function(&mut self, tok: Token) -> Result<usize> {
        let name = tok.text.to_lowercase();
        let display = if matches!(name.as_str(), "array_length" | "text_match") {
            name.as_str()
        } else {
            tok.text.as_str()
        };
        let geometry = matches!(
            name.as_str(),
            "geometry_contains"
                | "geometry_within"
                | "geometry_intersects"
                | "st_contains"
                | "st_within"
                | "st_intersects"
                | "st_isvalid"
                | "geometry_isvalid"
                | "st_dwithin"
                | "geometry_dwithin"
        );
        let contains = matches!(
            name.as_str(),
            "array_contains"
                | "json_contains"
                | "array_contains_all"
                | "json_contains_all"
                | "array_contains_any"
                | "json_contains_any"
        );
        if !geometry && !contains && name != "array_length" && name != "text_match" {
            return Err(self.error(format!("unknown function {}",py_repr(&tok.text)),tok.pos,tok.text.chars().count(),Some("supported: text_match, geometry_contains, geometry_within, geometry_intersects, ST_CONTAINS, ST_WITHIN, ST_INTERSECTS, ST_ISVALID, ST_DWITHIN, array_contains, json_contains, array_contains_all, array_contains_any, array_length")));
        }
        self.take();
        if !self.has("ident") {
            let message = if geometry {
                format!("{}: expected geometry field name", tok.text)
            } else if name == "text_match" {
                format!(
                    "text_match: expected field name, got {}",
                    py_repr(&self.peek().text)
                )
            } else {
                format!("{display}: expected field name")
            };
            return Err(self.error(message, self.peek().pos, 1, None));
        }
        self.take();
        if contains {
            self.path()?;
        }
        if name == "array_length" || matches!(name.as_str(), "st_isvalid" | "geometry_isvalid") {
            self.required(")", &format!("{display}: expected ')'"))?;
            return Ok(self.node(Kind::Unsupported, tok.pos));
        }
        let comma = if name == "text_match" {
            "text_match: expected ',' after field name".into()
        } else if geometry {
            format!("{}: expected ',' after field name", tok.text)
        } else {
            format!("{}: expected ','", tok.text)
        };
        self.required(",", &comma)?;
        if contains {
            if self.has("[") {
                self.list()?;
            } else {
                self.primary()?;
            }
        } else {
            if !self.has("string") {
                let m = if geometry {
                    format!("{}: expected WKT string literal", tok.text)
                } else {
                    format!(
                        "text_match: expected query string, got {}",
                        py_repr(&self.peek().text)
                    )
                };
                return Err(self.error(m, self.peek().pos, 1, None));
            }
            self.take();
            if matches!(name.as_str(), "st_dwithin" | "geometry_dwithin") {
                self.required(",", &format!("{}: expected ',' after WKT string", tok.text))?;
                let d = self.unary()?;
                match &self.nodes[d].kind {
                    Kind::Lit(Literal::Int(v)) if !v.starts_with('-') => {}
                    Kind::Lit(Literal::Float(v)) if *v >= 0.0 => {}
                    Kind::Lit(Literal::Int(_) | Literal::Float(_)) => {
                        return Err(self.error(
                            format!("{}: distance must be non-negative", tok.text),
                            self.nodes[d].pos,
                            1,
                            None,
                        ));
                    }
                    _ => {
                        return Err(self.error(
                            format!("{}: expected numeric distance literal", tok.text),
                            self.nodes[d].pos,
                            1,
                            None,
                        ));
                    }
                }
            }
        }
        self.required(")", &format!("{display}: expected ')'"))?;
        Ok(self.node(Kind::Unsupported, tok.pos))
    }
}

pub(super) fn parse(source: &str) -> Result<Ast> {
    let tokens = lex(source)?;
    let mut parser = Parser {
        tokens,
        at: 0,
        source,
        nodes: Vec::new(),
        depth: 0,
    };
    let root = parser.boolean(true)?;
    if !parser.has("eof") {
        let tok = parser.peek();
        return Err(parser.error(
            format!("unexpected token {}", py_repr(&tok.text)),
            tok.pos,
            tok.text.chars().count(),
            Some("expected end of expression"),
        ));
    }
    Ok(Ast {
        nodes: parser.nodes,
        root,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn reference_parser_acceptance_and_diagnostics_match_the_generated_corpus() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/filter_parse_v321.json"))
                .unwrap();
        let mut mismatches = Vec::new();
        for case in cases["cases"].as_array().unwrap() {
            let source = case["source"].as_str().unwrap();
            let actual = parse(source).err().map(|error| error.to_string());
            let expected = case["error"].as_str().map(str::to_owned);
            if actual != expected {
                mismatches.push(format!("{source:?}: {actual:?} != {expected:?}"));
            }
        }
        assert!(
            mismatches.is_empty(),
            "{} mismatches: {}",
            mismatches.len(),
            mismatches.join("\n")
        );
    }
    #[test]
    fn deeply_nested_expressions_report_an_error_without_overflowing_the_host_stack() {
        let source = format!("{}id == 1{}", "(".repeat(1000), ")".repeat(1000));
        assert!(matches!(
            parse(&source),
            Err(EngineError::InvalidArgument(_))
        ));
        let source = format!(
            "{}id{}",
            "array_contains(id,".repeat(1000),
            ")".repeat(1000)
        );
        assert!(parse(&source).is_err());
    }
}
