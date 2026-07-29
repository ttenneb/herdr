//! Session-wide delegation provenance, independent of pane placement and agent detection.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::layout::PaneId;

static NEXT_DELEGATION_ID: AtomicU64 = AtomicU64::new(1);

/// Stable session identity for a delegation record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DelegationId(u64);

impl DelegationId {
    pub fn alloc() -> Result<Self, DelegationIdAllocationError> {
        allocate_from(&NEXT_DELEGATION_ID)
    }

    pub fn raw(self) -> u64 {
        self.0
    }

    /// Reconstruct an ID from persisted state while reserving it from allocation.
    pub fn from_raw(raw: u64) -> Result<Self, ParseDelegationIdError> {
        if raw == 0 {
            return Err(ParseDelegationIdError);
        }
        reserve_after(raw);
        Ok(Self(raw))
    }
}

impl Serialize for DelegationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for DelegationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let id = String::deserialize(deserializer)?
            .parse::<DelegationId>()
            .map_err(D::Error::custom)?;
        Self::from_raw(id.raw()).map_err(D::Error::custom)
    }
}

impl fmt::Display for DelegationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "d{}", self.0)
    }
}

impl FromStr for DelegationId {
    type Err = ParseDelegationIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let raw = value
            .strip_prefix('d')
            .ok_or(ParseDelegationIdError)?
            .parse::<u64>()
            .map_err(|_| ParseDelegationIdError)?;
        (raw > 0).then_some(Self(raw)).ok_or(ParseDelegationIdError)
    }
}

fn allocate_from(counter: &AtomicU64) -> Result<DelegationId, DelegationIdAllocationError> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .map(DelegationId)
        .map_err(|_| DelegationIdAllocationError)
}

fn reserve_after(raw: u64) {
    let Some(next) = raw.checked_add(1) else {
        // No allocatable ID follows u64::MAX; force all future allocation to fail.
        NEXT_DELEGATION_ID.store(u64::MAX, Ordering::Relaxed);
        return;
    };
    let _ = NEXT_DELEGATION_ID.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        (current < next).then_some(next)
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseDelegationIdError;

impl fmt::Display for ParseDelegationIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("delegation ID must be 'd' followed by a positive integer")
    }
}

impl std::error::Error for ParseDelegationIdError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelegationIdAllocationError;

impl fmt::Display for DelegationIdAllocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("delegation ID space is exhausted")
    }
}

impl std::error::Error for DelegationIdAllocationError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationRecord {
    pub id: DelegationId,
    pub pane_id: Option<PaneId>,
    pub parent_id: Option<DelegationId>,
    pub purpose: Option<String>,
    pub sibling_rank: u64,
    pub tombstone: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiblingPosition {
    First,
    Last,
    Before(DelegationId),
    After(DelegationId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectionEntry {
    pub id: DelegationId,
    pub depth: usize,
    /// The retained provenance parent when this entry is a local root because
    /// its parent is outside the projection.
    pub external_parent_id: Option<DelegationId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationError {
    NotFound(DelegationId),
    ParentNotFound(DelegationId),
    PaneAlreadyAssociated(PaneId),
    PaneNotAssociated(PaneId),
    PaneMismatch { expected: PaneId, actual: PaneId },
    SelfParent,
    Cycle,
    CorruptCycle,
    NotSibling(DelegationId),
    IdAllocationExhausted,
}

impl fmt::Display for DelegationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(formatter, "delegation {id} was not found"),
            Self::ParentNotFound(id) => write!(formatter, "parent delegation {id} was not found"),
            Self::PaneAlreadyAssociated(id) => {
                write!(formatter, "pane {} already has a delegation", id.raw())
            }
            Self::PaneNotAssociated(id) => {
                write!(formatter, "pane {} has no delegation", id.raw())
            }
            Self::PaneMismatch { expected, actual } => write!(
                formatter,
                "delegation is associated with pane {}, not {}",
                actual.raw(),
                expected.raw()
            ),
            Self::SelfParent => formatter.write_str("a delegation cannot parent itself"),
            Self::Cycle => formatter.write_str("delegation parent would create a cycle"),
            Self::CorruptCycle => formatter.write_str("delegation data contains a cycle"),
            Self::NotSibling(id) => write!(formatter, "delegation {id} is not a sibling"),
            Self::IdAllocationExhausted => formatter.write_str("delegation ID space is exhausted"),
        }
    }
}

impl std::error::Error for DelegationError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationValidationError {
    ZeroId,
    KeyIdMismatch {
        key: DelegationId,
        record: DelegationId,
    },
    DuplicateId(DelegationId),
    DuplicatePane(PaneId),
    DanglingParent {
        id: DelegationId,
        parent: DelegationId,
    },
    Cycle(DelegationId),
}

impl fmt::Display for DelegationValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroId => formatter.write_str("delegation ID zero is invalid"),
            Self::KeyIdMismatch { key, record } => {
                write!(
                    formatter,
                    "delegation key {key} does not match record ID {record}"
                )
            }
            Self::DuplicateId(id) => write!(formatter, "duplicate delegation ID {id}"),
            Self::DuplicatePane(pane) => {
                write!(formatter, "pane {} has duplicate delegations", pane.raw())
            }
            Self::DanglingParent { id, parent } => {
                write!(formatter, "delegation {id} has missing parent {parent}")
            }
            Self::Cycle(id) => write!(formatter, "delegation cycle includes {id}"),
        }
    }
}

