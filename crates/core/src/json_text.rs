//! JSON 文本解析的唯一入口。
//!
//! 开启 `arbitrary_precision` 后，serde_json 把数字编码成只含 `$serde_json::private::Number`
//! 一个键的 map 交给 visitor，而它自带的 `Value` 反序列化只看对象的首键是否为该名字。用户数据里
//! 这个键是普通字符串（3.x 用 `json.loads`），首键碰巧是它的对象会被改成数字或解析失败。这里的
//! visitor 只在三个条件同时成立时才当作数字：首键是该名字、键不经 `visit_string`（经 `visit_str` /
//! `visit_borrowed_str`）送来、值经 `visit_string` 送来。serde_json 的数字键固定由 `visit_borrowed_str` 送出、数字文本固定由
//! `visit_string` 送出；用户的键和值在文本里不会经 `visit_string`，在 `from_value` 里键经
//! `visit_string`，在 `&Value` 里值经 `visit_borrowed_str`，三种来源都能区分。
//!
//! 约束：类型化解析的结构里，每个 `Value` / `Map<String, Value>` 字段都必须用本模块的
//! `deserialize_with` 函数，否则仍走 serde_json 自带的 `Value` 反序列化。直接调用
//! `serde_json::from_*` 与 `Deserializer::from_*` 由 clippy.toml 禁止，只有本模块例外。
#![allow(clippy::disallowed_methods)]

use std::fmt;

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Number, Value};

const NUMBER_TOKEN: &str = "$serde_json::private::Number";

/// 把 JSON 文本解析为 `Value`；与 `serde_json::from_slice::<Value>` 相同，只是不误认私有键。
pub fn value(bytes: &[u8]) -> serde_json::Result<Value> {
    finish(serde_json::Deserializer::from_slice(bytes))
}

/// 同 [`value`]，输入为 `&str`。
pub fn value_str(text: &str) -> serde_json::Result<Value> {
    finish(serde_json::Deserializer::from_str(text))
}

fn finish<'de, R: serde_json::de::Read<'de>>(
    mut de: serde_json::Deserializer<R>,
) -> serde_json::Result<Value> {
    let value = ValueSeed.deserialize(&mut de)?;
    de.end()?;
    Ok(value)
}

/// 类型化解析；`T` 的 `Value` 字段须带本模块的 `deserialize_with`（见模块说明）。
pub fn parse<T: DeserializeOwned + 'static>(bytes: &[u8]) -> serde_json::Result<T> {
    debug_assert_typed::<T>();
    serde_json::from_slice(bytes)
}

/// 同 [`parse`]，输入为 `&str`。
pub fn parse_str<T: DeserializeOwned + 'static>(text: &str) -> serde_json::Result<T> {
    debug_assert_typed::<T>();
    serde_json::from_str(text)
}

/// `Value` 到类型化结构；`T` 的约束同 [`parse`]。
pub fn from_value<T: DeserializeOwned + 'static>(value: Value) -> serde_json::Result<T> {
    debug_assert_typed::<T>();
    serde_json::from_value(value)
}

/// 类型化入口内部用的是 serde_json 自带的 `Value` 反序列化；这几种裸 `Value` 容器必须改用 [`value`]，
/// 否则又回到误认私有键的路径。
fn debug_assert_typed<T: 'static>() {
    use std::any::TypeId;
    let id = TypeId::of::<T>();
    debug_assert!(
        ![
            TypeId::of::<Value>(),
            TypeId::of::<Map<String, Value>>(),
            TypeId::of::<Vec<Value>>(),
            TypeId::of::<Option<Value>>(),
        ]
        .contains(&id),
        "json_text: parse {} with json_text::value instead",
        std::any::type_name::<T>()
    );
}

/// `#[serde(deserialize_with)]`：`Option<Value>` 字段。`null` 与缺省一样为 `None`，同 serde 的 `Option`。
pub fn option_value<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    struct OptionVisitor;

    impl<'de> Visitor<'de> for OptionVisitor {
        type Value = Option<Value>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("option")
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D: Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            ValueSeed.deserialize(deserializer).map(Some)
        }
    }

    deserializer.deserialize_option(OptionVisitor)
}

