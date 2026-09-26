//! #159 server-owned covered-child domain, closure barrier, parent signal and
//! recovery. Everything here is inert unless `[experimental]
//! child_report_signals` is on AND a launch was registered through the
//! covered-launch seam. Uncovered children and gen1 methods are unchanged.

use serde_json::{json, Value};

use crate::app::{App, MailboxBootstrapError, MailboxBootstrapSession};
use crate::child_report::{ChildReportEvent, LocalTodoState, RouteIdentity};
use crate::child_report_closure::{
    self as closure, BirthRecord, ChildReportSignal, ClosureJournal, ClosureOutcome, ClosureRecord,
    CoveredDomain, CoveredLaunchPolicy, DeclineReason, DoneEvidence, EnforcementQuery, ParentTodo,
    SignalClosure, SignalRecovery, SignalType, UnknownReason, WriterIdentity,
};
use crate::mailbox::{MailboxStore, RecoveredMailbox};

/// Kinds of bound report egress a frozen covered child may attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoveredEgress {
    PathAttempt,
    Prepared,
    Submit,
}

fn store_error(_: crate::mailbox::MailboxError) -> MailboxBootstrapError {
    MailboxBootstrapError::GrantMissing
}

/// Latest TodoState for a delegated child on any route, with its mailbox cursor.
fn latest_todo<'a>(
    mailbox: &'a RecoveredMailbox,
    child_delegation_id: &str,
) -> Option<(u64, &'a ChildReportEvent)> {
    mailbox
        .child_report_events
        .iter()
        .zip(mailbox.child_report_event_cursors.iter())
        .rev()
        .find(|(event, _)| {
            matches!(event, ChildReportEvent::TodoState { route, .. }
                if route.child_delegation_id == child_delegation_id)
        })
        .map(|(event, cursor)| (*cursor, event))
}

/// The done evidence of the latest TodoState, if it is `done` on `route`.
fn latest_done_on_route(mailbox: &RecoveredMailbox, route: &RouteIdentity) -> Option<DoneEvidence> {
    match latest_todo(mailbox, &route.child_delegation_id)? {
        (
            cursor,
            ChildReportEvent::TodoState {
                route: todo_route,
                local_root,
                local_revision,
                state_digest,
                state: LocalTodoState::Done,
            },
        ) if todo_route == route => Some(DoneEvidence {
            local_root: local_root.clone(),
            local_revision: *local_revision,
            state_digest: state_digest.clone(),
            state: LocalTodoState::Done,
            todo_state_cursor: cursor,
        }),
        _ => None,
    }
}

impl App {
    fn closure_store(&self) -> Result<MailboxStore, MailboxBootstrapError> {
        MailboxStore::open(&self.sender_authority_dir).map_err(store_error)
    }

    fn closure_journal(&self) -> Result<ClosureJournal, MailboxBootstrapError> {
        closure::load_closure(&MailboxStore::existing(&self.sender_authority_dir))
            .map_err(store_error)
    }

    /// The covered-launch seam (#145). Registration must happen before the
    /// child's route becomes ready; a later registration cannot cover the
    /// interval already elapsed and is refused once a route is ready.
    #[cfg_attr(not(test), allow(dead_code))] // Called by the #145 launch seam.
    pub(crate) fn register_covered_child_launch(
        &mut self,
        terminal: crate::terminal::TerminalId,
        policy: CoveredLaunchPolicy,
    ) -> bool {
        if !self.child_report_signals_enabled
            || !policy.valid()
            || self
                .ready_delegation_routes
                .values()
                .any(|ready| ready.child_terminal == terminal)
        {
            return false;
        }
        self.covered_child_launches.insert(terminal, policy);
        true
    }

    fn writer_identity(&self) -> Option<WriterIdentity> {
        let (dev, ino) = self.session_writer.as_ref()?.lock_identity()?;
        Some(WriterIdentity { dev, ino })
    }

    /// Birth of the Pi process currently in the child terminal's foreground.
    fn current_pi_birth(&self, terminal: &crate::terminal::TerminalId) -> Option<BirthRecord> {
        let job = self.mailbox_bootstrap_foreground_job(terminal)?;
        let (agent, process) = crate::detect::identify_agent_process_in_job(&job)?;
        if agent != crate::detect::Agent::Pi {
            return None;
        }
        self.managed_pi_process_birth(process.pid)
            .map(BirthRecord::from)
    }

    /// The exact current route for a delegated child, independent of caller.
    fn current_child_route(&self, child_delegation_id: &str) -> Option<RouteIdentity> {
        let child: crate::delegation::DelegationId = child_delegation_id.parse().ok()?;
        let ready = self.ready_delegation_routes.get(&child)?;
        let mut shape = self.ready_route_shape(ready.child, ready.parent)?;
        shape.epoch = ready.epoch.clone();
        if &shape != ready {
            return None;
        }
        self.ready_report_identity(ready)
    }

