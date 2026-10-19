//! 分区位图的隔离组件模型；输入物理候选与删除状态，独立核对参照输出，不接入引擎。

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Instant;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type At = (usize, usize);
type Sparse = BTreeMap<usize, u64>;
type SnapshotSource = (Arc<Vec<Row>>, Arc<Vec<u64>>);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
enum Key {
    Int(i64),
    Text(String),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct InputRow {
    pk: Key,
    seq: u64,
    tag: String,
    partition: Option<String>,
}

#[derive(Deserialize)]
struct InputSource {
    name: String,
    partition: Option<String>,
    rows: Vec<InputRow>,
}

#[derive(Deserialize)]
struct Delete {
    pk: Key,
    seq: u64,
}

#[derive(Deserialize)]
struct State {
    sources: Vec<InputSource>,
    active_deletes: Vec<Delete>,
    #[serde(default)]
    frozen_deletes: Vec<Delete>,
    committed_deletes: Vec<Delete>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    pk: Key,
    seq: u64,
    tag: String,
    partition: usize,
}

#[derive(Clone, Copy)]
struct Winner {
    at: At,
    seq: u64,
    partition: usize,
}

enum Locations {
    One(At),
    Many(Vec<At>),
}

impl Locations {
    fn rows(&self) -> &[At] {
        match self {
            Self::One(at) => std::slice::from_ref(at),
            Self::Many(rows) => rows,
        }
    }

    fn insert(&mut self, at: At) {
        match self {
            Self::One(old) if *old != at => *self = Self::Many(vec![*old, at]),
            Self::Many(rows) if !rows.contains(&at) => rows.push(at),
            _ => {}
        }
    }

    fn remove(&mut self, at: At) -> bool {
        match self {
            Self::One(old) => *old == at,
            Self::Many(rows) => {
                rows.retain(|old| *old != at);
                if rows.len() == 1 {
                    *self = Self::One(rows[0]);
                }
                false
            }
        }
    }
}

#[derive(Default)]
struct Model {
    partitions: BTreeMap<String, usize>,
    names: HashMap<String, usize>,
    sources: HashMap<usize, Arc<Vec<Row>>>,
    order: Vec<usize>,
    ranks: HashMap<usize, usize>,
    index: HashMap<Key, Locations>,
    active: HashMap<Key, u64>,
    frozen: HashMap<Key, u64>,
    committed: HashMap<Key, u64>,
    local: BTreeMap<(usize, usize), Arc<Vec<u64>>>,
    global: BTreeMap<usize, Arc<Vec<u64>>>,
    shadows: BTreeMap<(usize, usize), Arc<Sparse>>,
    refreshed: usize,
}

#[derive(Clone)]
struct Snapshot {
    sources: Vec<SnapshotSource>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Output {
    pk: Key,
    tag: String,
}

impl Snapshot {
    fn count(&self) -> usize {
        self.sources
            .iter()
            .flat_map(|(_, bits)| bits.iter())
            .map(|w| w.count_ones() as usize)
            .sum()
    }

    fn rows(&self) -> Vec<Output> {
        let mut result = Vec::new();
        for (rows, bits) in &self.sources {
            for (word, bits) in bits.iter().enumerate() {
                let mut left = *bits;
                while left != 0 {
                    let bit = left.trailing_zeros() as usize;
                    let row = &rows[word * 64 + bit];
                    result.push(Output {
                        pk: row.pk.clone(),
                        tag: row.tag.clone(),
                    });
                    left &= left - 1;
                }
            }
        }
        result
    }
}

impl Model {
    fn partition(&mut self, name: &str) -> usize {
        let next = self.partitions.len();
        *self.partitions.entry(name.to_owned()).or_insert(next)
    }

    fn priority(
        &self,
        winner: Winner,
    ) -> (u64, std::cmp::Reverse<usize>, std::cmp::Reverse<usize>) {
        (
            winner.seq,
            std::cmp::Reverse(self.ranks[&winner.at.0]),
            std::cmp::Reverse(winner.at.1),
        )
    }

