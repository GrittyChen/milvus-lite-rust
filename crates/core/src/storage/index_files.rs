//! 创建及删除索引共用的侧车预检；所有检查均先于第一项删除。

use super::{index_platform, manifest::Manifest, snapshot};
use crate::index::{IndexChange, parse_sidecar_name};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) fn check_root(dir: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(dir).map_err(|e| format!("{dir:?}: {e}"))?;
    if !meta.file_type().is_dir() {
        return Err(format!(
            "collection path is not a regular directory: {dir:?}"
        ));
    }
    fs::read_dir(dir).map_err(|e| format!("{dir:?}: {e}"))?;
    Ok(())
}

pub(crate) fn preflight(
    dir: &Path,
    manifest: &Manifest,
    change: &IndexChange,
) -> Result<Vec<PathBuf>, String> {
    check_root(dir)?;
    let pins = snapshot::load_pins(dir).map_err(|e| e.to_string())?;
    super::partition_paths::validate(dir, manifest, &pins, None)?;
    let mut partitions: std::collections::BTreeSet<_> =
        manifest.partitions.keys().cloned().collect();
    // snapshot 只钉住数据、没有钉住后来生成的索引时，已 Drop 的分区仍须参与侧车清理。
    for paths in [&pins.data, &pins.delta, &pins.index] {
        partitions.extend(paths.iter().map(|(name, _)| name.clone()));
    }
    let mut files = Vec::new();
    for partition in partitions {
        files.extend(preflight_partition(dir, manifest, change, &partition)?);
    }
    Ok(files)
}

fn preflight_partition(
    dir: &Path,
    manifest: &Manifest,
    change: &IndexChange,
    partition: &str,
) -> Result<Vec<PathBuf>, String> {
    check_root(dir)?;
    let mut path = dir.to_owned();
    for part in ["partitions", partition, "indexes"] {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_dir() => {
                fs::read_dir(&path).map_err(|e| format!("{path:?}: {e}"))?;
            }
            Ok(_) => {
                return Err(format!(
                    "index directory chain is not a regular directory: {path:?}"
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("{path:?}: {e}")),
        }
    }
    let snapshots = snapshot::index_snapshots(dir).map_err(|e| e.to_string())?;
    let pinned: HashSet<_> = snapshots
        .iter()
        .flat_map(|s| s.index_files.get(partition).into_iter().flatten())
        .collect();
    let stems: HashSet<_> = manifest
        .partitions
        .get(partition)
        .into_iter()
        .flat_map(|files| &files.data_files)
        .filter_map(|name| Path::new(name).file_stem().and_then(|s| s.to_str()))
        .collect();
    let mut entries = fs::read_dir(&path)
        .map_err(|e| format!("{path:?}: {e}"))?
        .collect::<io::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    entries.sort_by_key(|e| e.file_name());
    let mut files = Vec::new();
    let mut copies = HashMap::new();
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(parsed) = parse_sidecar_name(name) else {
            continue;
        };
        let Some(spec) = change.specs().iter().find(|spec| {
            parsed.field_name == spec.field_name
                && parsed.index_type == spec.index_type.to_lowercase()
                && parsed.extension
                    == if spec.index_type == "INVERTED" {
                        ".sidx"
                    } else {
                        ".idx"
                    }
        }) else {
            continue;
        };
        if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            return Err(format!(
                "index sidecar is not a regular file: {:?}",
                entry.path()
            ));
        }
        let relative = format!("indexes/{name}");
        if pinned.contains(&relative) {
            if matches!(change, IndexChange::Create(_)) && stems.contains(parsed.source_stem) {
                for snap in &snapshots {
                    if !snap
                        .index_files
                        .get(partition)
                        .is_some_and(|refs| refs.contains(&relative))
                    {
                        continue;
                    }
                    let saved = match copies.entry(snap.manifest_file.clone()) {
                        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                        std::collections::hash_map::Entry::Vacant(e) => {
                            let copy = snapshot::manifest_copy(dir, snap).map_err(|error| {
                                format!(
                                    "cannot verify pinned index {:?} for field {:?} in snapshot {:?}: {error}",
                                    entry.path(), spec.field_name, snap.name
                                )
                            })?;
                            e.insert(copy)
                        }
                    };
                    if !saved.index_specs.get(&spec.field_name).is_some_and(|old| {
                        old.index_type == spec.index_type
                            && old.metric_type == spec.metric_type
                            && old.build_params == spec.params
                    }) {
                        return Err(format!(
                            "index definition for field {:?} conflicts with pinned file {:?} in snapshot {:?}",
                            spec.field_name,
                            entry.path(),
                            snap.name
                        ));
                    }
                }
            }
        } else {
            index_platform::check_delete(&path, &entry.path())?;
            files.push(entry.path());
        }
    }
    Ok(files)
}