    /// Everything that must still hold for a domain to cover its interval.
    fn domain_currency(&self, domain: &CoveredDomain) -> Result<(), (UnknownReason, String)> {
        if self.mailbox_bootstrap_boot_nonce.as_deref() != Some(domain.server_boot.as_str()) {
            return Err((UnknownReason::CoverageUnqualified, "server_restart".into()));
        }
        match self.current_child_route(&domain.route.child_delegation_id) {
            None => return Err((UnknownReason::RouteReplaced, "route_not_ready".into())),
            Some(current) if current.route_epoch != domain.route.route_epoch => {
                return Err((UnknownReason::StaleEpoch, "epoch_changed".into()))
            }
            Some(current) if current != domain.route => {
                return Err((UnknownReason::RouteReplaced, "route_changed".into()))
            }
            Some(_) => {}
        }
        let terminal = self
            .state
            .terminals
            .keys()
            .find(|id| id.to_string() == domain.route.child_terminal_id)
            .cloned();
        if terminal.and_then(|terminal| self.current_pi_birth(&terminal))
            != Some(domain.child_birth)
        {
            return Err((
                UnknownReason::ChildProcessGone,
                "child_birth_changed".into(),
            ));
        }
        if self.writer_identity() != domain.writer || domain.writer.is_none() {
            return Err((UnknownReason::CoverageUnqualified, "writer_replaced".into()));
        }
        if domain.enforcement_verified {
            let query = EnforcementQuery {
                policy: &domain.policy,
                managed_launch_birth: domain.child_birth,
                launch_floor_ticks: domain.launch_floor_ticks,
            };
            closure::verify_enforcement(self.enforcement_verifier.as_ref(), &query).map_err(
                |detail| {
                    (
                        UnknownReason::CoverageUnqualified,
                        format!("enforcement_lapsed:{detail}"),
                    )
                },
            )?;
        }
        Ok(())
    }

    /// Called by route readiness right after a fresh epoch is installed and
    /// before the accepted stream can offer the parent-report path. Returns
    /// an error only when a registered covered child's domain could not be
    /// made durable; the caller then withdraws readiness.
    pub(crate) fn create_covered_domain_on_route_ready(
        &mut self,
        child: crate::delegation::DelegationId,
    ) -> Result<(), String> {
        if !self.child_report_signals_enabled {
            return Ok(());
        }
        let Some(ready) = self.ready_delegation_routes.get(&child).cloned() else {
            return Ok(());
        };
        let Some(policy) = self
            .covered_child_launches
            .get(&ready.child_terminal)
            .cloned()
        else {
            return Ok(());
        };
        let route = self
            .ready_report_identity(&ready)
            .ok_or("covered route identity unavailable")?;
        let server_boot = self
            .mailbox_bootstrap_boot_nonce
            .clone()
            .ok_or("server boot nonce unavailable")?;
        let launch = self.managed_pi_launches.get(&ready.child_terminal);
        let launch_birth = launch
            .filter(|launch| launch.generation == ready.child_generation)
            .and_then(|launch| launch.process)
            .map(BirthRecord::from);
        let launch_floor_ticks = launch
            .map(|launch| launch.earliest_birth_ticks)
            .unwrap_or(u64::MAX);
        let current_birth = self.current_pi_birth(&ready.child_terminal);
        let writer = self.writer_identity();
        let mut unqualified_reason = None;
        let child_birth = match launch_birth {
            Some(birth) if current_birth == Some(birth) => birth,
            Some(birth) => {
                unqualified_reason = Some("managed_launch_not_foreground".to_string());
                birth
            }
            None => {
                unqualified_reason = Some("managed_launch_birth_unbound".to_string());
                policy.sandboxed_birth
            }
        };
        if writer.is_none() && unqualified_reason.is_none() {
            unqualified_reason = Some("writer_unavailable".into());
        }
        let query = EnforcementQuery {
            policy: &policy,
            managed_launch_birth: child_birth,
            launch_floor_ticks,
        };
        let verification = closure::verify_enforcement(self.enforcement_verifier.as_ref(), &query);
        let enforcement_verified = verification.is_ok() && unqualified_reason.is_none();
        if let Err(detail) = verification {
            unqualified_reason.get_or_insert(detail);
        }
        let store = MailboxStore::open(&self.sender_authority_dir).map_err(|e| e.to_string())?;
        closure::closure_transaction(&store, |mailbox, journal| {
            let mut unqualified_reason = unqualified_reason.clone();
            // Any report activity before the domain existed is outside it.
            let prior = mailbox
                .child_report_events
                .iter()
                .any(|event| event.route().child_delegation_id == route.child_delegation_id)
                || mailbox.heads.values().any(|head| {
                    head.sender == route.child_terminal_id
                        && (head.target == route.parent_terminal_id
                            || head.recipient.recipient_id == route.parent_terminal_id)
                });
            if prior {
                unqualified_reason.get_or_insert("pre_domain_report_activity".into());
            }
            let domain = CoveredDomain {
                route: route.clone(),
                child_birth,
                launch_floor_ticks,
                writer,
                policy: policy.clone(),
                enforcement_verified: enforcement_verified && unqualified_reason.is_none(),
                unqualified_reason,
                baseline_mailbox_cursor: mailbox.record_cursor,
                server_boot: server_boot.clone(),
            };
            Ok((
                vec![ClosureRecord::CoveredDomain {
                    cursor: journal.next_cursor(),
                    domain,
                }],
                (),
            ))
        })
        .map_err(|error| error.to_string())
    }

