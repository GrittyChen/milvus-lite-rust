//! 路径字符串按 CPython json.loads 解码；平面节点使深层值的解析与析构不占宿主调用栈。

use crate::error::{EngineError, Result};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Default)]
pub(super) struct JsonObject {
    entries: Vec<(Vec<u32>, usize)>,
    positions: HashMap<Vec<u32>, usize>,
}
impl JsonObject {
    fn insert(&mut self, key: Vec<u32>, value: usize) {
        // Python 保留首次插入的键序，重复键只替换值。
        let index = *self
            .positions
            .entry(key.clone())
            .or_insert(self.entries.len());
        if index == self.entries.len() {
            self.entries.push((key, value));
        } else {
            self.entries[index].1 = value;
        }
    }
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(super) fn keys(&self) -> impl Iterator<Item = &Vec<u32>> {
        self.entries.iter().map(|(key, _)| key)
    }
    pub(super) fn get(&self, key: &[u32]) -> Option<usize> {
        self.positions.get(key).map(|&index| self.entries[index].1)
    }
}

#[derive(Debug)]
pub(super) enum JsonNode {
    Null,
    Bool(bool),
    Int(String),
    Float(f64, bool),
    String(Vec<u32>),
    List(Vec<usize>),
    Object(JsonObject),
}
#[derive(Debug)]
pub(super) struct Document {
    pub(super) nodes: Vec<JsonNode>,
    pub(super) root: usize,
}
enum Phase {
    Array(bool),
    Object(bool),
    After,
}
struct Frame {
    id: usize,
    phase: Phase,
}
struct Decoder<'a> {
    input: &'a [u32],
    at: usize,
    nodes: Vec<JsonNode>,
}

impl Decoder<'_> {
    fn byte(&self) -> Option<u8> {
        self.input
            .get(self.at)
            .map(|code| u8::try_from(*code).unwrap_or(255))
    }
    fn white(&mut self) {
        while self
            .byte()
            .is_some_and(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.at += 1;
        }
    }
    fn hex(&mut self) -> Option<u32> {
        let mut value = 0;
        for _ in 0..4 {
            let d = char::from(self.byte()?).to_digit(16)?;
            value = value * 16 + d;
            self.at += 1;
        }
        Some(value)
    }
    fn string(&mut self) -> Option<Vec<u32>> {
        if self.byte() != Some(b'"') {
            return None;
        }
        self.at += 1;
        let mut out = Vec::new();
        loop {
            match self.byte()? {
                b'"' => {
                    self.at += 1;
                    return Some(out);
                }
                b'\\' => {
                    self.at += 1;
                    let escape = self.byte()?;
                    self.at += 1;
                    let code = match escape {
                        b'"' => 34,
                        b'\\' => 92,
                        b'/' => 47,
                        b'b' => 8,
                        b'f' => 12,
                        b'n' => 10,
                        b'r' => 13,
                        b't' => 9,
                        b'u' => {
                            let first = self.hex()?;
                            if (0xd800..=0xdbff).contains(&first)
                                && self.input[self.at..].starts_with(&[92, 117])
                            {
                                let saved = self.at;
                                self.at += 2;
                                if let Some(second) = self.hex() {
                                    if (0xdc00..=0xdfff).contains(&second) {
                                        0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
                                    } else {
                                        self.at = saved;
                                        first
                                    }
                                } else {
                                    self.at = saved;
                                    first
                                }
                            } else {
                                first
                            }
                        }
                        _ => return None,
                    };
                    out.push(code);
                }
                0..=31 => return None,
                _ => {
                    out.push(*self.input.get(self.at)?);
                    self.at += 1;
                }
            }
        }
    }
    fn atom(&mut self, depth: usize) -> Result<Option<(usize, Option<Phase>)>> {
        self.white();
        let Some(byte) = self.byte() else {
            return Ok(None);
        };
        let (node, phase) = match byte {
            b'[' | b'{' => {
                if depth >= 9997 {
                    return Err(EngineError::invalid(format!(
                        "maximum recursion depth exceeded while decoding a JSON {} from a unicode string",
                        if byte == b'[' { "array" } else { "object" }
                    )));
                }
                self.at += 1;
                if byte == b'[' {
                    (JsonNode::List(Vec::new()), Some(Phase::Array(true)))
                } else {
                    (
                        JsonNode::Object(JsonObject::default()),
                        Some(Phase::Object(true)),
                    )
                }
            }
            b'"' => {
                let Some(text) = self.string() else {
                    return Ok(None);
                };
                (JsonNode::String(text), None)
            }
            _ => {
                let mut keyword = None;
                for (word, node) in [
                    ("null", JsonNode::Null),
                    ("true", JsonNode::Bool(true)),
                    ("false", JsonNode::Bool(false)),
                    ("NaN", JsonNode::Float(f64::NAN, true)),
                    ("Infinity", JsonNode::Float(f64::INFINITY, false)),
                    ("-Infinity", JsonNode::Float(f64::NEG_INFINITY, false)),
                ] {
                    if self.input[self.at..]
                        .iter()
                        .copied()
                        .take(word.len())
                        .eq(word.bytes().map(u32::from))
                    {
                        self.at += word.len();
                        keyword = Some(node);
                        break;
                    }
                }
                if let Some(node) = keyword {
                    (node, None)
                } else {
                    let start = self.at;
                    if self.byte() == Some(b'-') {
                        self.at += 1;
                    }
                    match self.byte() {
                        Some(b'0') => self.at += 1,
                        Some(b'1'..=b'9') => {
                            while self.byte().is_some_and(|b| b.is_ascii_digit()) {
                                self.at += 1;
                            }
                        }
                        _ => return Ok(None),
                    }
                    let mut float = false;
                    if self.byte() == Some(b'.') {
                        float = true;
                        self.at += 1;
                        let digits = self.at;
                        while self.byte().is_some_and(|b| b.is_ascii_digit()) {
                            self.at += 1;
                        }
                        if self.at == digits {
                            return Ok(None);
                        }
                    }
                    if matches!(self.byte(), Some(b'e' | b'E')) {
                        float = true;
                        self.at += 1;
                        if matches!(self.byte(), Some(b'+' | b'-')) {
                            self.at += 1;
                        }
                        let digits = self.at;
                        while self.byte().is_some_and(|b| b.is_ascii_digit()) {
                            self.at += 1;
                        }
                        if self.at == digits {
                            return Ok(None);
                        }
                    }
                    let Some(raw) = self.input[start..self.at]
                        .iter()
                        .copied()
                        .map(char::from_u32)
                        .collect::<Option<String>>()
                    else {
                        return Ok(None);
                    };
                    let node = if float {
                        let Ok(value) = raw.parse::<f64>() else {
                            return Ok(None);
                        };
                        JsonNode::Float(value, false)
                    } else {
                        if raw.trim_start_matches('-').len() > 4300 {
                            return Ok(None);
                        }
                        JsonNode::Int(raw.to_owned())
                    };
                    (node, None)
                }
            }
        };
        let id = self.nodes.len();
        self.nodes.push(node);
        Ok(Some((id, phase)))
    }
}

