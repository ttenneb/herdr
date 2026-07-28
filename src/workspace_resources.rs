use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;

pub const MAX_RESOURCES_PER_PLUGIN: usize = 32;
pub const MAX_RESOURCES_PER_WORKSPACE: usize = 64;
pub const MAX_RESOURCE_PLUGINS: usize = 32;
pub const MAX_RESOURCE_ID_CHARS: usize = 120;
pub const MAX_RESOURCE_TEXT_CHARS: usize = 256;
pub const MAX_RESOURCE_DATA_BYTES: usize = 8 * 1024;
/// Bound the data retained by one workspace resource projection. Resource
/// updates are included in subscription events, so per-resource limits alone
/// can otherwise multiply into a large retained event backlog.
pub const MAX_RESOURCE_DATA_BYTES_PER_WORKSPACE: usize = 64 * 1024;
pub const MAX_RESOURCE_DATA_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceResource {
    pub plugin_id: String,
    pub resource_id: String,
    pub label: String,
    pub detail: Option<String>,
    pub data: Option<Value>,
}

#[derive(Debug, Default)]
pub struct WorkspaceResourceRegistry {
    /// Sources with visible resources. Empty reports deliberately do not retain
    /// a source slot.
    sources: HashMap<String, ResourceSource>,
    /// Bounded sequence fences for sources whose visible set was cleared or
    /// expired. These prevent old reports (including unsequenced ones) from
    /// resurrecting a newer cleared projection.
    sequence_tombstones: HashMap<String, u64>,
}

#[derive(Debug)]
struct ResourceSource {
    resources: Vec<WorkspaceResource>,
    sequence: Option<u64>,
    expires_at: Option<Instant>,
}

impl WorkspaceResourceRegistry {
    pub fn resources(&self) -> impl Iterator<Item = &WorkspaceResource> {
        let mut resources = self
            .sources
            .values()
            .flat_map(|source| source.resources.iter())
            .collect::<Vec<_>>();
        resources.sort_by(|left, right| {
            (&left.plugin_id, &left.resource_id).cmp(&(&right.plugin_id, &right.resource_id))
        });
        resources.into_iter()
    }

    pub fn find(&self, plugin_id: &str, resource_id: &str) -> Option<&WorkspaceResource> {
        self.sources
            .get(plugin_id)?
            .resources
            .iter()
            .find(|resource| resource.resource_id == resource_id)
    }

    pub fn report(
        &mut self,
        plugin_id: String,
        resources: Vec<WorkspaceResource>,
        sequence: Option<u64>,
        ttl: Option<Duration>,
        now: Instant,
    ) -> Result<bool, &'static str> {
        if resources.len() > MAX_RESOURCES_PER_PLUGIN {
            return Err("a resource report may contain at most 32 resources");
        }
        let mut resource_ids = std::collections::HashSet::new();
        if resources
            .iter()
            .any(|resource| !resource_ids.insert(&resource.resource_id))
        {
            return Err("a resource report may not contain duplicate resource_id values");
        }
        let previous_sequence = self
            .sources
            .get(&plugin_id)
            .and_then(|source| source.sequence)
            .or_else(|| self.sequence_tombstones.get(&plugin_id).copied());
        if let Some(previous) = previous_sequence {
            // Once an owner has opted into sequencing, an unsequenced report
            // must not bypass the ordering fence after an empty/expired update.
            if sequence.is_none_or(|sequence| sequence <= previous) {
                return Ok(false);
            }
        }

        let replacing = self.sources.contains_key(&plugin_id);
        let other_count = self
            .sources
            .iter()
            .filter(|(id, _)| *id != &plugin_id)
            .map(|(_, source)| source.resources.len())
            .sum::<usize>();
        if other_count + resources.len() > MAX_RESOURCES_PER_WORKSPACE {
            return Err("workspace may contain at most 64 resources");
        }
        let other_data_bytes = self
            .sources
            .iter()
            .filter(|(id, _)| *id != &plugin_id)
            .map(|(_, source)| resource_data_bytes(&source.resources))
            .sum::<usize>();
        let report_data_bytes = resource_data_bytes(&resources);
        if other_data_bytes.saturating_add(report_data_bytes)
            > MAX_RESOURCE_DATA_BYTES_PER_WORKSPACE
        {
            return Err("workspace resource data may total at most 65536 bytes");
        }

