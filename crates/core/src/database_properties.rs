//! 每个数据库 `database_properties.json` 的缓存与原子写入（0023 B「模块与数据结构」）。数据目录由 LOCK 独占，
//! 只有本进程写这个文件，加载后的缓存与磁盘一致；文件替换之后才更新缓存，替换之前失败时两者都不变。

use std::fs;
use std::path::Path;

use crate::error::{EngineError, Result};
use crate::storage::fs_commit::{self, ReplaceError, ReplacePoint};

const FILE: &str = "database_properties.json";

/// 保持顺序的属性：加载保持文件顺序，Alter 的新键追加、已有键原位替换，与 3.x 进程内的 dict 一致；
/// 落盘时按键序排序。
pub(crate) type Properties = Vec<(String, String)>;

/// 首次使用时读取；读取或解析失败不缓存，下次重试（3.x 的 `_database_properties` 同样只缓存成功的加载）。
pub(crate) fn load<'a>(
    cache: &'a mut Option<Properties>,
    root: &Path,
) -> Result<&'a mut Properties> {
    let properties = match cache.take() {
        Some(properties) => properties,
        None => read(root)?,
    };
    Ok(cache.insert(properties))
}

/// 3.x `_load_database_properties`：文件不存在或顶层不是对象时为空表；无法解析报错。值只接受字符串
/// （3.x 自己只写字符串，0023 B 登记）；`timezone` 的取值原样保留，不校验。
fn read(root: &Path) -> Result<Properties> {
    let bytes = match fs::read(root.join(FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Properties::new()),
        Err(error) => return Err(EngineError::Storage(error.to_string())),
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| EngineError::Storage(error.to_string()))?;
    let serde_json::Value::Object(map) = value else {
        return Ok(Properties::new());
    };
    map.into_iter()
        .map(|(key, value)| match value {
            serde_json::Value::String(value) => Ok((key, value)),
            _ => Err(EngineError::Storage(format!(
                "database property {key:?} in {FILE} is not a string"
            ))),
        })
        .collect()
}

/// 3.x `_normalize_database_properties` 之后的合并：先删 `delete_keys`（不存在的键忽略），再设 `properties`
/// （新键追加、已有键原位替换）。结果与现值相等（不计顺序）时返回 `None`，调用方不写文件。
pub(crate) fn merge(
    current: &Properties,
    properties: &[(String, String)],
    delete_keys: &[String],
) -> Option<Properties> {
    let mut next = current.clone();
    next.retain(|(key, _)| !delete_keys.contains(key));
    for (key, value) in properties {
        match next.iter_mut().find(|(existing, _)| existing == key) {
            Some(entry) => entry.1.clone_from(value),
            None => next.push((key.clone(), value.clone())),
        }
    }
    let mut a = current.clone();
    let mut b = next.clone();
    a.sort();
    b.sort();
    (a != b).then_some(next)
}

/// 与 3.x 写出的文件字节相同：键按码点递归排序，`indent=2`、`ensure_ascii=False`。
pub(crate) fn to_bytes(properties: &Properties) -> Vec<u8> {
    let map = properties
        .iter()
        .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
        .collect();
    crate::pyjson::dump_file(&serde_json::Value::Object(map)).into_bytes()
}