impl std::error::Error for DelegationValidationError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedDelegationEntry {
    key: DelegationId,
    record: DelegationRecord,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedDelegations {
    records: Vec<PersistedDelegationEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct Delegations {
    records: HashMap<DelegationId, DelegationRecord>,
}

impl Serialize for Delegations {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let records = self
            .preorder()
            .into_iter()
            .filter_map(|key| {
                self.records
                    .get(&key)
                    .cloned()
                    .map(|record| PersistedDelegationEntry { key, record })
            })
            .collect();
        PersistedDelegations { records }.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Delegations {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let persisted = PersistedDelegations::deserialize(deserializer)?;
        Self::from_keyed_records(
            persisted
                .records
                .into_iter()
                .map(|entry| (entry.key, entry.record)),
        )
        .map_err(D::Error::custom)
    }
}

impl Delegations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Validated construction for persistence and restore integration.
    pub fn from_records(
        records: impl IntoIterator<Item = DelegationRecord>,
    ) -> Result<Self, DelegationValidationError> {
        Self::from_keyed_records(records.into_iter().map(|record| (record.id, record)))
    }

    /// Validated construction that also checks persisted map keys against record IDs.
    pub fn from_keyed_records(
        records: impl IntoIterator<Item = (DelegationId, DelegationRecord)>,
    ) -> Result<Self, DelegationValidationError> {
        let mut map = HashMap::new();
        let mut panes = HashSet::new();
        for (key, record) in records {
            if key.raw() == 0 || record.id.raw() == 0 {
                return Err(DelegationValidationError::ZeroId);
            }
            if key != record.id {
                return Err(DelegationValidationError::KeyIdMismatch {
                    key,
                    record: record.id,
                });
            }
            if !map.contains_key(&key) {
                if let Some(pane_id) = record.pane_id {
                    if !panes.insert(pane_id) {
                        return Err(DelegationValidationError::DuplicatePane(pane_id));
                    }
                }
            }
            if map.insert(key, record).is_some() {
                return Err(DelegationValidationError::DuplicateId(key));
            }
        }

        for record in map.values() {
            if let Some(parent) = record.parent_id {
                if !map.contains_key(&parent) {
                    return Err(DelegationValidationError::DanglingParent {
                        id: record.id,
                        parent,
                    });
                }
            }
        }
        validate_acyclic(&map)?;

        let mut result = Self { records: map };
        result.normalize_all_siblings();
        Ok(result)
    }

    /// Repair untrusted snapshot records without affecting pane runtime state.
    /// Duplicate IDs keep the first deterministic record; duplicate or missing
    /// pane associations become tombstones; dangling and cyclic parents become roots.
    pub fn repair_records(records: impl IntoIterator<Item = DelegationRecord>) -> Self {
        let mut records: Vec<_> = records.into_iter().collect();
        records.sort_by_key(|record| (record.id, record.sibling_rank));
        let mut map = HashMap::new();
        let mut panes = HashSet::new();
        for mut record in records {
            if map.contains_key(&record.id) {
                continue;
            }
            if record.pane_id.is_some_and(|pane| !panes.insert(pane)) {
                record.pane_id = None;
                record.tombstone = true;
            }
            map.insert(record.id, record);
        }
        let ids: HashSet<_> = map.keys().copied().collect();
        for record in map.values_mut() {
            if record.parent_id == Some(record.id)
                || record
                    .parent_id
                    .is_some_and(|parent| !ids.contains(&parent))
            {
                record.parent_id = None;
            }
        }
        loop {
            let mut repaired = false;
            let mut starts: Vec<_> = map.keys().copied().collect();
            starts.sort_unstable();
            for start in starts {
                let mut path = HashSet::new();
                let mut current = Some(start);
                while let Some(id) = current {
                    if !path.insert(id) {
                        if let Some(record) = map.get_mut(&id) {
                            record.parent_id = None;
                            repaired = true;
                        }
                        break;
                    }
                    current = map.get(&id).and_then(|record| record.parent_id);
                }
                if repaired {
                    break;
                }
            }
            if !repaired {
                break;
            }
        }
        let mut result = Self { records: map };
        result.normalize_all_siblings();
        result
    }