        if resources.is_empty() {
            let accepted_sequence = sequence.or(previous_sequence);
            if accepted_sequence.is_some()
                && !self.sequence_tombstones.contains_key(&plugin_id)
                && self
                    .sources
                    .iter()
                    .filter(|(id, source)| *id != &plugin_id && source.sequence.is_some())
                    .count()
                    + self.sequence_tombstones.len()
                    >= MAX_RESOURCE_PLUGINS
            {
                return Err("workspace may track sequences for at most 32 resource plugins");
            }
            let removed = self.sources.remove(&plugin_id).is_some();
            let had_tombstone = self.sequence_tombstones.contains_key(&plugin_id);
            self.update_sequence_tombstone(plugin_id, accepted_sequence)?;
            return Ok(removed || had_tombstone || accepted_sequence.is_some());
        }
        if !replacing && self.sources.len() >= MAX_RESOURCE_PLUGINS {
            return Err("workspace may contain resources from at most 32 plugins");
        }
        let accepted_sequence = sequence.or(previous_sequence);
        if accepted_sequence.is_some()
            && previous_sequence.is_none()
            && !self.sequence_tombstones.contains_key(&plugin_id)
            && self
                .sources
                .values()
                .filter(|source| source.sequence.is_some())
                .count()
                + self.sequence_tombstones.len()
                >= MAX_RESOURCE_PLUGINS
        {
            return Err("workspace may track sequences for at most 32 resource plugins");
        }
        self.sequence_tombstones.remove(&plugin_id);
        self.sources.insert(
            plugin_id,
            ResourceSource {
                resources,
                sequence: accepted_sequence,
                expires_at: ttl.map(|ttl| now + ttl),
            },
        );
        Ok(true)
    }

    /// Plugin IDs currently owning a source. Registry reconciliation must
    /// revoke every source immediately; TTL only bounds normal expiration.
    pub fn source_ids(&self) -> Vec<String> {
        self.sources
            .keys()
            .chain(self.sequence_tombstones.keys())
            .cloned()
            .collect()
    }

    /// Removes a plugin's source and sequence fence immediately. Owner
    /// revocation intentionally permits a newly eligible owner to start over.
    pub fn clear_source(&mut self, plugin_id: &str) -> bool {
        self.sources.remove(plugin_id).is_some()
            || self.sequence_tombstones.remove(plugin_id).is_some()
    }

    pub fn expire(&mut self, now: Instant) -> bool {
        let expired = self
            .sources
            .iter()
            .filter(|(_, source)| source.expires_at.is_some_and(|deadline| deadline <= now))
            .map(|(plugin_id, source)| (plugin_id.clone(), source.sequence))
            .collect::<Vec<_>>();
        for (plugin_id, sequence) in &expired {
            self.sources.remove(plugin_id);
            // An existing sequenced source already occupies one bounded fence,
            // so this cannot grow the total sequence-owner count.
            if let Some(sequence) = sequence {
                self.sequence_tombstones
                    .insert(plugin_id.clone(), *sequence);
            }
        }
        !expired.is_empty()
    }

    fn update_sequence_tombstone(
        &mut self,
        plugin_id: String,
        sequence: Option<u64>,
    ) -> Result<(), &'static str> {
        let Some(sequence) = sequence else {
            self.sequence_tombstones.remove(&plugin_id);
            return Ok(());
        };
        let already_tracked = self.sequence_tombstones.contains_key(&plugin_id);
        let active_other_sequences = self
            .sources
            .iter()
            .filter(|(id, source)| *id != &plugin_id && source.sequence.is_some())
            .count();
        if !already_tracked
            && active_other_sequences + self.sequence_tombstones.len() >= MAX_RESOURCE_PLUGINS
        {
            return Err("workspace may track sequences for at most 32 resource plugins");
        }
        self.sequence_tombstones.insert(plugin_id, sequence);
        Ok(())
    }

    pub fn next_expiry(&self) -> Option<Instant> {
        self.sources
            .values()
            .filter_map(|source| source.expires_at)
            .min()
    }
}

