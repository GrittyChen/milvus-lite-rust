//! 3.x snapshot 元数据的读取与钉住集合。任何损坏都阻止 collection 的文件清理。

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotMetadata {
    pub snapshot_format_version: u32,
    pub id: String,
    pub name: String,
    pub description: String,
    pub collection_name: String,
    pub created_at: String,
    pub created_seq: u64,
    pub manifest_version: u64,
    pub schema_file: String,
    pub manifest_file: String,
    #[serde(default)]
    pub data_files: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub delta_files: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub index_files: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Default, Clone)]
pub struct PinnedFiles {
    pub data: HashSet<(String, String)>,
    pub delta: HashSet<(String, String)>,
    pub index: HashSet<(String, String)>,
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot I/O error at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid snapshot metadata at {path:?}: {reason}")]
    Invalid { path: PathBuf, reason: String },
}

pub fn load_pins(collection_dir: &Path) -> Result<PinnedFiles, SnapshotError> {
    let snapshots_dir = collection_dir.join("snapshots");
    match fs::symlink_metadata(&snapshots_dir) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => {
            return Err(SnapshotError::Invalid {
                path: snapshots_dir,
                reason: "snapshots path is not a regular directory".to_owned(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PinnedFiles::default());
        }
        Err(source) => {
            return Err(SnapshotError::Io {
                path: snapshots_dir,
                source,
            });
        }
    }
    let metadata_dir = collection_dir.join("snapshots/metadata");
    match fs::symlink_metadata(&metadata_dir) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => {
            return Err(SnapshotError::Invalid {
                path: metadata_dir,
                reason: "metadata directory is not a regular directory".to_owned(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PinnedFiles::default());
        }
        Err(source) => {
            return Err(SnapshotError::Io {
                path: metadata_dir,
                source,
            });
        }
    }
    let entries = match fs::read_dir(&metadata_dir) {
        Ok(entries) => entries,
        Err(source) => {
            return Err(SnapshotError::Io {
                path: metadata_dir,
                source,
            });
        }
    };
    let mut pins = PinnedFiles::default();
    for entry in entries {
        let entry = entry.map_err(|source| SnapshotError::Io {
            path: metadata_dir.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let meta = fs::symlink_metadata(&path).map_err(|source| SnapshotError::Io {
            path: path.clone(),
            source,
        })?;
        if !meta.file_type().is_file() {
            return Err(SnapshotError::Invalid {
                path,
                reason: "metadata is not a regular file".to_owned(),
            });
        }
        let bytes = fs::read(&path).map_err(|source| SnapshotError::Io {
            path: path.clone(),
            source,
        })?;
        let mut snap: SnapshotMetadata =
            serde_json::from_slice(&bytes).map_err(|error| SnapshotError::Invalid {
                path: path.clone(),
                reason: error.to_string(),
            })?;
        if snap.snapshot_format_version != 1 {
            return Err(SnapshotError::Invalid {
                path,
                reason: format!(
                    "unknown snapshot_format_version {}",
                    snap.snapshot_format_version
                ),
            });
        }
        snap.schema_file = snap.schema_file.replace('\\', "/");
        snap.manifest_file = snap.manifest_file.replace('\\', "/");
        if !valid_rel(&snap.schema_file, "snapshots")
            || !valid_rel(&snap.manifest_file, "snapshots")
        {
            return Err(SnapshotError::Invalid {
                path,
                reason: "invalid schema or manifest path".to_owned(),
            });
        }
        add_map(&path, &snap.data_files, "data", &mut pins.data)?;
        add_map(&path, &snap.delta_files, "delta", &mut pins.delta)?;
        add_map(&path, &snap.index_files, "indexes", &mut pins.index)?;
    }
    Ok(pins)
}

fn add_map(
    metadata_path: &Path,
    files: &BTreeMap<String, Vec<String>>,
    prefix: &str,
    pins: &mut HashSet<(String, String)>,
) -> Result<(), SnapshotError> {
    for (partition, paths) in files {
        if !valid_part(partition) {
            return Err(SnapshotError::Invalid {
                path: metadata_path.to_path_buf(),
                reason: format!("invalid partition {partition:?}"),
            });
        }
        for raw in paths {
            let normalized = raw.replace('\\', "/");
            if !valid_rel(&normalized, prefix) {
                return Err(SnapshotError::Invalid {
                    path: metadata_path.to_path_buf(),
                    reason: format!("invalid {prefix} path {normalized:?}"),
                });
            }
            pins.insert((partition.clone(), normalized));
        }
    }
    Ok(())
}

fn valid_part(part: &str) -> bool {
    !part.is_empty() && part != "." && part != ".." && !part.contains(['/', '\\'])
}

fn valid_rel(path: &str, prefix: &str) -> bool {
    let mut parts = path.split('/');
    parts.next() == Some(prefix)
        && parts.clone().next().is_some()
        && !parts.any(|part| part.is_empty() || part == "." || part == "..")
        && !path.contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_data_delta_and_index_files() {
        let root = tempfile::tempdir().unwrap();
        let metadata = root.path().join("snapshots/metadata");
        fs::create_dir_all(&metadata).unwrap();
        let json = serde_json::json!({
            "snapshot_format_version": 1,
            "id": "abc", "name": "s", "description": "", "collection_name": "c",
            "created_at": "2026-09-28T00:00:00+00:00", "created_seq": 5, "manifest_version": 2,
            "schema_file": "snapshots/manifests/s/schema.json", "manifest_file": "snapshots/manifests/s/manifest.json",
            "data_files": {"_default": ["data\\data_000001_000002.parquet"]},
            "delta_files": {"_default": ["delta/delta_000003_000003.parquet"]},
            "index_files": {"_default": ["indexes/data_000001_000002_vector.idx"]}
        });
        fs::write(metadata.join("s.json"), serde_json::to_vec(&json).unwrap()).unwrap();
        let pins = load_pins(root.path()).unwrap();
        assert!(pins.data.contains(&(
            "_default".to_owned(),
            "data/data_000001_000002.parquet".to_owned()
        )));
        assert_eq!(pins.delta.len(), 1);
        assert_eq!(pins.index.len(), 1);
    }

    #[test]
    fn malformed_metadata_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let metadata = root.path().join("snapshots/metadata");
        fs::create_dir_all(&metadata).unwrap();
        fs::write(metadata.join("bad.json"), b"not json").unwrap();
        assert!(load_pins(root.path()).is_err());
    }
}