    pub fn records(&self) -> &HashMap<DelegationId, DelegationRecord> {
        &self.records
    }

    pub fn get(&self, id: DelegationId) -> Option<&DelegationRecord> {
        self.records.get(&id)
    }

    pub fn delegation_for_pane(&self, pane_id: PaneId) -> Option<&DelegationRecord> {
        self.records
            .values()
            .find(|record| record.pane_id == Some(pane_id))
    }

    pub fn create(
        &mut self,
        pane_id: Option<PaneId>,
        parent_id: Option<DelegationId>,
        purpose: Option<String>,
    ) -> Result<DelegationId, DelegationError> {
        if let Some(parent_id) = parent_id {
            if !self.records.contains_key(&parent_id) {
                return Err(DelegationError::ParentNotFound(parent_id));
            }
        }
        if let Some(pane_id) = pane_id {
            if self.delegation_for_pane(pane_id).is_some() {
                return Err(DelegationError::PaneAlreadyAssociated(pane_id));
            }
        }

        let id = DelegationId::alloc().map_err(|_| DelegationError::IdAllocationExhausted)?;
        let sibling_rank = self.sibling_ids(parent_id).len() as u64;
        self.records.insert(
            id,
            DelegationRecord {
                id,
                pane_id,
                parent_id,
                purpose,
                sibling_rank,
                tombstone: false,
            },
        );
        Ok(id)
    }

    /// Associate or replace a record's pane after validating session-wide uniqueness.
    pub fn associate_pane(
        &mut self,
        id: DelegationId,
        pane_id: PaneId,
    ) -> Result<(), DelegationError> {
        if !self.records.contains_key(&id) {
            return Err(DelegationError::NotFound(id));
        }
        if self
            .delegation_for_pane(pane_id)
            .is_some_and(|record| record.id != id)
        {
            return Err(DelegationError::PaneAlreadyAssociated(pane_id));
        }
        if let Some(record) = self.records.get_mut(&id) {
            record.pane_id = Some(pane_id);
            record.tombstone = false;
        }
        Ok(())
    }

    /// Safely replace a restored pane ID without weakening pane uniqueness.
    pub fn remap_pane(
        &mut self,
        old_pane_id: PaneId,
        new_pane_id: PaneId,
    ) -> Result<DelegationId, DelegationError> {
        let Some(id) = self
            .delegation_for_pane(old_pane_id)
            .map(|record| record.id)
        else {
            return Err(DelegationError::PaneNotAssociated(old_pane_id));
        };
        if old_pane_id == new_pane_id {
            return Ok(id);
        }
        if self.delegation_for_pane(new_pane_id).is_some() {
            return Err(DelegationError::PaneAlreadyAssociated(new_pane_id));
        }
        if let Some(record) = self.records.get_mut(&id) {
            if record.pane_id != Some(old_pane_id) {
                return Err(DelegationError::PaneMismatch {
                    expected: old_pane_id,
                    actual: record.pane_id.unwrap_or(old_pane_id),
                });
            }
            record.pane_id = Some(new_pane_id);
        }
        Ok(id)
    }

    pub fn reparent(
        &mut self,
        id: DelegationId,
        parent_id: Option<DelegationId>,
    ) -> Result<(), DelegationError> {
        let old_parent = self
            .records
            .get(&id)
            .ok_or(DelegationError::NotFound(id))?
            .parent_id;
        if parent_id == Some(id) {
            return Err(DelegationError::SelfParent);
        }
        if let Some(parent_id) = parent_id {
            if !self.records.contains_key(&parent_id) {
                return Err(DelegationError::ParentNotFound(parent_id));
            }
            if self.is_descendant_of(parent_id, id)? {
                return Err(DelegationError::Cycle);
            }
        }
        if old_parent == parent_id {
            return Ok(());
        }

        let new_rank = self.sibling_ids(parent_id).len() as u64;
        if let Some(record) = self.records.get_mut(&id) {
            record.parent_id = parent_id;
            record.sibling_rank = new_rank;
        }
        self.normalize_siblings(old_parent);
        self.normalize_siblings(parent_id);
        Ok(())
    }

