use crate::api::schema::{MailboxOfflineSubmitParams, ResponseResult, SuccessResponse};
use crate::app::{App, MailboxBootstrapError, MailboxBootstrapSession};
use serde_json::Value;

use super::responses::{encode_error, encode_success};

impl App {
    /// Dispatches only the mailbox operations for an already verified,
    /// server-issued accepted-stream binding. Request payloads intentionally do
    /// not carry caller, grant, or recipient selectors; this method installs the
    /// descriptor scope after the exact Active generation recheck.
    pub(crate) fn dispatch_mailbox_bootstrap(
        &mut self,
        session: &MailboxBootstrapSession,
        method: &str,
        params: Value,
    ) -> Result<Value, MailboxBootstrapError> {
        self.mailbox_bootstrap_session_current(session)?;
        if method == "mailbox.history_snapshot" {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct HistorySnapshotParams {
                protocol: String,
            }
            let params: HistorySnapshotParams = serde_json::from_value(params)
                .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
            let snapshot = self.mailbox_bootstrap_history_snapshot(session, &params.protocol)?;
            return serde_json::to_value(ResponseResult::MailboxSnapshot { snapshot })
                .map_err(|_| MailboxBootstrapError::InvalidRequest);
        }
        if method == "delegation.child_report_disposition" {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct DispositionParams {
                protocol: String,
                child_delegation_id: String,
                #[serde(default)]
                after_cursor: Option<u64>,
            }
            let params: DispositionParams = serde_json::from_value(params)
                .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
            if params.protocol != crate::mailbox_v1::PROTOCOL {
                return Err(MailboxBootstrapError::InvalidRequest);
            }
            let child: crate::delegation::DelegationId = params
                .child_delegation_id
                .parse()
                .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
            let disposition = self.child_report_disposition_for_parent(session, child)?;
            if params
                .after_cursor
                .is_some_and(|cursor| cursor > disposition.cursor)
            {
                return Err(MailboxBootstrapError::InvalidRequest);
            }
            let changed = params
                .after_cursor
                .is_none_or(|cursor| disposition.cursor > cursor);
            return Ok(serde_json::json!({ "disposition": disposition, "changed": changed }));
        }
        if session.history_only {
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        if let Some(result) = self.dispatch_recipient_inbox(session, method, &params) {
            return result;
        }
        if session.recipient_only.is_some() {
            // No sender, grant, bound-report or route authority without a
            // trusted managed launch.
            return Err(MailboxBootstrapError::GrantMissing);
        }
        let id = "mailbox-bootstrap".to_owned();
        let response = match method {
            "mailbox.offline_submit" | "report_submit" => {
                let submit: crate::mailbox_v1::Submit = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                if method == "report_submit" && submit.kind != "report" {
                    return Err(MailboxBootstrapError::InvalidRequest);
                }
                let submit_via = if method == "report_submit" {
                    // Unchanged self-report path (pending a decision on its use).
                    Self::handle_mailbox_server_scoped_submit
                } else {
                    Self::handle_mailbox_offline_submit
                };
                submit_via(
                    self,
                    id,
                    MailboxOfflineSubmitParams {
                        caller: session.caller.clone(),
                        grant_id: session.grant_id.clone(),
                        recipient: session.recipient.clone(),
                        submit,
                    },
                )
            }
            "todo_state" => {
                let params: crate::child_report::TodoStateParams =
                    serde_json::from_value(params)
                        .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                let route = self.bound_parent_report_current(session)?;
                let identity = self
                    .child_report_route_identity(session, &route)
                    .ok_or(MailboxBootstrapError::GrantRevoked)?;
                let event = params
                    .bind(identity)
                    .ok_or(MailboxBootstrapError::InvalidRequest)?;
                let authority = self
                    .offline_mailbox_authorities
                    .get(&session.caller)
                    .ok_or(MailboxBootstrapError::GrantRevoked)?;
                let cursor = authority
                    .store
                    .append_child_report_event(event.clone())
                    .map_err(child_report_store_error)?;
                let recovered = authority
                    .store
                    .load()
                    .map_err(|_| MailboxBootstrapError::GrantMissing)?;
                if recovered.record_cursor < cursor
                    || !recovered.child_report_events.contains(&event)
                {
                    return Err(MailboxBootstrapError::GrantMissing);
                }
                self.bound_parent_report_current(session)?;
                return Ok(serde_json::json!({"type":"todo_state", "cursor":cursor,
                                            "routeEpoch":route.route_epoch()}));
            }
            "report_path_attempt" => {
                let params: crate::child_report::ReportPathAttemptParams =
                    serde_json::from_value(params)
                        .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                let route = self.bound_parent_report_current(session)?;
                let identity = self
                    .child_report_route_identity(session, &route)
                    .ok_or(MailboxBootstrapError::GrantRevoked)?;
                let event = params
                    .bind(identity)
                    .ok_or(MailboxBootstrapError::InvalidRequest)?;
                let authority = self
                    .offline_mailbox_authorities
                    .get(&session.caller)
                    .ok_or(MailboxBootstrapError::GrantRevoked)?;
                let cursor = authority
                    .store
                    .append_child_report_path_attempt(event)
                    .map_err(child_report_store_error)?;
                // A durable event may already exist if the live route changes
                // after fsync. The response must nevertheless fail closed.
                self.bound_parent_report_current(session)?;
                return Ok(
                    serde_json::json!({"type":"report_path_attempt", "cursor":cursor,
                    "routeEpoch":route.route_epoch(), "routeAuthenticated":true,
                    "canonicalCommitVerified":false, "coverageQualified":false,
                    "effectRetryAuthorized":false}),
                );
            }
            "report_prepared" => {
                let params: crate::child_report::PrepareParams = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                let route = self.bound_parent_report_current(session)?;
                let identity = self
                    .child_report_route_identity(session, &route)
                    .ok_or(MailboxBootstrapError::GrantRevoked)?;
                let preparation = params
                    .bind(identity)
                    .ok_or(MailboxBootstrapError::InvalidRequest)?;
                let authority = self
                    .offline_mailbox_authorities
                    .get(&session.caller)
                    .ok_or(MailboxBootstrapError::GrantRevoked)?;
                let cursor = authority
                    .store
                    .append_child_report_event(crate::child_report::ChildReportEvent::Prepared {
                        preparation: preparation.clone(),
                    })
                    .map_err(child_report_store_error)?;
                if !authority
                    .store
                    .load()
                    .map_err(|_| MailboxBootstrapError::GrantMissing)?
                    .child_report_events
                    .contains(&crate::child_report::ChildReportEvent::Prepared { preparation })
                {
                    return Err(MailboxBootstrapError::GrantMissing);
                }
                self.bound_parent_report_current(session)?;
                return Ok(
                    serde_json::json!({"type":"report_prepared", "cursor":cursor,
                                            "routeEpoch":route.route_epoch()}),
                );
            }
            "report_coverage" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct CoverageParams {
                    protocol: String,
                    local_root: String,
                    local_revision: u64,
                }
                let params: CoverageParams = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                if params.protocol != crate::mailbox_v1::PROTOCOL
                    || params.local_root.is_empty()
                    || params.local_root.len() > 128
                    || params.local_revision == 0
                {
                    return Err(MailboxBootstrapError::InvalidRequest);
                }
                let route = self.bound_parent_report_current(session)?;
                let identity = self
                    .child_report_route_identity(session, &route)
                    .ok_or(MailboxBootstrapError::GrantRevoked)?;
                let authority = self
                    .offline_mailbox_authorities
                    .get(&session.caller)
                    .ok_or(MailboxBootstrapError::GrantRevoked)?;
                let cursor = authority
                    .store
                    .append_child_report_event(
                        crate::child_report::ChildReportEvent::CoverageBarrier {
                            route: identity,
                            local_root: params.local_root,
                            local_revision: params.local_revision,
                            through_cursor: 0,
                            qualification: crate::child_report::CoverageQualification::ObservedOnly,
                        },
                    )
                    .map_err(child_report_store_error)?;
                if authority
                    .store
                    .load()
                    .map_err(|_| MailboxBootstrapError::GrantMissing)?
                    .record_cursor
                    < cursor
                {
                    return Err(MailboxBootstrapError::GrantMissing);
                }
                self.bound_parent_report_current(session)?;
                return Ok(
                    serde_json::json!({"type":"report_coverage", "cursor":cursor,
                                            "routeEpoch":route.route_epoch(), "coverageQualified":false}),
                );
            }
            "report_submit_parent" => {
                let submit: crate::mailbox_v1::Submit = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                if submit.kind != "report" {
                    return Err(MailboxBootstrapError::InvalidRequest);
                }
                let route = self.bound_parent_report_current(session)?;
                let identity = self
                    .child_report_route_identity(session, &route)
                    .ok_or(MailboxBootstrapError::GrantRevoked)?;
                // Only a durable exact preparation allows a bound-parent send.
                // Legacy self/cross grants continue on their own untracked paths.
                let authority = self
                    .offline_mailbox_authorities
                    .get(&session.caller)
                    .ok_or(MailboxBootstrapError::GrantRevoked)?;
                let recovered = authority
                    .store
                    .load()
                    .map_err(|_| MailboxBootstrapError::GrantMissing)?;
                let prepared: Vec<_> = recovered
                    .child_report_events
                    .iter()
                    .filter_map(|event| match event {
                        crate::child_report::ChildReportEvent::Prepared { preparation }
                            if preparation.route == identity
                                && preparation.matches_submit(&submit) =>
                        {
                            Some(preparation.clone())
                        }
                        _ => None,
                    })
                    .collect();
                if prepared.len() != 1 {
                    return Err(MailboxBootstrapError::GrantMissing);
                }
                authority
                    .store
                    .append_child_report_event(
                        crate::child_report::ChildReportEvent::PreparedAttempt {
                            preparation: prepared[0].clone(),
                        },
                    )
                    .map_err(child_report_store_error)?;
                self.bound_parent_report_current(session)?;
                self.handle_mailbox_server_scoped_submit(
                    id,
                    MailboxOfflineSubmitParams {
                        caller: session.caller.clone(),
                        grant_id: route.grant_id.clone(),
                        recipient: route.recipient.clone(),
                        submit,
                    },
                )
            }
            "mailbox.provision_recipient" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct ProvisionRecipientParams {
                    target: String,
                }
                let params: ProvisionRecipientParams = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                let grant = self.provision_mailbox_bootstrap_recipient(session, &params.target)?;
                encode_success(id, ResponseResult::MailboxGrantProvisioned { grant })
            }
            "mailbox.snapshot" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct SnapshotParams {
                    protocol: String,
                }
                let params: SnapshotParams = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                self.handle_mailbox_snapshot(
                    id,
                    crate::api::schema::MailboxSnapshotParams {
                        caller: session.caller.clone(),
                        grant_id: session.grant_id.clone(),
                        recipient: session.recipient.clone(),
                        protocol: params.protocol,
                    },
                )
            }
            "mailbox.claim" => {
                let claim = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                self.handle_mailbox_claim(
                    id,
                    crate::api::schema::MailboxClaimParams {
                        caller: session.caller.clone(),
                        grant_id: session.grant_id.clone(),
                        recipient: session.recipient.clone(),
                        claim,
                    },
                )
            }
            "mailbox.edit" => {
                let edit = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                self.handle_mailbox_edit(
                    id,
                    crate::api::schema::MailboxEditParams {
                        caller: session.caller.clone(),
                        grant_id: session.grant_id.clone(),
                        recipient: session.recipient.clone(),
                        edit,
                    },
                )
            }
            "mailbox.resolve" => {
                let resolve = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                self.handle_mailbox_resolve(
                    id,
                    crate::api::schema::MailboxResolveParams {
                        caller: session.caller.clone(),
                        grant_id: session.grant_id.clone(),
                        recipient: session.recipient.clone(),
                        resolve,
                    },
                )
            }
            _ => return Err(MailboxBootstrapError::InvalidRequest),
        };
        // A handler's refusal or post-admission failure keeps its specific
        // code on the wire; invalid_request is only for malformed requests.
        if let Ok(refused) = serde_json::from_str::<crate::api::schema::ErrorResponse>(&response) {
            return Err(MailboxBootstrapError::from_handler_error(
                &refused.error.code,
            ));
        }
        let success: SuccessResponse =
            serde_json::from_str(&response).map_err(|_| MailboxBootstrapError::InvalidRequest)?;
        serde_json::to_value(success.result).map_err(|_| MailboxBootstrapError::InvalidRequest)
    }

    /// The pane inbox surface for every current Messages session (managed or
    /// recipient-only): snapshot, claim, edit, resolve and drop over all of
    /// the pane's recipient keys, with claims bound to this execution.
    /// Returns `None` for any other method.
    fn dispatch_recipient_inbox(
        &mut self,
        session: &MailboxBootstrapSession,
        method: &str,
        params: &Value,
    ) -> Option<Result<Value, MailboxBootstrapError>> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct ProtocolParams {
            protocol: String,
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct HeadVersionParams {
            protocol: String,
            stable_id: String,
            expected_revision: u64,
        }
        if !matches!(
            method,
            "mailbox.snapshot"
                | "mailbox.claim"
                | "mailbox.edit"
                | "mailbox.resolve"
                | "mailbox.drop"
                | "mailbox.retry"
                | "mailbox.reprioritize"
                | "mailbox.enqueue_self"
        ) {
            return None;
        }
        Some((|| {
            let store = crate::mailbox::MailboxStore::open(&self.sender_authority_dir)
                .map_err(|_| MailboxBootstrapError::GrantMissing)?;
            let recipients = self.inbox_recipients(&session.caller);
            let execution = crate::app::messages::session_execution(session);
            let current = self.current_agent_session_value(&session.caller);
            let load = || {
                store
                    .load()
                    .map_err(|_| MailboxBootstrapError::GrantMissing)
            };
            let view = |recovered: &crate::mailbox::RecoveredMailbox| {
                crate::app::messages::inbox_snapshot(
                    recovered,
                    &recipients,
                    &execution,
                    current.as_deref(),
                )
                .map_err(|_| MailboxBootstrapError::GrantMissing)
            };
            // Scope: a head (or claim) that exists outside this pane's own
            // inbox is refused with a distinct error and never touched.
            let in_scope = |stable_id: &str| -> Result<(), MailboxBootstrapError> {
                match load()?.heads.get(stable_id) {
                    Some(head) if !recipients.contains(&head.recipient) => {
                        Err(MailboxBootstrapError::HeadOutOfScope)
                    }
                    _ => Ok(()),
                }
            };
            let protocol_ok = |protocol: &str| {
                (protocol == crate::mailbox_v1::PROTOCOL)
                    .then_some(())
                    .ok_or(MailboxBootstrapError::InvalidRequest)
            };
            let to_value = |result: ResponseResult| {
                serde_json::to_value(result).map_err(|_| MailboxBootstrapError::InvalidRequest)
            };
            match method {
                "mailbox.retry" => {
                    // The recipient's explicit Retry of a head that an ended
                    // Pi execution left claimed or admitted: re-deliver it as
                    // a new head (runs in normal order) and close the old
                    // claim. Deterministic, so a repeated Retry is idempotent.
                    let params: HeadVersionParams = serde_json::from_value(params.clone())
                        .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                    protocol_ok(&params.protocol)?;
                    in_scope(&params.stable_id)?;
                    let recovered = load()?;
                    let head = recovered
                        .heads
                        .get(&params.stable_id)
                        .filter(|head| {
                            recipients.contains(&head.recipient)
                                && head.revision == params.expected_revision
                        })
                        .cloned()
                        .ok_or(MailboxBootstrapError::InvalidRequest)?;
                    let claim = recovered
                        .claims
                        .get(&head.stable_id)
                        .filter(|claim| {
                            !crate::mailbox::is_withdrawn_claim(claim)
                                && !crate::app::messages::claim_is_current(claim, &execution)
                        })
                        .cloned()
                        .ok_or(MailboxBootstrapError::InvalidRequest)?;
                    let already_settled = matches!(
                        recovered.resolutions.get(&claim.claim_id),
                        Some(crate::mailbox::ClaimResolution {
                            outcome: crate::mailbox::ClaimResolutionOutcome::Settled,
                            ..
                        })
                    );
                    let copy = crate::app::messages::retry_head(&head, &claim);
                    let existing = recovered.heads.contains_key(&copy.stable_id);
                    if already_settled && !existing {
                        return Err(MailboxBootstrapError::InvalidRequest);
                    }
                    let receipt = if existing {
                        recovered
                            .receipts
                            .get(&copy.delivery_digest)
                            .cloned()
                            .ok_or(MailboxBootstrapError::GrantMissing)?
                    } else {
                        store
                            .append_offline_head(copy.clone())
                            .map_err(|_| MailboxBootstrapError::InvalidRequest)?
                    };
                    let resolution = store
                        .resolve_claim_closed_by(
                            &claim.claim_id,
                            crate::mailbox::ClaimResolutionOutcome::Settled,
                            Some("retried"),
                        )
                        .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                    Ok(serde_json::json!({
                        "type": "mailbox_retried",
                        "stableId": head.stable_id,
                        "revision": head.revision,
                        "newStableId": copy.stable_id,
                        "receipt": {"head": receipt, "claim": claim, "resolution": resolution},
                        "snapshot": view(&load()?)?,
                    }))
                }
                "mailbox.reprioritize" => {
                    // The recipient's explicit priority change on a held head
                    // in its own pane inbox: a new revision (F3 receipt), and
                    // claim order follows the new priority.
                    #[derive(serde::Deserialize)]
                    #[serde(rename_all = "camelCase", deny_unknown_fields)]
                    struct ReprioritizeParams {
                        protocol: String,
                        stable_id: String,
                        expected_revision: u64,
                        priority: String,
                    }
                    let params: ReprioritizeParams = serde_json::from_value(params.clone())
                        .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                    protocol_ok(&params.protocol)?;
                    in_scope(&params.stable_id)?;
                    if !matches!(params.priority.as_str(), "low" | "normal" | "high") {
                        return Err(MailboxBootstrapError::InvalidRequest);
                    }
                    let recovered = load()?;
                    let head = recovered
                        .heads
                        .get(&params.stable_id)
                        .filter(|head| {
                            recipients.contains(&head.recipient)
                                && head.revision == params.expected_revision
                                && !recovered.claims.contains_key(&head.stable_id)
                        })
                        .cloned()
                        .ok_or(MailboxBootstrapError::InvalidRequest)?;
                    let edited = store
                        .edit_unclaimed_head(crate::mailbox::MailboxHeadEdit {
                            stable_id: head.stable_id,
                            revision: head.revision,
                            digest: head.digest,
                            subject: head.subject,
                            body: head.body,
                            repin_recipient_session: None,
                            priority: Some(params.priority),
                        })
                        .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                    let recovered = load()?;
                    let receipt = recovered
                        .receipts
                        .get(&edited.delivery_digest)
                        .cloned()
                        .ok_or(MailboxBootstrapError::GrantMissing)?;
                    Ok(serde_json::json!({
                        "type": "mailbox_reprioritized",
                        "stableId": edited.stable_id,
                        "revision": edited.revision,
                        "priority": edited.priority,
                        "receipt": receipt,
                        "snapshot": view(&recovered)?,
                    }))
                }
                "mailbox.enqueue_self" => {
                    // The human's own typing at this pane, queued in this
                    // pane's inbox only. No recipient selector exists, so it
                    // cannot reach any other pane, and it grants nothing.
                    #[derive(serde::Deserialize)]
                    #[serde(rename_all = "camelCase", deny_unknown_fields)]
                    struct EnqueueSelfParams {
                        protocol: String,
                        subject: String,
                        body: String,
                        #[serde(default)]
                        priority: Option<String>,
                        #[serde(default)]
                        client_id: Option<String>,
                    }
                    let params: EnqueueSelfParams = serde_json::from_value(params.clone())
                        .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                    protocol_ok(&params.protocol)?;
                    let inbox = session
                        .pane_inbox
                        .clone()
                        .ok_or(MailboxBootstrapError::GrantMissing)?;
                    let pane_label = self
                        .public_pane_for_terminal(&session.caller)
                        .unwrap_or_else(|| session.caller.clone());
                    let head = crate::app::messages::human_self_head(
                        &inbox,
                        &session.caller,
                        &pane_label,
                        params.subject,
                        params.body,
                        params.priority.unwrap_or_else(|| "normal".into()),
                        params.client_id,
                        current.clone(),
                    )
                    .ok_or(MailboxBootstrapError::InvalidRequest)?;
                    let stable_id = head.stable_id.clone();
                    let existing = load()?
                        .heads
                        .get(&stable_id)
                        .map(|existing| existing.delivery_digest.clone());
                    let duplicate = existing.is_some();
                    let receipt = match existing {
                        // A retried clientId: the original head and receipt.
                        Some(delivery_digest) => load()?
                            .receipts
                            .get(&delivery_digest)
                            .cloned()
                            .ok_or(MailboxBootstrapError::GrantMissing)?,
                        None => store
                            .append_offline_head(head)
                            .map_err(|_| MailboxBootstrapError::InvalidRequest)?,
                    };
                    Ok(serde_json::json!({
                        "type": "mailbox_enqueued",
                        "stableId": stable_id,
                        "revision": receipt.revision,
                        "duplicate": duplicate,
                        "receipt": receipt,
                        "snapshot": view(&load()?)?,
                    }))
                }
                "mailbox.snapshot" => {
                    let params: ProtocolParams = serde_json::from_value(params.clone())
                        .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                    protocol_ok(&params.protocol)?;
                    to_value(ResponseResult::MailboxSnapshot {
                        snapshot: view(&load()?)?,
                    })
                }
                "mailbox.claim" => {
                    let _: crate::mailbox_v1::ClaimRequest = serde_json::from_value(params.clone())
                        .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                    let claim = store
                        .claim_next_for_execution(&recipients, &execution)
                        .map_err(|_| MailboxBootstrapError::GrantMissing)?;
                    to_value(ResponseResult::MailboxClaimed { claim })
                }
                "mailbox.edit" => {
                    let edit: crate::mailbox_v1::Edit = serde_json::from_value(params.clone())
                        .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                    // Own pane inbox only.
                    in_scope(&edit.stable_id)?;
                    if !load()?
                        .heads
                        .get(&edit.stable_id)
                        .is_some_and(|head| recipients.contains(&head.recipient))
                    {
                        return Err(MailboxBootstrapError::InvalidRequest);
                    }
                    crate::mailbox_v1::edit_unclaimed(&store, edit)
                        .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                    to_value(ResponseResult::MailboxEdited {
                        snapshot: view(&load()?)?,
                    })
                }
                "mailbox.resolve" => {
                    let resolve: crate::mailbox_v1::Resolve =
                        serde_json::from_value(params.clone())
                            .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                    let recovered = load()?;
                    if recovered.claims.values().any(|claim| {
                        claim.claim_id == resolve.claim_id && !recipients.contains(&claim.recipient)
                    }) {
                        return Err(MailboxBootstrapError::HeadOutOfScope);
                    }
                    let claim = recovered
                        .claims
                        .values()
                        .find(|claim| claim.claim_id == resolve.claim_id)
                        .filter(|claim| {
                            recipients.contains(&claim.recipient)
                                && !crate::mailbox::is_withdrawn_claim(claim)
                        })
                        .cloned()
                        .ok_or(MailboxBootstrapError::InvalidRequest)?;
                    let own = crate::app::messages::claim_is_current(&claim, &execution);
                    let outcome = match resolve.outcome {
                        crate::mailbox_v1::ResolveOutcome::Admitted => {
                            crate::mailbox::ClaimResolutionOutcome::Admitted
                        }
                        crate::mailbox_v1::ResolveOutcome::Settled => {
                            crate::mailbox::ClaimResolutionOutcome::Settled
                        }
                    };
                    // The pane's currently attached Pi may close a claim that a
                    // previous Pi in the same pane left claimed or admitted
                    // (rc2 property, extended to pane queues and receive-only
                    // Pis): settle only, never admit, recorded as recovered.
                    // Any other caller is outside this pane's inbox and refused.
                    if !own && outcome != crate::mailbox::ClaimResolutionOutcome::Settled {
                        return Err(MailboxBootstrapError::InvalidRequest);
                    }
                    let resolution = store
                        .resolve_claim_closed_by(
                            &claim.claim_id,
                            outcome,
                            (!own).then_some("recovered"),
                        )
                        .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                    to_value(ResponseResult::MailboxResolved { resolution })
                }
                "mailbox.drop" => {
                    // The human's Drop: any held head in this pane's inbox, or
                    // one left claimed by another (exited) Pi execution.
                    let params: HeadVersionParams = serde_json::from_value(params.clone())
                        .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                    protocol_ok(&params.protocol)?;
                    in_scope(&params.stable_id)?;
                    let recovered = load()?;
                    let head = recovered
                        .heads
                        .get(&params.stable_id)
                        .filter(|head| {
                            recipients.contains(&head.recipient)
                                && head.revision == params.expected_revision
                        })
                        .cloned()
                        .ok_or(MailboxBootstrapError::InvalidRequest)?;
                    match recovered.claims.get(&head.stable_id).cloned() {
                        None => store
                            .withdraw_unclaimed_head(&head.stable_id, head.revision, &head.digest)
                            .map_err(|_| MailboxBootstrapError::InvalidRequest)?,
                        Some(claim)
                            if !crate::mailbox::is_withdrawn_claim(&claim)
                                && !crate::app::messages::claim_is_current(&claim, &execution)
                                && !matches!(
                                    recovered.resolutions.get(&claim.claim_id),
                                    Some(crate::mailbox::ClaimResolution {
                                        outcome: crate::mailbox::ClaimResolutionOutcome::Settled,
                                        ..
                                    })
                                ) =>
                        {
                            store
                                .resolve_claim_closed_by(
                                    &claim.claim_id,
                                    crate::mailbox::ClaimResolutionOutcome::Settled,
                                    Some("dropped"),
                                )
                                .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                        }
                        Some(_) => return Err(MailboxBootstrapError::InvalidRequest),
                    }
                    let recovered = load()?;
                    let claim = recovered
                        .claims
                        .get(&head.stable_id)
                        .cloned()
                        .ok_or(MailboxBootstrapError::GrantMissing)?;
                    let resolution = recovered
                        .resolutions
                        .get(&claim.claim_id)
                        .cloned()
                        .ok_or(MailboxBootstrapError::GrantMissing)?;
                    Ok(serde_json::json!({
                        "type": "mailbox_dropped",
                        "stableId": head.stable_id,
                        "revision": head.revision,
                        "receipt": {"claim": claim, "resolution": resolution},
                        "snapshot": view(&recovered)?,
                    }))
                }
                _ => Err(MailboxBootstrapError::InvalidRequest),
            }
        })())
    }

    /// Current journal marker for `mailbox.watch`: changes on every append.
    pub(crate) fn mailbox_watch_marker(
        &self,
        session: &MailboxBootstrapSession,
    ) -> Result<u64, MailboxBootstrapError> {
        self.mailbox_bootstrap_session_current(session)?;
        Ok(
            crate::mailbox::MailboxStore::open(&self.sender_authority_dir)
                .map(|store| store.journal_len())
                .unwrap_or(0),
        )
    }

    pub(crate) fn handle_mailbox_offline_submit(
        &mut self,
        id: String,
        params: MailboxOfflineSubmitParams,
    ) -> String {
        // Durable bound-parent grants remain in old journals, but cannot be
        // exercised through the generic selector-bearing API, even after a
        // delegation reparent, parent replacement, or server restart. Older
        // `mailbox:` grants cannot be classified: typed and explicitly
        // provisioned grants previously used the identical durable ID.
        if params.grant_id.starts_with("bound-parent-report:") {
            return encode_error(
                id,
                "mailbox_capability_mismatch",
                "bound-parent report grants require their accepted stream",
            );
        }
        // A report head addressed to anyone else (a parent over a provisioned
        // cross grant) is admitted only through report_submit_parent on a ready
        // bound route. Self-addressed reports are unchanged pending review.
        if params.submit.kind == "report" && params.recipient.recipient_id != params.caller {
            return encode_error(
                id,
                "report_route_required",
                "report heads are admitted only through report_submit_parent on a ready bound route",
            );
        }
        let recipient = params.recipient.recipient_id.clone();
        let response = self.handle_mailbox_server_scoped_submit(id, params);
        // A head queued for a pane with no attached Pi requests its wake.
        if let Ok(SuccessResponse {
            result: ResponseResult::MailboxOfflineSubmitted { receipt },
            ..
        }) = serde_json::from_str::<SuccessResponse>(&response)
        {
            if let Some(terminal_key) = self.terminal_for_recipient(&recipient) {
                self.request_pane_wake_if_detached(&terminal_key, &receipt.stable_id);
            }
        }
        response
    }

    fn handle_mailbox_server_scoped_submit(
        &mut self,
        id: String,
        params: MailboxOfflineSubmitParams,
    ) -> String {
        match self.offline_mailbox_authority_current(&params.caller) {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                return encode_error(
                    id,
                    "mailbox_authority_unavailable",
                    "the server has no current authenticated local mailbox sender route",
                )
            }
        }
        let authority = self
            .offline_mailbox_authorities
            .get_mut(&params.caller)
            .expect("current route must remain installed during serialized dispatch");
        match authority.submit(params) {
            Ok(receipt) => encode_success(id, ResponseResult::MailboxOfflineSubmitted { receipt }),
            Err(crate::app::mailbox::OfflineMailboxError::CallerMismatch) => encode_error(
                id,
                "mailbox_caller_mismatch",
                "caller selector does not match the authenticated local sender",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::HeadOutOfScope) => encode_error(
                id,
                "mailbox_head_out_of_scope",
                "that message is neither in your own inbox nor one you sent to this recipient; nothing was changed",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::NotRecipient) => encode_error(
                id,
                "mailbox_not_recipient",
                "a send grant cannot claim, resolve or read another recipient's messages",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::CapabilityMismatch) => encode_error(
                id,
                "mailbox_capability_mismatch",
                "recipient or grant selector is outside the server-issued capability",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Replay) => encode_error(
                id,
                "mailbox_replay_rejected",
                "delivery digest was already admitted for this sender generation",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Transport(error)) => encode_error(
                id,
                error.code(),
                "offline mailbox request validation rejected",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Store(error)) => {
                encode_error(id, "mailbox_store_failed", error.to_string())
            }
            Err(crate::app::mailbox::OfflineMailboxError::ReceiptMissing) => encode_error(
                id,
                "mailbox_receipt_missing",
                "server admission did not read back its durable receipt",
            ),
        }
    }

    pub(crate) fn handle_mailbox_claim(
        &mut self,
        id: String,
        params: crate::api::schema::MailboxClaimParams,
    ) -> String {
        match self.offline_mailbox_authority_current(&params.caller) {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                return encode_error(
                    id,
                    "mailbox_authority_unavailable",
                    "the server has no current authenticated local mailbox sender route",
                )
            }
        }
        let current = self.current_agent_session_value(&params.recipient.recipient_id);
        let authority = self
            .offline_mailbox_authorities
            .get(&params.caller)
            .expect("current route must remain installed during serialized dispatch");
        match authority.claim(params, current.as_deref()) {
            Ok(claim) => encode_success(id, ResponseResult::MailboxClaimed { claim }),
            Err(crate::app::mailbox::OfflineMailboxError::CallerMismatch) => encode_error(
                id,
                "mailbox_caller_mismatch",
                "caller selector does not match the authenticated local sender",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::HeadOutOfScope) => encode_error(
                id,
                "mailbox_head_out_of_scope",
                "that message is neither in your own inbox nor one you sent to this recipient; nothing was changed",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::NotRecipient) => encode_error(
                id,
                "mailbox_not_recipient",
                "a send grant cannot claim, resolve or read another recipient's messages",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::CapabilityMismatch) => encode_error(
                id,
                "mailbox_capability_mismatch",
                "recipient or grant selector is outside the server-issued capability",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Transport(error)) => encode_error(
                id,
                error.code(),
                "offline mailbox request validation rejected",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Store(error)) => {
                encode_error(id, "mailbox_store_failed", error.to_string())
            }
            Err(_) => encode_error(id, "mailbox_claim_failed", "offline mailbox claim rejected"),
        }
    }

    pub(crate) fn handle_mailbox_snapshot(
        &mut self,
        id: String,
        params: crate::api::schema::MailboxSnapshotParams,
    ) -> String {
        match self.offline_mailbox_authority_current(&params.caller) {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                return encode_error(
                    id,
                    "mailbox_authority_unavailable",
                    "the server has no current authenticated local mailbox sender route",
                )
            }
        }
        let current = self.current_agent_session_value(&params.recipient.recipient_id);
        let authority = self
            .offline_mailbox_authorities
            .get(&params.caller)
            .expect("current route must remain installed during serialized dispatch");
        match authority.snapshot(params, current.as_deref()) {
            Ok(snapshot) => encode_success(id, ResponseResult::MailboxSnapshot { snapshot }),
            Err(crate::app::mailbox::OfflineMailboxError::CallerMismatch) => encode_error(
                id,
                "mailbox_caller_mismatch",
                "caller selector does not match the authenticated local sender",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::HeadOutOfScope) => encode_error(
                id,
                "mailbox_head_out_of_scope",
                "that message is neither in your own inbox nor one you sent to this recipient; nothing was changed",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::NotRecipient) => encode_error(
                id,
                "mailbox_not_recipient",
                "a send grant cannot claim, resolve or read another recipient's messages",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::CapabilityMismatch) => encode_error(
                id,
                "mailbox_capability_mismatch",
                "recipient or grant selector is outside the server-issued capability",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Transport(error)) => encode_error(
                id,
                error.code(),
                "offline mailbox request validation rejected",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Store(error)) => {
                encode_error(id, "mailbox_store_failed", error.to_string())
            }
            Err(_) => encode_error(
                id,
                "mailbox_snapshot_failed",
                "offline mailbox snapshot rejected",
            ),
        }
    }

    pub(crate) fn handle_mailbox_edit(
        &mut self,
        id: String,
        params: crate::api::schema::MailboxEditParams,
    ) -> String {
        match self.offline_mailbox_authority_current(&params.caller) {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                return encode_error(
                    id,
                    "mailbox_authority_unavailable",
                    "the server has no current authenticated local mailbox sender route",
                )
            }
        }
        let current = self.current_agent_session_value(&params.recipient.recipient_id);
        let authority = self
            .offline_mailbox_authorities
            .get(&params.caller)
            .expect("current route must remain installed during serialized dispatch");
        match authority.edit(params, current.as_deref()) {
            Ok(snapshot) => encode_success(id, ResponseResult::MailboxEdited { snapshot }),
            Err(crate::app::mailbox::OfflineMailboxError::CallerMismatch) => encode_error(
                id,
                "mailbox_caller_mismatch",
                "caller selector does not match the authenticated local sender",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::HeadOutOfScope) => encode_error(
                id,
                "mailbox_head_out_of_scope",
                "that message is neither in your own inbox nor one you sent to this recipient; nothing was changed",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::NotRecipient) => encode_error(
                id,
                "mailbox_not_recipient",
                "a send grant cannot claim, resolve or read another recipient's messages",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::CapabilityMismatch) => encode_error(
                id,
                "mailbox_capability_mismatch",
                "recipient or grant selector is outside the server-issued capability",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Transport(error)) => {
                encode_error(id, error.code(), "offline mailbox edit validation rejected")
            }
            Err(crate::app::mailbox::OfflineMailboxError::Store(
                crate::mailbox::MailboxError::EditConflict,
            )) => encode_error(
                id,
                "mailbox_edit_conflict",
                "stableId, revision, or digest no longer matches the authoritative head",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Store(
                crate::mailbox::MailboxError::HeadClaimed,
            )) => encode_error(
                id,
                "mailbox_edit_claimed",
                "the mailbox head is already claimed and immutable",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Store(error)) => {
                encode_error(id, "mailbox_store_failed", error.to_string())
            }
            Err(_) => encode_error(id, "mailbox_edit_failed", "offline mailbox edit rejected"),
        }
    }

    pub(crate) fn handle_mailbox_resolve(
        &mut self,
        id: String,
        params: crate::api::schema::MailboxResolveParams,
    ) -> String {
        match self.offline_mailbox_authority_current(&params.caller) {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                return encode_error(
                    id,
                    "mailbox_authority_unavailable",
                    "the server has no current authenticated local mailbox sender route",
                )
            }
        }
        let authority = self
            .offline_mailbox_authorities
            .get(&params.caller)
            .expect("current route must remain installed during serialized dispatch");
        match authority.resolve(params) {
            Ok(resolution) => encode_success(id, ResponseResult::MailboxResolved { resolution }),
            Err(crate::app::mailbox::OfflineMailboxError::CallerMismatch) => encode_error(
                id,
                "mailbox_caller_mismatch",
                "caller selector does not match the authenticated local sender",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::HeadOutOfScope) => encode_error(
                id,
                "mailbox_head_out_of_scope",
                "that message is neither in your own inbox nor one you sent to this recipient; nothing was changed",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::NotRecipient) => encode_error(
                id,
                "mailbox_not_recipient",
                "a send grant cannot claim, resolve or read another recipient's messages",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::CapabilityMismatch) => encode_error(
                id,
                "mailbox_capability_mismatch",
                "recipient or grant selector is outside the server-issued capability",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Transport(error)) => encode_error(
                id,
                error.code(),
                "offline mailbox request validation rejected",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Store(error)) => {
                encode_error(id, "mailbox_store_failed", error.to_string())
            }
            Err(_) => encode_error(
                id,
                "mailbox_resolve_failed",
                "offline mailbox resolve rejected",
            ),
        }
    }
}