    /// Recovery wait/decline/wake are advertised to a covered child whose
    /// domain names its current route. Pi registers its recovery loop from the
    /// bootstrap descriptor; the methods themselves act only after a done ACK,
    /// a closure barrier and an exact parent request.
    pub(crate) fn covered_child_recovery_advertised(
        &self,
        route: &crate::app::mailbox::BoundParentReportRoute,
    ) -> bool {
        if !self.child_report_signals_enabled {
            return false;
        }
        let Some(identity) = self.current_child_route(&route.child_delegation.to_string()) else {
            return false;
        };
        self.closure_journal().is_ok_and(|journal| {
            journal
                .latest_domain_for_delegation(&identity.child_delegation_id)
                .is_some_and(|domain| domain.route == identity)
        })
    }

    /// Generic/self/provisioned sends by a covered child are denied outright.
    pub(crate) fn covered_child_generic_denied(&self, caller: &str) -> bool {
        if !self.child_report_signals_enabled {
            return false;
        }
        if self
            .covered_child_launches
            .keys()
            .any(|terminal| terminal.to_string() == caller)
        {
            return true;
        }
        let Some(generation) = self
            .offline_mailbox_authorities
            .get(caller)
            .map(|authority| authority.sender_generation)
        else {
            return false;
        };
        match self.closure_journal() {
            Ok(journal) => journal.covers_child_terminal(caller, generation),
            // An unreadable closure journal cannot prove the caller uncovered.
            Err(_) => std::path::Path::new(
                &MailboxStore::existing(&self.sender_authority_dir).closure_path(),
            )
            .exists(),
        }
    }

    /// Bound report egress for a covered child is frozen once its closure
    /// barrier exists for the latest done revision. Only an open recovery
    /// request permits one further path attempt, preparation and submit.
    pub(crate) fn covered_egress_check(
        &self,
        route: &RouteIdentity,
        kind: CoveredEgress,
    ) -> Result<(), MailboxBootstrapError> {
        if !self.child_report_signals_enabled {
            return Ok(());
        }
        let registered = self
            .covered_child_launches
            .keys()
            .any(|terminal| terminal.to_string() == route.child_terminal_id);
        let journal = match self.closure_journal() {
            Ok(journal) => journal,
            Err(_) if registered => return Err(MailboxBootstrapError::EgressFrozen),
            Err(_) => return Ok(()),
        };
        if journal
            .latest_domain_for_delegation(&route.child_delegation_id)
            .is_none()
        {
            return Ok(());
        }
        let mailbox = MailboxStore::existing(&self.sender_authority_dir)
            .load()
            .map_err(store_error)?;
        let Some(done) = latest_done_on_route(&mailbox, route) else {
            return Ok(());
        };
        if !journal.frozen(route, &done.local_root, done.local_revision) {
            return Ok(());
        }
        let Some(ClosureRecord::ClosureBarrier { cursor, .. }) =
            journal.barrier(route, &done.local_root, done.local_revision)
        else {
            return Err(MailboxBootstrapError::EgressFrozen);
        };
        let Some(signal) = journal.signal_for_closure(*cursor) else {
            return Err(MailboxBootstrapError::EgressFrozen);
        };
        let Some((_, since)) = journal.recovery_request(signal.signal_cursor) else {
            return Err(MailboxBootstrapError::EgressFrozen);
        };
        if journal.decline(signal.signal_cursor).is_some() {
            return Err(MailboxBootstrapError::EgressFrozen);
        }
        let already = mailbox
            .child_report_events
            .iter()
            .zip(mailbox.child_report_event_cursors.iter())
            .filter(|(event, cursor)| **cursor > since && event.route() == route)
            .any(|(event, _)| match kind {
                CoveredEgress::PathAttempt => matches!(event, ChildReportEvent::PathAttempt { .. }),
                CoveredEgress::Prepared => matches!(event, ChildReportEvent::Prepared { .. }),
                CoveredEgress::Submit => {
                    matches!(event, ChildReportEvent::PreparedAttempt { .. })
                }
            });
        if already {
            return Err(MailboxBootstrapError::EgressFrozen);
        }
        Ok(())
    }

