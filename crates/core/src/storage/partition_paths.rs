//! 分区名只映射为一个真实目录；已有目录及 snapshot 别名在任何清理之前检查。

use super::manifest::Manifest;
use super::snapshot::PinnedFiles;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty() && !matches!(name, "." | ".." | "_all") && !name.contains(['/', '\\', '\0'])
}

fn directory(path: &Path) -> Result<Option<std::fs::Metadata>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(Some(meta)),
        Ok(_) => Err(format!(
            "partition directory path {path:?} is not a real directory"
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "cannot inspect partition directory {path:?}: {error}"
        )),
    }
}

pub(crate) fn validate(
    collection: &Path,
    manifest: &Manifest,
    pins: &PinnedFiles,
    additional: Option<&str>,
) -> Result<(), String> {
    let mut names: BTreeSet<&str> = manifest.partitions.keys().map(String::as_str).collect();
    for paths in [&pins.data, &pins.delta, &pins.index] {
        for (partition, _) in paths {
            names.insert(partition.as_str());
        }
    }
    if let Some(name) = additional {
        names.insert(name);
    }
    let root = collection.join("partitions");
    directory(&root)?;
    #[cfg(unix)]
    let mut identities = BTreeMap::new();
    for name in names {
        if !valid_name(name) {
            return Err(format!("unsafe partition name {name:?}"));
        }
        if let Some(meta) = directory(&root.join(name))? {
            for child in ["data", "delta", "indexes"] {
                directory(&root.join(name).join(child))?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if let Some(previous) = identities.insert((meta.dev(), meta.ino()), name) {
                    return Err(format!(
                        "partition names {previous:?} and {name:?} refer to the same directory"
                    ));
                }
            }
            #[cfg(not(unix))]
            {
                let _ = meta;
                return Err("partition directory identity validation requires Unix".into());
            }
        }
    }
    for (partition, files) in &manifest.partitions {
        for file in files.data_files.iter().chain(&files.delta_files) {
            validate_file_chain(&root.join(partition), file)?;
        }
    }
    for paths in [&pins.data, &pins.delta, &pins.index] {
        for (partition, file) in paths {
            validate_file_chain(&root.join(partition), file)?;
        }
    }
    Ok(())
}

/// Flush 首次物化隐含桶后也必须检查全部候选；失败只回收本次自建的空目录。
pub(crate) fn prepare_flush(
    collection: &Path,
    manifest: &Manifest,
    pins: &PinnedFiles,
    names: &[&str],
) -> Result<(), String> {
    validate(collection, manifest, pins, None)?;
    let mut created = Vec::new();
    let result = (|| {
        let root = collection.join("partitions");
        let mut paths = vec![root.clone()];
        for name in names {
            if !manifest.partitions.contains_key(*name) {
                return Err("flush partition is not registered".to_owned());
            }
            let partition = root.join(name);
            paths.extend([
                partition.clone(),
                partition.join("data"),
                partition.join("delta"),
            ]);
        }
        for path in paths {
            let existed = directory(&path)?.is_some();
            if !existed {
                created.push(path.clone());
            }
            super::fs_commit::create_dir_synced(&path).map_err(|e| e.to_string())?;
        }
        validate(collection, manifest, pins, None)
    })();
    if let Err(mut reason) = result {
        for path in created.iter().rev() {
            let cleanup = match std::fs::remove_dir(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                result => result,
            }
            .and_then(|()| {
                super::fs_commit::sync_dir(path.parent().unwrap_or(collection))
                    .map_err(std::io::Error::other)
            });
            if let Err(error) = cleanup {
                reason.push_str(&format!(
                    "; cannot remove prepared empty directory {path:?}: {error}"
                ));
            }
        }
        return Err(reason);
    }
    Ok(())
}

fn validate_file_chain(root: &Path, relative: &str) -> Result<(), String> {
    let path = Path::new(relative);
    let mut current = root.to_path_buf();
    if let Some(parent) = path.parent() {
        for component in parent.components() {
            current.push(component.as_os_str());
            directory(&current)?;
        }
    }
    let target = root.join(path);
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(format!("partition reference {target:?} is a symbolic link"))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "cannot inspect partition reference {target:?}: {error}"
        )),
    }
}