    fn clear_key(&mut self, pk: &Key) {
        let old = self.key_winners(pk);
        for winner in &old {
            let (source, row) = winner.at;
            let bit = !(1 << (row % 64));
            if let Some(bits) = self.local.get_mut(&(source, winner.partition)) {
                Arc::make_mut(bits)[row / 64] &= bit;
            }
            if let Some(bits) = self.global.get_mut(&source) {
                Arc::make_mut(bits)[row / 64] &= bit;
            }
            for other in &old {
                let key = (source, other.partition);
                if let Some(bits) = self.shadows.get_mut(&key) {
                    let bits = Arc::make_mut(bits);
                    if let Some(word) = bits.get_mut(&(row / 64)) {
                        *word &= bit;
                        if *word == 0 {
                            bits.remove(&(row / 64));
                        }
                    }
                    if bits.is_empty() {
                        self.shadows.remove(&key);
                    }
                }
            }
        }
    }

    fn key_winners(&self, pk: &Key) -> Vec<Winner> {
        let mut winners = BTreeMap::<usize, Winner>::new();
        if let Some(locations) = self.index.get(pk) {
            if let Locations::One(at) = locations {
                let row = &self.sources[&at.0][at.1];
                return vec![Winner {
                    at: *at,
                    seq: row.seq,
                    partition: row.partition,
                }];
            }
            for &at in locations.rows() {
                let row = &self.sources[&at.0][at.1];
                let candidate = Winner {
                    at,
                    seq: row.seq,
                    partition: row.partition,
                };
                let old = winners.get(&row.partition);
                if old.is_none_or(|old| self.priority(candidate) > self.priority(*old)) {
                    winners.insert(row.partition, candidate);
                }
            }
        }
        winners.into_values().collect()
    }

    fn refresh(&mut self, pk: Key) -> Result<()> {
        self.refreshed += 1;
        let winners = self.key_winners(&pk);
        let deleted = self
            .active
            .get(&pk)
            .into_iter()
            .chain(self.frozen.get(&pk))
            .chain(self.committed.get(&pk))
            .copied()
            .max();
        let best = winners
            .iter()
            .copied()
            .max_by_key(|winner| self.priority(*winner));
        for winner in &winners {
            if deleted.is_some_and(|seq| seq > winner.seq) {
                continue;
            }
            let (source, row) = winner.at;
            let words = self.sources[&source].len().div_ceil(64);
            let bits = self
                .local
                .entry((source, winner.partition))
                .or_insert_with(|| Arc::new(vec![0; words]));
            Arc::make_mut(bits)[row / 64] |= 1 << (row % 64);
            if best.is_some_and(|best| best.at == winner.at) {
                let global = self.global.get_mut(&source).ok_or("已登记源缺少全集位图")?;
                Arc::make_mut(global)[row / 64] |= 1 << (row % 64);
            }
            for other in &winners {
                if self.priority(*other) > self.priority(*winner) {
                    let bits = self.shadows.entry((source, other.partition)).or_default();
                    *Arc::make_mut(bits).entry(row / 64).or_default() |= 1 << (row % 64);
                }
            }
        }
        Ok(())
    }