    /// Run after the child's done TodoState ACK was durably read back on its
    /// current route. Freezes egress, classifies, then fsyncs and reads back a
    /// route/revision-specific barrier and at most one parent signal.
    /// Returns the closure cursor, or `None` for an uncovered child.
    pub(crate) fn run_covered_closure(
        &mut self,
        route: &RouteIdentity,
    ) -> Result<Option<u64>, crate::mailbox::MailboxError> {
        if !self.child_report_signals_enabled {
            return Ok(None);
        }
        let store = MailboxStore::open(&self.sender_authority_dir)?;
        let preview = closure::load_closure(&store)?;
        let Some(domain) = preview
            .latest_domain_for_delegation(&route.child_delegation_id)
            .cloned()
        else {
            return Ok(None);
        };
        let currency = self.domain_currency(&domain);
        closure::closure_transaction(&store, |mailbox, journal| {
            let Some(done) = latest_done_on_route(mailbox, route) else {
                return Err(crate::mailbox::MailboxError::InvalidRecord);
            };
            if let Some(existing) = journal.barrier(route, &done.local_root, done.local_revision) {
                return Ok((vec![], Some(existing.cursor())));
            }
            // The domain seen under the lock must be the one checked above.
            if journal.latest_domain_for_delegation(&route.child_delegation_id) != Some(&domain) {
                return Err(crate::mailbox::MailboxError::InvalidRecord);
            }
            let mut next = journal.next_cursor();
            let mut records = Vec::new();
            let mut suspension =
                journal.suspension(&domain.route.child_delegation_id, &domain.route.route_epoch);
            if let (Err((reason, detail)), None) = (&currency, suspension) {
                records.push(ClosureRecord::DomainSuspended {
                    cursor: next,
                    child_delegation_id: domain.route.child_delegation_id.clone(),
                    route_epoch: domain.route.route_epoch.clone(),
                    reason: *reason,
                    detail: detail.clone(),
                });
                suspension = Some(*reason);
                next += 1;
            }
            records.push(ClosureRecord::Frozen {
                cursor: next,
                route: route.clone(),
                local_root: done.local_root.clone(),
                local_revision: done.local_revision,
            });
            next += 1;
            let decision = closure::classify_closure(&closure::ClosureInputs {
                domain: Some(&domain),
                suspension,
                currency: currency.clone().map_err(|(reason, _)| reason),
                current_route: route,
                mailbox,
            });
            let closure_cursor = next;
            records.push(ClosureRecord::ClosureBarrier {
                cursor: closure_cursor,
                route: route.clone(),
                todo: done.clone(),
                through_mailbox_cursor: mailbox.record_cursor,
                outcome: decision.outcome,
                reason: decision.reason,
                counts: decision.counts,
                qualification: if decision.outcome == ClosureOutcome::MissingAfterDone {
                    crate::child_report::CoverageQualification::AllPathsTrusted
                } else {
                    crate::child_report::CoverageQualification::ObservedOnly
                },
            });
            next += 1;
            if decision.outcome != ClosureOutcome::Admitted {
                let missing = decision.outcome == ClosureOutcome::MissingAfterDone;
                records.push(ClosureRecord::Signal {
                    cursor: next,
                    signal: ChildReportSignal {
                        signal_cursor: next,
                        signal_type: if missing {
                            SignalType::MissingAfterDone
                        } else {
                            SignalType::ReportUnknown
                        },
                        reason: decision.reason,
                        route: route.clone(),
                        todo: done,
                        closure: SignalClosure::new(missing, closure_cursor, decision.counts),
                        parent_todo: journal
                            .parent_todo(&route.child_delegation_id, &route.route_epoch)
                            .cloned(),
                        recovery: SignalRecovery { available: missing },
                    },
                });
            }
            Ok((records, Some(closure_cursor)))
        })
    }

    /// Whether a signal is still the current qualified missing_after_done for
    /// a child whose domain, route and process are unchanged.
    fn signal_recoverable(
        &self,
        journal: &ClosureJournal,
        mailbox: &RecoveredMailbox,
        signal: &ChildReportSignal,
    ) -> bool {
        if signal.signal_type != SignalType::MissingAfterDone
            || journal
                .latest_signal_for_child(&signal.route.child_delegation_id)
                .map(|latest| latest.signal_cursor)
                != Some(signal.signal_cursor)
            || journal.decline(signal.signal_cursor).is_some()
            || journal
                .suspension(&signal.route.child_delegation_id, &signal.route.route_epoch)
                .is_some()
        {
            return false;
        }
        let Some(domain) = journal.latest_domain_for_delegation(&signal.route.child_delegation_id)
        else {
            return false;
        };
        if domain.route != signal.route || self.domain_currency(domain).is_err() {
            return false;
        }
        latest_done_on_route(mailbox, &signal.route).is_some_and(|done| done == signal.todo)
    }