pub(super) fn decode(input: &[u32]) -> Result<Option<Arc<Document>>> {
    let mut decoder = Decoder {
        input,
        at: 0,
        nodes: Vec::new(),
    };
    let Some((root, phase)) = decoder.atom(0)? else {
        return Ok(None);
    };
    let mut stack = Vec::new();
    if let Some(phase) = phase {
        stack.push(Frame { id: root, phase });
    }
    while let Some(frame) = stack.last_mut() {
        decoder.white();
        let id = frame.id;
        let object = matches!(decoder.nodes[id], JsonNode::Object(_));
        let key = match frame.phase {
            Phase::After => {
                if decoder.byte() == Some(if object { b'}' } else { b']' }) {
                    decoder.at += 1;
                    stack.pop();
                    continue;
                }
                if decoder.byte() != Some(b',') {
                    return Ok(None);
                }
                decoder.at += 1;
                frame.phase = if object {
                    Phase::Object(false)
                } else {
                    Phase::Array(false)
                };
                continue;
            }
            Phase::Array(first) => {
                if first && decoder.byte() == Some(b']') {
                    decoder.at += 1;
                    stack.pop();
                    continue;
                }
                None
            }
            Phase::Object(first) => {
                if first && decoder.byte() == Some(b'}') {
                    decoder.at += 1;
                    stack.pop();
                    continue;
                }
                let Some(key) = decoder.string() else {
                    return Ok(None);
                };
                decoder.white();
                if decoder.byte() != Some(b':') {
                    return Ok(None);
                }
                decoder.at += 1;
                Some(key)
            }
        };
        frame.phase = Phase::After;
        let Some((child, phase)) = decoder.atom(stack.len())? else {
            return Ok(None);
        };
        match &mut decoder.nodes[id] {
            JsonNode::List(values) => values.push(child),
            JsonNode::Object(values) => {
                let Some(key) = key else {
                    return Err(EngineError::Storage("JSON object parser lost key".into()));
                };
                values.insert(key, child);
            }
            _ => {
                return Err(EngineError::Storage(
                    "JSON container parser lost parent".into(),
                ));
            }
        }
        if let Some(phase) = phase {
            stack.push(Frame { id: child, phase });
        }
    }
    decoder.white();
    Ok((decoder.at == input.len()).then(|| {
        Arc::new(Document {
            nodes: decoder.nodes,
            root,
        })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_object_keeps_key_order_and_last_duplicate_value() {
        let mut raw = String::from("{");
        for i in 0..10_000 {
            raw.push_str(&format!("\"key{i}\":{i},"));
        }
        raw.push_str("\"key0\":-1,\"\\ud800\":1,\"\\ud800\":2}");
        let doc = decode(&raw.chars().map(u32::from).collect::<Vec<_>>())
            .unwrap()
            .unwrap();
        let JsonNode::Object(object) = &doc.nodes[doc.root] else {
            panic!("object root");
        };
        assert_eq!(object.len(), 10_001);
        for (i, key) in object.keys().take(10_000).enumerate() {
            assert_eq!(
                *key,
                format!("key{i}").chars().map(u32::from).collect::<Vec<_>>()
            );
            let JsonNode::Int(value) = &doc.nodes[object.get(key).unwrap()] else {
                panic!("integer value");
            };
            assert_eq!(value, &if i == 0 { "-1".into() } else { i.to_string() });
        }
        assert_eq!(object.keys().last().unwrap(), &[0xd800]);
        assert!(matches!(&doc.nodes[object.get(&[0xd800]).unwrap()], JsonNode::Int(n) if n == "2"));
        assert!(object.get(&[0xdc00]).is_none());
    }
}

/// `analyzer_params` 文本的处理结果，判定规则见 [`classify_analyzer_params`]。
#[derive(Debug, PartialEq)]
pub enum AnalyzerParams {
    /// 3.x 当作没有设置：解析失败或者是假值。
    Unset,
    Keep(serde_json::Value),
    /// 3.x 能接受，但无法写成本实现重开时读得回的严格 JSON。
    Unrepresentable,
}

/// `schema.json` 的字段比 `analyzer_params` 多包三层，serde_json 的递归上限在文件里只留下 124 层。
const MAX_ANALYZER_PARAMS_DEPTH: usize = 124;
/// 开启 arbitrary_precision 后，serde_json 把只含这个键的对象解析成数字，会静默改值。
const SERDE_NUMBER_TOKEN: &str = "$serde_json::private::Number";

fn exceeds_strict_json(value: &serde_json::Value, depth: usize) -> bool {
    use serde_json::Value;
    match value {
        Value::Number(n) => {
            crate::pyjson::json_float_text(&n.to_string()).is_some_and(f64::is_infinite)
        }
        Value::Array(items) => {
            depth == MAX_ANALYZER_PARAMS_DEPTH
                || items.iter().any(|v| exceeds_strict_json(v, depth + 1))
        }
        Value::Object(entries) => {
            depth == MAX_ANALYZER_PARAMS_DEPTH
                || entries.values().any(|v| exceeds_strict_json(v, depth + 1))
        }
        _ => false,
    }
}

/// 3.x 的建表翻译：`json.loads` 成功且为真值才保存，`JSONDecodeError` / `ValueError` 当作没有设置。
/// 判定用 Python 语义的解码器，取值用 serde_json，两者的分歧都落在 [`AnalyzerParams::Unrepresentable`]。
pub fn classify_analyzer_params(text: &str) -> AnalyzerParams {
    let input: Vec<u32> = text.chars().map(u32::from).collect();
    let document = match decode(&input) {
        Ok(Some(document)) => document,
        Ok(None) => return AnalyzerParams::Unset,
        // 只有递归深度错误，3.x 没有捕获它。
        Err(_) => return AnalyzerParams::Unrepresentable,
    };
    let token: Vec<u32> = SERDE_NUMBER_TOKEN.chars().map(u32::from).collect();
    if document
        .nodes
        .iter()
        .any(|node| matches!(node, JsonNode::Object(object) if object.get(&token).is_some()))
    {
        return AnalyzerParams::Unrepresentable;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return AnalyzerParams::Unrepresentable;
    };
    if !crate::pyjson::python_truthy(&value) {
        AnalyzerParams::Unset
    } else if exceeds_strict_json(&value, 0) {
        AnalyzerParams::Unrepresentable
    } else {
        AnalyzerParams::Keep(value)
    }
}

#[cfg(test)]
mod analyzer_params_tests {
    use super::*;
    use crate::storage::schema_file::SchemaFile;
    use serde_json::json;

    fn nested(depth: usize, object_at: impl Fn(usize) -> bool, inner: &str) -> String {
        let mut text = inner.to_owned();
        for level in 0..depth {
            text = if object_at(level) {
                format!("{{\"k\":{text}}}")
            } else {
                format!("[{text}]")
            };
        }
        text
    }

    #[test]
    fn unset_when_python_rejects_or_value_is_falsy() {
        let wide = format!("{{\"a\":{}}}", "1".repeat(4301));
        for text in [
            "not json",
            "{\"a\":1} x",
            "{",
            &wide,
            "{}",
            "[]",
            "0",
            "-0",
            "0.0",
            "-0.0",
            "0e5",
            "1e-999",
            "-1e-999",
            "\"\"",
            "null",
            "false",
        ] {
            assert_eq!(
                classify_analyzer_params(text),
                AnalyzerParams::Unset,
                "{text}"
            );
        }
    }

    #[test]
    fn keeps_any_truthy_json_in_input_order() {
        let keep = |text| match classify_analyzer_params(text) {
            AnalyzerParams::Keep(value) => value,
            other => panic!("{text}: {other:?}"),
        };
        assert_eq!(keep("[1]"), json!([1]));
        assert_eq!(keep("\"x\""), json!("x"));
        assert_eq!(keep("true"), json!(true));
        assert_eq!(keep("5"), json!(5));
        let wide = "1".repeat(400);
        assert_eq!(keep(&wide).to_string(), wide);
        let duplicate = keep("{\"b\":1,\"a\":2,\"b\":3}");
        assert_eq!(
            crate::pyjson::dumps_unsorted(&duplicate),
            "{\"b\": 3, \"a\": 2}"
        );
        assert_eq!(
            crate::pyjson::dumps_unsorted(&keep("{\"z\":1.0,\"y\":\"中\",\"x\":[1e5,null]}")),
            "{\"z\": 1.0, \"y\": \"\\u4e2d\", \"x\": [100000.0, null]}"
        );
    }

    #[test]
    fn rejects_values_strict_json_cannot_hold() {
        for text in [
            "NaN",
            "Infinity",
            "-Infinity",
            "{\"x\": NaN}",
            "[Infinity]",
            "1e999",
            "{\"x\":-1e999}",
            "\"\\ud800\"",
            "{\"x\":\"\\ud800\"}",
            "{\"$serde_json::private::Number\":\"1\"}",
            "{\"x\":{\"$serde_json::private::Number\":\"1e5\"}}",
            "{\"$serde_json::private::Number\":2,\"y\":1}",
        ] {
            assert_eq!(
                classify_analyzer_params(text),
                AnalyzerParams::Unrepresentable,
                "{text}"
            );
        }
        assert_eq!(
            classify_analyzer_params(&"[".repeat(20000)),
            AnalyzerParams::Unrepresentable
        );
    }

    #[test]
    fn deep_values_are_kept_only_while_schema_json_can_reopen() {
        let shapes: [&dyn Fn(usize) -> bool; 3] = [&|_| false, &|_| true, &|level| level % 2 == 0];
        for shape in shapes {
            for inner in ["1", "[]", "{}"] {
                // 最内层的空容器也算一层。
                let extra = usize::from(inner != "1");
                let text = nested(124 - extra, shape, inner);
                let AnalyzerParams::Keep(value) = classify_analyzer_params(&text) else {
                    panic!("depth 124 rejected: {inner}");
                };
                let mut field =
                    crate::schema::FieldSchema::new("t", crate::schema::DataType::Int64);
                field.is_primary = true;
                field.analyzer_params = Some(value.clone());
                let mut vector =
                    crate::schema::FieldSchema::new("v", crate::schema::DataType::FloatVector);
                vector.dim = Some(2);
                let schema = crate::schema::CollectionSchema {
                    fields: vec![field, vector],
                    ..Default::default()
                };
                let bytes = SchemaFile::from_core("c", &schema).to_bytes().unwrap();
                let reopened = SchemaFile::parse(&bytes).unwrap().to_core().unwrap();
                assert_eq!(reopened.fields[0].analyzer_params, Some(value));
                assert_eq!(
                    classify_analyzer_params(&nested(125 - extra, shape, inner)),
                    AnalyzerParams::Unrepresentable
                );
            }
        }
    }

    #[test]
    fn syntax_errors_stay_unset_at_any_depth() {
        assert_eq!(
            classify_analyzer_params(&"[".repeat(5000)),
            AnalyzerParams::Unset
        );
    }
}