pub(crate) fn prepare(
    collection: &Path,
    manifest: &Manifest,
    pins: &PinnedFiles,
    name: &str,
) -> Result<(), String> {
    validate(collection, manifest, pins, Some(name))?;
    let target = collection.join("partitions").join(name);
    let existed = directory(&target)?.is_some();
    if existed {
        let owned: BTreeSet<_> = [&pins.data, &pins.delta, &pins.index]
            .into_iter()
            .flat_map(|set| set.iter())
            .filter(|(part, _)| part == name)
            .map(|(_, file)| file.as_str())
            .collect();
        let mut pending = vec![target.clone()];
        while let Some(dir) = pending.pop() {
            for entry in
                std::fs::read_dir(&dir).map_err(|error| format!("cannot read {dir:?}: {error}"))?
            {
                let entry = entry.map_err(|error| error.to_string())?;
                let path = entry.path();
                let relative = path
                    .strip_prefix(&target)
                    .map_err(|error| error.to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                let kind = entry.file_type().map_err(|error| error.to_string())?;
                if kind.is_dir() {
                    if !matches!(relative.as_str(), "data" | "delta" | "indexes")
                        && !owned
                            .iter()
                            .any(|file| file.starts_with(&(relative.clone() + "/")))
                    {
                        return Err(format!("unowned directory in partition path: {path:?}"));
                    }
                    pending.push(path);
                } else if !kind.is_file() || !owned.contains(relative.as_str()) {
                    return Err(format!("unowned file in partition path: {path:?}"));
                }
            }
        }
    }
    let result = super::fs_commit::create_dir_synced(&target)
        .map_err(|error| error.to_string())
        // 缺失的已登记目录在物化后才可能显现大小写或 Unicode 别名。
        .and_then(|()| validate(collection, manifest, pins, Some(name)));
    if let Err(error) = result {
        if !existed {
            // 仅回收本次创建的空目录，不递归删除，也不触及复用目录中的钉住文件。
            let cleanup = match std::fs::remove_dir(&target) {
                Ok(()) => super::fs_commit::sync_dir(&collection.join("partitions")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            };
            if let Err(cleanup) = cleanup {
                return Err(format!(
                    "{error}; prepared partition directory cleanup failed: {cleanup}"
                ));
            }
        }
        return Err(error);
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct Registry {
    names: Vec<Option<String>>,
    ids: BTreeMap<String, u32>,
}

impl Registry {
    pub(crate) fn from_manifest(manifest: &Manifest) -> Result<Self, String> {
        let mut registry = Self {
            names: vec![Some("_default".into())],
            ids: BTreeMap::from([("_default".into(), 0)]),
        };
        for name in &manifest.partition_order {
            if name != "_default" {
                registry.add(name)?;
            }
        }
        for name in manifest.partitions.keys() {
            if !registry.ids.contains_key(name) {
                registry.add(name)?;
            }
        }
        Ok(registry)
    }
    pub(crate) fn add(&mut self, name: &str) -> Result<u32, String> {
        if let Some(&id) = self.ids.get(name) {
            return Ok(id);
        }
        let slot = self
            .names
            .iter()
            .position(Option::is_none)
            .unwrap_or(self.names.len());
        let id = u32::try_from(slot).map_err(|_| "partition identifier space exhausted")?;
        if slot == self.names.len() {
            self.names.push(Some(name.to_owned()));
        } else {
            self.names[slot] = Some(name.to_owned());
        }
        self.ids.insert(name.to_owned(), id);
        Ok(id)
    }
    pub(crate) fn id(&self, name: &str) -> Option<u32> {
        self.ids.get(name).copied()
    }
    pub(crate) fn name(&self, id: u32) -> Option<&str> {
        self.names.get(id as usize)?.as_deref()
    }
    pub(crate) fn remove(&mut self, name: &str) -> Result<(), String> {
        if name == "_default" {
            return Err("cannot remove default partition".into());
        }
        if let Some(id) = self.ids.remove(name) {
            self.names[id as usize] = None;
        }
        while self.names.last().is_some_and(Option::is_none) {
            self.names.pop();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_components_without_imposing_an_os_independent_length_limit() {
        for name in [
            "",
            ".",
            "..",
            "_all",
            "../escape",
            "/absolute",
            "a/b",
            "a\\b",
            "a\0b",
        ] {
            assert!(!valid_name(name), "{name:?}");
        }
        for name in [
            "_default",
            "分区",
            "a:b",
            "Café",
            "Cafe\u{301}",
            " ",
            &"x".repeat(256),
        ] {
            assert!(valid_name(name), "{name:?}");
        }
    }
    #[test]
    fn registry_keeps_live_ids_and_reuses_only_removed_slots() {
        let mut registry = Registry::from_manifest(&Manifest::default()).unwrap();
        let p1 = registry.add("p1").unwrap();
        let p2 = registry.add("p2").unwrap();
        registry.remove("p1").unwrap();
        assert_eq!(registry.add("p3").unwrap(), p1);
        assert_eq!(registry.id("p2"), Some(p2));
        assert_eq!(registry.name(p1), Some("p3"));
        assert!(registry.remove("_default").is_err());
        assert_eq!(registry.id("_default"), Some(0));
    }
    #[cfg(unix)]
    #[test]
    fn prepare_checks_aliases_of_missing_registered_and_pinned_directories() {
        use std::os::unix::fs::MetadataExt;
        for pinned in [false, true] {
            for (registered, requested) in [("_default", "_DEFAULT"), ("Café", "Cafe\u{301}")] {
                let root = tempfile::tempdir().unwrap();
                let mut manifest = Manifest::default();
                let mut pins = PinnedFiles::default();
                if pinned {
                    pins.data
                        .insert((registered.into(), "data/data_1_1.parquet".into()));
                } else {
                    manifest
                        .partitions
                        .insert(registered.into(), Default::default());
                }
                let result = prepare(root.path(), &manifest, &pins, requested);
                let target = root.path().join("partitions").join(requested);
                if let Err(error) = result {
                    assert!(error.contains("same directory"));
                    assert!(
                        !target.exists(),
                        "only the newly prepared empty directory is removed"
                    );
                    prepare(root.path(), &manifest, &pins, "safe").unwrap();
                } else {
                    let a = std::fs::metadata(&target).unwrap();
                    if let Ok(b) =
                        std::fs::metadata(root.path().join("partitions").join(registered))
                    {
                        assert_ne!(
                            (a.dev(), a.ino()),
                            (b.dev(), b.ino()),
                            "{registered} / {requested}"
                        );
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_links_before_cleanup_and_checks_physical_case_aliases() {
        use std::os::unix::fs::{MetadataExt, symlink};
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("partitions/_default")).unwrap();
        std::fs::write(outside.path().join("canary"), b"owned").unwrap();
        symlink(outside.path(), root.path().join("partitions/_default/data")).unwrap();
        assert!(
            validate(
                root.path(),
                &Manifest::default(),
                &PinnedFiles::default(),
                None
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(outside.path().join("canary")).unwrap(),
            b"owned"
        );
        std::fs::remove_file(root.path().join("partitions/_default/data")).unwrap();
        for name in ["p1", "P1"] {
            std::fs::create_dir_all(root.path().join("partitions").join(name)).unwrap();
        }
        let a = std::fs::metadata(root.path().join("partitions/p1")).unwrap();
        let b = std::fs::metadata(root.path().join("partitions/P1")).unwrap();
        let mut manifest = Manifest::default();
        manifest.partitions.insert("p1".into(), Default::default());
        let same = (a.dev(), a.ino()) == (b.dev(), b.ino());
        assert_eq!(
            validate(root.path(), &manifest, &PinnedFiles::default(), Some("P1")).is_err(),
            same
        );
    }
}