    pub fn reorder(
        &mut self,
        id: DelegationId,
        position: SiblingPosition,
    ) -> Result<(), DelegationError> {
        let parent_id = self
            .records
            .get(&id)
            .ok_or(DelegationError::NotFound(id))?
            .parent_id;
        let mut siblings = self.sibling_ids(parent_id);
        siblings.retain(|candidate| *candidate != id);
        let index = match position {
            SiblingPosition::First => 0,
            SiblingPosition::Last => siblings.len(),
            SiblingPosition::Before(anchor) | SiblingPosition::After(anchor) => {
                let Some(anchor_index) = siblings.iter().position(|candidate| *candidate == anchor)
                else {
                    return Err(DelegationError::NotSibling(anchor));
                };
                anchor_index + usize::from(matches!(position, SiblingPosition::After(_)))
            }
        };
        siblings.insert(index, id);
        self.set_sibling_order(&siblings);
        Ok(())
    }

    pub fn root(&self, id: DelegationId) -> Result<DelegationId, DelegationError> {
        let mut current = id;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current) {
                return Err(DelegationError::CorruptCycle);
            }
            let record = self
                .records
                .get(&current)
                .ok_or(DelegationError::NotFound(current))?;
            match record.parent_id {
                Some(parent) => current = parent,
                None => return Ok(current),
            }
        }
    }

    /// Descendants in deterministic depth-first sibling order.
    pub fn descendants(&self, id: DelegationId) -> Result<Vec<DelegationId>, DelegationError> {
        if !self.records.contains_key(&id) {
            return Err(DelegationError::NotFound(id));
        }
        let mut result = Vec::new();
        let mut visited = HashSet::from([id]);
        self.collect_descendants_checked(id, &mut visited, &mut result)?;
        Ok(result)
    }

    /// All records in deterministic forest preorder, including corrupt components once each.
    pub fn preorder(&self) -> Vec<DelegationId> {
        let mut result = Vec::new();
        let mut visited = HashSet::new();
        for root in self.sibling_ids(None) {
            self.collect_preorder_safe(root, &mut visited, &mut result);
        }
        let mut remaining: Vec<_> = self.records.keys().copied().collect();
        remaining.sort_unstable();
        for id in remaining {
            self.collect_preorder_safe(id, &mut visited, &mut result);
        }
        result
    }

    /// Deterministic preorder of a local projection. An included record whose
    /// parent is excluded becomes a local root while retaining an external-parent cue.
    pub fn preorder_projection(&self, included: &HashSet<DelegationId>) -> Vec<ProjectionEntry> {
        let included: HashSet<_> = included
            .iter()
            .copied()
            .filter(|id| self.records.contains_key(id))
            .collect();
        let mut result = Vec::new();
        let mut projected = HashSet::new();
        for id in self.preorder() {
            if !included.contains(&id) || projected.contains(&id) {
                continue;
            }
            let parent_id = self.records.get(&id).and_then(|record| record.parent_id);
            if parent_id.is_some_and(|parent| included.contains(&parent)) {
                continue;
            }
            let external_parent_id = parent_id.filter(|parent| !included.contains(parent));
            self.collect_projection_safe(
                id,
                0,
                external_parent_id,
                &included,
                &mut projected,
                &mut result,
            );
        }
        // Corrupt included cycles have no local root; still project every entry once.
        let mut remaining: Vec<_> = included.iter().copied().collect();
        remaining.sort_unstable();
        for id in remaining {
            self.collect_projection_safe(
                id,
                0,
                self.records.get(&id).and_then(|record| record.parent_id),
                &included,
                &mut projected,
                &mut result,
            );
        }
        result
    }

    pub fn preorder_for_panes(&self, panes: &HashSet<PaneId>) -> Vec<ProjectionEntry> {
        let included = self
            .records
            .values()
            .filter_map(|record| {
                record
                    .pane_id
                    .filter(|pane_id| panes.contains(pane_id))
                    .map(|_| record.id)
            })
            .collect();
        self.preorder_projection(&included)
    }

    /// Detach a closed pane but retain its provenance while descendants exist.
    pub fn tombstone_pane(&mut self, pane_id: PaneId) -> Option<DelegationId> {
        let id = self.delegation_for_pane(pane_id)?.id;
        if let Some(record) = self.records.get_mut(&id) {
            record.pane_id = None;
            record.tombstone = true;
        }
        Some(id)
    }

    /// Remove tombstoned leaves, then repeat for newly exposed tombstoned leaves.
    pub fn gc_tombstones(&mut self) -> Vec<DelegationId> {
        let mut removed = Vec::new();
        loop {
            let parents: HashSet<_> = self
                .records
                .values()
                .filter_map(|record| record.parent_id)
                .collect();
            let mut leaves: Vec<_> = self
                .records
                .values()
                .filter(|record| record.tombstone && !parents.contains(&record.id))
                .map(|record| record.id)
                .collect();
            leaves.sort_unstable();
            if leaves.is_empty() {
                break;
            }
            for id in leaves {
                if self.records.remove(&id).is_some() {
                    removed.push(id);
                }
            }
        }
        self.normalize_all_siblings();
        removed
    }

    fn sibling_ids(&self, parent_id: Option<DelegationId>) -> Vec<DelegationId> {
        let mut siblings: Vec<_> = self
            .records
            .values()
            .filter(|record| record.parent_id == parent_id)
            .map(|record| (record.sibling_rank, record.id))
            .collect();
        siblings.sort_unstable();
        siblings.into_iter().map(|(_, id)| id).collect()
    }

    fn normalize_all_siblings(&mut self) {
        let parents: HashSet<_> = self
            .records
            .values()
            .map(|record| record.parent_id)
            .collect();
        for parent in parents {
            self.normalize_siblings(parent);
        }
    }

    fn normalize_siblings(&mut self, parent_id: Option<DelegationId>) {
        let siblings = self.sibling_ids(parent_id);
        self.set_sibling_order(&siblings);
    }

    fn set_sibling_order(&mut self, siblings: &[DelegationId]) {
        for (rank, id) in siblings.iter().copied().enumerate() {
            if let Some(record) = self.records.get_mut(&id) {
                record.sibling_rank = rank as u64;
            }
        }
    }

    fn is_descendant_of(
        &self,
        candidate: DelegationId,
        ancestor: DelegationId,
    ) -> Result<bool, DelegationError> {
        let mut current = Some(candidate);
        let mut visited = HashSet::new();
        while let Some(id) = current {
            if id == ancestor {
                return Ok(true);
            }
            if !visited.insert(id) {
                return Err(DelegationError::CorruptCycle);
            }
            current = self.records.get(&id).and_then(|record| record.parent_id);
        }
        Ok(false)
    }

    fn collect_descendants_checked(
        &self,
        parent: DelegationId,
        visited: &mut HashSet<DelegationId>,
        result: &mut Vec<DelegationId>,
    ) -> Result<(), DelegationError> {
        for child in self.sibling_ids(Some(parent)) {
            if !visited.insert(child) {
                return Err(DelegationError::CorruptCycle);
            }
            result.push(child);
            self.collect_descendants_checked(child, visited, result)?;
        }
        Ok(())
    }

    fn collect_preorder_safe(
        &self,
        id: DelegationId,
        visited: &mut HashSet<DelegationId>,
        result: &mut Vec<DelegationId>,
    ) {
        if !visited.insert(id) {
            return;
        }
        result.push(id);
        for child in self.sibling_ids(Some(id)) {
            self.collect_preorder_safe(child, visited, result);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_projection_safe(
        &self,
        id: DelegationId,
        depth: usize,
        external_parent_id: Option<DelegationId>,
        included: &HashSet<DelegationId>,
        projected: &mut HashSet<DelegationId>,
        result: &mut Vec<ProjectionEntry>,
    ) {
        if !included.contains(&id) || !projected.insert(id) {
            return;
        }
        result.push(ProjectionEntry {
            id,
            depth,
            external_parent_id,
        });
        for child in self.sibling_ids(Some(id)) {
            self.collect_projection_safe(child, depth + 1, None, included, projected, result);
        }
    }
}

fn validate_acyclic(
    records: &HashMap<DelegationId, DelegationRecord>,
) -> Result<(), DelegationValidationError> {
    let mut complete = HashSet::new();
    let mut ids: Vec<_> = records.keys().copied().collect();
    ids.sort_unstable();
    for start in ids {
        if complete.contains(&start) {
            continue;
        }
        let mut path = Vec::new();
        let mut in_path = HashSet::new();
        let mut current = Some(start);
        while let Some(id) = current {
            if complete.contains(&id) {
                break;
            }
            if !in_path.insert(id) {
                return Err(DelegationValidationError::Cycle(id));
            }
            path.push(id);
            current = records.get(&id).and_then(|record| record.parent_id);
        }
        complete.extend(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(raw: u32) -> PaneId {
        PaneId::from_raw(raw)
    }

    fn id(raw: u64) -> DelegationId {
        DelegationId::from_raw(raw).expect("test ID should be valid")
    }

    fn record(raw: u64, pane_raw: Option<u32>, parent: Option<u64>, rank: u64) -> DelegationRecord {
        DelegationRecord {
            id: id(raw),
            pane_id: pane_raw.map(pane),
            parent_id: parent.map(id),
            purpose: None,
            sibling_rank: rank,
            tombstone: pane_raw.is_none(),
        }
    }

    fn create(
        delegations: &mut Delegations,
        pane_raw: u32,
        parent: Option<DelegationId>,
    ) -> DelegationId {
        delegations
            .create(Some(pane(pane_raw)), parent, None)
            .expect("test delegation should be created")
    }

    #[test]
    fn ids_serialize_as_strings_and_reject_zero_and_non_strings() {
        let delegation_id = id(50_000);
        assert_eq!(delegation_id.to_string().parse(), Ok(delegation_id));
        assert_eq!(
            serde_json::to_string(&delegation_id).expect("ID should serialize"),
            "\"d50000\""
        );
        assert_eq!(
            serde_json::from_str::<DelegationId>("\"d50000\"").expect("ID should deserialize"),
            delegation_id
        );
        assert!(DelegationId::alloc().expect("ID should allocate").raw() > delegation_id.raw());
        assert!(DelegationId::from_raw(0).is_err());
        assert!("d0".parse::<DelegationId>().is_err());
        assert!(serde_json::from_str::<DelegationId>("0").is_err());
    }

    #[test]
    fn untrusted_delegation_id_parsing_does_not_reserve_allocator_space() {
        let before = DelegationId::alloc().expect("delegation ID available");
        let parsed = "d18446744073709551615"
            .parse::<DelegationId>()
            .expect("near-max public ID parses");
        let after = DelegationId::alloc().expect("untrusted lookup must not exhaust allocation");

        assert!(parsed.raw() > after.raw());
        assert!(after.raw() > before.raw());
    }

    #[test]
    fn allocation_fails_without_wrapping_at_u64_max() {
        let counter = AtomicU64::new(u64::MAX);
        assert_eq!(allocate_from(&counter), Err(DelegationIdAllocationError));
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn aggregate_serialization_round_trip_normalizes_ranks() {
        let d = Delegations::from_records([
            record(61_001, Some(1), None, 99),
            record(61_002, Some(2), None, 2),
            record(61_003, Some(3), Some(61_001), 55),
        ])
        .expect("records should validate");
        let json = serde_json::to_string(&d).expect("aggregate should serialize");
        assert!(json.contains("\"d61001\""));
        let restored: Delegations =
            serde_json::from_str(&json).expect("aggregate should deserialize");
        assert_eq!(restored.preorder(), d.preorder());
        assert_eq!(restored.get(id(61_002)).map(|r| r.sibling_rank), Some(0));
        assert_eq!(restored.get(id(61_001)).map(|r| r.sibling_rank), Some(1));
        assert_eq!(restored.get(id(61_003)).map(|r| r.sibling_rank), Some(0));
    }

    #[test]
    fn persisted_aggregate_rejects_key_mismatch_and_duplicate_ids() {
        let mismatch =
            Delegations::from_keyed_records([(id(62_001), record(62_002, Some(1), None, 0))]);
        assert!(matches!(
            mismatch,
            Err(DelegationValidationError::KeyIdMismatch { .. })
        ));
        let duplicate = Delegations::from_keyed_records([
            (id(62_003), record(62_003, Some(1), None, 0)),
            (id(62_003), record(62_003, Some(2), None, 1)),
        ]);
        assert!(matches!(
            duplicate,
            Err(DelegationValidationError::DuplicateId(value)) if value == id(62_003)
        ));
    }

    #[test]
    fn persisted_aggregate_rejects_duplicate_panes_dangling_parents_and_cycles() {
        let duplicate_pane = Delegations::from_records([
            record(63_001, Some(1), None, 0),
            record(63_002, Some(1), None, 1),
        ]);
        assert!(matches!(
            duplicate_pane,
            Err(DelegationValidationError::DuplicatePane(value)) if value == pane(1)
        ));
        let dangling = Delegations::from_records([record(63_003, Some(2), Some(63_999), 0)]);
        assert!(matches!(
            dangling,
            Err(DelegationValidationError::DanglingParent { .. })
        ));
        let cycle = Delegations::from_records([
            record(63_004, Some(3), Some(63_005), 0),
            record(63_005, Some(4), Some(63_004), 0),
        ]);
        assert!(matches!(cycle, Err(DelegationValidationError::Cycle(_))));
    }

    #[test]
    fn custom_deserialization_rejects_invalid_aggregate_data() {
        let json = r#"{"records":[{"key":"d64001","record":{"id":"d64002","pane_id":1,"parent_id":null,"purpose":null,"sibling_rank":0,"tombstone":false}}]}"#;
        assert!(serde_json::from_str::<Delegations>(json).is_err());
        let zero = r#"{"records":[{"key":"d0","record":{"id":"d0","pane_id":1,"parent_id":null,"purpose":null,"sibling_rank":0,"tombstone":false}}]}"#;
        assert!(serde_json::from_str::<Delegations>(zero).is_err());
    }

    #[test]
    fn create_appends_siblings_and_preserves_fields() {
        let mut d = Delegations::new();
        let root = d
            .create(Some(pane(1)), None, Some("primary".into()))
            .expect("root should be created");
        let child = create(&mut d, 2, Some(root));
        let second = create(&mut d, 3, Some(root));
        assert_eq!(
            d.get(root).and_then(|r| r.purpose.as_deref()),
            Some("primary")
        );
        assert_eq!(d.get(child).map(|r| r.sibling_rank), Some(0));
        assert_eq!(d.get(second).map(|r| r.sibling_rank), Some(1));
    }

    #[test]
    fn create_rejects_missing_parent_and_duplicate_pane_without_mutation() {
        let mut d = Delegations::new();
        let missing = id(65_001);
        assert_eq!(
            d.create(Some(pane(1)), Some(missing), None),
            Err(DelegationError::ParentNotFound(missing))
        );
        create(&mut d, 1, None);
        assert_eq!(
            d.create(Some(pane(1)), None, None),
            Err(DelegationError::PaneAlreadyAssociated(pane(1)))
        );
        assert_eq!(d.records().len(), 1);
    }

    #[test]
    fn associate_and_remap_panes_preserve_uniqueness() {
        let mut d = Delegations::new();
        let first = d
            .create(None, None, None)
            .expect("record should be created");
        let second = create(&mut d, 2, None);
        d.associate_pane(first, pane(1))
            .expect("association should work");
        assert_eq!(d.delegation_for_pane(pane(1)).map(|r| r.id), Some(first));
        assert_eq!(
            d.associate_pane(first, pane(2)),
            Err(DelegationError::PaneAlreadyAssociated(pane(2)))
        );
        assert_eq!(
            d.remap_pane(pane(1), pane(2)),
            Err(DelegationError::PaneAlreadyAssociated(pane(2)))
        );
        assert_eq!(d.remap_pane(pane(1), pane(3)), Ok(first));
        assert_eq!(d.delegation_for_pane(pane(3)).map(|r| r.id), Some(first));
        assert_eq!(d.delegation_for_pane(pane(2)).map(|r| r.id), Some(second));
    }

    #[test]
    fn associate_revives_tombstone_and_missing_remap_is_typed_error() {
        let mut d = Delegations::new();
        let root = create(&mut d, 1, None);
        d.tombstone_pane(pane(1));
        d.associate_pane(root, pane(4))
            .expect("association should revive");
        assert_eq!(
            d.get(root).map(|r| (r.pane_id, r.tombstone)),
            Some((Some(pane(4)), false))
        );
        assert_eq!(
            d.remap_pane(pane(99), pane(100)),
            Err(DelegationError::PaneNotAssociated(pane(99)))
        );
    }

    #[test]
    fn reparent_rejects_self_cycles_and_missing_parents_atomically() {
        let mut d = Delegations::new();
        let root = create(&mut d, 1, None);
        let child = create(&mut d, 2, Some(root));
        let grandchild = create(&mut d, 3, Some(child));
        assert_eq!(
            d.reparent(root, Some(root)),
            Err(DelegationError::SelfParent)
        );
        assert_eq!(
            d.reparent(root, Some(grandchild)),
            Err(DelegationError::Cycle)
        );
        let missing = id(66_001);
        assert_eq!(
            d.reparent(child, Some(missing)),
            Err(DelegationError::ParentNotFound(missing))
        );
        assert_eq!(d.get(root).and_then(|r| r.parent_id), None);
        assert_eq!(d.get(child).and_then(|r| r.parent_id), Some(root));
    }

    #[test]
    fn reparent_and_reorder_normalize_sibling_order() {
        let mut d = Delegations::new();
        let first_root = create(&mut d, 1, None);
        let second_root = create(&mut d, 2, None);
        let first = create(&mut d, 3, Some(first_root));
        let moved = create(&mut d, 4, Some(first_root));
        let existing = create(&mut d, 5, Some(second_root));
        d.reparent(moved, Some(second_root))
            .expect("reparent should work");
        d.reorder(moved, SiblingPosition::First)
            .expect("first should work");
        assert_eq!(d.descendants(first_root), Ok(vec![first]));
        assert_eq!(d.descendants(second_root), Ok(vec![moved, existing]));
        d.reorder(moved, SiblingPosition::Last)
            .expect("last should work");
        d.reorder(moved, SiblingPosition::Before(existing))
            .expect("before should work");
        d.reorder(moved, SiblingPosition::After(existing))
            .expect("after should work");
        assert_eq!(d.descendants(second_root), Ok(vec![existing, moved]));
    }

    #[test]
    fn reorder_rejects_non_sibling_without_changing_order() {
        let mut d = Delegations::new();
        let root = create(&mut d, 1, None);
        let child = create(&mut d, 2, Some(root));
        let other_root = create(&mut d, 3, None);
        assert_eq!(
            d.reorder(child, SiblingPosition::Before(other_root)),
            Err(DelegationError::NotSibling(other_root))
        );
        assert_eq!(d.descendants(root), Ok(vec![child]));
    }

    #[test]
    fn root_descendants_and_preorder_are_deterministic() {
        let mut d = Delegations::new();
        let root = create(&mut d, 1, None);
        let a = create(&mut d, 2, Some(root));
        let b = create(&mut d, 3, Some(root));
        let grandchild = create(&mut d, 4, Some(a));
        assert_eq!(d.root(grandchild), Ok(root));
        assert_eq!(d.descendants(root), Ok(vec![a, grandchild, b]));
        assert_eq!(d.preorder(), vec![root, a, grandchild, b]);
    }

    #[test]
    fn queries_terminate_and_report_corrupt_in_memory_cycles() {
        let mut d = Delegations::new();
        let root = create(&mut d, 1, None);
        let child = create(&mut d, 2, Some(root));
        d.records.get_mut(&root).expect("root exists").parent_id = Some(child);
        assert_eq!(d.root(root), Err(DelegationError::CorruptCycle));
        assert_eq!(d.descendants(root), Err(DelegationError::CorruptCycle));
        let preorder = d.preorder();
        assert_eq!(preorder.len(), 2);
        assert_eq!(preorder.iter().copied().collect::<HashSet<_>>().len(), 2);
        let projection = d.preorder_projection(&HashSet::from([root, child]));
        assert_eq!(projection.len(), 2);
    }

    #[test]
    fn rank_ties_are_broken_by_stable_id() {
        let d = Delegations::from_records([
            record(67_002, Some(2), None, 7),
            record(67_001, Some(1), None, 7),
        ])
        .expect("records should validate");
        assert_eq!(d.preorder(), vec![id(67_001), id(67_002)]);
    }

    #[test]
    fn projection_flattens_external_or_missing_local_parents() {
        let mut d = Delegations::new();
        let root = create(&mut d, 1, None);
        let child = create(&mut d, 2, Some(root));
        let grandchild = create(&mut d, 3, Some(child));
        assert_eq!(
            d.preorder_projection(&HashSet::from([child, grandchild])),
            vec![
                ProjectionEntry {
                    id: child,
                    depth: 0,
                    external_parent_id: Some(root)
                },
                ProjectionEntry {
                    id: grandchild,
                    depth: 1,
                    external_parent_id: None
                },
            ]
        );
        assert_eq!(
            d.preorder_projection(&HashSet::from([root, grandchild])),
            vec![
                ProjectionEntry {
                    id: root,
                    depth: 0,
                    external_parent_id: None
                },
                ProjectionEntry {
                    id: grandchild,
                    depth: 0,
                    external_parent_id: Some(child)
                },
            ]
        );
    }

    #[test]
    fn pane_projection_ignores_tombstones_and_unselected_panes() {
        let mut d = Delegations::new();
        let root = create(&mut d, 1, None);
        let child = create(&mut d, 2, Some(root));
        let other = create(&mut d, 3, Some(root));
        d.tombstone_pane(pane(1));
        let projection = d.preorder_for_panes(&HashSet::from([pane(2), pane(99)]));
        assert_eq!(
            projection,
            vec![ProjectionEntry {
                id: child,
                depth: 0,
                external_parent_id: Some(root)
            }]
        );
        assert!(!projection.iter().any(|entry| entry.id == other));
    }

    #[test]
    fn pane_close_tombstones_and_gc_waits_for_retained_descendants() {
        let mut d = Delegations::new();
        let root = create(&mut d, 1, None);
        let child = create(&mut d, 2, Some(root));
        assert_eq!(d.tombstone_pane(pane(1)), Some(root));
        assert!(d.gc_tombstones().is_empty());
        assert_eq!(d.root(child), Ok(root));
        d.tombstone_pane(pane(2));
        assert_eq!(d.gc_tombstones(), vec![child, root]);
        assert!(d.records().is_empty());
    }

    #[test]
    fn gc_preserves_non_tombstone_record_without_a_pane() {
        let mut d = Delegations::new();
        let retained = d
            .create(None, None, None)
            .expect("record should be created");
        assert!(d.gc_tombstones().is_empty());
        assert!(d.get(retained).is_some());
    }

    #[test]
    fn missing_queries_return_typed_errors() {
        let d = Delegations::new();
        let missing = id(68_001);
        assert_eq!(d.root(missing), Err(DelegationError::NotFound(missing)));
        assert_eq!(
            d.descendants(missing),
            Err(DelegationError::NotFound(missing))
        );
    }
}