/// 写入 `database_properties.json`（[`fs_commit::replace_file`]），同步数据库根目录。
pub(crate) fn write(
    root: &Path,
    properties: &Properties,
    fault: impl Fn(ReplacePoint) -> std::io::Result<()>,
) -> std::result::Result<(), ReplaceError> {
    fs_commit::replace_file(root, FILE, &to_bytes(properties), fault)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, &str)]) -> Properties {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn read_follows_the_python_loader_and_keeps_file_order() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(FILE);
        let mut cache = None;
        assert!(load(&mut cache, root.path()).unwrap().is_empty());
        // 已缓存：本进程是唯一写者，不再读盘。
        fs::write(&path, br#"{"a":"1"}"#).unwrap();
        assert!(load(&mut cache, root.path()).unwrap().is_empty());

        for (text, expected) in [
            (
                &br#"{"z":"1","a":"2","":"e"}"#[..],
                props(&[("z", "1"), ("a", "2"), ("", "e")]),
            ),
            (
                br#"{"a":"1","a":"2","b":"3"}"#,
                props(&[("a", "2"), ("b", "3")]),
            ),
            (
                br#"{"timezone":"Mars/Base"}"#,
                props(&[("timezone", "Mars/Base")]),
            ),
            (b"[]", Properties::new()),
            (b"\"s\"", Properties::new()),
            (b"null", Properties::new()),
        ] {
            fs::write(&path, text).unwrap();
            assert_eq!(*load(&mut None, root.path()).unwrap(), expected);
        }
    }

    #[test]
    fn unreadable_content_is_reported_and_not_cached() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(FILE);
        for text in [
            &b"{"[..],
            br#"{"a":1}"#,
            br#"{"a":true}"#,
            br#"{"a":null}"#,
            br#"{"a":["x"]}"#,
        ] {
            fs::write(&path, text).unwrap();
            let mut cache = None;
            assert!(matches!(
                load(&mut cache, root.path()),
                Err(EngineError::Storage(_))
            ));
            assert!(cache.is_none());
        }
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            load(&mut None, root.path()),
            Err(EngineError::Storage(_))
        ));
    }

    #[test]
    fn serialization_matches_the_bytes_the_reference_engine_wrote() {
        // 样本由 3.x 的 `create_database(properties={"zz": "2", "aa": "", "键": "值"})` 写出，见 fixtures/generate.py。
        let bytes = include_bytes!("../tests/fixtures/database_properties_3x.json");
        assert_eq!(
            to_bytes(&props(&[("zz", "2"), ("aa", ""), ("键", "值")])),
            bytes
        );
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(FILE), bytes).unwrap();
        // 读回为文件的键序；再写出得到相同字节。
        let read = load(&mut None, root.path()).unwrap().clone();
        assert_eq!(read, props(&[("aa", ""), ("zz", "2"), ("键", "值")]));
        assert_eq!(to_bytes(&read), bytes);
    }

    #[test]
    fn merge_deletes_before_setting_and_skips_no_ops() {
        let current = props(&[("zeta", "1"), ("alpha", "2")]);
        // 新键追加、已有键原位替换。
        assert_eq!(
            merge(
                &current,
                &[("alpha".into(), "9".into()), ("new".into(), "n".into())],
                &[]
            ),
            Some(props(&[("zeta", "1"), ("alpha", "9"), ("new", "n")]))
        );
        // 同一请求先删后设同一键：该键移到末尾。
        assert_eq!(
            merge(&current, &[("zeta".into(), "9".into())], &["zeta".into()]),
            Some(props(&[("alpha", "2"), ("zeta", "9")]))
        );
        // 删除不存在的键、设置为现值、两者的组合都不产生写入。
        assert_eq!(merge(&current, &[], &["nokey".into()]), None);
        assert_eq!(merge(&current, &[("zeta".into(), "1".into())], &[]), None);
        assert_eq!(merge(&Properties::new(), &[], &[]), None);
        // 删除后重设为原值：内容不变（只是位置变了）也不写。
        assert_eq!(
            merge(&current, &[("zeta".into(), "1".into())], &["zeta".into()]),
            None
        );
        // 空键与空串键的删除按精确匹配。
        let with_empty = props(&[("", "e"), ("a", "1")]);
        assert_eq!(
            merge(&with_empty, &[], &["".into()]),
            Some(props(&[("a", "1")]))
        );
    }

    #[test]
    fn written_bytes_match_python_json_dump_and_report_whether_the_file_changed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(FILE);
        fs::write(&path, b"old").unwrap();
        // json.dump({"zz": "2", "aa": "", "键": "值"}, indent=2, sort_keys=True, ensure_ascii=False)
        let properties = props(&[("zz", "2"), ("aa", ""), ("键", "值")]);
        let before = write(root.path(), &properties, |point| match point {
            ReplacePoint::BeforeRename => Err(std::io::Error::other("before")),
            ReplacePoint::AfterRename => Ok(()),
        });
        assert!(matches!(before, Err(ReplaceError::Unchanged(_))));
        assert_eq!(fs::read(&path).unwrap(), b"old");
        let after = write(root.path(), &properties, |point| match point {
            ReplacePoint::BeforeRename => Ok(()),
            ReplacePoint::AfterRename => Err(std::io::Error::other("after")),
        });
        assert!(matches!(after, Err(ReplaceError::Unconfirmed(_))));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\n  \"aa\": \"\",\n  \"zz\": \"2\",\n  \"键\": \"值\"\n}"
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        write(root.path(), &Properties::new(), |_| Ok(())).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{}");
    }
}
