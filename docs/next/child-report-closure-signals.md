# #159 covered-child closure barrier and `child_report_signal` — server contract

Consumer: stabilization TPM, Pi owner #115 through the TPM, and the #145 launch seam. Everything below exists only with `[experimental] child_report_signals = true` (default off) **and** a child launch registered through the covered-launch seam. Uncovered children, the gen1 accepted-stream methods and every existing response shape are unchanged; with the flag off the descriptor has no new field and the new methods are `invalid_request`.

## Journal and cursor spaces

Records live in `child-report-closure.v1.jsonl` beside `mailbox.v1.jsonl`, appended under the same exclusive mailbox lock. Closure cursors (the signal `cursor`, `closureCursor`, `recoveryCursor`, decline and wake cursors, bind `cursor`) are strictly increasing and are always minted **above the mailbox cursor current at write time**. `todoStateCursor` is a mailbox-journal cursor. The spaces stay separate files, but the minting rule guarantees Pi's cross-check `todoStateCursor < closureCursor < cursor` and `cursor < recoveryCursor < declineCursor`, `signalCursor < wakeCursor`. An older Herdr ignores this file.

Each write is fsynced and read back exactly; only then is a `commit {first,last}` record appended. Authority-bearing reads (domains, signals, recovery requests, parent Todo bindings) use committed records only. Restrictive reads (freeze, suspension, decline, wake, "is covered") use every record. A readback failure therefore yields no signal and no recovery, but still freezes.

## Covered domain (before any bound work)

The seam calls `register_covered_child_launch(terminal, {policyId, policyHash, receiptDigest, sandboxedBirth})` before the child's route is ready; later registration is refused. When `delegation.route_ready` mints a fresh epoch for that child, the domain is written durably before the call returns (failure withdraws readiness with `coverage_persistence_failed`), so no accepted stream can offer the parent-report path first. The domain binds the full route identity, the managed-launch process birth, the launch birth floor, the session-writer lock identity, the policy and receipt, the enforcement result, the baseline mailbox cursor and the server boot nonce.

**Enforcement must be attested from launch/exec, not from route readiness.** The verifier receives the policy/receipt, the managed-launch birth and the launch floor, and must return an attestation whose `coveredFromBirth` is exactly the managed-launch birth under the same `policyHash`. The receipt's `sandboxedBirth` must also be that birth, and the birth must not precede the floor. Any gap is unqualified. Production uses `UnprovenEnforcementVerifier` (`enforcement_unproven`) until #145 lands, so an installed build emits only `report_unknown(coverage_unqualified)`. Any report activity by the child delegation before the domain existed also makes it unqualified.

A server restart (boot nonce), route/epoch/generation change, child process birth change, writer-lock change or enforcement lapse fails the domain's currency check. At a closure or recovery decision this is appended as a durable, monotonic `domain_suspended`; the domain is never re-armed.

## Closure barrier

After a covered child's `todo_state` **done** ACK has been read back on its current route, in the same dispatch and under the mailbox lock the server: appends `frozen` for that route/root/revision; classifies the mailbox for the child delegation; appends a `closure_barrier` with `throughMailboxCursor`; and, unless the outcome is `admitted`, one `signal`. A repeated done ACK for the same revision is idempotent. Outcomes, in order:

1. domain route ≠ current route → `stale_epoch` (epoch changed) or `route_replaced`;
2. suspended, or currency failure → that reason (`child_process_gone`, `coverage_unqualified`, …);
3. any event of the delegation on another route → `route_replaced`;
4. an exact admitted bound report → barrier `admitted`, **no signal**;
5. domain unverified/unqualified → `coverage_unqualified`;
6. legacy attempt/coverage/bypass on the route, or an orphan receipt → `coverage_unqualified`;
7. a prepared attempt without an exact admitted receipt → `in_flight`;
8. a preparation never attempted → `prepared_not_admitted`;
9. any path attempt → `path_attempt_uncertain`;
10. any other child→parent head → `coverage_unqualified`;
11. otherwise `missing_after_done`, barrier `AllPathsTrusted`, all three counts zero.

The existing mailbox journal still rejects any `AllPathsTrusted` append; the closure barrier is the only mint. `delegation.child_report_disposition` is unchanged.

## Parent surface (parent's own accepted stream)

Every ACK's `type` equals its method name. Descriptor: `parentSignals {method:"child_report_signals", recoveryMethod:"report_recovery_request", bindMethod:"todo_delegation_bind", protocol}`.

