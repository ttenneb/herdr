use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use crate::{
    app::collection_view::{CollectionGeometryProjection, TerminalGeometry},
    terminal::TerminalId,
};

const DRAG_DEBOUNCE: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
struct ForegroundClaim {
    client_id: u64,
    revision: u64,
    geometry: TerminalGeometry,
    accepted: Option<TerminalGeometry>,
    changed_at: Instant,
}

/// Server-owned arbitration for foreground full-app collection geometry.
///
/// Writable direct attaches are represented by `direct_locks` at application
/// time and therefore take precedence without destroying the app claim. A
/// withdrawn app claim deliberately leaves the PTY at its last accepted size.
#[derive(Default)]
pub(crate) struct GeometryClaims {
    claims: HashMap<TerminalId, ForegroundClaim>,
    next_revision: u64,
}

impl GeometryClaims {
    pub(crate) fn submit_foreground(
        &mut self,
        client_id: u64,
        desired: &CollectionGeometryProjection,
        dragging: bool,
        now: Instant,
    ) {
        self.claims.retain(|terminal_id, claim| {
            claim.client_id != client_id || desired.contains_key(terminal_id)
        });
        for (terminal_id, geometry) in desired {
            match self.claims.get_mut(terminal_id) {
                Some(claim) if claim.client_id == client_id && claim.geometry == *geometry => {}
                Some(claim) => {
                    self.next_revision = self.next_revision.saturating_add(1);
                    *claim = ForegroundClaim {
                        client_id,
                        revision: self.next_revision,
                        geometry: *geometry,
                        accepted: claim.accepted,
                        changed_at: if dragging { now } else { now - DRAG_DEBOUNCE },
                    };
                }
                None => {
                    self.next_revision = self.next_revision.saturating_add(1);
                    self.claims.insert(
                        terminal_id.clone(),
                        ForegroundClaim {
                            client_id,
                            revision: self.next_revision,
                            geometry: *geometry,
                            accepted: None,
                            changed_at: if dragging { now } else { now - DRAG_DEBOUNCE },
                        },
                    );
                }
            }
        }
    }

    pub(crate) fn withdraw_client(&mut self, client_id: u64) {
        self.claims.retain(|_, claim| claim.client_id != client_id);
    }

    pub(crate) fn ready(
        &mut self,
        direct_locks: &std::collections::HashSet<TerminalId>,
        now: Instant,
    ) -> Vec<(TerminalId, TerminalGeometry, u64)> {
        self.claims
            .iter_mut()
            .filter_map(|(terminal_id, claim)| {
                if direct_locks.contains(terminal_id)
                    || now.saturating_duration_since(claim.changed_at) < DRAG_DEBOUNCE
                    || claim.accepted == Some(claim.geometry)
                {
                    return None;
                }
                claim.accepted = Some(claim.geometry);
                Some((terminal_id.clone(), claim.geometry, claim.revision))
            })
            .collect()
    }

    /// Reassert the retained foreground claim after a direct attach disconnects.
    pub(crate) fn recover_after_direct_attach(
        &mut self,
        terminal_id: &TerminalId,
    ) -> Option<TerminalGeometry> {
        let claim = self.claims.get_mut(terminal_id)?;
        claim.accepted = Some(claim.geometry);
        Some(claim.geometry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn terminal(_raw: u64) -> TerminalId {
        TerminalId::alloc()
    }

    fn geometry(rows: u16, cols: u16) -> TerminalGeometry {
        TerminalGeometry {
            rows,
            cols,
            cell_width_px: 8,
            cell_height_px: 16,
        }
    }

    #[test]
    fn revisions_debounce_drag_and_withdraw_without_resize() {
        let now = Instant::now();
        let id = terminal(1);
        let mut claims = GeometryClaims::default();
        claims.submit_foreground(
            7,
            &HashMap::from([(id.clone(), geometry(8, 40))]),
            false,
            now,
        );
        let first = claims.ready(&Default::default(), now);
        assert_eq!(first.len(), 1);
        let first_revision = first[0].2;

        claims.submit_foreground(
            7,
            &HashMap::from([(id.clone(), geometry(10, 40))]),
            true,
            now,
        );
        assert!(claims.ready(&Default::default(), now).is_empty());
        let resized = claims.ready(&Default::default(), now + DRAG_DEBOUNCE);
        assert_eq!(resized[0].1, geometry(10, 40));
        assert!(resized[0].2 > first_revision);

        claims.submit_foreground(7, &HashMap::new(), false, now + DRAG_DEBOUNCE);
        assert!(claims
            .ready(&Default::default(), now + DRAG_DEBOUNCE)
            .is_empty());
    }

    #[test]
    fn direct_attach_blocks_then_disconnect_reasserts_foreground_claim() {
        let now = Instant::now();
        let id = terminal(2);
        let mut claims = GeometryClaims::default();
        claims.submit_foreground(
            3,
            &HashMap::from([(id.clone(), geometry(8, 50))]),
            false,
            now,
        );
        assert!(claims.ready(&HashSet::from([id.clone()]), now).is_empty());
        assert_eq!(
            claims.recover_after_direct_attach(&id),
            Some(geometry(8, 50))
        );
    }
}