    pub(crate) fn require_child_report_signals(&self) -> Result<(), MailboxBootstrapError> {
        if self.child_report_signals_enabled {
            Ok(())
        } else {
            Err(MailboxBootstrapError::InvalidRequest)
        }
    }

    /// Make a lapse observed at a recovery decision durable and monotonic.
    fn suspend_if_not_current(&self, store: &MailboxStore, child_delegation_id: &str) {
        let Ok(journal) = closure::load_closure(store) else {
            return;
        };
        let Some(domain) = journal
            .latest_domain_for_delegation(child_delegation_id)
            .cloned()
        else {
            return;
        };
        let Err((reason, detail)) = self.domain_currency(&domain) else {
            return;
        };
        let _ = closure::closure_transaction(store, |_, journal| {
            if journal
                .suspension(&domain.route.child_delegation_id, &domain.route.route_epoch)
                .is_some()
            {
                return Ok((vec![], ()));
            }
            Ok((
                vec![ClosureRecord::DomainSuspended {
                    cursor: journal.next_cursor(),
                    child_delegation_id: domain.route.child_delegation_id.clone(),
                    route_epoch: domain.route.route_epoch.clone(),
                    reason,
                    detail: detail.clone(),
                }],
                (),
            ))
        });
    }

    fn require_live_session(
        &self,
        session: &MailboxBootstrapSession,
    ) -> Result<(), MailboxBootstrapError> {
        self.mailbox_bootstrap_session_current(session)?;
        if session.history_only {
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        Ok(())
    }

    /// A decline (by the child, or by the server settling a wake it could not
    /// issue or verify) plus the parent's `report_unknown` for it.
    fn decline_records(
        journal: &ClosureJournal,
        signal: &ChildReportSignal,
        recovery_cursor: u64,
        reason: DeclineReason,
        server_settled: bool,
    ) -> Vec<ClosureRecord> {
        let decline_cursor = journal.next_cursor();
        let mut unknown = signal.clone();
        unknown.signal_cursor = decline_cursor + 1;
        unknown.signal_type = SignalType::ReportUnknown;
        unknown.reason = Some(UnknownReason::declined(reason));
        unknown.recovery = SignalRecovery { available: false };
        vec![
            ClosureRecord::RecoveryDeclined {
                cursor: decline_cursor,
                signal_cursor: signal.signal_cursor,
                recovery_cursor,
                route: signal.route.clone(),
                reason,
                server_settled,
            },
            ClosureRecord::Signal {
                cursor: decline_cursor + 1,
                signal: unknown,
            },
        ]
    }

    /// Settle an open recovery as UNKNOWN when the server cannot issue or
    /// verify its wake. Idempotent; an existing decline wins.
    fn settle_unissued_wake(&self, signal_cursor: u64) {
        let Ok(store) = MailboxStore::open(&self.sender_authority_dir) else {
            return;
        };
        let _ = closure::closure_transaction(&store, |_, journal| {
            let (Some(signal), Some((recovery_cursor, _))) = (
                journal.signal(signal_cursor).cloned(),
                journal.recovery_request(signal_cursor),
            ) else {
                return Ok((vec![], ()));
            };
            if journal.decline(signal_cursor).is_some() {
                return Ok((vec![], ()));
            }
            Ok((
                Self::decline_records(
                    journal,
                    &signal,
                    recovery_cursor,
                    DeclineReason::NoExportedReport,
                    true,
                ),
                (),
            ))
        });
    }

    /// Parent-owned Pi Todo binding for one of its own current child
    /// delegations, resolved from the child pane and exact child session on
    /// the parent's own accepted stream. None, ambiguity and a conflicting
    /// rebind are rejected.
    pub(crate) fn bind_todo_delegation(
        &mut self,
        session: &MailboxBootstrapSession,
        child_pane_id: &str,
        child_session: &crate::api::schema::AgentSessionInfo,
        parent_todo: ParentTodo,
    ) -> Result<u64, MailboxBootstrapError> {
        self.require_child_report_signals()?;
        self.require_live_session(session)?;
        if !parent_todo.valid() || child_pane_id.is_empty() || child_pane_id.len() > 256 {
            return Err(MailboxBootstrapError::InvalidRequest);
        }
        let children: Vec<_> = self
            .ready_delegation_routes
            .values()
            .filter(|ready| {
                ready.parent_terminal.to_string() == session.caller
                    && ready.parent_generation == session.active_execution_generation
            })
            .map(|ready| ready.child)
            .collect();
        let matches: Vec<RouteIdentity> = children
            .into_iter()
            .filter_map(|child| self.parent_current_route_identity(session, child).ok())
            .filter(|identity| {
                identity.child_pane_id == child_pane_id && &identity.child_session == child_session
            })
            .collect();
        let [identity] = matches.as_slice() else {
            return Err(MailboxBootstrapError::InvalidRequest);
        };
        let identity = identity.clone();
        let store = self.closure_store()?;
        let result = closure::closure_transaction(&store, |_, journal| {
            if let Some((cursor, existing)) = journal
                .parent_todo_binding_cursor(&identity.child_delegation_id, &identity.route_epoch)
            {
                return if existing == &parent_todo {
                    Ok((vec![], Ok(cursor)))
                } else {
                    Ok((vec![], Err(MailboxBootstrapError::InvalidRequest)))
                };
            }
            let cursor = journal.next_cursor();
            Ok((
                vec![ClosureRecord::ParentTodoBinding {
                    cursor,
                    child_delegation_id: identity.child_delegation_id.clone(),
                    route_epoch: identity.route_epoch.clone(),
                    parent_terminal_id: identity.parent_terminal_id.clone(),
                    parent_process_generation: identity.parent_process_generation,
                    parent_todo: parent_todo.clone(),
                }],
                Ok(cursor),
            ))
        })
        .map_err(store_error)?;
        let cursor = result?;
        let child: crate::delegation::DelegationId = identity
            .child_delegation_id
            .parse()
            .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
        self.parent_current_route_identity(session, child)?;
        Ok(cursor)
    }

    /// One bounded page of signals for this exact parent execution. The
    /// returned `throughCursor` never skips an unreturned signal.
    pub(crate) fn child_report_signals_page(
        &self,
        session: &MailboxBootstrapSession,
        after_cursor: u64,
    ) -> Result<Value, MailboxBootstrapError> {
        self.require_child_report_signals()?;
        self.require_live_session(session)?;
        let journal = self.closure_journal()?;
        if after_cursor > journal.last_cursor() {
            return Err(MailboxBootstrapError::InvalidRequest);
        }
        let mailbox = MailboxStore::existing(&self.sender_authority_dir)
            .load()
            .map_err(store_error)?;
        let signals: Vec<ChildReportSignal> = journal
            .signals_for_parent(
                &session.caller,
                session.active_execution_generation,
                after_cursor,
            )
            .into_iter()
            .map(|mut signal| {
                signal.recovery.available = self.signal_recoverable(&journal, &mailbox, &signal);
                signal
            })
            .collect();
        let through_cursor = if signals.len() == closure::MAX_PAGE {
            signals
                .last()
                .map_or(after_cursor, |signal| signal.signal_cursor)
        } else {
            journal.last_cursor().max(after_cursor)
        };
        Ok(json!({"type":"child_report_signals","signals":signals,"throughCursor":through_cursor}))
    }

    /// Parent asks the server to relay one recovery request to the child.
    /// A repeat returns the same recovery cursor.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn request_report_recovery(
        &mut self,
        session: &MailboxBootstrapSession,
        signal_cursor: u64,
        route_epoch: &str,
        child_terminal_id: &str,
        local_root: &str,
        local_revision: u64,
        state_digest: &str,
    ) -> Result<u64, MailboxBootstrapError> {
        self.require_child_report_signals()?;
        self.require_live_session(session)?;
        let journal = self.closure_journal()?;
        let signal = journal
            .signal(signal_cursor)
            .cloned()
            .ok_or(MailboxBootstrapError::InvalidRequest)?;
        if signal.route.parent_terminal_id != session.caller
            || signal.route.parent_process_generation != session.active_execution_generation
        {
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        if signal.route.route_epoch != route_epoch
            || signal.route.child_terminal_id != child_terminal_id
            || signal.todo.local_root != local_root
            || signal.todo.local_revision != local_revision
            || signal.todo.state_digest != state_digest
        {
            return Err(MailboxBootstrapError::InvalidRequest);
        }
        if let Some((cursor, _)) = journal.recovery_request(signal_cursor) {
            return Ok(cursor);
        }
        let child: crate::delegation::DelegationId = signal
            .route
            .child_delegation_id
            .parse()
            .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
        if self.parent_current_route_identity(session, child)? != signal.route {
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        let store = self.closure_store()?;
        let recoverable = {
            let mailbox = store.load().map_err(store_error)?;
            self.signal_recoverable(&journal, &mailbox, &signal)
        };
        if !recoverable {
            self.suspend_if_not_current(&store, &signal.route.child_delegation_id);
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        closure::closure_transaction(&store, |mailbox, journal| {
            if let Some((cursor, _)) = journal.recovery_request(signal_cursor) {
                return Ok((vec![], cursor));
            }
            if journal
                .latest_signal_for_child(&signal.route.child_delegation_id)
                .map(|latest| latest.signal_cursor)
                != Some(signal_cursor)
                || latest_done_on_route(mailbox, &signal.route).as_ref() != Some(&signal.todo)
            {
                return Err(crate::mailbox::MailboxError::InvalidRecord);
            }
            let cursor = journal.next_cursor();
            Ok((
                vec![ClosureRecord::RecoveryRequested {
                    cursor,
                    signal_cursor,
                    route: signal.route.clone(),
                    mailbox_cursor: mailbox.record_cursor,
                }],
                cursor,
            ))
        })
        .map_err(|_| MailboxBootstrapError::GrantRevoked)
    }

    /// Open (undeclined) recovery requests for this covered child's route.
    pub(crate) fn recovery_requests_page(
        &self,
        session: &MailboxBootstrapSession,
        after_cursor: u64,
    ) -> Result<Value, MailboxBootstrapError> {
        self.require_child_report_signals()?;
        let route = self.covered_child_route(session)?;
        let journal = self.closure_journal()?;
        if after_cursor > journal.last_cursor() {
            return Err(MailboxBootstrapError::InvalidRequest);
        }
        let requests: Vec<Value> = journal
            .recovery_requests_for_child(
                &route.child_terminal_id,
                route.child_process_generation,
                &route.route_epoch,
                after_cursor,
            )
            .into_iter()
            .filter(|(_, signal)| journal.decline(signal.signal_cursor).is_none())
            .map(|(cursor, signal)| {
                json!({
                    "signalCursor": signal.signal_cursor,
                    "recoveryCursor": cursor,
                    "routeEpoch": signal.route.route_epoch,
                    "localRoot": signal.todo.local_root,
                    "localRevision": signal.todo.local_revision,
                    "stateDigest": signal.todo.state_digest,
                })
            })
            .collect();
        Ok(json!({"type":"report_recovery_wait","requests":requests}))
    }

    fn covered_child_route(
        &self,
        session: &MailboxBootstrapSession,
    ) -> Result<RouteIdentity, MailboxBootstrapError> {
        let bound = self.bound_parent_report_current(session)?;
        let identity = self
            .child_report_route_identity(session, &bound)
            .ok_or(MailboxBootstrapError::GrantRevoked)?;
        let journal = self.closure_journal()?;
        if journal
            .latest_domain_for_delegation(&identity.child_delegation_id)
            .is_none_or(|domain| domain.route != identity)
        {
            return Err(MailboxBootstrapError::GrantMissing);
        }
        Ok(identity)
    }

    /// The child declines a recovery request; the parent then receives a
    /// `report_unknown` with reason `recovery_declined_<reason>`.
    pub(crate) fn decline_report_recovery(
        &mut self,
        session: &MailboxBootstrapSession,
        signal_cursor: u64,
        recovery_cursor: u64,
        reason: DeclineReason,
    ) -> Result<u64, MailboxBootstrapError> {
        self.require_child_report_signals()?;
        let route = self.covered_child_route(session)?;
        let store = self.closure_store()?;
        let result = closure::closure_transaction(&store, |_, journal| {
            let Some(signal) = journal.signal(signal_cursor).cloned() else {
                return Ok((vec![], Err(MailboxBootstrapError::InvalidRequest)));
            };
            if signal.route != route
                || journal
                    .recovery_request(signal_cursor)
                    .map(|(cursor, _)| cursor)
                    != Some(recovery_cursor)
            {
                return Ok((vec![], Err(MailboxBootstrapError::InvalidRequest)));
            }
            if let Some((cursor, existing)) = journal.decline(signal_cursor) {
                return Ok((
                    vec![],
                    if existing == reason {
                        Ok(cursor)
                    } else {
                        Err(MailboxBootstrapError::InvalidRequest)
                    },
                ));
            }
            let records = Self::decline_records(journal, &signal, recovery_cursor, reason, false);
            let cursor = records[0].cursor();
            Ok((records, Ok(cursor)))
        })
        .map_err(store_error)?;
        result
    }

    /// #161: one report-only wake for a covered child that finished without
    /// exporting its report. Journals exactly one wake per signal, then
    /// delivers one server-minted `recovery_wake` head whose stable ID is
    /// derived from the signal. A repeat returns the same cursor and delivers
    /// nothing. An unissuable or unverifiable wake settles the request as
    /// UNKNOWN through the decline path.
    pub(crate) fn request_recovery_wake(
        &mut self,
        session: &MailboxBootstrapSession,
        signal_cursor: u64,
    ) -> Result<u64, MailboxBootstrapError> {
        self.require_child_report_signals()?;
        let route = self.covered_child_route(session)?;
        let journal = self.closure_journal()?;
        let signal = journal
            .signal(signal_cursor)
            .cloned()
            .ok_or(MailboxBootstrapError::InvalidRequest)?;
        if signal.route != route {
            return Err(MailboxBootstrapError::InvalidRequest);
        }
        let Some((recovery_cursor, _)) = journal.recovery_request(signal_cursor) else {
            return Err(MailboxBootstrapError::InvalidRequest);
        };
        let store = self.closure_store()?;
        let stable_id = closure::recovery_wake_stable_id(signal_cursor);
        let delivered = |store: &MailboxStore, wake_cursor: u64| {
            store.load().is_ok_and(|mailbox| {
                mailbox.heads.get(&stable_id).is_some_and(|head| {
                    head.kind == closure::RECOVERY_WAKE_KIND
                        && head.recipient == session.recipient
                        && head.body == closure::recovery_wake_body(signal_cursor, wake_cursor)
                        && mailbox
                            .receipts
                            .get(&head.delivery_digest)
                            .is_some_and(|receipt| {
                                receipt.stable_id == head.stable_id
                                    && receipt.status == crate::mailbox::ReceiptStatus::Admitted
                            })
                })
            })
        };
        if let Some((wake_cursor, _)) = journal.wake(signal_cursor) {
            if delivered(&store, wake_cursor) {
                return Ok(wake_cursor);
            }
            self.settle_unissued_wake(signal_cursor);
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        if journal.decline(signal_cursor).is_some() {
            return Err(MailboxBootstrapError::InvalidRequest);
        }
        let recoverable = {
            let mailbox = store.load().map_err(store_error)?;
            self.signal_recoverable(&journal, &mailbox, &signal)
        };
        if !recoverable {
            self.suspend_if_not_current(&store, &signal.route.child_delegation_id);
            self.settle_unissued_wake(signal_cursor);
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        let issued = closure::closure_transaction(&store, |_, journal| {
            if let Some((cursor, _)) = journal.wake(signal_cursor) {
                return Ok((vec![], Some(cursor)));
            }
            if journal.decline(signal_cursor).is_some() {
                return Ok((vec![], None));
            }
            let cursor = journal.next_cursor();
            Ok((
                vec![ClosureRecord::RecoveryWakeIssued {
                    cursor,
                    signal_cursor,
                    recovery_cursor,
                    route: route.clone(),
                    stable_id: stable_id.clone(),
                }],
                Some(cursor),
            ))
        });
        let wake_cursor = match issued {
            Ok(Some(cursor)) => cursor,
            Ok(None) => return Err(MailboxBootstrapError::InvalidRequest),
            Err(_) => {
                self.settle_unissued_wake(signal_cursor);
                return Err(MailboxBootstrapError::GrantRevoked);
            }
        };
        let head = recovery_wake_head(&session.recipient, &stable_id, signal_cursor, wake_cursor);
        let appended = store.append_server_recovery_wake(head);
        if appended.is_err() || !delivered(&store, wake_cursor) {
            self.settle_unissued_wake(signal_cursor);
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        Ok(wake_cursor)
    }
}

/// The server-minted wake head: fixed subject, revision 1, normal priority
/// and a canonical `{signalCursor,wakeCursor}` body that Pi never renders.
fn recovery_wake_head(
    recipient: &crate::mailbox::RecipientKey,
    stable_id: &str,
    signal_cursor: u64,
    wake_cursor: u64,
) -> crate::mailbox::MailboxHead {
    use sha2::Digest as _;
    let body = closure::recovery_wake_body(signal_cursor, wake_cursor);
    let hex = |bytes: &[u8]| format!("{:x}", sha2::Sha256::digest(bytes));
    crate::mailbox::MailboxHead {
        stable_id: stable_id.to_owned(),
        revision: 1,
        digest: hex(format!("{}\n{body}", closure::RECOVERY_WAKE_SUBJECT).as_bytes()),
        delivery_digest: hex(format!("recovery-wake-delivery:{stable_id}").as_bytes()),
        recipient: recipient.clone(),
        subject: closure::RECOVERY_WAKE_SUBJECT.into(),
        body,
        recipient_generation: recipient.generation.clone(),
        sender: "herdr:server".into(),
        target: recipient.recipient_id.clone(),
        grant_id: "server:recovery-wake".into(),
        message_id: stable_id.to_owned(),
        kind: closure::RECOVERY_WAKE_KIND.into(),
        priority: "normal".into(),
        original_sequence: 1,
        enqueue_epoch: 0,
        accepted_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |elapsed| elapsed.as_secs().max(1)),
    }
}
