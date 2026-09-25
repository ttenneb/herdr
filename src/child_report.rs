//! Server-side *projection* for delegated-child report visibility.
//!
//! This cannot manufacture a Todo completion: only a future, explicitly
//! trusted Pi Todo producer may append a local-state/coverage event. An absent
//! producer/coverage is unknown, never a missing report.

use serde::{Deserialize, Serialize};

use crate::api::schema::AgentSessionInfo;
use crate::mailbox::{ReceiptStatus, RecoveredMailbox};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteIdentity {
    pub child_delegation_id: String,
    pub parent_delegation_id: String,
    pub child_pane_id: String,
    pub parent_pane_id: String,
    pub child_terminal_id: String,
    pub parent_terminal_id: String,
    pub child_process_generation: u64,
    pub parent_process_generation: u64,
    pub child_session: AgentSessionInfo,
    pub parent_session: AgentSessionInfo,
    pub child_route_revision: u64,
    pub parent_route_revision: u64,
    pub route_epoch: String,
}

impl RouteIdentity {
    fn bound_grant_id(&self) -> String {
        format!(
            "bound-parent-report:{}:1:{}:1:{}",
            self.child_terminal_id, self.parent_terminal_id, self.route_epoch
        )
    }

    fn is_exact_pi_route(&self) -> bool {
        !self.child_delegation_id.is_empty()
            && !self.parent_delegation_id.is_empty()
            && self.child_delegation_id != self.parent_delegation_id
            && !self.child_terminal_id.is_empty()
            && !self.parent_terminal_id.is_empty()
            && self.child_terminal_id != self.parent_terminal_id
            && !self.child_pane_id.is_empty()
            && !self.parent_pane_id.is_empty()
            && self.child_process_generation > 0
            && self.parent_process_generation > 0
            && !self.route_epoch.is_empty()
            && [&self.child_session, &self.parent_session]
                .iter()
                .all(|session| {
                    session.source == "herdr:pi"
                        && session.agent == "pi"
                        && session.kind == crate::agent_resume::AgentSessionRefKind::Path
                        && crate::agent_resume::AgentSessionRef::path(session.value.clone())
                            .is_some()
                })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalTodoState {
    NotDone,
    Done,
}

/// All values other than the Todo-local root/revision and attempt selector are
/// copied from the server's exact route, never from a wire selector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChildReportEvent {
    TodoState {
        route: RouteIdentity,
        local_root: String,
        local_revision: u64,
        state_digest: String,
        state: LocalTodoState,
    },
    Attempt {
        route: RouteIdentity,
        attempt_id: String,
        delivery_digest: String,
    },
    /// The trusted Pi owner, not Herdr, must certify that every report path for
    /// this Todo revision preannounced attempts and no earlier send is untracked.
    Coverage {
        route: RouteIdentity,
        local_root: String,
        local_revision: u64,
    },
}

impl ChildReportEvent {
    pub fn route(&self) -> &RouteIdentity {
        match self {
            Self::TodoState { route, .. }
            | Self::Attempt { route, .. }
            | Self::Coverage { route, .. } => route,
        }
    }

