use milvus_lite_core::Pk;
use milvus_lite_core::storage::visibility::partitions::{
    PartitionRow, PartitionVisibility, Removal, SourceKind, SourceReplacement,
};
use milvus_lite_core::storage::visibility::{RowLocation, SourceId};

fn row(partition: u32, source: u64, at: usize, seq: u64) -> PartitionRow {
    PartitionRow {
        partition,
        location: RowLocation {
            source: SourceId(source),
            row: at,
            seq,
        },
    }
}

#[test]
fn active_delete_clears_but_frozen_and_committed_deletes_survive_new_active_insert() {
    for committed in [false, true] {
        let pk = Pk::VarChar("键".into());
        let mut vis = PartitionVisibility::default();
        vis.register_source(SourceId(1), 1, SourceKind::Immutable(0))
            .unwrap();
        vis.register_source(SourceId(2), 1, SourceKind::Active)
            .unwrap();
        vis.insert(pk.clone(), row(0, 1, 0, 1)).unwrap();
        vis.delete(SourceId(2), pk.clone(), 2).unwrap();
        let deleted = vis.snapshot(Some(&[0]));
        if committed {
            vis.freeze(SourceId(2)).unwrap();
            vis.commit_deletes(SourceId(2)).unwrap();
            vis.register_source(SourceId(3), 1, SourceKind::Active)
                .unwrap();
        }
        vis.insert(pk.clone(), row(1, if committed { 3 } else { 2 }, 0, 3))
            .unwrap();
        assert_eq!(vis.snapshot(Some(&[0])).live_count, usize::from(!committed));
        assert_eq!(vis.snapshot(None).live_count, 1);
        assert_eq!(deleted.live_count, 0);
        assert_eq!(deleted.is_live(SourceId(1), 0), Some(false));
    }
    let mut vis = PartitionVisibility::default();
    vis.register_source(SourceId(1), 1, SourceKind::Immutable(0))
        .unwrap();
    vis.register_source(SourceId(2), 0, SourceKind::Active)
        .unwrap();
    vis.insert(Pk::Int64(1), row(0, 1, 0, 1)).unwrap();
    vis.delete(SourceId(2), Pk::Int64(1), 2).unwrap();
    vis.freeze(SourceId(2)).unwrap();
    vis.register_source(SourceId(3), 1, SourceKind::Active)
        .unwrap();
    vis.insert(Pk::Int64(1), row(1, 3, 0, 3)).unwrap();
    assert_eq!(vis.snapshot(Some(&[0])).live_count, 0);
    vis.commit_deletes(SourceId(2)).unwrap();
    assert_eq!(vis.snapshot(Some(&[0])).live_count, 0);
}

#[test]
fn active_cross_partition_overwrite_returns_to_the_existing_segment() {
    let mut vis = PartitionVisibility::default();
    vis.register_source(SourceId(1), 1, SourceKind::Immutable(0))
        .unwrap();
    vis.register_source(SourceId(2), 2, SourceKind::Active)
        .unwrap();
    vis.insert(Pk::Int64(1), row(0, 1, 0, 1)).unwrap();
    vis.insert(Pk::Int64(1), row(0, 2, 0, 2)).unwrap();
    let old = vis.snapshot(Some(&[0]));
    vis.insert(Pk::Int64(1), row(1, 2, 1, 3)).unwrap();
    assert_eq!(
        vis.visible_data(&Pk::Int64(1), Some(&[0])),
        Some(row(0, 1, 0, 1).location)
    );
    assert_eq!(old.is_live(SourceId(2), 0), Some(true));
    assert_eq!(
        vis.snapshot(Some(&[0])).is_live(SourceId(2), 0),
        Some(false)
    );
    assert_eq!(vis.snapshot(Some(&[0])).is_live(SourceId(2), 2), None);
    assert_eq!(vis.snapshot(Some(&[0])).is_live(SourceId(99), 0), None);
    assert_eq!(vis.snapshot(Some(&[0, 1])).live_count, 1);
}

#[test]
fn one_partition_change_updates_only_its_incident_relations() {
    let mut vis = PartitionVisibility::default();
    for part in 0..256 {
        vis.register_source(SourceId(part as u64 + 1), 2, SourceKind::Immutable(part))
            .unwrap();
        vis.insert(Pk::Int64(1), row(part, part as u64 + 1, 0, part as u64 + 1))
            .unwrap();
    }
    let old = vis.snapshot(None);
    let stats = vis.stats();
    vis.insert(Pk::Int64(1), row(0, 1, 1, 300)).unwrap();
    assert_eq!(
        vis.stats().version_comparisons - stats.version_comparisons,
        255
    );
    assert!(vis.stats().relation_edits - stats.relation_edits <= 3 * 255);
    assert_eq!(vis.stats().relations, 256 * 255 / 2);
    assert_eq!(old.is_live(SourceId(256), 0), Some(true));
    let stats = vis.stats();
    vis.committed_delete(Pk::Int64(1), 999).unwrap();
    assert_eq!(vis.stats().relation_edits, stats.relation_edits);
    assert_eq!(vis.snapshot(None).live_count, 0);
    assert!(vis.gc_committed_before(Some(1000)).is_err());
    assert_eq!(vis.snapshot(None).live_count, 0);
}

