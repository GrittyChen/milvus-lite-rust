//! 0014：从真实 Db 入口核对定义、侧车、提交点与持久化意图的共同状态。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use milvus_lite_core::storage::{index_intent::INTENT_FILE, manifest::Manifest};
use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, EngineError, FieldSchema, IndexRequest,
    InputValues, InsertBatch, LoadState, Pk,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

// exec 前继承的 flock 引用会干扰其他用例的关闭重开；只隔离用例，不串行化用例内线程。
static CASE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[test]
fn failed_case_does_not_poison_the_serialization_lock() {
    let failed = std::panic::catch_unwind(|| {
        let _case = CASE_LOCK.lock();
        panic!("deliberate isolated case failure");
    });
    assert!(failed.is_err());
    let _next = CASE_LOCK.lock();
}

fn open(path: &Path) -> Arc<Db> {
    Db::new(
        DataDir::open(path).unwrap(),
        Compute::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap(),
    )
    .unwrap()
}
fn schema() -> CollectionSchema {
    let mut id = FieldSchema::new("id", DataType::Int64);
    id.is_primary = true;
    let mut v = FieldSchema::new("v", DataType::FloatVector);
    v.dim = Some(2);
    CollectionSchema {
        fields: vec![id, v],
        enable_dynamic_field: false,
        properties: vec![],
    }
}
fn req(kind: &str, metric: &str) -> IndexRequest {
    IndexRequest {
        index_type: kind.into(),
        metric_type: Some(metric.into()),
        ..Default::default()
    }
}
fn batch(id: i64) -> InsertBatch {
    InsertBatch {
        num_rows: 1,
        columns: vec![
            ("id".into(), InputValues::Int(vec![id])),
            (
                "v".into(),
                InputValues::FloatVector {
                    dim: 2,
                    values: vec![id as f32, 1.],
                },
            ),
        ],
    }
}
fn fixture() -> (tempfile::TempDir, PathBuf, Arc<Db>) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("index.db");
    let db = open(&path);
    db.create_collection("", "c", schema()).unwrap();
    db.create_index("", "c", "v", req("HNSW", "L2")).unwrap();
    for id in [1, 2] {
        db.insert("", "c", batch(id)).unwrap();
        db.flush_collection("", "c").unwrap();
    }
    db.collection("", "c").unwrap().write().release();
    (root, path, db)
}
fn col(path: &Path) -> PathBuf {
    path.join("collections/c")
}
fn manifest(path: &Path) -> Vec<u8> {
    fs::read(col(path).join("manifest.json")).unwrap()
}
fn sidecars(path: &Path, kind: &str) -> Vec<PathBuf> {
    let dir = col(path);
    let m = Manifest::read(&dir.join("manifest.json")).unwrap();
    let idx = dir.join("partitions/_default/indexes");
    fs::create_dir_all(&idx).unwrap();
    m.partitions["_default"]
        .data_files
        .iter()
        .map(|f| {
            let name = format!(
                "{}.v.{}.idx",
                Path::new(f).file_stem().unwrap().to_str().unwrap(),
                kind.to_lowercase()
            );
            let file = idx.join(name);
            fs::write(&file, b"retained 3.x sidecar").unwrap();
            file
        })
        .collect()
}
fn snapshot(path: &Path, files: &[PathBuf]) {
    let dir = col(path);
    let md = dir.join("snapshots/metadata");
    let copies = dir.join("snapshots/manifests/s");
    fs::create_dir_all(&md).unwrap();
    fs::create_dir_all(&copies).unwrap();
    fs::copy(dir.join("manifest.json"), copies.join("manifest.json")).unwrap();
    fs::copy(dir.join("schema.json"), copies.join("schema.json")).unwrap();
    let m = Manifest::read(&dir.join("manifest.json")).unwrap();
    let value = serde_json::json!({"snapshot_format_version":1,"id":"s","name":"s","description":"",
        "collection_name":"c","created_at":"2026-10-14T00:00:00+00:00","created_seq":m.current_seq,
        "manifest_version":m.version,"schema_file":"snapshots/manifests/s/schema.json",
        "manifest_file":"snapshots/manifests/s/manifest.json","data_files":{"_default":m.partitions["_default"].data_files},
        "delta_files":{},"index_files":{"_default":files.iter().map(|f|format!("indexes/{}",f.file_name().unwrap().to_str().unwrap())).collect::<Vec<_>>()}});
    fs::write(md.join("s.json"), serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn accepted_definitions_preserve_parameters_state_and_reopen() {
    let _case = CASE_LOCK.lock();
    for kind in [
        "FLAT",
        "BRUTE_FORCE",
        "AUTOINDEX",
        "HNSW",
        "HNSW_SQ",
        "IVF_FLAT",
        "IVF_SQ8",
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("c.db");
        let db = open(&path);
        db.create_collection("", "c", schema()).unwrap();
        let mut request = req(&kind.to_lowercase(), "IP");
        request.params.insert("M".into(), "abc".into());
        request.search_params.insert("ef".into(), 7.into());
        db.create_index("", "c", "v", request).unwrap();
        let c = db.collection("", "c").unwrap();
        assert_eq!(c.read().load_state(), LoadState::Loaded);
        assert_eq!(c.read().index("v").unwrap().index_type, kind);
        db.create_index(
            "",
            "c",
            "id",
            IndexRequest {
                index_type: "inverted".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(c.read().index("id").unwrap().metric_type, "NONE");
        assert!(!col(&path).join(INTENT_FILE).exists());
        db.close();
        drop(db);
        drop(c);
        let db = open(&path);
        let c = db.collection("", "c").unwrap();
        assert_eq!(c.read().load_state(), LoadState::Released);
        let spec = c.read().index("v").unwrap().clone();
        assert_eq!(spec.index_type, kind);
        assert_eq!(spec.params["M"], "abc");
        assert_eq!(spec.search_params["ef"], 7);
        c.write().load().unwrap();
        db.close();
    }
}

#[test]
fn drop_names_error_priority_and_auto_loaded_match_reference() {
    let _case = CASE_LOCK.lock();
    for name in ["v", "not-a-name", ""] {
        let (_root, path, db) = fixture();
        db.create_index(
            "",
            "c",
            "id",
            IndexRequest {
                index_type: "INVERTED".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            db.drop_index("", "c", "absent", "")
                .unwrap_err()
                .to_string(),
            "no index on field 'absent'; indexed fields: ['v', 'id']"
        );
        db.collection("", "c").unwrap().write().load().unwrap();
        assert_eq!(
            db.drop_index("", "c", "", name).unwrap_err().to_string(),
            "index cannot be dropped on loaded collection; call release() first"
        );
        db.collection("", "c").unwrap().write().release();
        db.drop_index("", "c", "", name).unwrap();
        let c = db.collection("", "c").unwrap();
        assert!(c.read().indexes().is_empty());
        assert_eq!(c.read().load_state(), LoadState::Loaded);
        assert_eq!(
            db.drop_index("", "c", "", name).unwrap_err().to_string(),
            "no index to drop"
        );
        assert!(
            Manifest::read(&col(&path).join("manifest.json"))
                .unwrap()
                .index_specs
                .is_empty()
        );
    }
    let (_root, _path, db) = fixture();
    db.create_index(
        "",
        "c",
        "id",
        IndexRequest {
            index_type: "INVERTED".into(),
            ..Default::default()
        },
    )
    .unwrap();
    db.drop_index("", "c", "", "id_idx").unwrap();
    let c = db.collection("", "c").unwrap();
    assert_eq!(c.read().indexes().len(), 1);
    assert_eq!(c.read().load_state(), LoadState::Released);
    db.drop_index("", "c", "v", "invalid-name").unwrap();
    assert_eq!(c.read().load_state(), LoadState::Loaded);
}

#[test]
fn drop_removes_unpinned_files_and_create_removes_stale_files() {
    let _case = CASE_LOCK.lock();
    let (_root, path, db) = fixture();
    let files = sidecars(&path, "HNSW");
    assert_eq!(files.len(), 2);
    db.drop_index("", "c", "v", "").unwrap();
    assert!(files.iter().all(|f| !f.exists()));
    assert!(!col(&path).join(INTENT_FILE).exists());
    for file in &files {
        fs::write(file, b"stale COSINE index").unwrap();
    }
    db.create_index("", "c", "v", req("HNSW", "IP")).unwrap();
    assert!(files.iter().all(|f| !f.exists()));
    assert_eq!(
        db.collection("", "c")
            .unwrap()
            .read()
            .index("v")
            .unwrap()
            .metric_type,
        "IP"
    );
}

#[test]
fn pinned_files_require_all_saved_definitions_to_match() {
    let _case = CASE_LOCK.lock();
    for conflict in [false, true] {
        let (_root, path, db) = fixture();
        let files = sidecars(&path, "HNSW");
        snapshot(&path, &files);
        db.drop_index("", "c", "v", "").unwrap();
        assert!(files.iter().all(|f| f.exists()));
        let before = manifest(&path);
        let result = db.create_index(
            "",
            "c",
            "v",
            req("HNSW", if conflict { "COSINE" } else { "L2" }),
        );
        if conflict {
            assert!(matches!(result, Err(EngineError::InvalidArgument(_))));
            assert_eq!(manifest(&path), before);
        } else {
            result.unwrap();
        }
        assert!(
            files
                .iter()
                .all(|f| fs::read(f).unwrap() == b"retained 3.x sidecar")
        );
        assert!(!col(&path).join(INTENT_FILE).exists());
    }
    let (_root, path, db) = fixture();
    let files = sidecars(&path, "HNSW");
    snapshot(&path, &files);
    db.drop_index("", "c", "v", "").unwrap();
    fs::write(
        col(&path).join("snapshots/manifests/s/manifest.json"),
        b"bad manifest",
    )
    .unwrap();
    let before = manifest(&path);
    assert!(db.create_index("", "c", "v", req("HNSW", "L2")).is_err());
    assert_eq!(manifest(&path), before);
    assert!(files.iter().all(|f| f.exists()));
}

#[cfg(unix)]
#[test]
fn cached_directory_chain_and_candidate_symlinks_fail_before_deletion() {
    let _case = CASE_LOCK.lock();
    for relative in [
        "partitions",
        "partitions/_default",
        "partitions/_default/indexes",
    ] {
        for link in [false, true] {
            for create in [false, true] {
                let (root, path, db) = fixture();
                let files = sidecars(&path, "HNSW");
                if create {
                    db.drop_index("", "c", "v", "").unwrap();
                    for file in &files {
                        fs::write(file, b"old").unwrap();
                    }
                }
                let before = manifest(&path);
                let target = col(&path).join(relative);
                let backup = root.path().join("held");
                fs::rename(&target, &backup).unwrap();
                let outside = root.path().join("outside");
                fs::create_dir(&outside).unwrap();
                fs::write(outside.join("victim"), b"unchanged").unwrap();
                if link {
                    std::os::unix::fs::symlink(&outside, &target).unwrap();
                } else {
                    fs::write(&target, b"not a directory").unwrap();
                }
                let result = if create {
                    db.create_index("", "c", "v", req("HNSW", "L2"))
                } else {
                    db.drop_index("", "c", "v", "")
                };
                assert!(
                    matches!(result, Err(EngineError::InvalidArgument(_))),
                    "{result:?}"
                );
                assert_eq!(manifest(&path), before);
                assert_eq!(fs::read(outside.join("victim")).unwrap(), b"unchanged");
                fs::remove_file(&target).unwrap();
                fs::rename(&backup, &target).unwrap();
            }
        }
    }
    let (root, path, db) = fixture();
    let files = sidecars(&path, "HNSW");
    let victim = root.path().join("victim");
    fs::write(&victim, b"outside").unwrap();
    fs::remove_file(&files[0]).unwrap();
    std::os::unix::fs::symlink(&victim, &files[0]).unwrap();
    let before = manifest(&path);
    assert!(db.drop_index("", "c", "v", "").is_err());
    assert_eq!(manifest(&path), before);
    assert_eq!(fs::read(victim).unwrap(), b"outside");
    assert!(files[1].exists());
}

fn marker(path: &Path) -> serde_json::Value {
    serde_json::json!({"format_version":1,"operation":"drop_index","field_names":["v"],
        "files":["partitions/_default/indexes/data_1_1.v.hnsw.idx"],"boot_id":null,"boot_source":null,"auto_recovery":false,
        "unused_path_for_tests":path.file_name().unwrap().to_str().unwrap()})
}
#[test]
fn malformed_and_unknown_markers_only_block_index_changes() {
    let _case = CASE_LOCK.lock();
    for variant in 0..9 {
        let (_root, path, db) = fixture();
        let before = manifest(&path);
        let file = col(&path).join(INTENT_FILE);
        let mut value = marker(&path);
        value
            .as_object_mut()
            .unwrap()
            .remove("unused_path_for_tests");
        match variant {
            0 => {
                fs::create_dir(&file).unwrap();
            }
            1 => {
                fs::write(&file, b"not JSON").unwrap();
            }
            2 => {
                value["format_version"] = 99.into();
                fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            3 => {
                value.as_object_mut().unwrap().remove("boot_id");
                fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            4 => {
                value["boot_id"] = "bad".into();
                value["boot_source"] = "linux-kernel-procfs".into();
                fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            5 => {
                value["files"] = serde_json::json!(["../victim.idx"]);
                fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            6 => {
                value["auto_recovery"] = true.into();
                fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            7 => {
                value["operation"] = "unknown".into();
                fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            _ => {
                fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
            }
        }
        assert!(db.drop_index("", "c", "v", "").is_err());
        assert_eq!(manifest(&path), before);
        assert!(
            db.create_index(
                "",
                "c",
                "id",
                IndexRequest {
                    index_type: "INVERTED".into(),
                    ..Default::default()
                }
            )
            .is_err()
        );
        db.collection("", "c").unwrap().write().load().unwrap();
        assert_eq!(
            db.collection("", "c")
                .unwrap()
                .read()
                .get(&[Pk::Int64(1)], &[])
                .unwrap()
                .rows
                .pks
                .len(),
            1
        );
        db.insert("", "c", batch(3)).unwrap();
        db.flush_collection("", "c").unwrap();
        db.create_collection("", "healthy", schema()).unwrap();
        db.create_index("", "healthy", "v", req("HNSW", "L2"))
            .unwrap();
    }
}

#[cfg(feature = "fault-injection")]
mod faults {
    use super::*;
    use milvus_lite_core::storage::{
        fs_commit::{self, CommitPhase, FsAction},
        index_intent::{self, BootContext, BootSource, FaultKind as K, IndexPoint as P},
    };
    fn known(id: &str) {
        index_intent::override_boot_context(Some(BootContext {
            boot_id: Some(id.into()),
            boot_source: Some(BootSource::LinuxKernelProcfs),
            local: true,
        }));
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            index_intent::override_boot_context(None);
        }
    }
    const BOOT: &str = "11111111-1111-4111-8111-111111111111";

    #[test]
    fn every_preparation_failure_preserves_sidecars_and_cleans_owned_tmp() {
        let _case = CASE_LOCK.lock();
        for create in [false, true] {
            for point in [
                P::InheritedSync,
                P::ManifestWrite,
                P::ManifestSync,
                P::MarkerWrite,
                P::MarkerSync,
                P::MarkerRename,
                P::MarkerDirSync,
            ] {
                for kind in [K::Error, K::Panic] {
                    let (_root, path, db) = fixture();
                    let files = sidecars(&path, "HNSW");
                    if create {
                        db.drop_index("", "c", "v", "").unwrap();
                        for f in &files {
                            fs::write(f, b"old").unwrap();
                        }
                    }
                    if point == P::InheritedSync {
                        db.close();
                        drop(db);
                        let db = open(&path);
                        db.collection("", "c").unwrap();
                        exercise_prepare(&path, &db, &files, create, point, kind);
                    } else {
                        exercise_prepare(&path, &db, &files, create, point, kind);
                    }
                }
            }
        }
    }
    fn exercise_prepare(path: &Path, db: &Db, files: &[PathBuf], create: bool, point: P, kind: K) {
        let before = manifest(path);
        index_intent::inject_fault(point, kind);
        let result = if create {
            db.create_index("", "c", "v", req("HNSW", "L2"))
        } else {
            db.drop_index("", "c", "v", "")
        };
        assert!(result.is_err(), "{point:?} {kind:?}");
        assert!(files.iter().all(|f| f.exists()));
        assert_eq!(manifest(path), before);
        assert!(!col(path).join("manifest.json.tmp").exists());
        assert!(
            !col(path)
                .join("milvus-lite-rust.index-intent.json.tmp")
                .exists()
        );
        assert!(!col(path).join(INTENT_FILE).exists());
    }

    #[test]
    fn interrupted_deletion_blocks_both_endpoints_and_same_boot_reopen() {
        let _case = CASE_LOCK.lock();
        let _reset = Reset;
        known(BOOT);
        for create in [false, true] {
            for point in [
                P::CapabilitySync,
                P::Delete(1),
                P::AfterLastDelete,
                P::DeleteSync,
            ] {
                for kind in [K::Error, K::Panic] {
                    let (_root, path, db) = fixture();
                    let files = sidecars(&path, "HNSW");
                    if create {
                        db.drop_index("", "c", "v", "").unwrap();
                        for f in &files {
                            fs::write(f, b"old").unwrap();
                        }
                    }
                    let before = manifest(&path);
                    index_intent::inject_fault(point, kind);
                    let result = if create {
                        db.create_index("", "c", "v", req("HNSW", "L2"))
                    } else {
                        db.drop_index("", "c", "v", "")
                    };
                    assert!(result.is_err(), "{point:?}");
                    assert_eq!(manifest(&path), before);
                    assert!(col(&path).join(INTENT_FILE).exists());
                    assert!(!col(&path).join("manifest.json.tmp").exists());
                    assert!(
                        db.create_index(
                            "",
                            "c",
                            "id",
                            IndexRequest {
                                index_type: "INVERTED".into(),
                                ..Default::default()
                            }
                        )
                        .unwrap_err()
                        .to_string()
                        .contains("blocked")
                    );
                    if !create {
                        assert!(
                            db.drop_index("", "c", "v", "")
                                .unwrap_err()
                                .to_string()
                                .contains("blocked")
                        );
                    }
                    db.close();
                    drop(db);
                    let db = open(&path);
                    assert!(
                        db.create_index(
                            "",
                            "c",
                            "id",
                            IndexRequest {
                                index_type: "INVERTED".into(),
                                ..Default::default()
                            }
                        )
                        .unwrap_err()
                        .to_string()
                        .contains("blocked")
                    );
                }
            }
        }
    }

    #[test]
    fn trusted_reboot_clears_but_unknown_sources_never_do() {
        let _case = CASE_LOCK.lock();
        let _reset = Reset;
        for old_known in [false, true] {
            for reboot in [false, true] {
                known(BOOT);
                let (_root, path, db) = fixture();
                let files = sidecars(&path, "HNSW");
                if !old_known {
                    index_intent::override_boot_context(Some(BootContext::default()));
                }
                index_intent::inject_fault(P::AfterLastDelete, K::Error);
                assert!(db.drop_index("", "c", "v", "").is_err());
                assert!(files.iter().all(|f| !f.exists()));
                db.close();
                drop(db);
                known(if reboot {
                    "22222222-2222-4222-8222-222222222222"
                } else {
                    BOOT
                });
                let db = open(&path);
                let result = db.drop_index("", "c", "v", "");
                if old_known && reboot {
                    result.unwrap();
                    assert!(!col(&path).join(INTENT_FILE).exists());
                } else {
                    assert!(result.is_err());
                    assert!(col(&path).join(INTENT_FILE).exists());
                }
            }
        }
    }

    #[test]
    fn manifest_phase_cleanup_failure_and_retry_are_independent() {
        let _case = CASE_LOCK.lock();
        let _reset = Reset;
        known(BOOT);
        for phase in [
            None,
            Some(CommitPhase::BeforeReplace),
            Some(CommitPhase::AfterReplace),
        ] {
            for point in [P::CleanupRead, P::CleanupUnlink, P::CleanupSync] {
                let (_root, path, db) = fixture();
                let files = sidecars(&path, "HNSW");
                let before = manifest(&path);
                if let Some(phase) = phase {
                    fs_commit::inject_manifest_panic_once(phase);
                }
                index_intent::inject_fault(point, K::Error);
                let result = db.drop_index("", "c", "v", "");
                let committed = phase != Some(CommitPhase::BeforeReplace);
                assert_eq!(result.is_ok(), committed);
                let c = db.collection("", "c").unwrap();
                assert_eq!(c.read().indexes().is_empty(), committed);
                assert_eq!(
                    c.read().load_state(),
                    if committed {
                        LoadState::Loaded
                    } else {
                        LoadState::Released
                    }
                );
                assert!(files.iter().all(|f| !f.exists()));
                if !committed {
                    assert_eq!(manifest(&path), before);
                    db.drop_index("", "c", "v", "").unwrap();
                } else {
                    db.create_index("", "c", "v", req("HNSW", "IP")).unwrap();
                }
                assert!(!col(&path).join(INTENT_FILE).exists());
                assert_eq!(c.read().indexes().len(), usize::from(committed));
            }
        }
    }

    #[test]
    fn lost_cleanup_knowledge_on_reopen_and_marker_tampering_fail_closed() {
        let _case = CASE_LOCK.lock();
        let _reset = Reset;
        known(BOOT);
        for reopen in [false, true] {
            let (_root, path, db) = fixture();
            sidecars(&path, "HNSW");
            index_intent::inject_fault(P::CleanupUnlink, K::Error);
            db.drop_index("", "c", "v", "").unwrap();
            if reopen {
                db.close();
                drop(db);
                let db = open(&path);
                assert!(db.create_index("", "c", "v", req("HNSW", "L2")).is_err());
            } else {
                fs::write(col(&path).join(INTENT_FILE), b"different marker").unwrap();
                let error = db
                    .create_index("", "c", "v", req("HNSW", "L2"))
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("has rebooted"));
                assert!(!error.contains("retry the index change"));
                assert_eq!(
                    fs::read(col(&path).join(INTENT_FILE)).unwrap(),
                    b"different marker"
                );
            }
        }
    }

    #[test]
    fn concurrent_load_rejects_drop_after_confirmed_delete_and_cleans_marker() {
        let _case = CASE_LOCK.lock();
        let (_root, path, db) = fixture();
        let files = sidecars(&path, "HNSW");
        let before = manifest(&path);
        let c = db.collection("", "c").unwrap();
        index_intent::before_commit_once(move || c.write().load().unwrap());
        assert_eq!(
            db.drop_index("", "c", "v", "").unwrap_err().to_string(),
            "index cannot be dropped on loaded collection; call release() first"
        );
        assert_eq!(manifest(&path), before);
        assert_eq!(
            db.collection("", "c").unwrap().read().load_state(),
            LoadState::Loaded
        );
        assert!(files.iter().all(|f| !f.exists()));
        assert!(!col(&path).join(INTENT_FILE).exists());
    }

    #[test]
    fn trace_proves_intent_sync_precedes_unlink_and_commit_follows_delete_sync() {
        let _case = CASE_LOCK.lock();
        let (_root, path, db) = fixture();
        let files = sidecars(&path, "HNSW");
        fs_commit::begin_fs_trace();
        db.drop_index("", "c", "v", "").unwrap();
        let events = fs_commit::finish_fs_trace();
        let position = |action, path: &Path| {
            events
                .iter()
                .position(|e| e.action == action && e.path == path)
                .unwrap()
        };
        let marker = position(FsAction::Rename, &col(&path).join(INTENT_FILE));
        let first = position(FsAction::Unlink, &files[0]);
        assert!(
            events[marker + 1..first]
                .iter()
                .any(|e| e.action == FsAction::SyncDir && e.path == col(&path))
        );
        let last = position(FsAction::Unlink, &files[1]);
        let commit = position(FsAction::Rename, &col(&path).join("manifest.json"));
        assert!(
            events[last + 1..commit]
                .iter()
                .any(|e| e.action == FsAction::SyncDir
                    && e.path == col(&path).join("partitions/_default/indexes"))
        );
    }

    #[test]
    fn crash_child() {
        let _case = CASE_LOCK.lock();
        let Some(path) = std::env::var_os("MLR_INDEX_CRASH_PATH") else {
            return;
        };
        let path = PathBuf::from(path);
        let db = open(&path);
        known(BOOT);
        index_intent::inject_fault(P::AfterLastDelete, K::Exit);
        db.drop_index("", "c", "v", "").unwrap();
        panic!("process did not exit");
    }
    #[test]
    fn process_exit_before_delete_sync_survives_reopen() {
        let _case = CASE_LOCK.lock();
        let _reset = Reset;
        known(BOOT);
        let (_root, path, db) = fixture();
        let files = sidecars(&path, "HNSW");
        let before = manifest(&path);
        db.close();
        drop(db);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "faults::crash_child", "--nocapture"])
            .env("MLR_INDEX_CRASH_PATH", &path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(86));
        assert_eq!(manifest(&path), before);
        assert!(files.iter().all(|f| !f.exists()));
        assert!(col(&path).join(INTENT_FILE).exists());
        let db = open(&path);
        assert!(
            db.drop_index("", "c", "v", "")
                .unwrap_err()
                .to_string()
                .contains("blocked")
        );
    }
}

#[cfg(unix)]
mod permissions {
    #![allow(unsafe_code)]
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[cfg(target_os = "linux")]
    #[test]
    fn differing_credentials_child() {
        let _case = CASE_LOCK.lock();
        if std::env::var_os("MLR_INDEX_DIFFERING_CREDENTIALS").is_none() {
            return;
        }
        // SAFETY: 只在专用子进程中改变有效凭据，真实凭据保留为 root。
        assert_eq!(unsafe { libc::setgroups(0, std::ptr::null()) }, 0);
        // SAFETY: 数值参数不引用内存；此子进程不再恢复特权。
        assert_eq!(unsafe { libc::setegid(65534) }, 0);
        // SAFETY: 父进程与其他测试的凭据不受影响。
        assert_eq!(unsafe { libc::seteuid(65534) }, 0);
        for (kind, field, metric) in [
            ("FLAT", "v", "L2"),
            ("AUTOINDEX", "v", "L2"),
            ("INVERTED", "id", "NONE"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("c.db");
            let db = open(&path);
            db.create_collection("", "c", schema()).unwrap();
            db.create_index("", "c", field, req(kind, metric)).unwrap();
            db.collection("", "c").unwrap().write().release();
            db.drop_index("", "c", field, "").unwrap();
            assert!(db.collection("", "c").unwrap().read().indexes().is_empty());
            assert!(!col(&path).join(INTENT_FILE).exists());

            // 有删除时仍保守拒绝这种凭据，不能把收窄变成绕过预检。
            db.create_index("", "c", "v", req("HNSW", "L2")).unwrap();
            db.insert("", "c", batch(1)).unwrap();
            db.flush_collection("", "c").unwrap();
            db.collection("", "c").unwrap().write().release();
            let files = sidecars(&path, "HNSW");
            let before = manifest(&path);
            let error = db.drop_index("", "c", "v", "").unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("matching real/effective credentials")
            );
            assert_eq!(manifest(&path), before);
            assert!(files.iter().all(|p| p.exists()));
            assert!(!col(&path).join(INTENT_FILE).exists());
        }
        println!("differing credentials: no-deletion changes allowed; deletion preflight retained");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn no_deletion_does_not_require_deletion_credentials() {
        let _case = CASE_LOCK.lock();
        // SAFETY: 仅查询；降权的场景在独立子进程内执行。
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "permissions::differing_credentials_child",
                "--nocapture",
            ])
            .env("MLR_INDEX_DIFFERING_CREDENTIALS", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("deletion preflight retained"));
    }

    #[test]
    fn permission_child() {
        let _case = CASE_LOCK.lock();
        let Some(root) = std::env::var_os("MLR_INDEX_PERMISSION_ROOT") else {
            return;
        };
        // SAFETY: 只在隔离的测试子进程内降权；不修改运行其他测试的父进程凭据。
        if unsafe { libc::geteuid() } == 0 {
            // SAFETY: 空组列表无需读取指针；设置的是子进程自己的 gid/uid。
            assert_eq!(unsafe { libc::setgroups(0, std::ptr::null()) }, 0);
            // SAFETY: setgid/setuid 只接收数值，进程从此以普通用户运行。
            assert_eq!(unsafe { libc::setgid(65534) }, 0);
            // SAFETY: 此测试子进程不再需要恢复特权。
            assert_eq!(unsafe { libc::setuid(65534) }, 0);
        }
        for create in [false, true] {
            for case in [
                "index_readonly",
                "collection_readonly",
                "collection_no_read",
                "tmp_directory",
                "prev_symlink",
            ] {
                let path = PathBuf::from(&root).join(format!("{create}-{case}.db"));
                let db = open(&path);
                db.create_collection("", "c", schema()).unwrap();
                db.create_index("", "c", "v", req("HNSW", "L2")).unwrap();
                for id in [1, 2] {
                    db.insert("", "c", batch(id)).unwrap();
                    db.flush_collection("", "c").unwrap();
                }
                db.collection("", "c").unwrap().write().release();
                let files = sidecars(&path, "HNSW");
                if create {
                    db.drop_index("", "c", "v", "").unwrap();
                    for file in &files {
                        fs::write(file, b"old").unwrap();
                    }
                }
                let dir = col(&path);
                let index_dir = dir.join("partitions/_default/indexes");
                let before = manifest(&path);
                match case {
                    "index_readonly" => {
                        fs::set_permissions(&index_dir, fs::Permissions::from_mode(0o555)).unwrap()
                    }
                    "collection_readonly" => {
                        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap()
                    }
                    "collection_no_read" => {
                        fs::set_permissions(&dir, fs::Permissions::from_mode(0o333)).unwrap()
                    }
                    "tmp_directory" => fs::create_dir(dir.join("manifest.json.tmp")).unwrap(),
                    _ => {
                        let prev = dir.join("manifest.json.prev");
                        if prev.exists() {
                            fs::remove_file(&prev).unwrap();
                        }
                        std::os::unix::fs::symlink(dir.join("manifest.json"), prev).unwrap();
                    }
                }
                let result = if create {
                    db.create_index("", "c", "v", req("HNSW", "L2"))
                } else {
                    db.drop_index("", "c", "v", "")
                };
                assert!(result.is_err(), "{case}");
                assert_eq!(manifest(&path), before);
                assert!(files.iter().all(|f| f.exists()));
                assert!(!dir.join(INTENT_FILE).exists());
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
                fs::set_permissions(&index_dir, fs::Permissions::from_mode(0o755)).unwrap();
                if case == "tmp_directory" {
                    fs::remove_dir(dir.join("manifest.json.tmp")).unwrap();
                }
                if case == "prev_symlink" {
                    fs::remove_file(dir.join("manifest.json.prev")).unwrap();
                }
                db.close();
            }
        }
        let owned = PathBuf::from(&root).join("owned-sticky.db");
        let db = open(&owned);
        db.create_collection("", "c", schema()).unwrap();
        db.create_index("", "c", "v", req("HNSW", "L2")).unwrap();
        db.insert("", "c", batch(1)).unwrap();
        db.flush_collection("", "c").unwrap();
        db.collection("", "c").unwrap().write().release();
        let files = sidecars(&owned, "HNSW");
        fs::set_permissions(
            col(&owned).join("partitions/_default/indexes"),
            fs::Permissions::from_mode(0o1777),
        )
        .unwrap();
        db.drop_index("", "c", "v", "").unwrap();
        assert!(files.iter().all(|f| !f.exists()));
        db.close();
        if let Some(path) = std::env::var_os("MLR_FOREIGN_STICKY_PATH") {
            let path = PathBuf::from(path);
            let db = open(&path);
            let before = manifest(&path);
            let error = db.drop_index("", "c", "v", "").unwrap_err();
            assert!(error.to_string().contains("sticky"), "{error}");
            assert_eq!(manifest(&path), before);
            assert!(!col(&path).join(INTENT_FILE).exists());
            assert_eq!(
                fs::read_dir(col(&path).join("partitions/_default/indexes"))
                    .unwrap()
                    .count(),
                2
            );
            db.close();
        }
    }
    #[test]
    fn permissions_are_checked_before_any_sidecar_is_removed() {
        let _case = CASE_LOCK.lock();
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777)).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "permissions::permission_child", "--nocapture"])
            .env("MLR_INDEX_PERMISSION_ROOT", root.path());
        let mut foreign = None;
        // SAFETY: geteuid 仅查询父进程凭据；外来所有权情形只在具备测试子进程降权条件时创建。
        if unsafe { libc::geteuid() } == 0 {
            let (held, path, db) = fixture();
            let files = sidecars(&path, "HNSW");
            db.close();
            drop(db);
            fs::set_permissions(held.path(), fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(path.join("LOCK"), fs::Permissions::from_mode(0o666)).unwrap();
            fs::set_permissions(
                col(&path).join("partitions/_default/indexes"),
                fs::Permissions::from_mode(0o1777),
            )
            .unwrap();
            command.env("MLR_FOREIGN_STICKY_PATH", &path);
            foreign = Some((held, files));
        }
        let output = command.output().unwrap();
        if let Some((_held, files)) = &foreign {
            assert!(files.iter().all(|f| f.exists()));
        }
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_delete_acl_and_manifest_protection_fail_in_preflight() {
        let _case = CASE_LOCK.lock();
        for create in [false, true] {
            for case in [
                "index_acl",
                "index_immutable",
                "index_append",
                "index_dir_immutable",
                "target_acl",
                "target_immutable",
                "target_append",
                "tmp_inherit_acl",
            ] {
                let (_root, path, db) = fixture();
                let files = sidecars(&path, "HNSW");
                if create {
                    db.drop_index("", "c", "v", "").unwrap();
                    for f in &files {
                        fs::write(f, b"old").unwrap();
                    }
                }
                let dir = col(&path);
                let target = if case == "index_dir_immutable" {
                    dir.join("partitions/_default/indexes")
                } else if case.starts_with("index_") {
                    files[1].clone()
                } else if case == "tmp_inherit_acl" {
                    dir.clone()
                } else {
                    dir.join("manifest.json")
                };
                let before = manifest(&path);
                let user = String::from_utf8(
                    std::process::Command::new("id")
                        .arg("-un")
                        .output()
                        .unwrap()
                        .stdout,
                )
                .unwrap();
                if case.ends_with("acl") {
                    let acl = format!(
                        "user:{} deny delete{}",
                        user.trim(),
                        if case == "tmp_inherit_acl" {
                            ",file_inherit"
                        } else {
                            ""
                        }
                    );
                    assert!(
                        std::process::Command::new("chmod")
                            .args(["+a", &acl])
                            .arg(&target)
                            .status()
                            .unwrap()
                            .success()
                    );
                } else {
                    let path_c = std::ffi::CString::new(target.to_str().unwrap()).unwrap();
                    // SAFETY: 临时目录中的路径 NUL 结尾；只设置当前测试拥有的文件标志。
                    assert_eq!(
                        unsafe {
                            libc::chflags(
                                path_c.as_ptr(),
                                if case.ends_with("immutable") {
                                    libc::UF_IMMUTABLE
                                } else {
                                    libc::UF_APPEND
                                },
                            )
                        },
                        0
                    );
                }
                let result = if create {
                    db.create_index("", "c", "v", req("HNSW", "L2"))
                } else {
                    db.drop_index("", "c", "v", "")
                };
                assert!(result.is_err(), "{case}");
                assert_eq!(manifest(&path), before);
                assert!(files.iter().all(|f| f.exists()));
                assert!(!dir.join(INTENT_FILE).exists());
                if case.ends_with("acl") {
                    assert!(
                        std::process::Command::new("chmod")
                            .arg("-N")
                            .arg(&target)
                            .status()
                            .unwrap()
                            .success()
                    );
                } else {
                    let path_c = std::ffi::CString::new(target.to_str().unwrap()).unwrap();
                    // SAFETY: 清除当前测试设置的保护标志，以便临时目录正常回收。
                    assert_eq!(unsafe { libc::chflags(path_c.as_ptr(), 0) }, 0);
                }
                if dir.join("manifest.json.tmp").exists() {
                    assert!(
                        std::process::Command::new("chmod")
                            .arg("-N")
                            .arg(dir.join("manifest.json.tmp"))
                            .status()
                            .unwrap()
                            .success()
                    );
                    fs::remove_file(dir.join("manifest.json.tmp")).unwrap();
                }
            }
        }
    }
}

#[test]
fn manifest_version_exhaustion_rejects_before_any_deletion() {
    let _case = CASE_LOCK.lock();
    let (_root, path, db) = fixture();
    let files = sidecars(&path, "HNSW");
    db.close();
    drop(db);
    let file = col(&path).join("manifest.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    value["version"] = u64::MAX.into();
    fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
    let before = manifest(&path);
    let db = open(&path);
    assert!(
        db.drop_index("", "c", "v", "")
            .unwrap_err()
            .to_string()
            .contains("version space exhausted")
    );
    assert_eq!(manifest(&path), before);
    assert!(files.iter().all(|f| f.exists()));
    assert!(!col(&path).join(INTENT_FILE).exists());
    assert!(!col(&path).join("manifest.json.tmp").exists());
}

#[cfg(feature = "fault-injection")]
#[test]
fn cleanup_failure_during_retry_is_not_immediately_retried_again() {
    let _case = CASE_LOCK.lock();
    use milvus_lite_core::storage::index_intent::{FaultKind as K, IndexPoint as P, inject_fault};
    let (_root, path, db) = fixture();
    sidecars(&path, "HNSW");
    inject_fault(P::CleanupUnlink, K::Error);
    db.drop_index("", "c", "v", "").unwrap();
    assert!(col(&path).join(INTENT_FILE).exists());
    inject_fault(P::CleanupRead, K::Error);
    let error = db
        .create_index("", "c", "v", req("HNSW", "L2"))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("retry the index change in this service instance")
    );
    assert!(col(&path).join(INTENT_FILE).exists());
    assert!(db.collection("", "c").unwrap().read().indexes().is_empty());
    db.create_index("", "c", "v", req("HNSW", "L2")).unwrap();
    assert!(!col(&path).join(INTENT_FILE).exists());
}

#[cfg(feature = "fault-injection")]
#[test]
fn marker_preparation_failure_and_cleanup_failure_preserve_safe_retry_state() {
    let _case = CASE_LOCK.lock();
    use milvus_lite_core::storage::index_intent::{FaultKind as K, IndexPoint as P, inject_fault};
    for create in [false, true] {
        let (_root, path, db) = fixture();
        let files = sidecars(&path, "HNSW");
        if create {
            db.drop_index("", "c", "v", "").unwrap();
            for file in &files {
                fs::write(file, b"old").unwrap();
            }
        }
        let before = manifest(&path);
        inject_fault(P::MarkerDirSync, K::Error);
        inject_fault(P::CleanupUnlink, K::Error);
        let result = if create {
            db.create_index("", "c", "v", req("HNSW", "L2"))
        } else {
            db.drop_index("", "c", "v", "")
        };
        assert!(result.is_err());
        assert!(files.iter().all(|f| f.exists()));
        assert_eq!(manifest(&path), before);
        assert!(col(&path).join(INTENT_FILE).exists());
        if create {
            db.create_index("", "c", "v", req("HNSW", "L2")).unwrap();
        } else {
            db.drop_index("", "c", "v", "").unwrap();
        }
        assert!(!col(&path).join(INTENT_FILE).exists());
        assert!(files.iter().all(|f| !f.exists()));
    }
}

#[test]
fn every_pinning_snapshot_is_checked_before_any_stale_file_deletion() {
    let _case = CASE_LOCK.lock();
    let (_root, path, db) = fixture();
    let files = sidecars(&path, "HNSW");
    snapshot(&path, &files);
    let dir = col(&path);
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("snapshots/metadata/s.json")).unwrap()).unwrap();
    metadata["name"] = "other".into();
    metadata["id"] = "other".into();
    metadata["manifest_file"] = "snapshots/manifests/other/manifest.json".into();
    // 历史 Windows snapshot：合并 pins 与逐份 manifest 比对都须规范化路径。
    for file in metadata["index_files"]["_default"].as_array_mut().unwrap() {
        *file = file.as_str().unwrap().replace('/', "\\").into();
    }
    let copy = dir.join("snapshots/manifests/other");
    fs::create_dir_all(&copy).unwrap();
    let mut saved: serde_json::Value = serde_json::from_slice(&manifest(&path)).unwrap();
    saved["index_specs"]["v"]["metric_type"] = "COSINE".into();
    fs::write(
        copy.join("manifest.json"),
        serde_json::to_vec(&saved).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join("snapshots/metadata/other.json"),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
    db.drop_index("", "c", "v", "").unwrap();
    let before = manifest(&path);
    assert!(
        db.create_index("", "c", "v", req("HNSW", "L2"))
            .unwrap_err()
            .to_string()
            .contains("other")
    );
    assert_eq!(manifest(&path), before);
    assert!(files.iter().all(|p| p.exists()));
}

#[test]
fn windows_snapshot_pins_survive_drop_and_same_definition_recreation() {
    let _case = CASE_LOCK.lock();
    let (_root, path, db) = fixture();
    let files = sidecars(&path, "HNSW");
    snapshot(&path, &files);
    let metadata = col(&path).join("snapshots/metadata/s.json");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&fs::read(&metadata).unwrap()).unwrap();
    for map in ["data_files", "delta_files", "index_files"] {
        for refs in saved[map].as_object_mut().unwrap().values_mut() {
            for file in refs.as_array_mut().unwrap() {
                *file = file.as_str().unwrap().replace('/', "\\").into();
            }
        }
    }
    fs::write(metadata, serde_json::to_vec(&saved).unwrap()).unwrap();
    db.drop_index("", "c", "v", "").unwrap();
    assert!(files.iter().all(|p| p.exists()));
    let before = manifest(&path);
    assert!(
        db.create_index("", "c", "v", req("HNSW", "COSINE"))
            .is_err()
    );
    assert_eq!(manifest(&path), before);
    db.create_index("", "c", "v", req("HNSW", "L2")).unwrap();
    assert!(
        files
            .iter()
            .all(|p| fs::read(p).unwrap() == b"retained 3.x sidecar")
    );
}

#[cfg(feature = "fault-injection")]
#[test]
fn concurrent_load_error_keeps_its_category_when_marker_cleanup_also_fails() {
    let _case = CASE_LOCK.lock();
    use milvus_lite_core::storage::index_intent::{
        FaultKind as K, IndexPoint as P, before_commit_once, inject_fault,
    };
    let (_root, path, db) = fixture();
    sidecars(&path, "HNSW");
    let c = db.collection("", "c").unwrap();
    let before = manifest(&path);
    before_commit_once(move || {
        c.write().load().unwrap();
        inject_fault(P::CleanupUnlink, K::Error);
    });
    let error = db.drop_index("", "c", "v", "").unwrap_err();
    assert!(matches!(error, EngineError::SchemaValidation(_)), "{error}");
    assert!(
        error
            .to_string()
            .starts_with("index cannot be dropped on loaded collection; call release() first")
    );
    assert_eq!(manifest(&path), before);
    assert!(col(&path).join(INTENT_FILE).exists());
    db.collection("", "c").unwrap().write().release();
    db.drop_index("", "c", "v", "").unwrap();
    assert!(!col(&path).join(INTENT_FILE).exists());
}