pub fn normalize_resource(
    plugin_id: &str,
    resource_id: String,
    label: String,
    detail: Option<String>,
    data: Option<Value>,
) -> Result<WorkspaceResource, String> {
    let resource_id = normalize_text(resource_id, MAX_RESOURCE_ID_CHARS, "resource_id")?;
    let label = normalize_text(label, MAX_RESOURCE_TEXT_CHARS, "label")?;
    let detail = detail
        .map(|detail| normalize_text(detail, MAX_RESOURCE_TEXT_CHARS, "detail"))
        .transpose()?;
    if let Some(data) = &data {
        validate_data(data, 0)?;
    }
    Ok(WorkspaceResource {
        plugin_id: plugin_id.to_string(),
        resource_id,
        label,
        detail,
        data,
    })
}

fn resource_data_bytes(resources: &[WorkspaceResource]) -> usize {
    resources
        .iter()
        .filter_map(|resource| resource.data.as_ref())
        .map(|data| serde_json::to_vec(data).map_or(0, |encoded| encoded.len()))
        .sum()
}

fn normalize_text(value: String, maximum: usize, field: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("resource {field} must not be empty"));
    }
    if value.chars().count() > maximum {
        return Err(format!(
            "resource {field} must be {maximum} characters or fewer"
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(format!(
            "resource {field} must not contain control characters"
        ));
    }
    Ok(value.to_owned())
}

fn validate_data(value: &Value, depth: usize) -> Result<(), String> {
    if depth > MAX_RESOURCE_DATA_DEPTH {
        return Err("resource data exceeds 16 nesting levels".into());
    }
    match value {
        Value::Array(values) => {
            for value in values {
                validate_data(value, depth + 1)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_data(value, depth + 1)?;
            }
        }
        _ => {}
    }
    if serde_json::to_vec(value)
        .map_err(|_| "resource data cannot be encoded")?
        .len()
        > MAX_RESOURCE_DATA_BYTES
    {
        return Err("resource data exceeds 8192 bytes".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(id: &str) -> WorkspaceResource {
        normalize_resource("hs.jail", id.into(), "lock jail".into(), None, None).unwrap()
    }

    #[test]
    fn report_replaces_atomically_and_rejects_stale_sequences() {
        let now = Instant::now();
        let mut registry = WorkspaceResourceRegistry::default();
        assert!(registry
            .report("hs.jail".into(), vec![resource("a")], Some(2), None, now)
            .unwrap());
        assert!(!registry
            .report("hs.jail".into(), vec![resource("b")], Some(2), None, now)
            .unwrap());
        assert_eq!(registry.find("hs.jail", "a").unwrap().label, "lock jail");
        assert!(registry
            .report("hs.jail".into(), vec![resource("b")], Some(3), None, now)
            .unwrap());
        assert!(registry.find("hs.jail", "a").is_none());
        assert!(registry.find("hs.jail", "b").is_some());
    }

    #[test]
    fn duplicate_resource_ids_are_rejected_without_replacing_existing_set() {
        let now = Instant::now();
        let mut registry = WorkspaceResourceRegistry::default();
        registry
            .report("hs.jail".into(), vec![resource("kept")], None, None, now)
            .unwrap();
        assert!(registry
            .report(
                "hs.jail".into(),
                vec![resource("duplicate"), resource("duplicate")],
                None,
                None,
                now
            )
            .is_err());
        assert!(registry.find("hs.jail", "kept").is_some());
    }

    #[test]
    fn clear_source_removes_non_expiring_resources_immediately() {
        let now = Instant::now();
        let mut registry = WorkspaceResourceRegistry::default();
        registry
            .report("hs.jail".into(), vec![resource("kept")], None, None, now)
            .unwrap();
        assert!(registry.clear_source("hs.jail"));
        assert!(registry.find("hs.jail", "kept").is_none());
        assert!(!registry.clear_source("hs.jail"));
    }

    #[test]
    fn report_expiry_removes_only_expired_plugin_set() {
        let now = Instant::now();
        let mut registry = WorkspaceResourceRegistry::default();
        registry
            .report(
                "one".into(),
                vec![resource("a")],
                None,
                Some(Duration::from_millis(1)),
                now,
            )
            .unwrap();
        registry
            .report("two".into(), vec![resource("b")], None, None, now)
            .unwrap();
        assert!(registry.expire(now + Duration::from_millis(1)));
        assert!(registry.find("one", "a").is_none());
        assert!(registry.find("two", "b").is_some());
    }

    #[test]
    fn empty_sequenced_report_clears_visible_source_but_fences_old_and_unsequenced_reports() {
        let now = Instant::now();
        let mut registry = WorkspaceResourceRegistry::default();
        registry
            .report("owner".into(), vec![resource("old")], Some(4), None, now)
            .unwrap();
        assert!(registry
            .report("owner".into(), Vec::new(), Some(5), None, now)
            .unwrap());
        assert!(registry.resources().next().is_none());
        assert!(!registry
            .report("owner".into(), vec![resource("stale")], Some(4), None, now)
            .unwrap());
        assert!(!registry
            .report(
                "owner".into(),
                vec![resource("unsequenced")],
                None,
                None,
                now
            )
            .unwrap());
        assert!(registry
            .report("owner".into(), vec![resource("fresh")], Some(6), None, now)
            .unwrap());
        assert!(registry.find("owner", "fresh").is_some());
        assert!(registry.clear_source("owner"));
        assert!(registry
            .report("owner".into(), vec![resource("new-owner")], None, None, now)
            .unwrap());
    }

    #[test]
    fn empty_reports_do_not_exhaust_visible_plugin_slots() {
        let now = Instant::now();
        let mut registry = WorkspaceResourceRegistry::default();
        for index in 0..MAX_RESOURCE_PLUGINS {
            assert!(!registry
                .report(format!("empty-{index}"), Vec::new(), None, None, now)
                .unwrap());
        }
        for index in 0..MAX_RESOURCE_PLUGINS {
            assert!(registry
                .report(
                    format!("visible-{index}"),
                    vec![resource(&format!("r{index}"))],
                    None,
                    None,
                    now,
                )
                .unwrap());
        }
    }

    #[test]
    fn expiry_preserves_sequence_fence_without_retaining_a_visible_source_slot() {
        let now = Instant::now();
        let mut registry = WorkspaceResourceRegistry::default();
        registry
            .report(
                "owner".into(),
                vec![resource("old")],
                Some(7),
                Some(Duration::from_millis(1)),
                now,
            )
            .unwrap();
        assert!(registry.expire(now + Duration::from_millis(1)));
        assert!(registry.resources().next().is_none());
        assert!(!registry
            .report("owner".into(), vec![resource("revived")], None, None, now)
            .unwrap());
        assert!(registry
            .report("owner".into(), vec![resource("new")], Some(8), None, now)
            .unwrap());
    }

    #[test]
    fn report_enforces_workspace_aggregate_data_budget() {
        let now = Instant::now();
        let data = Value::String("x".repeat(MAX_RESOURCE_DATA_BYTES - 2));
        let resources = (0..9)
            .map(|index| {
                normalize_resource(
                    "owner",
                    format!("r{index}"),
                    "resource".into(),
                    None,
                    Some(data.clone()),
                )
                .unwrap()
            })
            .collect();
        let mut registry = WorkspaceResourceRegistry::default();
        assert_eq!(
            registry
                .report("owner".into(), resources, None, None, now)
                .unwrap_err(),
            "workspace resource data may total at most 65536 bytes"
        );
    }
}
