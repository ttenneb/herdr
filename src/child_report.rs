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

/// A current accepted child Pi stream supplies only its local canonical Todo
/// selectors. No caller, parent, session, process or epoch is wire-selectable.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TodoStateParams {
    pub protocol: String,
    pub local_root: String,
    pub local_revision: u64,
    pub state_digest: String,
    pub state: LocalTodoState,
}

impl TodoStateParams {
    pub fn bind(self, route: RouteIdentity) -> Option<ChildReportEvent> {
        if self.protocol != crate::mailbox_v1::PROTOCOL {
            return None;
        }
        let event = ChildReportEvent::TodoState {
            route,
            local_root: self.local_root,
            local_revision: self.local_revision,
            state_digest: self.state_digest,
            state: self.state,
        };
        event.valid().then_some(event)
    }
}

/// The accepted Pi stream supplies only Todo-local and immutable report
/// selectors. Caller, parent, grant, session, generation and epoch are never
/// accepted from the wire.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrepareParams {
    pub protocol: String,
    pub local_root: String,
    pub local_revision: u64,
    pub report_id: String,
    pub report_digest: String,
    pub stable_id: String,
    pub submit_revision: u64,
    pub submit_digest: String,
    pub delivery_digest: String,
    pub message_id: String,
}

impl PrepareParams {
    pub fn bind(self, route: RouteIdentity) -> Option<PreparedReport> {
        if self.protocol != crate::mailbox_v1::PROTOCOL {
            return None;
        }
        let prepared = PreparedReport {
            route,
            local_root: self.local_root,
            local_revision: self.local_revision,
            report_id: self.report_id,
            report_digest: self.report_digest,
            stable_id: self.stable_id,
            submit_revision: self.submit_revision,
            submit_digest: self.submit_digest,
            delivery_digest: self.delivery_digest,
            message_id: self.message_id,
        };
        prepared.valid().then_some(prepared)
    }
}

/// Immutable exact child report preparation. The caller provides only the
/// local Todo/report selectors; the route is minted by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreparedReport {
    pub route: RouteIdentity,
    pub local_root: String,
    pub local_revision: u64,
    pub report_id: String,
    pub report_digest: String,
    pub stable_id: String,
    pub submit_revision: u64,
    pub submit_digest: String,
    pub delivery_digest: String,
    pub message_id: String,
}

impl PreparedReport {
    fn valid(&self) -> bool {
        self.route.is_exact_pi_route()
            && self.local_revision > 0
            && [
                &self.local_root,
                &self.report_id,
                &self.stable_id,
                &self.message_id,
            ]
            .iter()
            .all(|id| !id.is_empty() && id.len() <= 128)
            && self.submit_revision > 0
            && [
                &self.report_digest,
                &self.submit_digest,
                &self.delivery_digest,
            ]
            .iter()
            .all(|digest| valid_digest(digest))
    }