fn child_report_store_error(error: crate::mailbox::MailboxError) -> MailboxBootstrapError {
    match error {
        crate::mailbox::MailboxError::InvalidRecord
        | crate::mailbox::MailboxError::ConflictingDuplicate => {
            MailboxBootstrapError::InvalidRequest
        }
        _ => MailboxBootstrapError::GrantMissing,
    }
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{ErrorResponse, Method, Request, SuccessResponse};
    use crate::app::Mode;
    use crate::config::Config;
    use crate::detect::Agent;
    use crate::events::AppEvent;
    use crate::mailbox::RecipientKey;
    use crate::mailbox_v1::{ClaimRequest, Resolve, ResolveOutcome, Submit, PROTOCOL};
    use crate::workspace::Workspace;

    use super::*;

    fn sender_directory() -> std::path::PathBuf {
        // Parallel tests may read the same clock value; the process-wide
        // counter keeps every test directory distinct.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "herdr-active-offline-mailbox-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ))
    }

    fn submit(
        caller: String,
        grant_id: String,
        recipient: RecipientKey,
        delivery_digest: String,
    ) -> MailboxOfflineSubmitParams {
        MailboxOfflineSubmitParams {
            caller,
            grant_id,
            recipient,
            submit: Submit {
                protocol: PROTOCOL.into(),
                stable_id: "stable-1".into(),
                revision: 1,
                digest: "a".repeat(64),
                delivery_digest,
                subject: "offline subject".into(),
                body: "offline body".into(),
                message_id: "message-1".into(),
                kind: "advisory".into(),
                priority: "normal".into(),
                original_sequence: 1,
            },
        }
    }

    fn app_with_active_sender() -> (App, crate::layout::PaneId, String, std::path::PathBuf) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("sender")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("sender pane");
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .expect("sender terminal")
            .clone();
        let directory = sender_directory();
        app.sender_authority_dir = directory.clone();
        let store = crate::sender_authority::SenderAuthorityStore::for_sender(
            &directory,
            &terminal_id.to_string(),
        )
        .expect("sender authority store");
        store
            .cas(
                None,
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: terminal_id.to_string(),
                    process_generation: 1,
                    phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                    transition_revision: 1,
                },
            )
            .expect("persist preparing sender");
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("terminal state");
        terminal.begin_managed_agent(
            "sender".into(),
            Agent::Pi,
            std::time::Instant::now(),
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(30),
        );
        terminal.set_managed_agent_generation(1);
        app.handle_internal_event(AppEvent::AgentProcessDetected {
            pane_id,
            agent: Agent::Pi,
            process_generation: 1,
            observed_at: std::time::Instant::now(),
        });
        (app, pane_id, terminal_id.to_string(), directory)
    }

    fn active_recipient(sender_key: &str) -> RecipientKey {
        RecipientKey {
            recipient_id: sender_key.into(),
            generation: "1".into(),
        }
    }

    fn active_submit(sender_key: String, delivery_digest: String) -> MailboxOfflineSubmitParams {
        submit(
            sender_key.clone(),
            format!("offline:{sender_key}:1"),
            active_recipient(&sender_key),
            delivery_digest,
        )
    }

    fn active_claim(sender_key: String) -> crate::api::schema::MailboxClaimParams {
        crate::api::schema::MailboxClaimParams {
            caller: sender_key.clone(),
            grant_id: format!("offline:{sender_key}:1"),
            recipient: active_recipient(&sender_key),
            claim: ClaimRequest {
                protocol: PROTOCOL.into(),
            },
        }
    }

    fn mailbox_snapshot(
        caller: String,
        grant_id: String,
        recipient: RecipientKey,
    ) -> crate::api::schema::MailboxSnapshotParams {
        crate::api::schema::MailboxSnapshotParams {
            caller,
            grant_id,
            recipient,
            protocol: PROTOCOL.into(),
        }
    }

    fn active_edit(
        sender_key: String,
        revision: u64,
        digest: String,
        subject: &str,
        body: &str,
    ) -> crate::api::schema::MailboxEditParams {
        crate::api::schema::MailboxEditParams {
            caller: sender_key.clone(),
            grant_id: format!("offline:{sender_key}:1"),
            recipient: active_recipient(&sender_key),
            edit: crate::mailbox_v1::Edit {
                protocol: PROTOCOL.into(),
                stable_id: "stable-1".into(),
                revision,
                digest,
                subject: subject.into(),
                body: body.into(),
            },
        }
    }

    fn active_resolve(
        sender_key: String,
        claim_id: String,
        outcome: ResolveOutcome,
    ) -> crate::api::schema::MailboxResolveParams {
        crate::api::schema::MailboxResolveParams {
            caller: sender_key.clone(),
            grant_id: format!("offline:{sender_key}:1"),
            recipient: active_recipient(&sender_key),
            resolve: Resolve {
                protocol: PROTOCOL.into(),
                claim_id,
                outcome,
            },
        }
    }

    #[test]
    fn mailbox_authority_promotes_active_and_installs_generation_bound_capability() {
        let (mut app, _pane_id, sender_key, directory) = app_with_active_sender();
        let sender_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender_key)
                .expect("sender authority store");
        assert_eq!(
            sender_store.load().expect("read sender authority"),
            Some(crate::sender_authority::SenderAuthorityRecord {
                sender_key: sender_key.clone(),
                process_generation: 1,
                phase: crate::sender_authority::SenderAuthorityPhase::Active,
                transition_revision: 2,
            })
        );
        let response = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key, "b".repeat(64))),
        });
        let success: SuccessResponse = serde_json::from_str(&response).expect("success response");
        assert!(matches!(
            success.result,
            ResponseResult::MailboxOfflineSubmitted { .. }
        ));
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn mailbox_authority_rejects_replay_and_replaced_generation() {
        let (mut app, _pane_id, sender_key, directory) = app_with_active_sender();
        let first = active_submit(sender_key.clone(), "b".repeat(64));
        let first_response = app.handle_api_request(Request {
            id: "first".into(),
            method: Method::MailboxOfflineSubmit(first.clone()),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&first_response).is_ok());
        let replay_response = app.handle_api_request(Request {
            id: "replay".into(),
            method: Method::MailboxOfflineSubmit(first),
        });
        let replay: ErrorResponse = serde_json::from_str(&replay_response).expect("replay error");
        assert_eq!(replay.error.code, "mailbox_replay_rejected");

        let sender_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender_key)
                .expect("sender authority store");
        sender_store
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender_key.clone(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .expect("replace sender generation");
        let stale_response = app.handle_api_request(Request {
            id: "stale".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key, "c".repeat(64))),
        });
        let stale: ErrorResponse = serde_json::from_str(&stale_response).expect("stale error");
        assert_eq!(stale.error.code, "mailbox_authority_unavailable");
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn stale_pane_died_after_replacement_keeps_current_authority_and_claim() {
        let (mut app, pane_id, sender_key, directory) = app_with_active_sender();
        let submitted = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key.clone(), "c".repeat(64))),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&submitted).is_ok());
        let claimed = app.handle_api_request(Request {
            id: "claim".into(),
            method: Method::MailboxClaim(active_claim(sender_key.clone())),
        });
        let claimed: SuccessResponse = serde_json::from_str(&claimed).expect("claim response");
        let ResponseResult::MailboxClaimed { claim: Some(claim) } = claimed.result else {
            panic!("expected durable claim")
        };

        let sender_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender_key)
                .expect("sender authority store");
        sender_store
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender_key.clone(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .expect("replace active sender generation");
        app.state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == sender_key)
            .expect("sender terminal")
            .set_managed_agent_generation(2);
        app.install_offline_mailbox_authority(
            sender_store
                .load()
                .expect("read replacement")
                .expect("active sender"),
        )
        .expect("install replacement authority");

        let lifecycle_sequence = app.event_hub.current_sequence();
        let terminal = app
            .state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == sender_key)
            .expect("sender terminal");
        terminal.set_detected_state(Some(Agent::Pi), crate::detect::AgentState::Working);
        terminal.respawn_shell_on_exit = true;

        // This event was queued by generation 1 before generation 2 became
        // Active; it must not revoke the replacement authority or affect the
        // live generation-2 pane lifecycle.
        app.handle_internal_event(AppEvent::PaneDied {
            pane_id,
            process_generation: Some(1),
        });

        assert!(
            app.find_pane(pane_id).is_some(),
            "stale exit must not close pane"
        );
        let terminal = app
            .state
            .terminals
            .values()
            .find(|terminal| terminal.id.to_string() == sender_key)
            .expect("sender terminal");
        assert!(terminal.accepts_managed_agent_generation(2));
        assert_eq!(terminal.state, crate::detect::AgentState::Working);
        assert!(
            terminal.respawn_shell_on_exit,
            "stale exit must not consume respawn state"
        );
        let lifecycle_events = app.event_hub.events_after(lifecycle_sequence);
        assert!(!lifecycle_events
            .iter()
            .any(|(_, event)| matches!(event.event, crate::api::schema::EventKind::PaneExited)));
        assert!(!lifecycle_events.iter().any(|(_, event)| matches!(
            event.data,
            crate::api::schema::EventData::PaneAgentDetected { released: true, .. }
        )));
        assert!(app
            .offline_mailbox_authority_current(&sender_key)
            .expect("read current authority"));
        let replay = app.handle_api_request(Request {
            id: "claim-after-stale-exit".into(),
            method: Method::MailboxClaim(crate::api::schema::MailboxClaimParams {
                caller: sender_key.clone(),
                grant_id: format!("offline:{sender_key}:2"),
                recipient: active_recipient(&sender_key),
                claim: ClaimRequest {
                    protocol: PROTOCOL.into(),
                },
            }),
        });
        let replay: SuccessResponse = serde_json::from_str(&replay).expect("replay claim response");
        assert_eq!(
            replay.result,
            ResponseResult::MailboxClaimed { claim: Some(claim) }
        );
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn mailbox_authority_exit_invalidates_exact_active_generation() {
        let (mut app, pane_id, sender_key, directory) = app_with_active_sender();
        app.handle_internal_event(AppEvent::PaneDied {
            pane_id,
            process_generation: Some(1),
        });
        let sender_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender_key)
                .expect("sender authority store");
        assert_eq!(
            sender_store
                .load()
                .expect("read sender authority")
                .expect("sender record")
                .phase,
            crate::sender_authority::SenderAuthorityPhase::Invalidated
        );
        let response = app.handle_api_request(Request {
            id: "after-exit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key, "d".repeat(64))),
        });
        let error: ErrorResponse = serde_json::from_str(&response).expect("exit error");
        assert_eq!(error.error.code, "mailbox_authority_unavailable");
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn mailbox_edit_cas_returns_a_fsynced_authoritative_refreshed_head() {
        let (mut app, _pane_id, sender_key, directory) = app_with_active_sender();
        let submitted = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key.clone(), "e".repeat(64))),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&submitted).is_ok());
        let edited = app.handle_api_request(Request {
            id: "edit".into(),
            method: Method::MailboxEdit(active_edit(
                sender_key.clone(),
                1,
                "a".repeat(64),
                "edited subject",
                "edited body",
            )),
        });
        let edited: SuccessResponse = serde_json::from_str(&edited).expect("edit response");
        let ResponseResult::MailboxEdited { snapshot } = edited.result else {
            panic!("expected authoritative edit snapshot")
        };
        assert_eq!(snapshot.heads.len(), 1);
        let head = &snapshot.heads[0];
        assert_eq!(head.revision, 2);
        assert_ne!(head.digest, "a".repeat(64));
        assert_eq!(head.subject, "edited subject");
        assert_eq!(head.body, "edited body");
        assert_eq!(head.sender, sender_key);
        assert_eq!(head.target, sender_key);
        assert_eq!(head.grant_id, format!("offline:{sender_key}:1"));
        assert_eq!(head.message_id, "message-1");
        let durable = crate::mailbox::MailboxStore::open(&directory)
            .expect("open durable mailbox")
            .load()
            .expect("reload durable mailbox");
        assert_eq!(durable.heads["stable-1"], *head);
        // F3: the edited revision carries its own exact admitted receipt, so a
        // recipient join and the settled-history view accept it.
        assert_eq!(
            snapshot.receipts,
            vec![crate::mailbox::AdmissionReceipt {
                delivery_digest: head.delivery_digest.clone(),
                stable_id: head.stable_id.clone(),
                revision: 2,
                digest: head.digest.clone(),
                status: crate::mailbox::ReceiptStatus::Admitted,
            }]
        );
        assert_eq!(durable.receipts[&head.delivery_digest].revision, 2);
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    /// Authority scope on the API path: a recipient edits only its own
    /// inbox; a sender edits only heads it sent to the named recipient; a
    /// send grant never claims, resolves or reads the recipient's messages.
    /// Every refusal has a distinct code and changes nothing.
    #[test]
    fn mailbox_api_never_touches_heads_outside_the_callers_scope() {
        let (mut app, _pane_id, x, directory) = app_with_active_sender();
        let y = RecipientKey {
            recipient_id: "term_y".into(),
            generation: "1".into(),
        };
        let w = RecipientKey {
            recipient_id: "term_w".into(),
            generation: "1".into(),
        };
        let store = crate::mailbox::MailboxStore::open(&directory).unwrap();
        // Z's message to Y, and Y's message to W: neither involves X.
        store
            .append_offline_head(crate::app::api::messages_tests::scoped_test_head(
                "z-to-y", "term_z", &y, 'b',
            ))
            .unwrap();
        store
            .append_offline_head(crate::app::api::messages_tests::scoped_test_head(
                "y-to-w", "term_y", &w, 'c',
            ))
            .unwrap();
        let before = store.load().unwrap();
        let edit_with = |grant: &str, recipient: &RecipientKey, stable_id: &str, digest: char| {
            Method::MailboxEdit(crate::api::schema::MailboxEditParams {
                caller: x.clone(),
                grant_id: grant.into(),
                recipient: recipient.clone(),
                edit: crate::mailbox_v1::Edit {
                    protocol: PROTOCOL.into(),
                    stable_id: stable_id.into(),
                    revision: 1,
                    digest: digest.to_string().repeat(64),
                    subject: "tampered".into(),
                    body: "tampered".into(),
                },
            })
        };
        let code = |app: &mut App, method: Method| {
            let response = app.handle_api_request(Request {
                id: "scope".into(),
                method,
            });
            serde_json::from_str::<ErrorResponse>(&response)
                .unwrap_or_else(|_| panic!("expected a refusal: {response}"))
                .error
                .code
        };
        let own = format!("offline:{x}:1");
        let own_recipient = active_recipient(&x);
        // X, through its own inbox grant, edits a head addressed to Y.
        assert_eq!(
            code(&mut app, edit_with(&own, &own_recipient, "z-to-y", 'b')),
            "mailbox_head_out_of_scope"
        );
        // X edits a head Y sent (to W).
        assert_eq!(
            code(&mut app, edit_with(&own, &own_recipient, "y-to-w", 'c')),
            "mailbox_head_out_of_scope"
        );
        // With a send grant to Y, X still cannot edit Z's message to Y.
        let to_y = app
            .provision_cross_recipient_mailbox_grant(&x, y.clone())
            .unwrap();
        assert_eq!(
            code(&mut app, edit_with(&to_y, &y, "z-to-y", 'b')),
            "mailbox_head_out_of_scope"
        );
        assert_eq!(
            store.load().unwrap().heads,
            before.heads,
            "no refused edit changed anything"
        );
        // ...nor claim, resolve or read Y's inbox through the send grant.
        assert_eq!(
            code(
                &mut app,
                Method::MailboxClaim(crate::api::schema::MailboxClaimParams {
                    caller: x.clone(),
                    grant_id: to_y.clone(),
                    recipient: y.clone(),
                    claim: ClaimRequest {
                        protocol: PROTOCOL.into(),
                    },
                })
            ),
            "mailbox_not_recipient"
        );
        assert!(store.load().unwrap().claims.is_empty());
        assert_eq!(
            code(
                &mut app,
                Method::MailboxResolve(crate::api::schema::MailboxResolveParams {
                    caller: x.clone(),
                    grant_id: to_y.clone(),
                    recipient: y.clone(),
                    resolve: Resolve {
                        protocol: PROTOCOL.into(),
                        claim_id: "any".into(),
                        outcome: ResolveOutcome::Settled,
                    },
                })
            ),
            "mailbox_not_recipient"
        );
        // X's own message to Y is editable through the send grant, and the
        // send-grant view shows only X's own messages.
        let submitted = app.handle_api_request(Request {
            id: "x-to-y".into(),
            method: Method::MailboxOfflineSubmit(submit(
                x.clone(),
                to_y.clone(),
                y.clone(),
                "9".repeat(64),
            )),
        });
        assert!(
            serde_json::from_str::<SuccessResponse>(&submitted).is_ok(),
            "{submitted}"
        );
        let edited = app.handle_api_request(Request {
            id: "edit-own".into(),
            method: edit_with(&to_y, &y, "stable-1", 'a'),
        });
        let edited: SuccessResponse = serde_json::from_str(&edited).expect("own edit");
        let ResponseResult::MailboxEdited { snapshot } = edited.result else {
            panic!("edited")
        };
        assert_eq!(
            snapshot
                .heads
                .iter()
                .map(|head| head.stable_id.as_str())
                .collect::<Vec<_>>(),
            ["stable-1"],
            "the send-grant view hides Z's message to Y"
        );
        assert_eq!(snapshot.heads[0].revision, 2);
        drop(app);
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn mailbox_edit_rejects_stale_exact_version() {
        let (mut app, _pane_id, sender_key, directory) = app_with_active_sender();
        let submitted = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key.clone(), "1".repeat(64))),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&submitted).is_ok());
        let first = app.handle_api_request(Request {
            id: "first-edit".into(),
            method: Method::MailboxEdit(active_edit(
                sender_key.clone(),
                1,
                "a".repeat(64),
                "subject two",
                "body two",
            )),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&first).is_ok());
        let stale = app.handle_api_request(Request {
            id: "stale-edit".into(),
            method: Method::MailboxEdit(active_edit(
                sender_key,
                1,
                "a".repeat(64),
                "stale subject",
                "stale body",
            )),
        });
        let stale: ErrorResponse = serde_json::from_str(&stale).expect("stale edit error");
        assert_eq!(stale.error.code, "mailbox_edit_conflict");
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn mailbox_edit_rejects_post_claim_head() {
        let (mut app, _pane_id, sender_key, directory) = app_with_active_sender();
        let submitted = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key.clone(), "2".repeat(64))),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&submitted).is_ok());
        let claimed = app.handle_api_request(Request {
            id: "claim".into(),
            method: Method::MailboxClaim(active_claim(sender_key.clone())),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&claimed).is_ok());
        let rejected = app.handle_api_request(Request {
            id: "claimed-edit".into(),
            method: Method::MailboxEdit(active_edit(
                sender_key,
                1,
                "a".repeat(64),
                "late subject",
                "late body",
            )),
        });
        let rejected: ErrorResponse = serde_json::from_str(&rejected).expect("claimed edit error");
        assert_eq!(rejected.error.code, "mailbox_edit_claimed");
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn admitted_high_claim_remains_outstanding_until_settled_before_normal_claim() {
        let (mut app, _pane_id, sender, directory) = app_with_active_sender();
        for (stable_id, priority, digest) in [
            ("a-high", "high", "a".repeat(64)),
            ("z-normal", "normal", "b".repeat(64)),
        ] {
            let mut submit = active_submit(sender.clone(), digest);
            submit.submit.stable_id = stable_id.into();
            submit.submit.priority = priority.into();
            let response = app.handle_api_request(Request {
                id: stable_id.into(),
                method: Method::MailboxOfflineSubmit(submit),
            });
            assert!(serde_json::from_str::<SuccessResponse>(&response).is_ok());
        }
        let claim = |app: &mut App, id: &str| {
            let response = app.handle_api_request(Request {
                id: id.into(),
                method: Method::MailboxClaim(active_claim(sender.clone())),
            });
            let response: SuccessResponse = serde_json::from_str(&response).unwrap();
            let ResponseResult::MailboxClaimed { claim: Some(claim) } = response.result else {
                panic!("expected claim")
            };
            claim
        };
        let high = claim(&mut app, "high");
        assert_eq!(high.stable_id, "a-high");
        let resolve = |app: &mut App, outcome| {
            app.handle_api_request(Request {
                id: "resolve".into(),
                method: Method::MailboxResolve(active_resolve(
                    sender.clone(),
                    high.claim_id.clone(),
                    outcome,
                )),
            })
        };
        let admitted = resolve(&mut app, ResolveOutcome::Admitted);
        assert!(serde_json::from_str::<SuccessResponse>(&admitted).is_ok());
        assert_eq!(claim(&mut app, "after-admitted"), high);
        let snapshot = |app: &mut App| {
            let response = app.handle_api_request(Request {
                id: "snapshot".into(),
                method: Method::MailboxSnapshot(mailbox_snapshot(
                    sender.clone(),
                    format!("offline:{sender}:1"),
                    active_recipient(&sender),
                )),
            });
            let response: SuccessResponse = serde_json::from_str(&response).unwrap();
            let ResponseResult::MailboxSnapshot { snapshot } = response.result else {
                panic!("expected snapshot")
            };
            snapshot
        };
        let admitted_snapshot = snapshot(&mut app);
        assert_eq!(admitted_snapshot.claim, Some(high.clone()));
        assert_eq!(
            admitted_snapshot.head_states[0].lifecycle,
            crate::mailbox_v1::HeadLifecycle::Admitted
        );
        let settled = resolve(&mut app, ResolveOutcome::Settled);
        assert!(
            serde_json::from_str::<SuccessResponse>(&settled).is_ok(),
            "{settled}"
        );
        let settled_snapshot = snapshot(&mut app);
        assert_eq!(settled_snapshot.claim, None);
        assert_eq!(settled_snapshot.heads.len(), 2);
        assert_eq!(settled_snapshot.receipts.len(), 2);
        let wire = serde_json::to_value(&settled_snapshot).unwrap();
        assert_eq!(wire["headStates"][0]["stableId"], "a-high");
        assert_eq!(wire["headStates"][0]["claimId"], high.claim_id);
        assert_eq!(wire["headStates"][0]["lifecycle"], "settled");
        assert_eq!(wire["headStates"][1]["stableId"], "z-normal");
        assert_eq!(wire["headStates"][1]["lifecycle"], "held");
        assert!(wire["headStates"][1].get("claimId").is_none());
        let normal = claim(&mut app, "after-settled");
        assert_eq!(normal.stable_id, "z-normal");
        assert_eq!(snapshot(&mut app).claim, Some(normal.clone()));
        assert_eq!(claim(&mut app, "normal-replay"), normal);
        let backwards: ErrorResponse =
            serde_json::from_str(&resolve(&mut app, ResolveOutcome::Admitted)).unwrap();
        assert_eq!(backwards.error.code, "mailbox_store_failed");
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    fn generation_claim(
        sender_key: &str,
        generation: u64,
    ) -> crate::api::schema::MailboxClaimParams {
        crate::api::schema::MailboxClaimParams {
            caller: sender_key.into(),
            grant_id: format!("offline:{sender_key}:{generation}"),
            recipient: active_recipient(sender_key),
            claim: ClaimRequest {
                protocol: PROTOCOL.into(),
            },
        }
    }

    /// Herdr sleep → queued head → wake: the woken Pi runs in the same pane and
    /// terminal (the same mailbox recipient), a second append coalesces, and the
    /// queued head is claimed exactly once by the woken generation.
    #[tokio::test]
    async fn slept_pane_wakes_once_and_the_same_terminal_claims_the_queued_head_once() {
        let (mut app, pane_id, sender_key, directory) = app_with_active_sender();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .unwrap()
            .clone();
        let (runtime, mut input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal_id.clone(), runtime);
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .launch_recipe = crate::launch_recipe::LaunchRecipe::capture("sender", "pi", &[], &[]);
        let submitted = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key.clone(), "e".repeat(64))),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&submitted).is_ok());
        let public = app.public_pane_id(0, pane_id).unwrap();

        let slept = app.handle_api_request(Request {
            id: "sleep".into(),
            method: Method::AgentSleep(crate::api::schema::AgentTarget {
                target: public.clone(),
            }),
        });
        assert!(
            serde_json::from_str::<SuccessResponse>(&slept).is_ok(),
            "{slept}"
        );
        assert!(app.state.terminals[&terminal_id].sleep.is_some());
        assert_eq!(
            input.try_recv().unwrap().as_ref(),
            b"\x04",
            "guarded ctrl+d"
        );
        // The Pi exits back to the shell.
        app.handle_internal_event(AppEvent::StateChanged {
            pane_id,
            agent: None,
            state: crate::detect::AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: true,
            observed_at: std::time::Instant::now(),
        });

        let trigger = || crate::app::wake::WakeTrigger {
            cause: crate::app::wake::WakeCause::HeadAppended,
            recipient_id: sender_key.clone(),
            head_id: "stable-1".into(),
        };
        let outcome = app.wake_pane(&public, trigger());
        let crate::app::wake::WakeOutcome::Started {
            wake_id,
            generation,
        } = outcome
        else {
            panic!("the slept pane wakes: {outcome:?}")
        };
        assert_eq!(generation, 2);
        assert_eq!(
            app.wake_pane(&public, trigger()),
            crate::app::wake::WakeOutcome::Duplicate {
                wake_id: wake_id.clone()
            },
            "a second head coalesces into the outstanding wake"
        );
        let launched = input.try_recv().expect("one launch command");
        assert!(String::from_utf8_lossy(&launched).contains("pi"));
        assert!(input.try_recv().is_err(), "exactly one launch");
        assert!(app.pane_wake_record_exists(&wake_id));
        assert_eq!(
            app.state.workspaces[0].terminal_id(pane_id),
            Some(&terminal_id),
            "same pane, same terminal"
        );

        // The woken Pi attaches: the wake resolves and the sleep ends.
        app.handle_internal_event(AppEvent::AgentProcessDetected {
            pane_id,
            agent: Agent::Pi,
            process_generation: 2,
            observed_at: std::time::Instant::now(),
        });
        assert!(app.pane_wakes.is_empty());
        assert!(app.state.terminals[&terminal_id].sleep.is_none());

        let claim = |app: &mut App, id: &str| {
            let response = app.handle_api_request(Request {
                id: id.into(),
                method: Method::MailboxClaim(generation_claim(&sender_key, 2)),
            });
            let response: SuccessResponse = serde_json::from_str(&response).expect("claim");
            let ResponseResult::MailboxClaimed { claim } = response.result else {
                panic!("claim result")
            };
            claim
        };
        let first = claim(&mut app, "claim").expect("the woken generation claims the head");
        assert_eq!(first.stable_id, "stable-1");
        let resolved = app.handle_api_request(Request {
            id: "resolve".into(),
            method: Method::MailboxResolve(crate::api::schema::MailboxResolveParams {
                caller: sender_key.clone(),
                grant_id: format!("offline:{sender_key}:2"),
                recipient: active_recipient(&sender_key),
                resolve: Resolve {
                    protocol: PROTOCOL.into(),
                    claim_id: first.claim_id,
                    outcome: ResolveOutcome::Settled,
                },
            }),
        });
        assert!(
            serde_json::from_str::<SuccessResponse>(&resolved).is_ok(),
            "{resolved}"
        );
        assert_eq!(claim(&mut app, "claim-again"), None, "claimed exactly once");
        assert!(
            matches!(
                app.wake_pane(&public, trigger()),
                crate::app::wake::WakeOutcome::Refused {
                    reason: crate::app::wake::WakeRefusal::NotSleeping,
                    ..
                }
            ),
            "awake again: no further wake"
        );
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn mailbox_claim_replay_returns_one_durable_claim_and_resolve_is_idempotent() {
        let (mut app, _pane_id, sender_key, directory) = app_with_active_sender();
        let submitted = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key.clone(), "e".repeat(64))),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&submitted).is_ok());
        let first = app.handle_api_request(Request {
            id: "claim-first".into(),
            method: Method::MailboxClaim(active_claim(sender_key.clone())),
        });
        let first: SuccessResponse = serde_json::from_str(&first).expect("claim response");
        let ResponseResult::MailboxClaimed { claim: Some(claim) } = first.result else {
            panic!("expected durable claim")
        };
        let replay = app.handle_api_request(Request {
            id: "claim-replay".into(),
            method: Method::MailboxClaim(active_claim(sender_key.clone())),
        });
        let replay: SuccessResponse = serde_json::from_str(&replay).expect("replay claim response");
        assert_eq!(
            replay.result,
            ResponseResult::MailboxClaimed {
                claim: Some(claim.clone())
            }
        );
        let resolved = app.handle_api_request(Request {
            id: "resolve".into(),
            method: Method::MailboxResolve(active_resolve(
                sender_key.clone(),
                claim.claim_id.clone(),
                ResolveOutcome::Settled,
            )),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&resolved).is_ok());
        let resolve_replay = app.handle_api_request(Request {
            id: "resolve-replay".into(),
            method: Method::MailboxResolve(active_resolve(
                sender_key,
                claim.claim_id,
                ResolveOutcome::Settled,
            )),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&resolve_replay).is_ok());
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn mailbox_claim_and_resolve_fail_closed_after_sender_replacement_or_recovery() {
        let (mut app, _pane_id, sender_key, directory) = app_with_active_sender();
        let submitted = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key.clone(), "f".repeat(64))),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&submitted).is_ok());
        let sender_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender_key)
                .expect("sender authority store");
        sender_store
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender_key.clone(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .expect("replace sender generation");
        let stale = app.handle_api_request(Request {
            id: "stale-claim".into(),
            method: Method::MailboxClaim(active_claim(sender_key.clone())),
        });
        let stale: ErrorResponse = serde_json::from_str(&stale).expect("stale claim error");
        assert_eq!(stale.error.code, "mailbox_authority_unavailable");
        sender_store
            .cas(
                Some(3),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender_key.clone(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                    transition_revision: 4,
                },
            )
            .expect("persist unconfirmed replacement");
        sender_store.recover().expect("recover replacement");
        let recovered = app.handle_api_request(Request {
            id: "recovered-resolve".into(),
            method: Method::MailboxResolve(active_resolve(
                sender_key,
                "claim-unavailable".into(),
                ResolveOutcome::Settled,
            )),
        });
        let recovered: ErrorResponse = serde_json::from_str(&recovered).expect("recovery error");
        assert_eq!(recovered.error.code, "mailbox_authority_unavailable");
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    fn install_committed_active(app: &mut App, sender_key: &str, generation: u64) {
        let store = crate::sender_authority::SenderAuthorityStore::for_sender(
            &app.sender_authority_dir,
            sender_key,
        )
        .expect("authority store");
        store
            .cas(
                None,
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender_key.into(),
                    process_generation: generation,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 1,
                },
            )
            .expect("committed active record");
        app.install_offline_mailbox_authority(
            store.load().expect("read active").expect("active record"),
        )
        .expect("install active authority");
    }

    /// Defense in depth: a report head for another recipient (a parent over a
    /// provisioned cross grant) is never admitted by the generic submit path;
    /// only report_submit_parent on a ready bound route may admit one.
    /// The offline-submit wake decision uses the typed result: an admitted
    /// head for a slept pane wakes it; a refused submit does not.
    #[tokio::test]
    async fn offline_submit_to_a_slept_pane_wakes_it_only_when_admitted() {
        let (mut app, _pane_id, sender_a, directory) = app_with_active_sender();
        app.state.workspaces.push(Workspace::test_new("slept"));
        app.state.ensure_test_terminals();
        let pane = app.state.workspaces[1].tabs[0].root_pane.unwrap();
        let terminal = app.state.workspaces[1].terminal_id(pane).unwrap().clone();
        let (runtime, mut input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal.clone(), runtime);
        {
            let state = app.state.terminals.get_mut(&terminal).unwrap();
            state.launch_recipe =
                crate::launch_recipe::LaunchRecipe::capture("slept", "pi", &[], &[]);
            state.sleep = Some(App::new_pane_sleep("slept".into(), 1));
        }
        let recipient = RecipientKey {
            recipient_id: terminal.to_string(),
            generation: "1".into(),
        };
        let grant_id = app
            .provision_cross_recipient_mailbox_grant(&sender_a, recipient.clone())
            .expect("grant");
        let mut refused = submit(
            sender_a.clone(),
            grant_id.clone(),
            recipient.clone(),
            "a".repeat(64),
        );
        refused.submit.kind = "report".into();
        let response = app.handle_api_request(Request {
            id: "refused".into(),
            method: Method::MailboxOfflineSubmit(refused),
        });
        assert!(
            serde_json::from_str::<ErrorResponse>(&response).is_ok(),
            "{response}"
        );
        assert!(app.pane_wakes.is_empty(), "a refused submit wakes nothing");
        let response = app.handle_api_request(Request {
            id: "admitted".into(),
            method: Method::MailboxOfflineSubmit(submit(
                sender_a,
                grant_id,
                recipient,
                "b".repeat(64),
            )),
        });
        assert!(
            serde_json::from_str::<SuccessResponse>(&response).is_ok(),
            "{response}"
        );
        assert_eq!(app.pane_wakes[&terminal].trigger.head_id, "stable-1");
        assert!(input.try_recv().is_ok(), "one launch");
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn generic_report_submit_over_a_cross_grant_is_refused_without_a_head() {
        let (mut app, _pane_id, sender_a, directory) = app_with_active_sender();
        let parent = RecipientKey {
            recipient_id: "parent-b".into(),
            generation: "1".into(),
        };
        let grant_id = app
            .provision_cross_recipient_mailbox_grant(&sender_a, parent.clone())
            .expect("server provisioned A to B grant");
        let mut report = submit(
            sender_a.clone(),
            grant_id.clone(),
            parent.clone(),
            "a".repeat(64),
        );
        report.submit.kind = "report".into();
        let refused = app.handle_api_request(Request {
            id: "cross-report".into(),
            method: Method::MailboxOfflineSubmit(report),
        });
        let refused: ErrorResponse = serde_json::from_str(&refused).expect("refused");
        assert_eq!(refused.error.code, "report_route_required");
        let recovered = crate::mailbox::MailboxStore::open(&directory)
            .unwrap()
            .load()
            .unwrap();
        assert!(recovered.heads.is_empty(), "no head was admitted");
        // The same grant still carries ordinary messages.
        let ordinary = app.handle_api_request(Request {
            id: "cross-advisory".into(),
            method: Method::MailboxOfflineSubmit(submit(
                sender_a,
                grant_id,
                parent,
                "c".repeat(64),
            )),
        });
        assert!(
            serde_json::from_str::<SuccessResponse>(&ordinary).is_ok(),
            "{ordinary}"
        );
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn cross_recipient_offline_delivery_survives_fresh_consumer_execution() {
        let (mut app, _pane_id, sender_a, directory) = app_with_active_sender();
        let recipient_b = RecipientKey {
            recipient_id: "recipient-b".into(),
            generation: "1".into(),
        };
        let grant_id = app
            .provision_cross_recipient_mailbox_grant(&sender_a, recipient_b.clone())
            .expect("server provisioned A to B grant");
        let submitted = app.handle_api_request(Request {
            id: "a-to-offline-b".into(),
            method: Method::MailboxOfflineSubmit(submit(
                sender_a.clone(),
                grant_id.clone(),
                recipient_b.clone(),
                "a".repeat(64),
            )),
        });
        let receipt: SuccessResponse = serde_json::from_str(&submitted).expect("receipt");
        let ResponseResult::MailboxOfflineSubmitted { receipt } = receipt.result else {
            panic!("expected durable receipt")
        };
        install_committed_active(&mut app, "recipient-b", 1);
        let snapshot = app.handle_api_request(Request {
            id: "fresh-b-snapshot".into(),
            method: Method::MailboxSnapshot(mailbox_snapshot(
                "recipient-b".into(),
                "offline:recipient-b:1".into(),
                recipient_b.clone(),
            )),
        });
        let snapshot: SuccessResponse = serde_json::from_str(&snapshot).expect("snapshot");
        let ResponseResult::MailboxSnapshot { snapshot } = snapshot.result else {
            panic!("expected B snapshot")
        };
        assert_eq!(snapshot.heads.len(), 1);
        assert_eq!(snapshot.heads[0].subject, "offline subject");
        assert_eq!(snapshot.heads[0].body, "offline body");
        assert_eq!(snapshot.heads[0].recipient_generation, "1");
        assert_eq!(snapshot.heads[0].sender, sender_a);
        assert_eq!(snapshot.heads[0].target, "recipient-b");
        assert_eq!(snapshot.heads[0].grant_id, grant_id);
        assert_eq!(snapshot.heads[0].message_id, "message-1");
        assert_eq!(snapshot.heads[0].kind, "advisory");
        assert_eq!(snapshot.heads[0].priority, "normal");
        assert_eq!(snapshot.heads[0].original_sequence, 1);
        assert!(snapshot.heads[0].enqueue_epoch > 0);
        assert!(snapshot.heads[0].accepted_at > 0);
        assert_eq!(snapshot.receipts, vec![receipt.clone()]);
        let claimed = app.handle_api_request(Request {
            id: "fresh-b-claim".into(),
            method: Method::MailboxClaim(crate::api::schema::MailboxClaimParams {
                caller: "recipient-b".into(),
                grant_id: "offline:recipient-b:1".into(),
                recipient: recipient_b.clone(),
                claim: ClaimRequest {
                    protocol: PROTOCOL.into(),
                },
            }),
        });
        let claimed: SuccessResponse = serde_json::from_str(&claimed).expect("claim");
        let ResponseResult::MailboxClaimed { claim: Some(claim) } = claimed.result else {
            panic!("expected B claim")
        };
        assert_eq!(claim.stable_id, receipt.stable_id);
        assert_eq!(claim.digest, receipt.digest);
        assert_eq!(claim.recipient, recipient_b.clone());
        let claimed_snapshot = app.handle_api_request(Request {
            id: "claimed-snapshot".into(),
            method: Method::MailboxSnapshot(mailbox_snapshot(
                "recipient-b".into(),
                "offline:recipient-b:1".into(),
                recipient_b,
            )),
        });
        let claimed_snapshot: SuccessResponse =
            serde_json::from_str(&claimed_snapshot).expect("claimed snapshot");
        let ResponseResult::MailboxSnapshot { snapshot } = claimed_snapshot.result else {
            panic!("expected claimed snapshot")
        };
        assert_eq!(
            snapshot.claim.as_ref().map(|claim| &claim.digest),
            Some(&receipt.digest)
        );
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn cross_recipient_rejects_unprovisioned_sender_and_stale_consumer_execution() {
        let (mut app, _pane_id, sender_a, directory) = app_with_active_sender();
        let recipient_b = RecipientKey {
            recipient_id: "recipient-b".into(),
            generation: "1".into(),
        };
        let unauthorized = app.handle_api_request(Request {
            id: "unauthorized".into(),
            method: Method::MailboxOfflineSubmit(submit(
                sender_a.clone(),
                "offline:unauthorized:1".into(),
                recipient_b.clone(),
                "b".repeat(64),
            )),
        });
        let unauthorized: ErrorResponse =
            serde_json::from_str(&unauthorized).expect("unauthorized error");
        assert_eq!(unauthorized.error.code, "mailbox_capability_mismatch");
        let grant_id = app
            .provision_cross_recipient_mailbox_grant(&sender_a, recipient_b.clone())
            .expect("server provisioned grant");
        let submitted = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(submit(
                sender_a,
                grant_id,
                recipient_b.clone(),
                "c".repeat(64),
            )),
        });
        assert!(
            serde_json::from_str::<SuccessResponse>(&submitted).is_ok(),
            "{submitted}"
        );
        install_committed_active(&mut app, "recipient-b", 1);
        let b_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, "recipient-b")
                .expect("B authority store");
        b_store
            .cas(
                Some(1),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: "recipient-b".into(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 2,
                },
            )
            .expect("replace B execution");
        let stale = app.handle_api_request(Request {
            id: "stale-b".into(),
            method: Method::MailboxClaim(crate::api::schema::MailboxClaimParams {
                caller: "recipient-b".into(),
                grant_id: "offline:recipient-b:1".into(),
                recipient: recipient_b.clone(),
                claim: ClaimRequest {
                    protocol: PROTOCOL.into(),
                },
            }),
        });
        let stale: ErrorResponse = serde_json::from_str(&stale).expect("stale B error");
        assert_eq!(stale.error.code, "mailbox_authority_unavailable");
        let stale_snapshot = app.handle_api_request(Request {
            id: "stale-b-snapshot".into(),
            method: Method::MailboxSnapshot(mailbox_snapshot(
                "recipient-b".into(),
                "offline:recipient-b:1".into(),
                recipient_b.clone(),
            )),
        });
        let stale_snapshot: ErrorResponse =
            serde_json::from_str(&stale_snapshot).expect("stale snapshot error");
        assert_eq!(stale_snapshot.error.code, "mailbox_authority_unavailable");
        app.install_offline_mailbox_authority(b_store.load().expect("read B").expect("B active"))
            .expect("fresh B authority");
        let fresh = app.handle_api_request(Request {
            id: "fresh-b".into(),
            method: Method::MailboxClaim(crate::api::schema::MailboxClaimParams {
                caller: "recipient-b".into(),
                grant_id: "offline:recipient-b:2".into(),
                recipient: recipient_b,
                claim: ClaimRequest {
                    protocol: PROTOCOL.into(),
                },
            }),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&fresh).is_ok());
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn mailbox_authority_recovery_invalidates_unconfirmed_sender() {
        let directory = sender_directory();
        let store = crate::sender_authority::SenderAuthorityStore::for_sender(&directory, "sender")
            .expect("sender authority store");
        store
            .cas(
                None,
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: "sender".into(),
                    process_generation: 1,
                    phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                    transition_revision: 1,
                },
            )
            .expect("persist preparing sender");
        let recovered = store
            .recover()
            .expect("recover sender record")
            .expect("record remains for audit");
        assert_eq!(
            recovered.phase,
            crate::sender_authority::SenderAuthorityPhase::Invalidated
        );
        assert!(!recovered.authoritative());
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }
}
