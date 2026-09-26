//! Server-owned covered-child closure journal (#159).
//!
//! A *covered* child is one whose launch was registered through the
//! enforcement seam before its route became ready. For such a child the server
//! records a durable domain, and after the child's ACKed Todo `done` it runs a
//! closure barrier that classifies every report attempt on that route. Only a
//! barrier that covered the whole interval under verified enforcement, with no
//! path attempt, preparation or admitted report, may yield
//! `missing_after_done`. Every other outcome is `report_unknown` with a reason,
//! or (for an exact admitted report) no signal at all.
//!
//! The records live in their own append-only file next to the mailbox journal
//! and are written under the mailbox's exclusive lock. Closure cursors (the
//! signal `cursor`, `closureCursor`, recovery, decline and wake cursors) are
//! strictly increasing and always minted above the mailbox cursor current at
//! write time, so `todoStateCursor < closureCursor < cursor` holds although
//! `todoStateCursor` is a mailbox-journal cursor. An older Herdr that does not
//! know this file ignores it.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};

use serde::{Deserialize, Serialize};

use crate::child_report::{ChildReportEvent, LocalTodoState, RouteIdentity};
use crate::mailbox::{MailboxError, MailboxStore, RecoveredMailbox};

pub const CLOSURE_STREAM_FILE: &str = "child-report-closure.v1.jsonl";
/// Upper bound on records returned by one signal/recovery page.
pub const MAX_PAGE: usize = 64;
/// Upper bound on recovery requests returned to a child by one wait.
pub const MAX_RECOVERY_PAGE: usize = 8;
pub const RECOVERY_WAKE_KIND: &str = "recovery_wake";
pub const RECOVERY_WAKE_SUBJECT: &str = "System recovery request";

/// Canonical wake head body; Pi renders fixed text and never this body.
pub fn recovery_wake_body(signal_cursor: u64, wake_cursor: u64) -> String {
    format!("{{\"signalCursor\":{signal_cursor},\"wakeCursor\":{wake_cursor}}}")
}

pub fn recovery_wake_stable_id(signal_cursor: u64) -> String {
    format!("recovery-wake-{signal_cursor}")
}
/// Upper bound on a parked long-poll.
pub const MAX_WAIT_MS: u64 = 30_000;
/// The only HERDR_* variables a covered (sandboxed) child launch may inherit.
/// Pi reads only the bootstrap address; HERDR_AGENT is Herdr's own process
/// classification hint. The CLI/API socket variables are deliberately absent.
pub const COVERED_CHILD_HERDR_ENV_ALLOWLIST: &[&str] =
    &["HERDR_MAILBOX_BOOTSTRAP_ADDRESS", "HERDR_AGENT"];