    pub fn matches_submit(&self, submit: &crate::mailbox_v1::Submit) -> bool {
        submit.kind == "report"
            && submit.protocol == crate::mailbox_v1::PROTOCOL
            && self.stable_id == submit.stable_id
            && self.submit_revision == submit.revision
            && self.submit_digest == submit.digest
            && self.delivery_digest == submit.delivery_digest
            && self.message_id == submit.message_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CoverageQualification {
    #[default]
    ObservedOnly,
    AllPathsTrusted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportBypassPath {
    GenericOffline,
    HandoffPty,
}

/// Legacy Attempt/Coverage records remain decodable but cannot establish
/// report-root-specific authority. The new Prepared/PreparedAttempt/Barrier
/// records are additive and share the mailbox head/receipt journal.
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
    Prepared {
        preparation: PreparedReport,
    },
    PreparedAttempt {
        preparation: PreparedReport,
    },
    Bypass {
        route: RouteIdentity,
        path: ReportBypassPath,
        message_id: String,
    },
    /// This durable ACK is not a Todo-done signal. The Pi owner must bind
    /// future canonical local Todo state to this exact root/revision.
    CoverageBarrier {
        route: RouteIdentity,
        local_root: String,
        local_revision: u64,
        through_cursor: u64,
        #[serde(default)]
        qualification: CoverageQualification,
    },
    /// Legacy unscoped coverage cannot qualify missing.
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
            | Self::Bypass { route, .. }
            | Self::CoverageBarrier { route, .. }
            | Self::Coverage { route, .. } => route,
            Self::Prepared { preparation } | Self::PreparedAttempt { preparation } => {
                &preparation.route
            }
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
            } => {
                !local_root.is_empty()
                    && local_root.len() <= 128
                    && *local_revision > 0
                    && valid_digest(state_digest)
            }
            Self::Attempt {
                attempt_id,
                delivery_digest,
                ..
            } => !attempt_id.is_empty() && attempt_id.len() <= 128 && valid_digest(delivery_digest),
            Self::Prepared { preparation } | Self::PreparedAttempt { preparation } => {
                preparation.valid()
            }
            Self::Bypass { message_id, .. } => !message_id.is_empty() && message_id.len() <= 128,
            Self::CoverageBarrier {
                local_root,
                local_revision,
                ..
            }
            | Self::Coverage {
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
    // No legacy attempt/coverage or unprepared handoff may be reclassified as
    // complete coverage, including after a newer local Todo revision.
    if matching.iter().any(|event| {
        matches!(
            event,
            ChildReportEvent::Attempt { .. }
                | ChildReportEvent::Coverage { .. }
                | ChildReportEvent::Bypass { .. }
        )
    }) {
        disposition.kind = ReportDispositionKind::InFlightOrUncertain;
        return disposition;
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
    let all_prepared_attempts: Vec<_> = matching
        .iter()
        .filter_map(|event| match event {
            ChildReportEvent::PreparedAttempt { preparation } => Some(preparation),
            _ => None,
        })
        .collect();
    if all_prepared_attempts
        .iter()
        .any(|prepared| !exact_admitted(prepared, mailbox))
        || matching.iter().any(|event| {
            matches!(event,
            ChildReportEvent::Prepared { preparation }
                if !all_prepared_attempts.contains(&preparation))
        })
    {
        disposition.kind = ReportDispositionKind::InFlightOrUncertain;
        return disposition;
    }
    // Generic cross-recipient and earlier unprepared mailbox reports have no
    // trustworthy full-session/root join, even when their receipt is admitted.
    let untracked_head = mailbox.heads.values().any(|head| {
        head.kind == "report"
            && head.sender == current.child_terminal_id
            && head.target == current.parent_terminal_id
            && head.recipient.recipient_id == current.parent_terminal_id
            && !all_prepared_attempts.iter().any(|prepared| {
                prepared.delivery_digest == head.delivery_digest
                    && exact_admitted(prepared, mailbox)
            })
    });
    let orphan_receipt =
        mailbox
            .receipts
            .values()
            .any(|receipt| match mailbox.heads.get(&receipt.stable_id) {
                None => true,
                Some(head)
                    if head.sender == current.child_terminal_id
                        && head.target == current.parent_terminal_id =>
                {
                    head.delivery_digest != receipt.delivery_digest
                        || head.digest != receipt.digest
                        || head.revision != receipt.revision
                }
                _ => false,
            });
    if untracked_head || orphan_receipt {
        disposition.kind = ReportDispositionKind::InFlightOrUncertain;
        return disposition;
    }
    let current_preparations: Vec<_> = matching
        .iter()
        .filter_map(|event| match event {
            ChildReportEvent::Prepared { preparation }
                if preparation.local_root == local_root
                    && preparation.local_revision == revision =>
            {
                Some(preparation)
            }
            _ => None,
        })
        .collect();
    for preparation in &current_preparations {
        if all_prepared_attempts.contains(preparation) && exact_admitted(preparation, mailbox) {
            disposition.kind = ReportDispositionKind::AdmittedExactReport;
            return disposition;
        }
    }
    // A preparation not yet submitted is itself in-flight. A report whose
    // attempt has no exact durable receipt is uncertain across a crash.
    if !current_preparations.is_empty() {
        disposition.kind = ReportDispositionKind::InFlightOrUncertain;
        return disposition;
    }
    if state == LocalTodoState::NotDone {
        disposition.kind = ReportDispositionKind::NotDone;
        return disposition;
    }
    let covered = matching.iter().any(|event| matches!(event,
        ChildReportEvent::CoverageBarrier { local_root: root, local_revision: covered_revision,
            through_cursor, qualification: CoverageQualification::AllPathsTrusted, .. }
            if root == local_root && *covered_revision == revision && *through_cursor <= mailbox.record_cursor));
    disposition.kind = if covered {
        ReportDispositionKind::MissingAfterDoneNoAdmittedReceipt
    } else {
        ReportDispositionKind::InFlightOrUncertain
    };
    disposition
}

pub(crate) fn matches_prepared_head(
    prepared: &PreparedReport,
    head: &crate::mailbox::MailboxHead,
) -> bool {
    head.delivery_digest == prepared.delivery_digest
        && head.stable_id == prepared.stable_id
        && head.revision == prepared.submit_revision
        && head.digest == prepared.submit_digest
        && head.message_id == prepared.message_id
        && head.kind == "report"
        && head.sender == prepared.route.child_terminal_id
        && head.target == prepared.route.parent_terminal_id
        && head.recipient.recipient_id == prepared.route.parent_terminal_id
        && head.recipient.generation == "1"
        && head.grant_id == prepared.route.bound_grant_id()
}

fn exact_admitted(prepared: &PreparedReport, mailbox: &RecoveredMailbox) -> bool {
    let heads: Vec<_> = mailbox
        .heads
        .values()
        .filter(|head| matches_prepared_head(prepared, head))
        .collect();
    if heads.len() != 1 {
        return false;
    }
    let head = heads[0];
    mailbox
        .receipts
        .get(&prepared.delivery_digest)
        .is_some_and(|receipt| {
            receipt.status == ReceiptStatus::Admitted
                && receipt.stable_id == head.stable_id
                && receipt.revision == head.revision
                && receipt.digest == head.digest
                && receipt.delivery_digest == head.delivery_digest
        })
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
            ) if a == b && (root_a != root_b || rev_a >= rev_b) => return Err(()),
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
            (
                ChildReportEvent::Prepared { preparation: a },
                ChildReportEvent::Prepared { preparation: b },
            ) if a.route.child_delegation_id == b.route.child_delegation_id
                && (a.report_id == b.report_id
                    || a.stable_id == b.stable_id
                    || a.delivery_digest == b.delivery_digest
                    || a.message_id == b.message_id) =>
            {
                return Err(())
            }
            (
                ChildReportEvent::PreparedAttempt { preparation: a },
                ChildReportEvent::PreparedAttempt { preparation: b },
            ) if a.route.child_delegation_id == b.route.child_delegation_id
                && (a.report_id == b.report_id || a.delivery_digest == b.delivery_digest) =>
            {
                return Err(())
            }
            (
                ChildReportEvent::Bypass {
                    route: a,
                    message_id: id_a,
                    ..
                },
                ChildReportEvent::Bypass {
                    route: b,
                    message_id: id_b,
                    ..
                },
            ) if a == b && id_a == id_b => return Err(()),
            (
                ChildReportEvent::CoverageBarrier {
                    route: a,
                    local_root: root_a,
                    local_revision: rev_a,
                    ..
                },
                ChildReportEvent::CoverageBarrier {
                    route: b,
                    local_root: root_b,
                    local_revision: rev_b,
                    ..
                },
            ) if a == b && root_a == root_b && rev_a >= rev_b => return Err(()),
            _ => {}
        }
    }
    if identical {
        return Ok(false);
    }
    if let ChildReportEvent::PreparedAttempt { preparation } = next {
        if !events.iter().any(|event| {
            matches!(event,
            ChildReportEvent::Prepared { preparation: existing } if existing == preparation)
        }) {
            return Err(());
        }
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
    fn prepared(route: RouteIdentity) -> PreparedReport {
        PreparedReport {
            route,
            local_root: "root".into(),
            local_revision: 1,
            report_id: "report-one".into(),
            report_digest: "c".repeat(64),
            stable_id: "s".into(),
            submit_revision: 1,
            submit_digest: "c".repeat(64),
            delivery_digest: "b".repeat(64),
            message_id: "message".into(),
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
        let legacy = coverage(r.clone());
        m.child_report_events.push(legacy);
        assert_eq!(
            project(&r, &m).kind,
            ReportDispositionKind::InFlightOrUncertain
        );
        m.child_report_events.pop();
        m.child_report_events
            .push(ChildReportEvent::CoverageBarrier {
                route: r.clone(),
                local_root: "root".into(),
                local_revision: 2,
                through_cursor: 0,
                qualification: CoverageQualification::AllPathsTrusted,
            });
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
        let p = prepared(r.clone());
        m.child_report_events.extend([
            todo(r.clone(), LocalTodoState::Done),
            ChildReportEvent::Prepared {
                preparation: p.clone(),
            },
            ChildReportEvent::PreparedAttempt {
                preparation: p.clone(),
            },
            ChildReportEvent::CoverageBarrier {
                route: r.clone(),
                local_root: "root".into(),
                local_revision: 1,
                through_cursor: 0,
                qualification: CoverageQualification::AllPathsTrusted,
            },
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
        // A head without its exact prepared attempt cannot join full sessions.
        let mut untracked = m.clone();
        untracked
            .child_report_events
            .retain(|event| !matches!(event, ChildReportEvent::PreparedAttempt { .. }));
        assert_eq!(
            project(&r, &untracked).kind,
            ReportDispositionKind::InFlightOrUncertain
        );
        let mut newer = todo(r.clone(), LocalTodoState::Done);
        if let ChildReportEvent::TodoState { local_revision, .. } = &mut newer {
            *local_revision = 2;
        }
        m.child_report_events.push(newer);
        m.child_report_events
            .push(ChildReportEvent::CoverageBarrier {
                route: r.clone(),
                local_root: "root".into(),
                local_revision: 2,
                through_cursor: 0,
                qualification: CoverageQualification::AllPathsTrusted,
            });
        assert_eq!(
            project(&r, &m).kind,
            ReportDispositionKind::MissingAfterDoneNoAdmittedReceipt,
            "an older admitted report cannot satisfy a newer Todo revision"
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
    fn observed_coverage_and_bypass_never_prove_missing() {
        let r = route();
        let done = todo(r.clone(), LocalTodoState::Done);
        let observed = ChildReportEvent::CoverageBarrier {
            route: r.clone(),
            local_root: "root".into(),
            local_revision: 1,
            through_cursor: 0,
            qualification: CoverageQualification::ObservedOnly,
        };
        let mut m = RecoveredMailbox::default();
        m.child_report_events.extend([done, observed.clone()]);
        assert_eq!(
            project(&r, &m).kind,
            ReportDispositionKind::InFlightOrUncertain
        );
        m.child_report_events[1] = ChildReportEvent::CoverageBarrier {
            route: r.clone(),
            local_root: "root".into(),
            local_revision: 1,
            through_cursor: 0,
            qualification: CoverageQualification::AllPathsTrusted,
        };
        assert_eq!(
            project(&r, &m).kind,
            ReportDispositionKind::MissingAfterDoneNoAdmittedReceipt
        );
        m.child_report_events.push(ChildReportEvent::Bypass {
            route: r.clone(),
            path: ReportBypassPath::HandoffPty,
            message_id: "legacy-message".into(),
        });
        assert_eq!(
            project(&r, &m).kind,
            ReportDispositionKind::InFlightOrUncertain
        );
    }

    #[test]
    fn todo_state_ingress_replays_exact_and_rejects_root_revision_and_digest_conflicts() {
        use crate::mailbox::{MailboxError, MailboxStore};
        let path = std::env::temp_dir().join(format!(
            "herdr-132-todo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = MailboxStore::open(&path).unwrap();
        let r = route();
        let wire = serde_json::json!({"protocol":crate::mailbox_v1::PROTOCOL,
            "localRoot":"canonical","localRevision":1,"stateDigest":"a".repeat(64),
            "state":"not_done"});
        let params: TodoStateParams = serde_json::from_value(wire.clone()).unwrap();
        let first = params.bind(r.clone()).unwrap();
        assert_eq!(store.append_child_report_event(first.clone()).unwrap(), 1);
        assert_eq!(
            MailboxStore::existing(&path)
                .append_child_report_event(first.clone())
                .unwrap(),
            1
        );
        let mut conflicting = wire.clone();
        conflicting["stateDigest"] = serde_json::json!("b".repeat(64));
        let conflict = serde_json::from_value::<TodoStateParams>(conflicting)
            .unwrap()
            .bind(r.clone())
            .unwrap();
        assert_eq!(
            store.append_child_report_event(conflict),
            Err(MailboxError::ConflictingDuplicate)
        );
        let mut changed_root = wire.clone();
        changed_root["localRoot"] = serde_json::json!("replacement-root");
        changed_root["localRevision"] = serde_json::json!(2);
        let root_conflict = serde_json::from_value::<TodoStateParams>(changed_root)
            .unwrap()
            .bind(r.clone())
            .unwrap();
        assert_eq!(
            store.append_child_report_event(root_conflict),
            Err(MailboxError::ConflictingDuplicate)
        );
        let mut route_selector = wire.clone();
        route_selector["parentSession"] = serde_json::json!("forged");
        assert!(serde_json::from_value::<TodoStateParams>(route_selector).is_err());
        let mut invalid = wire.clone();
        invalid["stateDigest"] = serde_json::json!("A".repeat(64));
        assert!(serde_json::from_value::<TodoStateParams>(invalid)
            .unwrap()
            .bind(r.clone())
            .is_none());
        let mut second = wire;
        second["localRevision"] = serde_json::json!(2);
        second["stateDigest"] = serde_json::json!("b".repeat(64));
        second["state"] = serde_json::json!("done");
        let done = serde_json::from_value::<TodoStateParams>(second)
            .unwrap()
            .bind(r.clone())
            .unwrap();
        assert_eq!(store.append_child_report_event(done.clone()).unwrap(), 2);
        assert_eq!(
            store.append_child_report_event(first),
            Err(MailboxError::ConflictingDuplicate)
        );
        let recovered = MailboxStore::existing(&path).load().unwrap();
        assert_eq!(recovered.record_cursor, 2);
        assert_eq!(recovered.child_report_events[1], done);
        assert_eq!(
            project(&r, &recovered).kind,
            ReportDispositionKind::InFlightOrUncertain
        );
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn prepared_attempt_and_observed_barrier_recover_without_a_false_receipt() {
        use crate::mailbox::{MailboxError, MailboxStore};
        let path = std::env::temp_dir().join(format!(
            "herdr-129-prepared-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = MailboxStore::open(&path).unwrap();
        let r = route();
        let p = prepared(r.clone());
        let prep = ChildReportEvent::Prepared {
            preparation: p.clone(),
        };
        let sent = ChildReportEvent::PreparedAttempt {
            preparation: p.clone(),
        };
        assert_eq!(
            store.append_child_report_event(sent.clone()),
            Err(MailboxError::ConflictingDuplicate)
        );
        assert_eq!(
            store
                .append_child_report_event(todo(r.clone(), LocalTodoState::Done))
                .unwrap(),
            1
        );
        assert_eq!(store.append_child_report_event(prep.clone()).unwrap(), 2);
        assert_eq!(store.append_child_report_event(prep).unwrap(), 2);
        let mut conflict = p.clone();
        conflict.local_revision = 2;
        assert_eq!(
            store.append_child_report_event(ChildReportEvent::Prepared {
                preparation: conflict
            }),
            Err(MailboxError::ConflictingDuplicate)
        );
        assert_eq!(store.append_child_report_event(sent).unwrap(), 3);
        let barrier = ChildReportEvent::CoverageBarrier {
            route: r.clone(),
            local_root: "root".into(),
            local_revision: 1,
            through_cursor: 0,
            qualification: CoverageQualification::ObservedOnly,
        };
        assert_eq!(store.append_child_report_event(barrier.clone()).unwrap(), 4);
        assert_eq!(store.append_child_report_event(barrier).unwrap(), 4);
        assert_eq!(
            store.append_child_report_event(ChildReportEvent::CoverageBarrier {
                route: r.clone(),
                local_root: "root".into(),
                local_revision: 1,
                through_cursor: 0,
                qualification: CoverageQualification::AllPathsTrusted,
            }),
            Err(MailboxError::ConflictingDuplicate)
        );
        let recovered = MailboxStore::existing(&path).load().unwrap();
        assert_eq!(recovered.record_cursor, 4);
        assert!(matches!(
            recovered.child_report_events[3],
            ChildReportEvent::CoverageBarrier {
                through_cursor: 3,
                qualification: CoverageQualification::ObservedOnly,
                ..
            }
        ));
        assert_eq!(
            project(&r, &recovered).kind,
            ReportDispositionKind::InFlightOrUncertain
        );
        std::fs::remove_dir_all(path).unwrap();
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