#[test]
fn invalid_mapping_rejects_before_mutation_and_valid_mapping_keeps_old_snapshot() {
    let mut vis = PartitionVisibility::default();
    vis.register_source(SourceId(1), 1, SourceKind::Immutable(0))
        .unwrap();
    vis.register_source(SourceId(2), 0, SourceKind::Active)
        .unwrap();
    vis.insert(Pk::Int64(1), row(0, 1, 0, 1)).unwrap();
    let old = vis.snapshot(None);
    let target = row(0, 99, 0, 1).location;
    let input = |pk| {
        [SourceReplacement {
            source: SourceId(1),
            rows: vec![(Pk::Int64(pk), 0, Some(target))],
        }]
    };
    let output = [(SourceId(99), 1, SourceKind::Immutable(0))];
    let order = [SourceId(99), SourceId(2)];
    assert!(
        vis.replace_sources(&input(2), &output, &order, Removal::Replace)
            .is_err()
    );
    assert_eq!(vis.snapshot(None).source_order, old.source_order);
    assert_eq!(
        vis.latest_data(&Pk::Int64(1)),
        Some(row(0, 1, 0, 1).location)
    );
    vis.replace_sources(&input(1), &output, &order, Removal::Replace)
        .unwrap();
    assert_eq!(vis.latest_data(&Pk::Int64(1)), Some(target));
    assert_eq!(old.is_live(SourceId(1), 0), Some(true));
    assert_eq!(vis.snapshot(None).live_count, 1);
}

#[test]
fn replacement_protects_a_locally_visible_row_even_when_globally_hidden() {
    let mut vis = PartitionVisibility::default();
    for part in 0..2 {
        vis.register_source(SourceId(part as u64 + 1), 1, SourceKind::Immutable(part))
            .unwrap();
        vis.insert(Pk::Int64(1), row(part, part as u64 + 1, 0, part as u64 + 1))
            .unwrap();
    }
    let input = [SourceReplacement {
        source: SourceId(1),
        rows: vec![(Pk::Int64(1), 0, None)],
    }];
    assert!(
        vis.replace_sources(&input, &[], &[SourceId(2)], Removal::Replace)
            .is_err()
    );
    assert_eq!(vis.snapshot(Some(&[0])).live_count, 1);
    vis.replace_sources(&input, &[], &[SourceId(2)], Removal::DropPartition(0))
        .unwrap();
    assert_eq!(vis.snapshot(Some(&[0])).live_count, 0);
    assert_eq!(vis.snapshot(None).live_count, 1);
}

#[test]
fn invalid_append_and_uncommitted_source_replacement_keep_the_original_state() {
    let mut vis = PartitionVisibility::default();
    vis.register_source(SourceId(1), 1, SourceKind::Active)
        .unwrap();
    assert!(vis.insert(Pk::Int64(1), row(0, 1, 1, 1)).is_err());
    assert_eq!(vis.stats().keys, 0);
    vis.delete(SourceId(1), Pk::Int64(1), 2).unwrap();
    vis.freeze(SourceId(1)).unwrap();
    assert!(vis.insert(Pk::Int64(1), row(0, 1, 0, 3)).is_err());
    let input = [SourceReplacement {
        source: SourceId(1),
        rows: vec![(Pk::Int64(1), 0, None)],
    }];
    assert!(
        vis.replace_sources(&input, &[], &[], Removal::Replace)
            .is_err()
    );
    vis.commit_deletes(SourceId(1)).unwrap();
    vis.replace_sources(&input, &[], &[], Removal::Replace)
        .unwrap();
    assert_eq!(vis.snapshot(None).live_count, 0);
}

#[test]
fn empty_resolved_selection_means_no_matches_and_does_not_become_all() {
    let mut vis = PartitionVisibility::default();
    vis.register_source(SourceId(1), 1, SourceKind::Immutable(0))
        .unwrap();
    vis.insert(Pk::Int64(1), row(0, 1, 0, 1)).unwrap();
    assert_eq!(vis.snapshot(None).live_count, 1);
    assert_eq!(vis.snapshot(Some(&[])).live_count, 0);
    assert_eq!(vis.visible_data(&Pk::Int64(1), Some(&[])), None);
}