    fn apply(&mut self, state: State) -> Result<()> {
        let mut incoming = HashMap::new();
        let mut order = Vec::new();
        for input in state.sources {
            let next = self.names.len();
            let source = *self.names.entry(input.name).or_insert(next);
            let mut rows = Vec::new();
            for row in input.rows {
                let name = row
                    .partition
                    .as_ref()
                    .or(input.partition.as_ref())
                    .ok_or("候选缺少分区")?;
                let partition = self.partition(name);
                rows.push(Row {
                    pk: row.pk,
                    seq: row.seq,
                    tag: row.tag,
                    partition,
                });
            }
            if incoming.insert(source, Arc::new(rows)).is_some() {
                return Err("重复数据源".into());
            }
            order.push(source);
        }
        let active: HashMap<_, _> = state
            .active_deletes
            .into_iter()
            .map(|d| (d.pk, d.seq))
            .collect();
        let committed: HashMap<_, _> = state
            .committed_deletes
            .into_iter()
            .map(|d| (d.pk, d.seq))
            .collect();
        let frozen: HashMap<_, _> = state
            .frozen_deletes
            .into_iter()
            .map(|d| (d.pk, d.seq))
            .collect();
        let changed: HashSet<_> = self
            .sources
            .keys()
            .chain(incoming.keys())
            .copied()
            .filter(|source| self.sources.get(source) != incoming.get(source))
            .collect();
        let mut affected = HashSet::new();
        for source in &changed {
            for rows in [self.sources.get(source), incoming.get(source)]
                .into_iter()
                .flatten()
            {
                affected.extend(rows.iter().map(|r| r.pk.clone()));
            }
        }
        for (old, new) in [
            (&self.active, &active),
            (&self.frozen, &frozen),
            (&self.committed, &committed),
        ] {
            affected.extend(
                old.keys()
                    .chain(new.keys())
                    .filter(|pk| old.get(*pk) != new.get(*pk))
                    .cloned(),
            );
        }
        let old_common: Vec<_> = self
            .order
            .iter()
            .filter(|id| incoming.contains_key(id))
            .collect();
        let new_common: Vec<_> = order
            .iter()
            .filter(|id| self.sources.contains_key(id))
            .collect();
        if old_common != new_common {
            affected.extend(self.index.keys().cloned());
        }
        for pk in &affected {
            self.clear_key(pk);
        }
        for source in &changed {
            if let Some(rows) = self.sources.remove(source) {
                for (row, data) in rows.iter().enumerate() {
                    if let Some(entries) = self.index.get_mut(&data.pk)
                        && entries.remove((*source, row))
                    {
                        self.index.remove(&data.pk);
                    }
                }
            }
            self.local.retain(|(id, _), _| id != source);
            self.shadows.retain(|(id, _), _| id != source);
            self.global.remove(source);
            if let Some(rows) = incoming.remove(source) {
                for (row, data) in rows.iter().enumerate() {
                    match self.index.entry(data.pk.clone()) {
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            entry.insert(Locations::One((*source, row)));
                        }
                        std::collections::hash_map::Entry::Occupied(mut entry) => {
                            entry.get_mut().insert((*source, row))
                        }
                    }
                }
                self.global
                    .insert(*source, Arc::new(vec![0; rows.len().div_ceil(64)]));
                self.sources.insert(*source, rows);
            }
        }
        self.order = order;
        self.ranks = self
            .order
            .iter()
            .enumerate()
            .map(|(rank, &id)| (id, rank))
            .collect();
        self.active = active;
        self.frozen = frozen;
        self.committed = committed;
        for pk in affected {
            self.refresh(pk)?;
        }
        Ok(())
    }

    fn snapshot(&self, parts: Option<&[String]>) -> Snapshot {
        let selected = parts.filter(|parts| !parts.is_empty()).map(|parts| {
            parts
                .iter()
                .filter_map(|name| self.partitions.get(name).copied())
                .collect::<HashSet<_>>()
        });
        if selected.as_ref().is_some_and(HashSet::is_empty) {
            return Snapshot {
                sources: Vec::new(),
            };
        }
        let mut result = Vec::new();
        for &source in &self.order {
            let bits = if let Some(selected) = &selected {
                if selected.len() == 1 {
                    if let Some(local) = selected
                        .iter()
                        .next()
                        .and_then(|p| self.local.get(&(source, *p)))
                    {
                        result.push((self.sources[&source].clone(), local.clone()));
                    }
                    continue;
                }
                let mut local = self
                    .local
                    .range((source, 0)..=(source, usize::MAX))
                    .filter(|((_, p), _)| selected.contains(p))
                    .peekable();
                if local.peek().is_none() {
                    continue;
                }
                let mut bits = vec![0; self.sources[&source].len().div_ceil(64)];
                for (_, local) in local {
                    for (dst, src) in bits.iter_mut().zip(local.iter()) {
                        *dst |= src;
                    }
                }
                // 先合并本地候选再统一扣除，不能让后一个 OR 加回已遮住的位。
                for ((_, partition), shadow) in
                    self.shadows.range((source, 0)..=(source, usize::MAX))
                {
                    if selected.contains(partition) {
                        for (&word, &mask) in shadow.iter() {
                            bits[word] &= !mask;
                        }
                    }
                }
                Arc::new(bits)
            } else {
                self.global[&source].clone()
            };
            result.push((self.sources[&source].clone(), bits));
        }
        Snapshot { sources: result }
    }