- `todo_delegation_bind {protocol, childPaneId, childSession{agent,kind,source,value}, todoDelegationId, parentTaskId}` → `{type, cursor}`. The server resolves the child delegation from `childPaneId` plus the exact `childSession` among the caller's own current ready child routes. None, more than one, or a conflicting rebind is `invalid_request`; an identical rebind returns the same cursor. `todoDelegationId` must be Pi's 22-character URL-safe ID and `parentTaskId` a positive safe integer, so the echo is always decodable. Echoed as `parentTodo {delegationId, parentTaskId}` in later signals for that route epoch; omitted if absent.
- `child_report_signals {protocol, afterCursor, waitMs≤30000}` → `{type, signals[≤64], throughCursor}`. Only signals naming this parent terminal and process generation. `throughCursor` is the last returned cursor when the page is full, else the journal's last cursor (never below `afterCursor`). An empty page with `waitMs>0` parks on the connection (later frames stay buffered) and is re-authenticated on every event-loop pass.
- Signal: `{cursor, type, reason?, route, todo {localRoot, localRevision, stateDigest, state:"done", todoStateCursor}, closure {coverageQualified, closureCursor, pathAttemptCount, preparedCount, admittedReportCount}, parentTodo?, recovery {available}}`. `recovery.available` is recomputed on every read: true only for the child's latest signal, if it is `missing_after_done`, undeclined, unsuspended, the domain is still current and the child's latest TodoState is still that done evidence. After a decline (by the child or a server-settled wake) the parent receives a further `report_unknown` with reason `recovery_declined_<reason>`.
- `report_recovery_request {protocol, signalCursor, routeEpoch, childTerminalId, localRoot, localRevision, stateDigest}` → `{type, signalCursor, recoveryCursor}`. Exact parent only (`grant_revoked`); selectors must match (`invalid_request`); the signal must be recoverable, else `grant_revoked` and any lapse is recorded as a suspension. One request per signal; a repeat returns the same `recoveryCursor`.

## Child surface (covered child's own accepted stream)

`parentReport` of a covered child (a committed domain names its current route) carries `recoveryWaitMethod:"report_recovery_wait"`, `recoveryDeclineMethod:"report_recovery_decline"` and `recoveryWakeMethod:"report_recovery_wake_request"`, always alongside `todoStateMethod`. They are advertised from bootstrap because Pi registers its recovery loop from the descriptor; the methods act only after a done ACK, a barrier, a signal and an exact parent request.

- `report_recovery_wait {protocol, afterCursor, waitMs}` → `{type, requests[≤8]}` of open (undeclined) requests `{signalCursor, recoveryCursor, routeEpoch, localRoot, localRevision, stateDigest}`, parked like the parent poll.
- `report_recovery_decline {protocol, signalCursor, recoveryCursor, reason}` with `no_exported_report | prior_attempt_uncertain | already_admitted | state_mismatch` → `{type, cursor}`. The recovery cursor must match; an identical repeat returns the same cursor, a different reason is `invalid_request`.
- `report_recovery_wake_request {protocol, signalCursor}` → `{type, signalCursor, wakeCursor}` (#161). Only the exact covered child on its current route, only with an open undeclined request for that signal, and only while the signal is still recoverable. The server journals exactly one `recovery_wake_issued` keyed by the signal (fsync, readback, commit), then mints one head in the child's own mailbox: kind `recovery_wake`, subject `System recovery request`, revision 1, priority `normal`, body exactly `{"signalCursor":N,"wakeCursor":M}`, stable ID `recovery-wake-N`. A repeat (including after a restart) returns the same `wakeCursor` and never a second head. If the wake cannot be journaled, minted or verified, the server settles the request through the decline path (`no_exported_report`, server-settled) and the parent receives `report_unknown(recovery_declined_no_exported_report)`.
- Delivery reuses `report_path_attempt` → `report_prepared` → `report_submit_parent`. After the barrier these fail with `egress_frozen` unless the signal has an open (requested, undeclined) recovery; then each may succeed **once** after the request. A new not-done TodoState revision reopens normal bound reporting and makes the old signal non-current.

A covered child's generic `mailbox.offline_submit`, self `report_submit` and `mailbox.provision_recipient` are denied outright. No peer or generic submit may carry kind `recovery_wake` (rejected at request validation and in the store), a wake head is never editable, and none of the accepted-stream methods above decode as a generic API request. Target-only API/PTY paths are closed by the #145 sandbox, not by this server change.

## Covered launch environment

`covered_child_herdr_environment` keeps exactly `HERDR_MAILBOX_BOOTSTRAP_ADDRESS` and `HERDR_AGENT` from Herdr's child environment (a name given twice is dropped). No other `HERDR_*` name, including `HERDR_PANE_ID` and the client/API socket paths, enters the sandbox.

## Open seam

Production has no caller of `register_covered_child_launch` and uses the unproven verifier. Finishing #145 means: launch the child under the pinned sandbox policy with this environment, register the policy/receipt naming the managed-launch birth before route readiness, and replace the verifier with one that attests coverage from that birth. 
