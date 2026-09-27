# C2 `child_report_signals`: "child reported done, no report seen yet"

Consumer: the Pi owner (the parent-side reader). Base: `release/rc3`. It is a **fact**, never a claim that a report is missing: Herdr sees only reports sent through the bound report route (`report_prepared` → `report_submit_parent`). The certain-missing work (#159/#145, closure barrier, recovery, #161 wake) stays parked and is not part of this.

## When a fact is open

For each **current ready bound route** whose parent is the caller (parent terminal and process generation match the caller's accepted stream), a fact is open when:

1. the child's latest `todo_state` on this exact route (epoch, generations, sessions) is `done`; and
2. no report sent through this route's bound report path has an `Admitted` receipt at a journal position after the child's latest `not_done` on this route (or anywhere on the route, if it never sent `not_done`).

It clears when such a report is admitted, when the child sends a later `not_done` (reopen), or when the route is revoked or replaced. A report admitted before a reopen does not clear the next `done`. A later edit of an admitted report head does not undo its admission. Other epochs of the same delegation never count, either way.

## Records (none new)

- Inputs are existing mailbox-journal (`mailbox.v1.jsonl`) records: `child_report`/`todo_state`, `child_report`/`prepared_attempt`, `head` (kind `report`) and `receipt`. The journal positions used for ordering are computed in memory while loading; nothing new is written.
- `doneAt` is Herdr's wall clock (unix ms) when it committed the `done` `todo_state`, kept **in memory only**, keyed by delegation, route epoch and journal position. An identical repeat keeps the first time. Bound routes do not survive a server restart, so after a restart the fact is simply absent until the child's route is ready and it reports `done` again.
- **rc2 rollback:** C2 writes no record rc3 doesn't already write, and `child_report.rs` records decode identically in rc2 (c4cbcd03). rc2 has no `childDoneSignals`, so the Pi consumer stays off.

## Wire (the parent's own accepted bootstrap stream)

Descriptor key `childDoneSignals`, on managed sessions only (not `history_only` or `recipient_only`). It is deliberately **not** `parentSignals`: installed Pi d74c3a4 validates `parentSignals` strictly (it requires the #115 `recoveryMethod`/`bindMethod`) and would report "invalid parent signal advertisement" and turn Messages off. Herdr never sends `parentSignals`; an older Pi ignores the new key.

```json
"childDoneSignals": {"method": "child_report_signals", "protocol": "mailbox.v1", "maxWaitMs": 30000}
```

Request: `child_report_signals {protocol, afterCursor?, waitMs?}`. Unknown fields, a wrong protocol, `waitMs > 30000` or a second parked poll on the same connection are `invalid_request`. A forged or stale binding is `grant_revoked`, as is a `history_only` or `recipient_only` session.

- It answers at once unless `afterCursor` equals the current journal marker and `waitMs > 0`. In that case it parks until the journal changes or `waitMs` passes, and is re-authenticated on every event-loop pass (like `mailbox.watch`, which can be parked alongside it).
- A route revocation does not change the journal, so a parked poll reflects it at its deadline at the latest.

Response, a **snapshot** of the currently open facts (a fact missing from a later snapshot has cleared):

```json
{"type": "child_report_signals", "throughCursor": 6743, "truncated": false,
 "signals": [{
   "type": "report_unknown", "reason": "coverage_unqualified",
   "delegationId": "d2", "routeEpoch": "60c0…",
   "doneAt": 1790468769429,
   "child": {"paneId": "w1:p1", "terminalId": "term_…",
             "session": {"agent": "pi", "kind": "path", "source": "herdr:pi", "value": "/…/session.jsonl"}},
   "todo": {"localRoot": "root", "localRevision": 4, "stateDigest": "…", "state": "done", "todoStateCursor": 5}
 }]}
```

- `throughCursor` is an opaque journal marker (the same marker as `mailbox.watch`'s `cursor`). Pass it back as `afterCursor`.
- Signals are ordered by `todoStateCursor`, at most 64; `truncated` says more exist.
- `delegationId` is Herdr's child delegation ID. There is no Pi Todo binding, so Pi maps a signal to its delegated Todo by `child.paneId` plus `child.session`.
- `doneAt` is `null` only if the time is not in memory (a done recorded by an earlier server run on a route that is somehow still current). Treat `null` as "time unknown".

## What it can wrongly claim

- **"No report" while the parent has one.** A report sent by a typed prompt (`herdr agent prompt`, `herdr-structured-prompt`), a handoff to the PTY, or only exported to a file is not a bound report and never clears the fact. Pi's wording: "no report through the report route yet".
- **Cleared is not read.** It clears on admission; the report may still be waiting unclaimed in the parent's inbox.
- **Stale done.** A child that reopens its Todo locally without sending `not_done` keeps the fact open.
- **`doneAt` is when Herdr heard it**, not when the child finished.
- It **never invents a fact** for an unready, replaced or inexact route; loss (restart, revoked route) makes it silently absent.