    fn stats(&self) -> serde_json::Value {
        serde_json::json!({"physical_rows": self.sources.values().map(|r| r.len()).sum::<usize>(),
            "relations": self.shadows.len(), "shadow_words": self.shadows.values().map(|b| b.len()).sum::<usize>(),
            "local_words": self.local.values().map(|b| b.len()).sum::<usize>(),
            "global_words": self.global.values().map(|b| b.len()).sum::<usize>(), "key_refreshes": self.refreshed})
    }
}

#[derive(Deserialize)]
struct Query {
    partitions: Option<Vec<String>>,
    expected: Vec<Output>,
}

#[derive(Deserialize)]
struct Step {
    label: String,
    state: State,
    queries: Vec<Query>,
}

fn verify(path: &str) -> Result<()> {
    let fixture: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let cases = fixture["cases"].as_object().ok_or("缺少 cases")?;
    let mut checks = 0;
    let mut old_checks = 0;
    let mut max_relations = 0;
    for (name, case) in cases {
        if name.starts_with("raw/") {
            continue;
        }
        let steps: Vec<Step> = serde_json::from_value(case.clone())?;
        let mut model = Model::default();
        let mut old = Vec::<(Snapshot, Vec<Output>)>::new();
        for step in steps {
            model.apply(step.state)?;
            for (snapshot, expected) in &old {
                if snapshot.rows() != *expected {
                    return Err(format!("旧快照变化 {name}/{}", step.label).into());
                }
                old_checks += 1;
            }
            old.clear();
            for query in step.queries {
                let snapshot = model.snapshot(query.partitions.as_deref());
                if snapshot.rows() != query.expected || snapshot.count() != query.expected.len() {
                    return Err(format!(
                        "组件结果不同 {name}/{} {:?}: {:?} != {:?}",
                        step.label,
                        query.partitions,
                        snapshot.rows(),
                        query.expected
                    )
                    .into());
                }
                old.push((snapshot, query.expected));
                checks += 1;
            }
            max_relations = max_relations.max(model.shadows.len());
        }
    }
    if checks == 0 {
        return Err("没有组件核验".into());
    }
    println!(
        "{}",
        serde_json::json!({"query_checks": checks, "old_snapshot_checks": old_checks, "max_relations": max_relations})
    );
    Ok(())
}

fn verify_core(path: &str) -> Result<()> {
    use milvus_lite_core::Pk;
    use milvus_lite_core::storage::visibility::partitions::{
        PartitionRow, PartitionVisibility, SourceKind,
    };
    use milvus_lite_core::storage::visibility::{RowLocation, SourceId};
    let key = |key: &Key| match key {
        Key::Int(value) => Pk::Int64(*value),
        Key::Text(value) => Pk::VarChar(value.clone()),
    };
    let fixture: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let mut checks = 0;
    for (name, case) in fixture["cases"].as_object().ok_or("cases missing")? {
        if name.starts_with("raw/") {
            continue;
        }
        let steps: Vec<Step> = serde_json::from_value(case.clone())?;
        for step in steps {
            let mut parts = BTreeMap::<String, u32>::new();
            let mut intern = |part: &str| {
                let next = parts.len() as u32;
                *parts.entry(part.to_owned()).or_insert(next)
            };
            let mut visibility = PartitionVisibility::default();
            let mut active = None;
            let mut sequences = 0;
            for (index, source) in step.state.sources.iter().enumerate() {
                let id = SourceId(index as u64);
                let kind = if let Some(part) = &source.partition {
                    SourceKind::Immutable(intern(part))
                } else {
                    active = Some(id);
                    SourceKind::Active
                };
                visibility.register_source(id, source.rows.len(), kind)?;
                for (row, data) in source.rows.iter().enumerate() {
                    let part = data
                        .partition
                        .as_ref()
                        .or(source.partition.as_ref())
                        .ok_or("row partition missing")?;
                    visibility.insert(
                        key(&data.pk),
                        PartitionRow {
                            partition: intern(part),
                            location: RowLocation {
                                source: id,
                                row,
                                seq: data.seq,
                            },
                        },
                    )?;
                    sequences = sequences.max(data.seq);
                }
            }
            for delete in &step.state.committed_deletes {
                visibility.committed_delete(key(&delete.pk), delete.seq)?;
                sequences = sequences.max(delete.seq);
            }
            for delete in &step.state.active_deletes {
                visibility.delete(
                    active.ok_or("active source missing")?,
                    key(&delete.pk),
                    delete.seq,
                )?;
                sequences = sequences.max(delete.seq);
            }
            let materialize =
                |snapshot: &milvus_lite_core::storage::visibility::VisibilitySnapshot| {
                    step.state
                        .sources
                        .iter()
                        .enumerate()
                        .flat_map(|(source, data)| {
                            data.rows
                                .iter()
                                .enumerate()
                                .filter(move |(row, _)| {
                                    snapshot.is_live(SourceId(source as u64), *row) == Some(true)
                                })
                                .map(|(_, data)| Output {
                                    pk: data.pk.clone(),
                                    tag: data.tag.clone(),
                                })
                        })
                        .collect::<Vec<_>>()
                };
            let mut snapshots = Vec::new();
            for query in step.queries {
                let selected = query
                    .partitions
                    .as_ref()
                    .map(|names| names.iter().map(|part| intern(part)).collect::<Vec<_>>());
                let snapshot = visibility.snapshot(selected.as_deref());
                if materialize(&snapshot) != query.expected
                    || snapshot.live_count != query.expected.len()
                {
                    return Err(format!("core visibility differs {name}/{}", step.label).into());
                }
                snapshots.push((snapshot, query.expected));
                checks += 1;
            }
            let first = materialize(&visibility.snapshot(None)).into_iter().next();
            if let Some(first) = first {
                visibility.committed_delete(key(&first.pk), sequences + 1)?;
            }
            for (snapshot, expected) in snapshots {
                if materialize(&snapshot) != expected {
                    return Err("core old snapshot changed".into());
                }
            }
        }
    }
    println!(
        "{}",
        serde_json::json!({"core_query_checks":checks,"core_old_snapshot_checks":checks})
    );
    Ok(())
}

fn barrier() -> Result<()> {
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    if line.trim() != "continue" {
        return Err("缺少 continue 同步命令".into());
    }
    Ok(())
}

fn stress(parts: usize, rows: usize, overlap: &str, kind: &str) -> Result<()> {
    if parts == 0
        || rows == 0
        || !rows.is_multiple_of(parts)
        || !matches!(overlap, "none" | "full" | "one")
        || !matches!(kind, "int64" | "varchar")
    {
        return Err("需要正数分区、可整除行数、none/full/one、int64/varchar".into());
    }
    let mut model = Model::default();
    println!("{}", serde_json::json!({"phase": "empty"}));
    barrier()?;
    let n = rows / parts;
    let state = State {
        sources: (0..parts)
            .map(|part| InputSource {
                name: part.to_string(),
                partition: Some(part.to_string()),
                rows: (0..n)
                    .map(|row| {
                        let pk = if overlap == "full" || (overlap == "one" && row == 0) {
                            row
                        } else {
                            part * n + row
                        };
                        InputRow {
                            pk: if kind == "int64" {
                                Key::Int(pk as i64)
                            } else {
                                Key::Text(format!("{pk:016}"))
                            },
                            seq: (part * n + row + 1) as u64,
                            tag: String::new(),
                            partition: None,
                        }
                    })
                    .collect(),
            })
            .collect(),
        active_deletes: Vec::new(),
        frozen_deletes: Vec::new(),
        committed_deletes: Vec::new(),
    };
    let start = Instant::now();
    model.apply(state)?;
    let update_ms = start.elapsed().as_secs_f64() * 1000.;
    println!(
        "{}",
        serde_json::json!({"phase": "loaded", "update_ms": update_ms, "stats": model.stats()})
    );
    barrier()?;
    let parts_selected: Vec<_> = (0..parts)
        .filter(|p| p % 2 == 0)
        .map(|p| p.to_string())
        .collect();
    let expected = if overlap == "full" {
        n
    } else if overlap == "one" {
        parts_selected.len() * (n - 1) + 1
    } else {
        parts_selected.len() * n
    };
    let expected_all = if overlap == "full" {
        n
    } else if overlap == "one" {
        parts * (n - 1) + 1
    } else {
        rows
    };
    if model.snapshot(None).count() != expected_all {
        return Err("全集计数不同".into());
    }
    let start = Instant::now();
    for _ in 0..100 {
        let snapshot = model.snapshot(Some(&parts_selected));
        if snapshot.count() != expected {
            return Err("子集计数不同".into());
        }
        std::hint::black_box(snapshot);
    }
    println!(
        "{}",
        serde_json::json!({"phase": "queried", "subset_us": start.elapsed().as_secs_f64() * 10000., "expected": expected})
    );
    barrier()?;
    Ok(())
}

fn stress_core(parts: usize, rows: usize, overlap: &str, kind: &str) -> Result<()> {
    use milvus_lite_core::storage::visibility::partitions::{
        PartitionRow, PartitionVisibility, SourceKind,
    };
    use milvus_lite_core::storage::visibility::{RowLocation, SourceId};
    use milvus_lite_core::{DataType, Pk};
    if parts == 0
        || rows == 0
        || !rows.is_multiple_of(parts)
        || !matches!(overlap, "none" | "full" | "one" | "full-bidirectional")
        || !matches!(kind, "int64" | "varchar")
    {
        return Err("需要正数分区、可整除行数、none/full/one、int64/varchar".into());
    }
    let mut visibility = PartitionVisibility::default();
    println!("{}", serde_json::json!({"phase":"empty"}));
    barrier()?;
    let n = rows / parts;
    let key = |value: usize| {
        if kind == "int64" {
            Pk::Int64(value as i64)
        } else {
            Pk::VarChar(format!("{value:016}"))
        }
    };
    let keys = match overlap {
        "full" | "full-bidirectional" => n,
        "one" => rows - parts + 1,
        _ => rows,
    };
    visibility.reserve(
        if kind == "int64" {
            DataType::Int64
        } else {
            DataType::VarChar
        },
        keys,
    );
    let start = Instant::now();
    for part in 0..parts {
        let source = SourceId(part as u64 + 1);
        visibility.register_source(source, n, SourceKind::Immutable(u32::try_from(part)?))?;
        for row in 0..n {
            let pk = if overlap.starts_with("full") || (overlap == "one" && row == 0) {
                row
            } else {
                part * n + row
            };
            visibility.insert(
                key(pk),
                PartitionRow {
                    partition: part as u32,
                    location: RowLocation {
                        source,
                        row,
                        seq: ((if overlap == "full-bidirectional" && row % 2 == 1 {
                            parts - 1 - part
                        } else {
                            part
                        }) * n
                            + row
                            + 1) as u64,
                    },
                },
            )?;
        }
    }
    let stats = visibility.stats();
    println!(
        "{}",
        serde_json::json!({"phase":"loaded","update_ms":start.elapsed().as_secs_f64()*1000.,
        "stats":{"keys":stats.keys,"local_masks":stats.local_masks,"relations":stats.relations,"shadow_words":stats.shadow_words,"version_comparisons":stats.version_comparisons}})
    );
    barrier()?;
    let selected: Vec<_> = (0..parts as u32).filter(|p| p % 2 == 0).collect();
    let expected = match overlap {
        "full" | "full-bidirectional" => n,
        "one" => selected.len() * (n - 1) + 1,
        _ => selected.len() * n,
    };
    let all = visibility.snapshot(None);
    let old = visibility.snapshot(Some(&selected));
    if all.live_count != keys || old.live_count != expected {
        return Err("实际组件计数不同".into());
    }
    let start = Instant::now();
    for _ in 0..100 {
        let snapshot = visibility.snapshot(Some(&selected));
        if snapshot.live_count != expected {
            return Err("实际组件子集计数不同".into());
        }
        std::hint::black_box(snapshot);
    }
    let subset_us = start.elapsed().as_secs_f64() * 10000.;
    visibility.committed_delete(key(0), rows as u64 + 1)?;
    if all.live_count != keys
        || old.live_count != expected
        || visibility.snapshot(Some(&selected)).live_count != expected - 1
    {
        return Err("实际组件新删除或旧快照不同".into());
    }
    println!(
        "{}",
        serde_json::json!({"phase":"queried","subset_us":subset_us,"expected":expected})
    );
    barrier()?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [mode, path] if mode == "verify" => verify(path),
        [mode, path] if mode == "verify-core" => verify_core(path),
        [mode, parts, rows, overlap, kind] if mode == "stress-core" => {
            stress_core(parts.parse()?, rows.parse()?, overlap, kind)
        }
        [mode, parts, rows, overlap, kind] if mode == "stress" => {
            stress(parts.parse()?, rows.parse()?, overlap, kind)
        }
        _ => Err(
            "用法：verify <fixture> 或 stress <分区数> <物理行数> <none/full/one> <int64/varchar>"
                .into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(active: u64, frozen: u64, committed: u64, newer: bool) -> State {
        let mut sources = vec![InputSource {
            name: "segment/p1".into(),
            partition: Some("p1".into()),
            rows: vec![InputRow {
                pk: Key::Int(1),
                seq: 1,
                tag: "old".into(),
                partition: None,
            }],
        }];
        if newer {
            sources.push(InputSource {
                name: "active".into(),
                partition: Some("p2".into()),
                rows: vec![InputRow {
                    pk: Key::Int(1),
                    seq: 3,
                    tag: "new".into(),
                    partition: None,
                }],
            });
        }
        let deletes = |seq| {
            if seq == 0 {
                Vec::new()
            } else {
                vec![Delete {
                    pk: Key::Int(1),
                    seq,
                }]
            }
        };
        State {
            sources,
            active_deletes: deletes(active),
            frozen_deletes: deletes(frozen),
            committed_deletes: deletes(committed),
        }
    }

    fn p1(model: &Model) -> Snapshot {
        model.snapshot(Some(&["p1".to_owned()]))
    }

    #[test]
    fn active_delete_can_disappear_without_removing_committed_or_frozen_delete() {
        for (frozen, committed) in [(0, 0), (2, 0), (0, 2)] {
            let mut model = Model::default();
            model.apply(frame(2, frozen, committed, false)).unwrap();
            let old = p1(&model);
            assert_eq!(old.count(), 0);
            model.apply(frame(0, frozen, committed, true)).unwrap();
            assert_eq!(
                p1(&model).count(),
                usize::from(frozen == 0 && committed == 0)
            );
            assert_eq!(old.count(), 0);
            assert_eq!(model.snapshot(None).rows()[0].tag, "new");
        }
    }

    #[test]
    fn frozen_delete_survives_failed_publication_then_moves_to_committed() {
        let mut model = Model::default();
        model.apply(frame(0, 2, 0, true)).unwrap();
        let old = p1(&model);
        model.apply(frame(0, 2, 0, true)).unwrap();
        assert_eq!(p1(&model).count(), 0);
        model.apply(frame(0, 0, 2, true)).unwrap();
        assert_eq!(p1(&model).count(), 0);
        assert_eq!(old.count(), 0);
    }

    #[test]
    fn subset_mask_deduplicates_and_drop_reveals_old_partition() {
        let mut model = Model::default();
        model.apply(frame(0, 0, 0, true)).unwrap();
        let old = model.snapshot(Some(&["p1".into(), "p2".into()]));
        assert_eq!(old.count(), 1);
        assert_eq!(old.rows()[0].tag, "new");
        assert_eq!(p1(&model).rows()[0].tag, "old");
        model.apply(frame(0, 0, 0, false)).unwrap();
        assert_eq!(model.snapshot(None).rows()[0].tag, "old");
        assert_eq!(old.rows()[0].tag, "new");
        assert!(model.shadows.is_empty());
    }

    #[test]
    fn equal_sequence_uses_source_order_and_refreshes_after_reorder() {
        let mut model = Model::default();
        let mut state = frame(0, 0, 0, true);
        state.sources[1].rows[0].seq = 1;
        model.apply(state).unwrap();
        let old = model.snapshot(None);
        assert_eq!(old.rows()[0].tag, "old");
        let mut state = frame(0, 0, 0, true);
        state.sources[1].rows[0].seq = 1;
        state.sources.reverse();
        model.apply(state).unwrap();
        assert_eq!(model.snapshot(None).rows()[0].tag, "new");
        assert_eq!(old.rows()[0].tag, "old");
    }
}