/// Keep exactly the allowlisted HERDR_* names from a Herdr-provided child
/// environment. A name given twice is ambiguous and is dropped. Non-HERDR
/// variables are the sandbox policy's concern and are not passed through.
pub fn covered_child_herdr_environment(
    environment: impl IntoIterator<Item = (String, String)>,
) -> Vec<(String, String)> {
    let mut seen: std::collections::BTreeMap<String, Option<String>> = Default::default();
    for (name, value) in environment {
        if !COVERED_CHILD_HERDR_ENV_ALLOWLIST.contains(&name.as_str()) || value.contains('\0') {
            continue;
        }
        seen.entry(name)
            .and_modify(|existing| *existing = None)
            .or_insert(Some(value));
    }
    seen.into_iter()
        .filter_map(|(name, value)| value.map(|value| (name, value)))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BirthRecord {
    pub pid: u32,
    pub start_ticks: u64,
}

impl From<crate::platform::ProcessBirthIdentity> for BirthRecord {
    fn from(value: crate::platform::ProcessBirthIdentity) -> Self {
        Self {
            pid: value.pid,
            start_ticks: value.start_ticks,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WriterIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// What the covered-launch seam (#145) registers for one managed Pi launch.
/// `sandboxed_birth` is the process the sandbox receipt names; it must be the
/// same process Herdr bound as the managed launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CoveredLaunchPolicy {
    pub policy_id: String,
    pub policy_hash: String,
    pub receipt_digest: String,
    pub sandboxed_birth: BirthRecord,
}

impl CoveredLaunchPolicy {
    pub fn valid(&self) -> bool {
        !self.policy_id.is_empty()
            && self.policy_id.len() <= 128
            && valid_digest(&self.policy_hash)
            && valid_digest(&self.receipt_digest)
    }
}

/// Input to the enforcement verifier: the registered policy/receipt and the
/// server's own managed-launch evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnforcementQuery<'a> {
    pub policy: &'a CoveredLaunchPolicy,
    pub managed_launch_birth: BirthRecord,
    pub launch_floor_ticks: u64,
}

/// A verifier must attest that the sandbox covered the child *from its
/// launch/exec*, i.e. from the exact managed-launch birth, not merely from
/// route readiness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnforcementAttestation {
    pub policy_hash: String,
    pub covered_from_birth: BirthRecord,
}

pub trait EnforcementVerifier: Send + Sync {
    fn verify(&self, query: &EnforcementQuery<'_>) -> Result<EnforcementAttestation, String>;
}

/// Production verifier until the #145 seam lands: nothing is attested.
#[cfg_attr(target_os = "linux", allow(dead_code))] // Linux uses the kernel verifier.
pub struct UnprovenEnforcementVerifier;

impl EnforcementVerifier for UnprovenEnforcementVerifier {
    fn verify(&self, _query: &EnforcementQuery<'_>) -> Result<EnforcementAttestation, String> {
        Err("enforcement_unproven".into())
    }
}

/// Any gap between the managed launch and the attested enforcement interval
/// is unqualified: the receipt must name the managed-launch process, the
/// attestation must start at that same birth, the birth must not precede the
/// launch floor, and the policy hash must match.
pub fn enforcement_covers_launch(
    query: &EnforcementQuery<'_>,
    attestation: &EnforcementAttestation,
) -> bool {
    query.policy.valid()
        && query.policy.sandboxed_birth == query.managed_launch_birth
        && attestation.covered_from_birth == query.managed_launch_birth
        && query.managed_launch_birth.start_ticks >= query.launch_floor_ticks
        && attestation.policy_hash == query.policy.policy_hash
}

pub fn verify_enforcement(
    verifier: &dyn EnforcementVerifier,
    query: &EnforcementQuery<'_>,
) -> Result<(), String> {
    let attestation = verifier.verify(query)?;
    if enforcement_covers_launch(query, &attestation) {
        Ok(())
    } else {
        Err("enforcement_gap".into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownReason {
    InFlight,
    PathAttemptUncertain,
    PreparedNotAdmitted,
    StaleEpoch,
    RouteReplaced,
    ChildProcessGone,
    CoverageUnqualified,
    RecoveryDeclinedNoExportedReport,
    RecoveryDeclinedPriorAttemptUncertain,
    RecoveryDeclinedAlreadyAdmitted,
    RecoveryDeclinedStateMismatch,
}

impl UnknownReason {
    pub fn declined(reason: DeclineReason) -> Self {
        match reason {
            DeclineReason::NoExportedReport => Self::RecoveryDeclinedNoExportedReport,
            DeclineReason::PriorAttemptUncertain => Self::RecoveryDeclinedPriorAttemptUncertain,
            DeclineReason::AlreadyAdmitted => Self::RecoveryDeclinedAlreadyAdmitted,
            DeclineReason::StateMismatch => Self::RecoveryDeclinedStateMismatch,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalType {
    MissingAfterDone,
    ReportUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureOutcome {
    MissingAfterDone,
    Admitted,
    ReportUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclineReason {
    NoExportedReport,
    PriorAttemptUncertain,
    AlreadyAdmitted,
    StateMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CoveredDomain {
    pub route: RouteIdentity,
    pub child_birth: BirthRecord,
    pub launch_floor_ticks: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer: Option<WriterIdentity>,
    pub policy: CoveredLaunchPolicy,
    pub enforcement_verified: bool,
    /// Set when the domain could not cover the whole interval from creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unqualified_reason: Option<String>,
    pub baseline_mailbox_cursor: u64,
    pub server_boot: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DoneEvidence {
    pub local_root: String,
    pub local_revision: u64,
    pub state_digest: String,
    pub state: LocalTodoState,
    /// Mailbox-journal cursor of the ACKed done TodoState.
    pub todo_state_cursor: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClosureCounts {
    pub path_attempt_count: u64,
    pub prepared_count: u64,
    pub admitted_report_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignalClosure {
    pub coverage_qualified: bool,
    pub closure_cursor: u64,
    pub path_attempt_count: u64,
    pub prepared_count: u64,
    pub admitted_report_count: u64,
}

impl SignalClosure {
    pub fn new(coverage_qualified: bool, closure_cursor: u64, counts: ClosureCounts) -> Self {
        Self {
            coverage_qualified,
            closure_cursor,
            path_attempt_count: counts.path_attempt_count,
            prepared_count: counts.prepared_count,
            admitted_report_count: counts.admitted_report_count,
        }
    }

    pub fn counts(&self) -> ClosureCounts {
        ClosureCounts {
            path_attempt_count: self.path_attempt_count,
            prepared_count: self.prepared_count,
            admitted_report_count: self.admitted_report_count,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ParentTodo {
    pub delegation_id: String,
    pub parent_task_id: u64,
}

impl ParentTodo {
    /// Pi's Todo delegation IDs are exactly 22 URL-safe base64 characters;
    /// anything else would make every echoing signal undecodable for Pi.
    pub fn valid(&self) -> bool {
        self.delegation_id.len() == 22
            && self
                .delegation_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            && self.parent_task_id > 0
            && self.parent_task_id < (1 << 53)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignalRecovery {
    pub available: bool,
}

/// The typed parent signal. Never a mailbox head and never prompt text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildReportSignal {
    #[serde(rename = "cursor")]
    pub signal_cursor: u64,
    #[serde(rename = "type")]
    pub signal_type: SignalType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<UnknownReason>,
    pub route: RouteIdentity,
    pub todo: DoneEvidence,
    pub closure: SignalClosure,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_todo: Option<ParentTodo>,
    pub recovery: SignalRecovery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClosureRecord {
    CoveredDomain {
        cursor: u64,
        domain: CoveredDomain,
    },
    DomainSuspended {
        cursor: u64,
        child_delegation_id: String,
        route_epoch: String,
        reason: UnknownReason,
        detail: String,
    },
    ParentTodoBinding {
        cursor: u64,
        child_delegation_id: String,
        route_epoch: String,
        parent_terminal_id: String,
        parent_process_generation: u64,
        parent_todo: ParentTodo,
    },
    Frozen {
        cursor: u64,
        route: RouteIdentity,
        local_root: String,
        local_revision: u64,
    },
    ClosureBarrier {
        cursor: u64,
        route: RouteIdentity,
        todo: DoneEvidence,
        through_mailbox_cursor: u64,
        outcome: ClosureOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<UnknownReason>,
        counts: ClosureCounts,
        qualification: crate::child_report::CoverageQualification,
    },
    Signal {
        cursor: u64,
        signal: ChildReportSignal,
    },
    RecoveryRequested {
        cursor: u64,
        signal_cursor: u64,
        route: RouteIdentity,
        /// Mailbox cursor at request time; bound egress after it is the
        /// single permitted recovery delivery.
        mailbox_cursor: u64,
    },
    RecoveryDeclined {
        cursor: u64,
        signal_cursor: u64,
        recovery_cursor: u64,
        route: RouteIdentity,
        reason: DeclineReason,
        /// True when the server settled an unissuable or unverifiable wake.
        #[serde(default)]
        server_settled: bool,
    },
    /// #161: exactly one report-only wake per signal. The head's stable ID is
    /// derived from the signal cursor, so it cannot be delivered twice.
    RecoveryWakeIssued {
        cursor: u64,
        signal_cursor: u64,
        recovery_cursor: u64,
        route: RouteIdentity,
        stable_id: String,
    },
    /// Written only after the preceding records `first..=last` were fsynced
    /// and read back exactly. Authority-bearing reads (domains, signals,
    /// recovery requests, parent bindings) consider committed records only;
    /// restrictive reads (freeze, suspension, decline, coverage) consider all.
    Commit {
        cursor: u64,
        first: u64,
        last: u64,
    },
}

impl ClosureRecord {
    pub fn cursor(&self) -> u64 {
        match self {
            Self::CoveredDomain { cursor, .. }
            | Self::DomainSuspended { cursor, .. }
            | Self::ParentTodoBinding { cursor, .. }
            | Self::Frozen { cursor, .. }
            | Self::ClosureBarrier { cursor, .. }
            | Self::Signal { cursor, .. }
            | Self::RecoveryRequested { cursor, .. }
            | Self::RecoveryDeclined { cursor, .. }
            | Self::RecoveryWakeIssued { cursor, .. }
            | Self::Commit { cursor, .. } => *cursor,
        }
    }

    fn valid(&self) -> bool {
        match self {
            Self::Signal { cursor, signal } => {
                signal.signal_cursor == *cursor
                    && signal.todo.todo_state_cursor < signal.closure.closure_cursor
                    && signal.closure.closure_cursor < *cursor
                    && signal.parent_todo.as_ref().is_none_or(ParentTodo::valid)
                    && match signal.signal_type {
                        SignalType::MissingAfterDone => {
                            signal.reason.is_none()
                                && signal.closure.coverage_qualified
                                && signal.closure.counts() == ClosureCounts::default()
                        }
                        SignalType::ReportUnknown => {
                            signal.reason.is_some() && !signal.recovery.available
                        }
                    }
            }
            Self::ClosureBarrier {
                outcome,
                reason,
                counts,
                qualification,
                ..
            } => match outcome {
                ClosureOutcome::MissingAfterDone => {
                    reason.is_none()
                        && *counts == ClosureCounts::default()
                        && *qualification
                            == crate::child_report::CoverageQualification::AllPathsTrusted
                }
                ClosureOutcome::Admitted => {
                    reason.is_none()
                        && counts.admitted_report_count > 0
                        && *qualification
                            == crate::child_report::CoverageQualification::ObservedOnly
                }
                ClosureOutcome::ReportUnknown => {
                    reason.is_some()
                        && *qualification
                            == crate::child_report::CoverageQualification::ObservedOnly
                }
            },
            _ => true,
        }
    }
}

/// Recovered closure journal. Cursors are strictly increasing, not dense.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClosureJournal {
    pub records: Vec<ClosureRecord>,
    committed: Vec<bool>,
    /// Mailbox cursor observed by the current transaction; new closure
    /// cursors are minted above it.
    floor: u64,
}

impl ClosureJournal {
    pub fn last_cursor(&self) -> u64 {
        self.records.last().map_or(0, ClosureRecord::cursor)
    }

    pub fn next_cursor(&self) -> u64 {
        self.last_cursor().max(self.floor) + 1
    }

    fn push(&mut self, record: ClosureRecord) -> Result<(), MailboxError> {
        if record.cursor() <= self.last_cursor() || !record.valid() {
            return Err(MailboxError::CorruptRecord);
        }
        if let ClosureRecord::Commit { first, last, .. } = &record {
            // A commit covers exactly an uncommitted run ending at the tail.
            if *first == 0 || first > last || *last != self.last_cursor() {
                return Err(MailboxError::CorruptRecord);
            }
            let mut covered = false;
            for (existing, committed) in self.records.iter().zip(self.committed.iter_mut()) {
                if (*first..=*last).contains(&existing.cursor()) {
                    if *committed || matches!(existing, ClosureRecord::Commit { .. }) {
                        return Err(MailboxError::CorruptRecord);
                    }
                    *committed = true;
                    covered = true;
                }
            }
            if !covered {
                return Err(MailboxError::CorruptRecord);
            }
        }
        self.records.push(record);
        self.committed.push(false);
        Ok(())
    }

    /// Records whose fsync and readback were confirmed by a Commit record.
    pub fn committed(&self) -> impl DoubleEndedIterator<Item = &ClosureRecord> {
        self.records
            .iter()
            .zip(self.committed.iter())
            .filter_map(|(record, committed)| committed.then_some(record))
    }

    /// Latest domain for a delegated child, on any epoch.
    pub fn latest_domain_for_delegation(
        &self,
        child_delegation_id: &str,
    ) -> Option<&CoveredDomain> {
        self.committed().rev().find_map(|record| match record {
            ClosureRecord::CoveredDomain { domain, .. }
                if domain.route.child_delegation_id == child_delegation_id =>
            {
                Some(domain)
            }
            _ => None,
        })
    }

    /// Any domain naming this child terminal at this process generation.
    pub fn covers_child_terminal(&self, child_terminal: &str, generation: u64) -> bool {
        self.records.iter().any(|record| {
            matches!(record, ClosureRecord::CoveredDomain { domain, .. }
                if domain.route.child_terminal_id == child_terminal
                    && domain.route.child_process_generation == generation)
        })
    }

    pub fn suspension(
        &self,
        child_delegation_id: &str,
        route_epoch: &str,
    ) -> Option<UnknownReason> {
        self.records.iter().find_map(|record| match record {
            ClosureRecord::DomainSuspended {
                child_delegation_id: child,
                route_epoch: epoch,
                reason,
                ..
            } if child == child_delegation_id && epoch == route_epoch => Some(*reason),
            _ => None,
        })
    }

    pub fn parent_todo(&self, child_delegation_id: &str, route_epoch: &str) -> Option<&ParentTodo> {
        self.committed().find_map(|record| match record {
            ClosureRecord::ParentTodoBinding {
                child_delegation_id: child,
                route_epoch: epoch,
                parent_todo,
                ..
            } if child == child_delegation_id && epoch == route_epoch => Some(parent_todo),
            _ => None,
        })
    }

    pub fn parent_todo_binding_cursor(
        &self,
        child_delegation_id: &str,
        route_epoch: &str,
    ) -> Option<(u64, &ParentTodo)> {
        self.committed().find_map(|record| match record {
            ClosureRecord::ParentTodoBinding {
                cursor,
                child_delegation_id: child,
                route_epoch: epoch,
                parent_todo,
                ..
            } if child == child_delegation_id && epoch == route_epoch => {
                Some((*cursor, parent_todo))
            }
            _ => None,
        })
    }

    pub fn frozen(&self, route: &RouteIdentity, local_root: &str, local_revision: u64) -> bool {
        self.records.iter().any(|record| {
            matches!(record, ClosureRecord::Frozen { route: frozen, local_root: root, local_revision: revision, .. }
                if frozen == route && root == local_root && *revision == local_revision)
        })
    }

    pub fn barrier(
        &self,
        route: &RouteIdentity,
        local_root: &str,
        local_revision: u64,
    ) -> Option<&ClosureRecord> {
        self.records.iter().find(|record| {
            matches!(record, ClosureRecord::ClosureBarrier { route: barrier, todo, .. }
                if barrier == route && todo.local_root == local_root && todo.local_revision == local_revision)
        })
    }

    pub fn signal(&self, signal_cursor: u64) -> Option<&ChildReportSignal> {
        self.committed().find_map(|record| match record {
            ClosureRecord::Signal { cursor, signal } if *cursor == signal_cursor => Some(signal),
            _ => None,
        })
    }

    pub fn signal_for_closure(&self, closure_cursor: u64) -> Option<&ChildReportSignal> {
        self.committed().find_map(|record| match record {
            ClosureRecord::Signal { signal, .. }
                if signal.closure.closure_cursor == closure_cursor =>
            {
                Some(signal)
            }
            _ => None,
        })
    }

    /// Latest signal for a delegated child on any route.
    pub fn latest_signal_for_child(&self, child_delegation_id: &str) -> Option<&ChildReportSignal> {
        self.committed().rev().find_map(|record| match record {
            ClosureRecord::Signal { signal, .. }
                if signal.route.child_delegation_id == child_delegation_id =>
            {
                Some(signal)
            }
            _ => None,
        })
    }

    pub fn recovery_request(&self, signal_cursor: u64) -> Option<(u64, u64)> {
        self.committed().find_map(|record| match record {
            ClosureRecord::RecoveryRequested {
                cursor,
                signal_cursor: signal,
                mailbox_cursor,
                ..
            } if *signal == signal_cursor => Some((*cursor, *mailbox_cursor)),
            _ => None,
        })
    }

    pub fn wake(&self, signal_cursor: u64) -> Option<(u64, &str)> {
        self.records.iter().find_map(|record| match record {
            ClosureRecord::RecoveryWakeIssued {
                cursor,
                signal_cursor: signal,
                stable_id,
                ..
            } if *signal == signal_cursor => Some((*cursor, stable_id.as_str())),
            _ => None,
        })
    }

    pub fn decline(&self, signal_cursor: u64) -> Option<(u64, DeclineReason)> {
        self.records.iter().find_map(|record| match record {
            ClosureRecord::RecoveryDeclined {
                cursor,
                signal_cursor: signal,
                reason,
                ..
            } if *signal == signal_cursor => Some((*cursor, *reason)),
            _ => None,
        })
    }

    pub fn signals_for_parent(
        &self,
        parent_terminal: &str,
        parent_generation: u64,
        after_cursor: u64,
    ) -> Vec<ChildReportSignal> {
        self.committed()
            .filter_map(|record| match record {
                ClosureRecord::Signal { cursor, signal }
                    if *cursor > after_cursor
                        && signal.route.parent_terminal_id == parent_terminal
                        && signal.route.parent_process_generation == parent_generation =>
                {
                    Some(signal.clone())
                }
                _ => None,
            })
            .take(MAX_PAGE)
            .collect()
    }

    /// Recovery requests addressed to this exact child execution and epoch.
    pub fn recovery_requests_for_child(
        &self,
        child_terminal: &str,
        child_generation: u64,
        route_epoch: &str,
        after_cursor: u64,
    ) -> Vec<(u64, &ChildReportSignal)> {
        self.committed()
            .filter_map(|record| match record {
                ClosureRecord::RecoveryRequested {
                    cursor,
                    signal_cursor,
                    route,
                    ..
                } if *cursor > after_cursor
                    && route.child_terminal_id == child_terminal
                    && route.child_process_generation == child_generation
                    && route.route_epoch == route_epoch =>
                {
                    self.signal(*signal_cursor).map(|signal| (*cursor, signal))
                }
                _ => None,
            })
            .take(MAX_RECOVERY_PAGE)
            .collect()
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub fn load_closure(store: &MailboxStore) -> Result<ClosureJournal, MailboxError> {
    let stream = match File::open(store.closure_path()) {
        Ok(stream) => stream,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ClosureJournal::default())
        }
        Err(error) => return Err(error.into()),
    };
    let mut journal = ClosureJournal::default();
    for line in BufReader::new(stream).lines() {
        let record: ClosureRecord =
            serde_json::from_str(&line?).map_err(|_| MailboxError::CorruptRecord)?;
        journal.push(record)?;
    }
    Ok(journal)
}

#[cfg(test)]
thread_local! {
    static FAIL_CLOSURE_READBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn test_fail_closure_readback(fail: bool) {
    FAIL_CLOSURE_READBACK.with(|flag| flag.set(fail));
}

/// Run `plan` under the mailbox's exclusive lock over a consistent mailbox and
/// closure snapshot, append the returned records with fsync, then read the
/// closure journal back. Any failure after the first append is an error and
/// the caller must treat the outcome as unknown.
pub fn closure_transaction<T>(
    store: &MailboxStore,
    plan: impl FnOnce(
        &RecoveredMailbox,
        &ClosureJournal,
    ) -> Result<(Vec<ClosureRecord>, T), MailboxError>,
) -> Result<T, MailboxError> {
    store.with_exclusive_lock(|| {
        let mailbox = store.load()?;
        let mut journal = load_closure(store)?;
        journal.floor = mailbox.record_cursor;
        let (records, result) = plan(&mailbox, &journal)?;
        let mut previous = journal.next_cursor() - 1;
        for record in &records {
            if record.cursor() <= previous || !record.valid() {
                return Err(MailboxError::InvalidRecord);
            }
            previous = record.cursor();
        }
        if records.is_empty() {
            return Ok(result);
        }
        let path = store.closure_path();
        let newly_created = !path.exists();
        let mut stream = OpenOptions::new().create(true).append(true).open(&path)?;
        for record in &records {
            serde_json::to_writer(&mut stream, record)
                .map_err(|error| MailboxError::Io(error.to_string()))?;
            stream.write_all(b"\n")?;
            stream.sync_all()?;
        }
        if newly_created {
            File::open(path.parent().ok_or(MailboxError::InvalidRecord)?)?.sync_all()?;
        }
        #[cfg(test)]
        if FAIL_CLOSURE_READBACK.with(|flag| flag.get()) {
            return Err(MailboxError::Io("injected closure readback failure".into()));
        }
        let readback = load_closure(store)?;
        if readback.records.len() != journal.records.len() + records.len()
            || readback.records[journal.records.len()..] != records[..]
        {
            return Err(MailboxError::CorruptRecord);
        }
        let commit = ClosureRecord::Commit {
            cursor: previous + 1,
            first: records[0].cursor(),
            last: previous,
        };
        serde_json::to_writer(&mut stream, &commit)
            .map_err(|error| MailboxError::Io(error.to_string()))?;
        stream.write_all(b"\n")?;
        stream.sync_all()?;
        Ok(result)
    })
}

/// Everything the App checked about the domain's continued validity at the
/// decision point. `Err` carries the reason the domain no longer covers.
pub struct ClosureInputs<'a> {
    pub domain: Option<&'a CoveredDomain>,
    pub suspension: Option<UnknownReason>,
    pub currency: Result<(), UnknownReason>,
    pub current_route: &'a RouteIdentity,
    pub mailbox: &'a RecoveredMailbox,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureDecision {
    pub outcome: ClosureOutcome,
    pub reason: Option<UnknownReason>,
    pub counts: ClosureCounts,
}

impl ClosureDecision {
    fn unknown(reason: UnknownReason, counts: ClosureCounts) -> Self {
        Self {
            outcome: ClosureOutcome::ReportUnknown,
            reason: Some(reason),
            counts,
        }
    }
}

/// Pure closure classification over a mailbox snapshot taken under the lock.
/// Anything not proven is `report_unknown`; `missing_after_done` requires a
/// qualified, current, unsuspended domain on this exact route and zero report
/// activity of any kind.
pub fn classify_closure(inputs: &ClosureInputs<'_>) -> ClosureDecision {
    let current = inputs.current_route;
    let mailbox = inputs.mailbox;
    let delegation = &current.child_delegation_id;
    let on_route: Vec<_> = mailbox
        .child_report_events
        .iter()
        .filter(|event| event.route() == current)
        .collect();
    let prepared: Vec<_> = on_route
        .iter()
        .filter_map(|event| match event {
            ChildReportEvent::Prepared { preparation } => Some(preparation),
            _ => None,
        })
        .collect();
    let attempted: Vec<_> = on_route
        .iter()
        .filter_map(|event| match event {
            ChildReportEvent::PreparedAttempt { preparation } => Some(preparation),
            _ => None,
        })
        .collect();
    let admitted = attempted
        .iter()
        .filter(|preparation| crate::child_report::exact_admitted(preparation, mailbox))
        .count() as u64;
    let counts = ClosureCounts {
        path_attempt_count: on_route
            .iter()
            .filter(|event| matches!(event, ChildReportEvent::PathAttempt { .. }))
            .count() as u64,
        prepared_count: prepared.len() as u64,
        admitted_report_count: admitted,
    };
    let Some(domain) = inputs.domain else {
        return ClosureDecision::unknown(UnknownReason::CoverageUnqualified, counts);
    };
    if &domain.route != current {
        let reason = if domain.route.route_epoch != current.route_epoch {
            UnknownReason::StaleEpoch
        } else {
            UnknownReason::RouteReplaced
        };
        return ClosureDecision::unknown(reason, counts);
    }
    if let Some(reason) = inputs.suspension {
        return ClosureDecision::unknown(reason, counts);
    }
    if let Err(reason) = inputs.currency {
        return ClosureDecision::unknown(reason, counts);
    }
    if mailbox
        .child_report_events
        .iter()
        .any(|event| &event.route().child_delegation_id == delegation && event.route() != current)
    {
        return ClosureDecision::unknown(UnknownReason::RouteReplaced, counts);
    }
    if admitted > 0 {
        return ClosureDecision {
            outcome: ClosureOutcome::Admitted,
            reason: None,
            counts,
        };
    }
    if !domain.enforcement_verified || domain.unqualified_reason.is_some() {
        return ClosureDecision::unknown(UnknownReason::CoverageUnqualified, counts);
    }
    let tainted_event = on_route.iter().any(|event| {
        matches!(
            event,
            ChildReportEvent::Attempt { .. }
                | ChildReportEvent::Coverage { .. }
                | ChildReportEvent::Bypass { .. }
        )
    });
    let child_to_parent_head = mailbox.heads.values().any(|head| {
        head.sender == current.child_terminal_id
            && (head.target == current.parent_terminal_id
                || head.recipient.recipient_id == current.parent_terminal_id)
    });
    let orphan_receipt = mailbox
        .receipts
        .values()
        .any(|receipt| !mailbox.heads.contains_key(&receipt.stable_id));
    if tainted_event || orphan_receipt {
        return ClosureDecision::unknown(UnknownReason::CoverageUnqualified, counts);
    }
    if !attempted.is_empty() {
        return ClosureDecision::unknown(UnknownReason::InFlight, counts);
    }
    if !prepared.is_empty() {
        return ClosureDecision::unknown(UnknownReason::PreparedNotAdmitted, counts);
    }
    if counts.path_attempt_count > 0 {
        return ClosureDecision::unknown(UnknownReason::PathAttemptUncertain, counts);
    }
    // Any child→parent head not explained above (for example a generic or
    // provisioned send) means the interval saw report activity.
    if child_to_parent_head {
        return ClosureDecision::unknown(UnknownReason::CoverageUnqualified, counts);
    }
    ClosureDecision {
        outcome: ClosureOutcome::MissingAfterDone,
        reason: None,
        counts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covered_child_environment_keeps_exactly_the_allowlist() {
        let kept = covered_child_herdr_environment([
            (
                "HERDR_MAILBOX_BOOTSTRAP_ADDRESS".into(),
                "/run/x.sock".into(),
            ),
            ("HERDR_AGENT".into(), "pi".into()),
            ("HERDR_PANE_ID".into(), "w1:p1".into()),
            ("HERDR_CLIENT_SOCKET_PATH".into(), "/run/h.sock".into()),
            ("HERDR_ENV".into(), "1".into()),
            ("HERDR_AGENT_EXTRA".into(), "x".into()),
            ("PATH".into(), "/usr/bin".into()),
        ]);
        assert_eq!(
            kept,
            vec![
                ("HERDR_AGENT".to_string(), "pi".to_string()),
                (
                    "HERDR_MAILBOX_BOOTSTRAP_ADDRESS".to_string(),
                    "/run/x.sock".to_string()
                ),
            ]
        );
        let ambiguous = covered_child_herdr_environment([
            ("HERDR_AGENT".into(), "pi".into()),
            ("HERDR_AGENT".into(), "claude".into()),
        ]);
        assert!(ambiguous.is_empty());
    }

    fn route() -> RouteIdentity {
        use crate::agent_resume::AgentSessionRefKind;
        let session = |value: &str| crate::api::schema::AgentSessionInfo {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: AgentSessionRefKind::Path,
            value: value.into(),
        };
        RouteIdentity {
            child_delegation_id: "d2".into(),
            parent_delegation_id: "d1".into(),
            child_pane_id: "w:p2".into(),
            parent_pane_id: "w:p1".into(),
            child_terminal_id: "child-terminal".into(),
            parent_terminal_id: "parent-terminal".into(),
            child_process_generation: 2,
            parent_process_generation: 3,
            child_session: session("/child.jsonl"),
            parent_session: session("/parent.jsonl"),
            child_route_revision: 2,
            parent_route_revision: 1,
            route_epoch: "epoch-a".into(),
        }
    }

    fn domain(route: RouteIdentity, verified: bool) -> CoveredDomain {
        let birth = BirthRecord {
            pid: 10,
            start_ticks: 500,
        };
        CoveredDomain {
            route,
            child_birth: birth,
            launch_floor_ticks: 400,
            writer: Some(WriterIdentity { dev: 1, ino: 2 }),
            policy: policy(birth),
            enforcement_verified: verified,
            unqualified_reason: (!verified).then(|| "enforcement_unproven".into()),
            baseline_mailbox_cursor: 0,
            server_boot: "boot".into(),
        }
    }

    fn done_mailbox(route: &RouteIdentity) -> RecoveredMailbox {
        RecoveredMailbox {
            child_report_events: vec![ChildReportEvent::TodoState {
                route: route.clone(),
                local_root: "root".into(),
                local_revision: 1,
                state_digest: "a".repeat(64),
                state: LocalTodoState::Done,
            }],
            child_report_event_cursors: vec![1],
            record_cursor: 1,
            ..Default::default()
        }
    }

    fn decide(
        domain: Option<&CoveredDomain>,
        suspension: Option<UnknownReason>,
        currency: Result<(), UnknownReason>,
        current: &RouteIdentity,
        mailbox: &RecoveredMailbox,
    ) -> ClosureDecision {
        classify_closure(&ClosureInputs {
            domain,
            suspension,
            currency,
            current_route: current,
            mailbox,
        })
    }

    #[test]
    fn only_a_current_verified_quiet_domain_is_missing_after_done() {
        let current = route();
        let mailbox = done_mailbox(&current);
        let qualified = domain(current.clone(), true);
        let unknown = |reason| ClosureDecision {
            outcome: ClosureOutcome::ReportUnknown,
            reason: Some(reason),
            counts: ClosureCounts::default(),
        };
        assert_eq!(
            decide(Some(&qualified), None, Ok(()), &current, &mailbox).outcome,
            ClosureOutcome::MissingAfterDone
        );
        assert_eq!(
            decide(None, None, Ok(()), &current, &mailbox),
            unknown(UnknownReason::CoverageUnqualified)
        );
        assert_eq!(
            decide(
                Some(&domain(current.clone(), false)),
                None,
                Ok(()),
                &current,
                &mailbox
            ),
            unknown(UnknownReason::CoverageUnqualified)
        );
        let mut new_epoch = current.clone();
        new_epoch.route_epoch = "epoch-b".into();
        assert_eq!(
            decide(
                Some(&qualified),
                None,
                Ok(()),
                &new_epoch,
                &done_mailbox(&new_epoch)
            ),
            unknown(UnknownReason::StaleEpoch)
        );
        let mut reparented = current.clone();
        reparented.parent_route_revision += 1;
        assert_eq!(
            decide(
                Some(&qualified),
                None,
                Ok(()),
                &reparented,
                &done_mailbox(&reparented)
            ),
            unknown(UnknownReason::RouteReplaced)
        );
        assert_eq!(
            decide(
                Some(&qualified),
                Some(UnknownReason::ChildProcessGone),
                Ok(()),
                &current,
                &mailbox
            ),
            unknown(UnknownReason::ChildProcessGone)
        );
        assert_eq!(
            decide(
                Some(&qualified),
                None,
                Err(UnknownReason::CoverageUnqualified),
                &current,
                &mailbox
            ),
            unknown(UnknownReason::CoverageUnqualified)
        );
        // A prior-route event for the same delegation is outside the interval.
        let mut mixed = mailbox.clone();
        mixed.child_report_events.insert(
            0,
            ChildReportEvent::Bypass {
                route: new_epoch.clone(),
                path: crate::child_report::ReportBypassPath::HandoffPty,
                message_id: "m".into(),
            },
        );
        assert_eq!(
            decide(Some(&qualified), None, Ok(()), &current, &mixed),
            unknown(UnknownReason::RouteReplaced)
        );
        // Any child-to-parent head not joined to a prepared attempt taints.
        let mut headed = mailbox.clone();
        headed.heads.insert(
            "generic".into(),
            crate::mailbox::MailboxHead {
                stable_id: "generic".into(),
                revision: 1,
                digest: "a".repeat(64),
                delivery_digest: "b".repeat(64),
                recipient: crate::mailbox::RecipientKey {
                    recipient_id: current.parent_terminal_id.clone(),
                    generation: "1".into(),
                },
                subject: "s".into(),
                body: "b".into(),
                recipient_generation: "1".into(),
                sender: current.child_terminal_id.clone(),
                target: current.parent_terminal_id.clone(),
                grant_id: "mailbox:generic".into(),
                message_id: "m".into(),
                kind: "info".into(),
                priority: "normal".into(),
                original_sequence: 1,
                enqueue_epoch: 1,
                accepted_at: 1,
            },
        );
        assert_eq!(
            decide(Some(&qualified), None, Ok(()), &current, &headed),
            unknown(UnknownReason::CoverageUnqualified)
        );
    }

    #[test]
    fn journal_serves_only_committed_authority_and_rejects_invalid_records() {
        let current = route();
        let signal = |cursor, signal_type, reason, counts: ClosureCounts| ClosureRecord::Signal {
            cursor,
            signal: ChildReportSignal {
                signal_cursor: cursor,
                signal_type,
                reason,
                route: current.clone(),
                todo: DoneEvidence {
                    local_root: "root".into(),
                    local_revision: 1,
                    state_digest: "a".repeat(64),
                    state: LocalTodoState::Done,
                    todo_state_cursor: 40,
                },
                closure: SignalClosure::new(
                    signal_type == SignalType::MissingAfterDone,
                    45,
                    counts,
                ),
                parent_todo: None,
                recovery: SignalRecovery {
                    available: signal_type == SignalType::MissingAfterDone,
                },
            },
        };
        let busy = ClosureCounts {
            path_attempt_count: 1,
            ..ClosureCounts::default()
        };
        assert!(!signal(50, SignalType::MissingAfterDone, None, busy).valid());
        assert!(!signal(
            50,
            SignalType::MissingAfterDone,
            Some(UnknownReason::InFlight),
            ClosureCounts::default()
        )
        .valid());
        assert!(!signal(50, SignalType::ReportUnknown, None, busy).valid());
        // Pi's ordering check: todoStateCursor < closureCursor < cursor.
        assert!(!signal(
            45,
            SignalType::MissingAfterDone,
            None,
            ClosureCounts::default()
        )
        .valid());
        let wire = serde_json::to_value(
            match signal(
                50,
                SignalType::MissingAfterDone,
                None,
                ClosureCounts::default(),
            ) {
                ClosureRecord::Signal { signal, .. } => signal,
                _ => unreachable!(),
            },
        )
        .unwrap();
        assert_eq!(wire["cursor"], 50);
        assert_eq!(
            wire["closure"],
            serde_json::json!({"coverageQualified":true,"closureCursor":45,
                "pathAttemptCount":0,"preparedCount":0,"admittedReportCount":0})
        );
        let mut journal = ClosureJournal::default();
        journal
            .push(signal(
                50,
                SignalType::MissingAfterDone,
                None,
                ClosureCounts::default(),
            ))
            .unwrap();
        assert!(
            journal.signal(50).is_none(),
            "uncommitted signal is not served"
        );
        assert!(journal
            .signals_for_parent("parent-terminal", 3, 0)
            .is_empty());
        journal
            .push(ClosureRecord::Commit {
                cursor: 51,
                first: 50,
                last: 50,
            })
            .unwrap();
        assert!(journal.signal(50).is_some());
        assert_eq!(journal.signals_for_parent("parent-terminal", 3, 0).len(), 1);
        assert!(journal
            .signals_for_parent("parent-terminal", 4, 0)
            .is_empty());
        // Cursors must increase; a commit must cover the uncommitted tail.
        assert!(journal
            .push(ClosureRecord::Commit {
                cursor: 51,
                first: 50,
                last: 51,
            })
            .is_err());
        assert!(journal
            .push(ClosureRecord::Commit {
                cursor: 60,
                first: 50,
                last: 51,
            })
            .is_err());
        journal.floor = 99;
        assert_eq!(
            journal.next_cursor(),
            100,
            "minted above the mailbox cursor"
        );
    }

    #[test]
    fn parent_todo_echo_must_be_decodable_by_pi() {
        let valid = ParentTodo {
            delegation_id: "AbCdEfGhIjKlMnOpQrSt_-".into(),
            parent_task_id: 7,
        };
        assert!(valid.valid());
        for invalid in [
            ParentTodo {
                delegation_id: "short".into(),
                ..valid.clone()
            },
            ParentTodo {
                delegation_id: "AbCdEfGhIjKlMnOpQrSt_!".into(),
                ..valid.clone()
            },
            ParentTodo {
                parent_task_id: 0,
                ..valid.clone()
            },
            ParentTodo {
                parent_task_id: 1 << 53,
                ..valid.clone()
            },
        ] {
            assert!(!invalid.valid(), "{invalid:?}");
        }
    }

    fn policy(birth: BirthRecord) -> CoveredLaunchPolicy {
        CoveredLaunchPolicy {
            policy_id: "sandbox-145".into(),
            policy_hash: "a".repeat(64),
            receipt_digest: "b".repeat(64),
            sandboxed_birth: birth,
        }
    }

    #[test]
    fn enforcement_must_cover_from_the_managed_launch_birth() {
        let birth = BirthRecord {
            pid: 10,
            start_ticks: 500,
        };
        let registered = policy(birth);
        let query = EnforcementQuery {
            policy: &registered,
            managed_launch_birth: birth,
            launch_floor_ticks: 400,
        };
        let exact = EnforcementAttestation {
            policy_hash: "a".repeat(64),
            covered_from_birth: birth,
        };
        assert!(enforcement_covers_launch(&query, &exact));
        // Coverage that began at a later exec/route-ready point leaves a gap.
        let late = EnforcementAttestation {
            covered_from_birth: BirthRecord {
                pid: 10,
                start_ticks: 501,
            },
            ..exact.clone()
        };
        assert!(!enforcement_covers_launch(&query, &late));
        let other_policy = EnforcementAttestation {
            policy_hash: "c".repeat(64),
            ..exact.clone()
        };
        assert!(!enforcement_covers_launch(&query, &other_policy));
        // The receipt must name the managed-launch process.
        let receipt_for_other = policy(BirthRecord {
            pid: 11,
            start_ticks: 500,
        });
        let mismatched = EnforcementQuery {
            policy: &receipt_for_other,
            ..query.clone()
        };
        assert!(!enforcement_covers_launch(&mismatched, &exact));
        // A process born before the launch floor is pre-existing.
        let early = EnforcementQuery {
            launch_floor_ticks: 501,
            ..query.clone()
        };
        assert!(!enforcement_covers_launch(&early, &exact));
        assert_eq!(
            verify_enforcement(&UnprovenEnforcementVerifier, &query),
            Err("enforcement_unproven".into())
        );
    }
}