/// `#[serde(deserialize_with)]`：`Map<String, Value>` 字段。键按文件顺序保留，重复键保留首次的位置、取最后的值，
/// 同 serde_json 自带的 `Map`。
pub fn map_value<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Map<String, Value>, D::Error> {
    struct MapVisitor;

    impl<'de> Visitor<'de> for MapVisitor {
        type Value = Map<String, Value>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a map")
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Map::new())
        }

        fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
            let mut values = Map::new();
            while let Some(key) = access.next_key::<String>()? {
                values.insert(key, access.next_value_seed(ValueSeed)?);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_map(MapVisitor)
}

struct ValueSeed;

impl<'de> DeserializeSeed<'de> for ValueSeed {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(ValueVisitor)
    }
}

/// 除 `visit_map` 外逐项同 serde_json 自带的 `ValueVisitor`。
struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any valid JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_i128<E: de::Error>(self, value: i128) -> Result<Value, E> {
        Number::deserialize(de::value::I128Deserializer::new(value)).map(Value::Number)
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u128<E: de::Error>(self, value: u128) -> Result<Value, E> {
        Number::deserialize(de::value::U128Deserializer::new(value)).map(Value::Number)
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Ok(Number::from_f64(value).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        ValueSeed.deserialize(deserializer)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = access.next_element_seed(ValueSeed)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let Some(first) = access.next_key_seed(KeySeed)? else {
            return Ok(Value::Object(Map::new()));
        };
        let (first, first_value) = match first {
            Key::BorrowedToken => match access.next_value_seed(TokenValueSeed)? {
                TokenValue::Number(text) => {
                    return text.parse().map(Value::Number).map_err(de::Error::custom);
                }
                TokenValue::Value(value) => (NUMBER_TOKEN.to_owned(), value),
            },
            Key::Other(text) => (text, access.next_value_seed(ValueSeed)?),
        };
        let mut values = Map::new();
        values.insert(first, first_value);
        while let Some(key) = access.next_key::<String>()? {
            values.insert(key, access.next_value_seed(ValueSeed)?);
        }
        Ok(Value::Object(values))
    }
}

enum Key {
    /// 私有键，经 `visit_str` / `visit_borrowed_str` 送来；serde_json 的数字键属于这一类。
    /// 不复制键文本，每个数字都会经过这里。
    BorrowedToken,
    Other(String),
}

struct KeySeed;

impl<'de> DeserializeSeed<'de> for KeySeed {
    type Value = Key;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Key, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl Visitor<'_> for KeySeed {
    type Value = Key;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a string key")
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Key, E> {
        Ok(if text == NUMBER_TOKEN {
            Key::BorrowedToken
        } else {
            Key::Other(text.to_owned())
        })
    }

    fn visit_string<E: de::Error>(self, text: String) -> Result<Key, E> {
        Ok(Key::Other(text))
    }
}

enum TokenValue {
    /// 经 `visit_string` 送来的数字文本。
    Number(String),
    Value(Value),
}

struct TokenValueSeed;

impl<'de> DeserializeSeed<'de> for TokenValueSeed {
    type Value = TokenValue;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<TokenValue, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for TokenValueSeed {
    type Value = TokenValue;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any valid JSON value")
    }

    fn visit_string<E: de::Error>(self, text: String) -> Result<TokenValue, E> {
        Ok(TokenValue::Number(text))
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<TokenValue, E> {
        ValueVisitor.visit_str(text).map(TokenValue::Value)
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<TokenValue, E> {
        ValueVisitor.visit_bool(value).map(TokenValue::Value)
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<TokenValue, E> {
        ValueVisitor.visit_i64(value).map(TokenValue::Value)
    }

    fn visit_i128<E: de::Error>(self, value: i128) -> Result<TokenValue, E> {
        ValueVisitor.visit_i128(value).map(TokenValue::Value)
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<TokenValue, E> {
        ValueVisitor.visit_u64(value).map(TokenValue::Value)
    }

    fn visit_u128<E: de::Error>(self, value: u128) -> Result<TokenValue, E> {
        ValueVisitor.visit_u128(value).map(TokenValue::Value)
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<TokenValue, E> {
        ValueVisitor.visit_f64(value).map(TokenValue::Value)
    }

    fn visit_none<E: de::Error>(self) -> Result<TokenValue, E> {
        ValueVisitor.visit_none().map(TokenValue::Value)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<TokenValue, D::Error> {
        ValueVisitor.visit_some(deserializer).map(TokenValue::Value)
    }

    fn visit_unit<E: de::Error>(self) -> Result<TokenValue, E> {
        ValueVisitor.visit_unit().map(TokenValue::Value)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, access: A) -> Result<TokenValue, A::Error> {
        ValueVisitor.visit_seq(access).map(TokenValue::Value)
    }

    fn visit_map<A: MapAccess<'de>>(self, access: A) -> Result<TokenValue, A::Error> {
        ValueVisitor.visit_map(access).map(TokenValue::Value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    const T: &str = NUMBER_TOKEN;

    fn obj(pairs: &[(&str, Value)]) -> Value {
        Value::Object(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        )
    }

    fn text(s: &str) -> Value {
        Value::String(s.to_owned())
    }

    /// 不含私有键的输入：结果、键序、数字原文与出错文案都与 serde_json 自带的实现相同。
    #[test]
    fn matches_builtin_without_token() {
        let deep_ok = format!("{}1{}", "[".repeat(127), "]".repeat(127));
        let deep_err = format!("{}1{}", "[".repeat(128), "]".repeat(128));
        let deep_obj = format!("{}1{}", "{\"a\":".repeat(128), "}".repeat(128));
        let cases = [
            "0",
            "-0",
            "1",
            "-1",
            "1.5",
            "-1.5e-3",
            "1E2",
            "1e999",
            "-1e999",
            "0.1",
            "18446744073709551615",
            "18446744073709551616",
            "-9223372036854775809",
            "184467440737095516160",
            "123456789012345678901234567890.5",
            "true",
            "null",
            r#""ab\n""#,
            r#"{"b":1,"a":[1,{"c":null}],"b":2}"#,
            r#"{"z":{"y":1},"a":2}"#,
            "[]",
            "{}",
            " [1 , 2] ",
            "[1,]",
            "{\"a\":1,}",
            "01",
            "[1] x",
            "{\"a\" 1}",
            "",
            r#""\ud800""#,
            "NaN",
            deep_ok.as_str(),
            deep_err.as_str(),
            deep_obj.as_str(),
        ];
        for case in cases {
            let ours = value_str(case).map_err(|e| e.to_string());
            let builtin = serde_json::from_str::<Value>(case).map_err(|e| e.to_string());
            assert_eq!(ours, builtin, "{case}");
            if let (Ok(ours), Ok(builtin)) = (&ours, &builtin) {
                assert_eq!(
                    serde_json::to_string(ours).unwrap(),
                    serde_json::to_string(builtin).unwrap()
                );
            }
            assert_eq!(
                value(case.as_bytes()).map_err(|e| e.to_string()),
                builtin,
                "{case}"
            );
        }
    }

    fn token_cases() -> Vec<(String, Value)> {
        vec![
            (format!(r#"{{"{T}":"1"}}"#), obj(&[(T, text("1"))])),
            (format!(r#"{{"{T}":"abc"}}"#), obj(&[(T, text("abc"))])),
            (format!(r#"{{"{T}":1}}"#), obj(&[(T, Value::from(1))])),
            (
                format!(r#"{{"{T}":"1","b":2}}"#),
                obj(&[(T, text("1")), ("b", Value::from(2))]),
            ),
            (
                format!(r#"{{"b":2,"{T}":"1"}}"#),
                obj(&[("b", Value::from(2)), (T, text("1"))]),
            ),
            (
                format!(r#"{{"a":{{"{T}":"1"}}}}"#),
                obj(&[("a", obj(&[(T, text("1"))]))]),
            ),
            (
                format!(r#"[{{"{T}":"1.5e3"}}]"#),
                Value::Array(vec![obj(&[(T, text("1.5e3"))])]),
            ),
            (
                format!(r#"{{"{T}":{{"{T}":"2"}}}}"#),
                obj(&[(T, obj(&[(T, text("2"))]))]),
            ),
            // 转义写法：值经 `visit_str`（非借用）送来，键的文本与私有键相同。
            (
                format!(r#"{{"{T}":"{}u0031"}}"#, '\\'),
                obj(&[(T, text("1"))]),
            ),
            (
                format!(r#"{{"{}u0024serde_json::private::Number":"1"}}"#, '\\'),
                obj(&[(T, text("1"))]),
            ),
            (
                format!(r#"{{"{T}":"1","{T}":"2"}}"#),
                obj(&[(T, text("2"))]),
            ),
            (format!(r#"{{"{T}":null}}"#), obj(&[(T, Value::Null)])),
            (
                format!(r#"{{"{T}":[1.5]}}"#),
                obj(&[(T, Value::Array(vec![Value::from(1.5)]))]),
            ),
        ]
    }

    #[test]
    fn keeps_token_objects_from_text() {
        let escaped = token_cases()
            .iter()
            .filter(|(input, _)| input.contains('\\'))
            .count();
        assert_eq!(escaped, 2, "escaped inputs");
        for (input, expected) in token_cases() {
            assert_eq!(value_str(&input).unwrap(), expected, "{input}");
            assert_eq!(value(input.as_bytes()).unwrap(), expected, "{input}");
            let mut de = serde_json::Deserializer::from_reader(input.as_bytes());
            let read = ValueSeed.deserialize(&mut de).unwrap();
            de.end().unwrap();
            assert_eq!(read, expected, "from_reader {input}");
        }
    }

    /// `Value` 与 `&Value` 作为反序列化器时，私有键对象与宽整数、浮点都原样保留。
    #[test]
    fn keeps_token_objects_and_numbers_from_values() {
        let mut cases = token_cases();
        for number in [
            "184467440737095516160",
            "-184467440737095516160",
            "1.5",
            "1e999",
            "-0",
            "7",
        ] {
            cases.push((number.to_owned(), value_str(number).unwrap()));
        }
        for (input, expected) in cases {
            assert_eq!(
                ValueSeed.deserialize(expected.clone()).unwrap(),
                expected,
                "from_value {input}"
            );
            assert_eq!(
                ValueSeed.deserialize(&expected).unwrap(),
                expected,
                "&Value {input}"
            );
        }
        assert!(matches!(
            value_str("184467440737095516160").unwrap(),
            Value::Number(_)
        ));
    }

    #[derive(Deserialize)]
    struct Fields {
        #[serde(default, deserialize_with = "option_value")]
        option: Option<Value>,
        #[serde(default, deserialize_with = "map_value")]
        map: Map<String, Value>,
    }

    #[test]
    fn field_helpers_keep_token_objects() {
        for (input, expected) in token_cases() {
            let doc = format!(r#"{{"option":{input},"map":{{"{T}":{input},"a":{input}}}}}"#);
            let fields: Fields = parse_str(&doc).unwrap();
            assert_eq!(fields.option.as_ref(), Some(&expected), "{doc}");
            assert_eq!(
                fields.map,
                *obj(&[(T, expected.clone()), ("a", expected.clone())])
                    .as_object()
                    .unwrap()
            );
            let fields: Fields = from_value(value_str(&doc).unwrap()).unwrap();
            assert_eq!(fields.option, Some(expected), "{doc}");
        }
        let fields: Fields = parse_str(r#"{"option":null,"map":{"b":1,"a":2,"b":3}}"#).unwrap();
        assert_eq!(fields.option, None);
        assert_eq!(fields.map.keys().collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(fields.map["b"], 3);
        let fields: Fields = parse_str("{}").unwrap();
        assert_eq!((fields.option, fields.map.len()), (None, 0));
        assert_eq!(
            parse_str::<Fields>(r#"{"map":1}"#)
                .map(|_| ())
                .unwrap_err()
                .to_string(),
            "invalid type: integer `1`, expected a map at line 1 column 8"
        );
    }

    /// 防回归：serde_json 自带的 `Value` 仍会误认私有键；若上游改了行为，本模块的判别依据需要重新核对。
    #[test]
    fn builtin_still_misreads_token() {
        let input = format!(r#"{{"{T}":"1"}}"#);
        assert_eq!(
            serde_json::from_str::<Value>(&input).unwrap(),
            Value::from(1)
        );
    }
}