    fn valid(&self) -> bool {
        if !self.route().is_exact_pi_route() {
            return false;
        }
        match self {
            Self::TodoState {
                local_root,
                local_revision,
                state_digest,
                ..
            } => !local_root.is_empty() && *local_revision > 0 && valid_digest(state_digest),
            Self::Attempt {
                attempt_id,
                delivery_digest,
                ..
            } => !attempt_id.is_empty() && attempt_id.len() <= 128 && valid_digest(delivery_digest),
            Self::Coverage {
                local_root,
                local_revision,
                ..
            } => !local_root.is_empty() && *local_revision > 0,
        }
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportDispositionKind {
    UnknownUnattested,
    NotDone,
    InFlightOrUncertain,
    MissingAfterDoneNoAdmittedReceipt,
    AdmittedExactReport,
    StaleOrReplaced,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportDisposition {
    pub kind: ReportDispositionKind,
    pub cursor: u64,
    pub route_epoch: String,
}

/// Pure conservative projection over a recovered server journal. A receipt is
/// admitted only if the exact attempted delivery joins a report head, bound
/// grant, sender, recipient, and durable receipt for this route.
pub fn project(current: &RouteIdentity, mailbox: &RecoveredMailbox) -> ReportDisposition {
    let mut disposition = ReportDisposition {
        kind: ReportDispositionKind::UnknownUnattested,
        cursor: mailbox.record_cursor,
        route_epoch: current.route_epoch.clone(),
    };
    if !current.is_exact_pi_route() {
        disposition.kind = ReportDispositionKind::StaleOrReplaced;
        return disposition;
    }
    let events = &mailbox.child_report_events;
    let matching: Vec<_> = events
        .iter()
        .filter(|event| event.route() == current)
        .collect();
    if matching.is_empty() {
        if events
            .iter()
            .any(|event| event.route().child_delegation_id == current.child_delegation_id)
        {
            disposition.kind = ReportDispositionKind::StaleOrReplaced;
        }
        return disposition;
    }
    let attempts: Vec<_> = matching
        .iter()
        .filter_map(|event| match event {
            ChildReportEvent::Attempt {
                delivery_digest, ..
            } => Some(delivery_digest.as_str()),
            _ => None,
        })
        .collect();
    let mut unmatched_attempt = false;
    for &delivery_digest in &attempts {
        let matching_heads: Vec<_> = mailbox
            .heads
            .values()
            .filter(|head| {
                head.delivery_digest == delivery_digest
                    && head.kind == "report"
                    && head.sender == current.child_terminal_id
                    && head.target == current.parent_terminal_id
                    && head.grant_id == current.bound_grant_id()
                    && head.recipient.recipient_id == current.parent_terminal_id
                    && head.recipient.generation == "1"
            })
            .collect();
        if matching_heads.len() != 1 {
            unmatched_attempt = true;
            continue;
        }
        let head = matching_heads[0];
        if mailbox
            .receipts
            .get(delivery_digest)
            .is_some_and(|receipt| {
                receipt.status == ReceiptStatus::Admitted
                    && receipt.stable_id == head.stable_id
                    && receipt.revision == head.revision
                    && receipt.digest == head.digest
                    && receipt.delivery_digest == head.delivery_digest
            })
        {
            disposition.kind = ReportDispositionKind::AdmittedExactReport;
            return disposition;
        }
        unmatched_attempt = true;
    }
    let todo = matching.iter().rev().find_map(|event| match event {
        ChildReportEvent::TodoState {
            local_root,
            local_revision,
            state,
            ..
        } => Some((local_root.as_str(), *local_revision, *state)),
        _ => None,
    });
    let Some((local_root, revision, state)) = todo else {
        disposition.kind = ReportDispositionKind::InFlightOrUncertain;
        return disposition;
    };
    // Older or external report paths may have written the mailbox head
    // without this new preannounced-attempt event. They can never establish
    // an exact full Pi-session join, and must veto a false missing result.
    let untracked_bound_head = mailbox.heads.values().any(|head| {
        head.kind == "report"
            && head.sender == current.child_terminal_id
            && head.target == current.parent_terminal_id
            && head.grant_id == current.bound_grant_id()
            && head.recipient.recipient_id == current.parent_terminal_id
            && !attempts.contains(&head.delivery_digest.as_str())
    });
    let orphan_receipt = mailbox.receipts.values().any(|receipt| {
        !mailbox.heads.values().any(|head| {
            head.stable_id == receipt.stable_id
                && head.delivery_digest == receipt.delivery_digest
                && head.digest == receipt.digest
                && head.revision == receipt.revision
        })
    });
    if unmatched_attempt || untracked_bound_head || orphan_receipt {
        disposition.kind = ReportDispositionKind::InFlightOrUncertain;
        return disposition;
    }
    if state == LocalTodoState::NotDone {
        disposition.kind = ReportDispositionKind::NotDone;
        return disposition;
    }
    let covered = matching.iter().any(|event| {
        matches!(event,
        ChildReportEvent::Coverage { local_root: root, local_revision: covered_revision, .. }
            if root == local_root && *covered_revision == revision)
    });
    disposition.kind = if !covered {
        ReportDispositionKind::InFlightOrUncertain
    } else {
        ReportDispositionKind::MissingAfterDoneNoAdmittedReceipt
    };
    disposition
}

/// Called under the mailbox store's exclusive append lock. A repeat is a
/// no-op; a conflicting same-revision/delivery selector fails closed.
pub fn validate_next(events: &[ChildReportEvent], next: &ChildReportEvent) -> Result<bool, ()> {
    if !next.valid() {
        return Err(());
    }
    let mut identical = false;
    for event in events {
        if event == next {
            identical = true;
            continue;
        }
        match (event, next) {
            (
                ChildReportEvent::TodoState {
                    route: a,
                    local_root: root_a,
                    local_revision: rev_a,
                    ..
                },
                ChildReportEvent::TodoState {
                    route: b,
                    local_root: root_b,
                    local_revision: rev_b,
                    ..
                },
            ) if a == b && root_a == root_b && rev_a >= rev_b => return Err(()),
            (
                ChildReportEvent::Attempt {
                    route: a,
                    attempt_id: id_a,
                    delivery_digest: digest_a,
                },
                ChildReportEvent::Attempt {
                    route: b,
                    attempt_id: id_b,
                    delivery_digest: digest_b,
                },
            ) if a == b && (id_a == id_b || digest_a == digest_b) => return Err(()),
            (
                ChildReportEvent::Coverage {
                    route: a,
                    local_root: root_a,
                    local_revision: rev_a,
                },
                ChildReportEvent::Coverage {
                    route: b,
                    local_root: root_b,
                    local_revision: rev_b,
                },
            ) if a == b && root_a == root_b && rev_a >= rev_b => return Err(()),
            _ => {}
        }
    }
    if identical {
        return Ok(false);
    }
    if let ChildReportEvent::Coverage {
        route,
        local_root,
        local_revision,
    } = next
    {
        if !events.iter().any(|event| matches!(event,
            ChildReportEvent::TodoState { route: existing, local_root: root, local_revision: revision, state: LocalTodoState::Done, .. }
                if existing == route && root == local_root && revision == local_revision)) {
            return Err(());
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_resume::AgentSessionRefKind;
    use crate::mailbox::{AdmissionReceipt, MailboxHead, RecipientKey};

    fn route() -> RouteIdentity {
        RouteIdentity {
            child_delegation_id: "d2".into(),
            parent_delegation_id: "d1".into(),
            child_pane_id: "w:p2".into(),
            parent_pane_id: "w:p1".into(),
            child_terminal_id: "child-terminal".into(),
            parent_terminal_id: "parent-terminal".into(),
            child_process_generation: 2,
            parent_process_generation: 3,
            child_session: AgentSessionInfo {
                source: "herdr:pi".into(),
                agent: "pi".into(),
                kind: AgentSessionRefKind::Path,
                value: "/child.jsonl".into(),
            },
            parent_session: AgentSessionInfo {
                source: "herdr:pi".into(),
                agent: "pi".into(),
                kind: AgentSessionRefKind::Path,
                value: "/parent.jsonl".into(),
            },
            child_route_revision: 2,
            parent_route_revision: 1,
            route_epoch: "epoch-a".into(),
        }
    }
    fn todo(route: RouteIdentity, state: LocalTodoState) -> ChildReportEvent {
        ChildReportEvent::TodoState {
            route,
            local_root: "root".into(),
            local_revision: 1,
            state_digest: "a".repeat(64),
            state,
        }
    }
    fn coverage(route: RouteIdentity) -> ChildReportEvent {
        ChildReportEvent::Coverage {
            route,
            local_root: "root".into(),
            local_revision: 1,
        }
    }
    fn attempt(route: RouteIdentity) -> ChildReportEvent {
        ChildReportEvent::Attempt {
            route,
            attempt_id: "one".into(),
            delivery_digest: "b".repeat(64),
        }
    }
    #[test]
    fn no_false_missing_without_durable_done_and_coverage() {
        let r = route();
        let mut m = RecoveredMailbox::default();
        assert_eq!(
            project(&r, &m).kind,
            ReportDispositionKind::UnknownUnattested
        );
        m.child_report_events
            .push(todo(r.clone(), LocalTodoState::NotDone));
        assert_eq!(project(&r, &m).kind, ReportDispositionKind::NotDone);
        let mut completed = todo(r.clone(), LocalTodoState::Done);
        if let ChildReportEvent::TodoState { local_revision, .. } = &mut completed {
            *local_revision = 2;
        }
        m.child_report_events.push(completed);
        assert_eq!(
            project(&r, &m).kind,
            ReportDispositionKind::InFlightOrUncertain
        );
        let mut barrier = coverage(r.clone());
        if let ChildReportEvent::Coverage { local_revision, .. } = &mut barrier {
            *local_revision = 2;
        }
        m.child_report_events.push(barrier);
        assert_eq!(
            project(&r, &m).kind,
            ReportDispositionKind::MissingAfterDoneNoAdmittedReceipt
        );
        m.child_report_events.push(attempt(r.clone()));
        assert_eq!(
            project(&r, &m).kind,
            ReportDispositionKind::InFlightOrUncertain
        );
    }
    #[test]
    fn exact_admission_and_stale_identity() {
        let r = route();
        let mut m = RecoveredMailbox::default();
        m.child_report_events.extend([
            todo(r.clone(), LocalTodoState::Done),
            attempt(r.clone()),
            coverage(r.clone()),
        ]);
        let head = MailboxHead {
            stable_id: "s".into(),
            revision: 1,
            digest: "c".repeat(64),
            delivery_digest: "b".repeat(64),
            recipient: RecipientKey {
                recipient_id: r.parent_terminal_id.clone(),
                generation: "1".into(),
            },
            subject: "report".into(),
            body: "body".into(),
            recipient_generation: "1".into(),
            sender: r.child_terminal_id.clone(),
            target: r.parent_terminal_id.clone(),
            grant_id: r.bound_grant_id(),
            message_id: "message".into(),
            kind: "report".into(),
            priority: "normal".into(),
            original_sequence: 1,
            enqueue_epoch: 1,
            accepted_at: 1,
        };
        m.heads.insert(head.stable_id.clone(), head.clone());
        assert_eq!(
            project(&r, &m).kind,
            ReportDispositionKind::InFlightOrUncertain
        );
        m.receipts.insert(
            head.delivery_digest.clone(),
            AdmissionReceipt {
                delivery_digest: head.delivery_digest.clone(),
                stable_id: head.stable_id.clone(),
                revision: 1,
                digest: head.digest.clone(),
                status: ReceiptStatus::Admitted,
            },
        );
        assert_eq!(
            project(&r, &m).kind,
            ReportDispositionKind::AdmittedExactReport
        );
        // An old bound-parent mailbox head without a durable attempt cannot
        // join full Pi sessions: even with Todo coverage, never call it missing.
        let mut untracked = m.clone();
        untracked
            .child_report_events
            .retain(|event| !matches!(event, ChildReportEvent::Attempt { .. }));
        assert_eq!(
            project(&r, &untracked).kind,
            ReportDispositionKind::InFlightOrUncertain
        );
        let mut changed = r.clone();
        changed.parent_session.value = "/replacement.jsonl".into();
        assert_eq!(
            project(&changed, &m).kind,
            ReportDispositionKind::StaleOrReplaced
        );
        changed = r.clone();
        changed.route_epoch = "epoch-b".into();
        assert_eq!(
            project(&changed, &m).kind,
            ReportDispositionKind::StaleOrReplaced
        );
    }
    #[test]
    fn journal_recovery_cursor_duplicate_and_crash_gap_are_fail_closed() {
        use crate::mailbox::{MailboxError, MailboxStore};
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "herdr-121-child-report-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = MailboxStore::open(&path).unwrap();
        let route = route();
        let done = todo(route.clone(), LocalTodoState::Done);
        let attempt = attempt(route.clone());
        assert_eq!(store.append_child_report_event(done.clone()).unwrap(), 1);
        assert_eq!(store.append_child_report_event(done.clone()).unwrap(), 1);
        assert_eq!(store.append_child_report_event(attempt).unwrap(), 2);
        assert_eq!(
            project(&route, &store.load().unwrap()).kind,
            ReportDispositionKind::InFlightOrUncertain
        );
        assert_eq!(
            store
                .append_child_report_event(coverage(route.clone()))
                .unwrap(),
            3
        );
        let restarted = MailboxStore::open(&path).unwrap();
        let recovered = restarted.load().unwrap();
        assert_eq!(recovered.record_cursor, 3);
        // The preannounced attempt with no admitted receipt survives restart.
        assert_eq!(
            project(&route, &recovered).kind,
            ReportDispositionKind::InFlightOrUncertain
        );
        assert_eq!(
            restarted.append_child_report_event(todo(route, LocalTodoState::NotDone)),
            Err(MailboxError::ConflictingDuplicate)
        );
        // A torn/corrupt journal is never interpreted as no reports.
        std::fs::OpenOptions::new()
            .append(true)
            .open(path.join(crate::mailbox::RECORD_STREAM_FILE))
            .unwrap()
            .write_all(b"{incomplete\n")
            .unwrap();
        assert_eq!(restarted.load(), Err(MailboxError::CorruptRecord));
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn duplicate_and_revision_guards() {
        let r = route();
        let e = todo(r.clone(), LocalTodoState::Done);
        assert_eq!(validate_next(&[], &e), Ok(true));
        assert_eq!(validate_next(std::slice::from_ref(&e), &e), Ok(false));
        assert_eq!(
            validate_next(
                std::slice::from_ref(&e),
                &todo(r.clone(), LocalTodoState::NotDone)
            ),
            Err(())
        );
        assert_eq!(validate_next(&[], &coverage(r.clone())), Err(()));
        assert_eq!(validate_next(&[e], &coverage(r.clone())), Ok(true));
        let a = attempt(r.clone());
        assert_eq!(validate_next(&[a.clone()], &a), Ok(false));
        if let ChildReportEvent::Attempt {
            route,
            delivery_digest,
            ..
        } = a
        {
            let conflict = ChildReportEvent::Attempt {
                route,
                attempt_id: "second".into(),
                delivery_digest,
            };
            assert_eq!(validate_next(&[attempt(r)], &conflict), Err(()));
        }
    }
}
