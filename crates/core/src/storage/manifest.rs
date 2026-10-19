//! 3.x `manifest.json` 的格式。这里只解析与编码，原子提交由 collection 的 flush 协议负责。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MANIFEST_FORMAT_VERSION: u32 = 2;
const DEFAULT_PARTITION: &str = "_default";

fn format_version() -> u32 {
    MANIFEST_FORMAT_VERSION
}

fn schema_version() -> u64 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexSpec {
    pub field_name: String,
    pub index_type: String,
    pub metric_type: String,
    #[serde(default)]
    pub build_params: Map<String, Value>,
    #[serde(default)]
    pub search_params: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartitionFiles {
    #[serde(default)]
    pub data_files: Vec<String>,
    #[serde(default)]
    pub delta_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default = "format_version")]
    pub manifest_format_version: u32,
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub current_seq: u64,
    #[serde(default = "schema_version")]
    pub schema_version: u64,
    #[serde(default)]
    pub active_wal_number: Option<u64>,
    #[serde(default)]
    pub partitions: BTreeMap<String, PartitionFiles>,
    #[serde(skip)]
    pub partition_order: Vec<String>,
    #[serde(default)]
    pub index_specs: BTreeMap<String, IndexSpec>,
}

impl Default for Manifest {
    fn default() -> Self {
        let mut partitions = BTreeMap::new();
        partitions.insert(DEFAULT_PARTITION.to_owned(), PartitionFiles::default());
        Self {
            manifest_format_version: MANIFEST_FORMAT_VERSION,
            version: 0,
            current_seq: 0,
            schema_version: 1,
            active_wal_number: None,
            partitions,
            partition_order: vec![DEFAULT_PARTITION.to_owned()],
            index_specs: BTreeMap::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("cannot read manifest {path:?}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid manifest: {0}")]
    Invalid(String),
    #[error("cannot encode manifest: {0}")]
    Encode(#[from] serde_json::Error),
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        let mut manifest: Self = serde_json::from_slice(bytes)
            .map_err(|source| ManifestError::Invalid(source.to_string()))?;
        let raw: Value = serde_json::from_slice(bytes).map_err(ManifestError::Encode)?;
        manifest.partition_order = raw
            .get("partitions")
            .and_then(Value::as_object)
            .map_or_else(Vec::new, |parts| parts.keys().cloned().collect());
        if !manifest
            .partition_order
            .iter()
            .any(|part| part == DEFAULT_PARTITION)
        {
            manifest.partition_order.push(DEFAULT_PARTITION.to_owned());
        }
        if manifest.manifest_format_version != MANIFEST_FORMAT_VERSION {
            return Err(ManifestError::Invalid(format!(
                "unsupported manifest_format_version {}",
                manifest.manifest_format_version
            )));
        }
        // 3.x 的 load 对缺失的默认分区补空列表；老目录可能没有 `partitions` 键。
        manifest
            .partitions
            .entry(DEFAULT_PARTITION.to_owned())
            .or_default();
        for (partition, files) in &mut manifest.partitions {
            for path in files.data_files.iter_mut().chain(&mut files.delta_files) {
                *path = path.replace('\\', "/");
                if !safe_relative_path(path) {
                    return Err(ManifestError::Invalid(format!(
                        "unsafe file path {path:?} in partition {partition:?}"
                    )));
                }
            }
        }
        for (field, spec) in &manifest.index_specs {
            if spec.field_name.is_empty() || spec.index_type.is_empty() {
                return Err(ManifestError::Invalid(format!(
                    "invalid index spec for field {field:?}"
                )));
            }
            if !matches!(
                spec.metric_type.as_str(),
                "COSINE" | "L2" | "IP" | "BM25" | "NONE"
            ) {
                return Err(ManifestError::Invalid(format!(
                    "unknown metric_type {:?} for field {field:?}",
                    spec.metric_type
                )));
            }
        }
        Ok(manifest)
    }

    pub fn read(path: &Path) -> Result<Self, ManifestError> {
        let bytes = std::fs::read(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&bytes)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, ManifestError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }
}

pub(crate) fn safe_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        && !path.contains(':')
        && !path.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_order_is_runtime_only_and_reopen_follows_saved_json_order() {
        let raw = br#"{"partitions":{"z":{},"_default":{},"a":{}}}"#;
        let manifest = Manifest::parse(raw).unwrap();
        assert_eq!(manifest.partition_order, ["z", "_default", "a"]);
        let bytes = manifest.to_bytes().unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.get("partition_order").is_none());
        assert_eq!(
            Manifest::parse(&bytes).unwrap().partition_order,
            ["_default", "a", "z"]
        );
    }

    #[test]
    fn default_matches_3x_shape() {
        let manifest = Manifest::default();
        let value: Value = serde_json::from_slice(&manifest.to_bytes().unwrap()).unwrap();
        assert_eq!(value["manifest_format_version"], 2);
        assert_eq!(value["version"], 0);
        assert_eq!(value["current_seq"], 0);
        assert_eq!(value["schema_version"], 1);
        assert!(value["active_wal_number"].is_null());
        assert_eq!(
            value["partitions"]["_default"]["data_files"],
            serde_json::json!([])
        );
        assert_eq!(value["index_specs"], serde_json::json!({}));
    }

    #[test]
    fn parses_old_windows_paths_and_missing_defaults() {
        let bytes = br#"{"version":3,"current_seq":12,"partitions":{"_default":{"data_files":["data\\data_000001_000012.parquet"]}}}"#;
        let manifest = Manifest::parse(bytes).unwrap();
        assert_eq!(manifest.manifest_format_version, 2);
        assert_eq!(manifest.schema_version, 1);
        assert_eq!(
            manifest.partitions["_default"].data_files,
            ["data/data_000001_000012.parquet"]
        );
        assert!(manifest.partitions["_default"].delta_files.is_empty());
    }

    #[test]
    fn unknown_keys_and_formats_fail_closed() {
        let mut value: Value =
            serde_json::from_slice(&Manifest::default().to_bytes().unwrap()).unwrap();
        value["new_field"] = serde_json::json!(true);
        assert!(Manifest::parse(&serde_json::to_vec(&value).unwrap()).is_err());
        value.as_object_mut().unwrap().remove("new_field");
        value["manifest_format_version"] = serde_json::json!(3);
        assert!(Manifest::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn unsafe_file_paths_fail_closed() {
        let bytes = br#"{"partitions":{"_default":{"data_files":["../outside.parquet"]}}}"#;
        assert!(Manifest::parse(bytes).is_err());
    }
}
