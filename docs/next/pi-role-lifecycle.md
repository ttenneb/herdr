# External Pi role lifecycle

## Frozen verification plan

The owner verification for this change is `python3 -m unittest scripts.test_herdr_role_lifecycle` plus the focused Rust agent/Collection tests. It covers authority-file ownership/mode/symlink/parent checks, immutable task/issuer/route/digest bindings, exact post-start rollback and relaunch inhibition, durable receipts and heartbeats, truthful systemd readiness, bounded restart policy, default-deny `ask_user_question`, and exact helper assignment transport. The TPM consumes this evidence during integration.

The distinct failure-seeking check is `python3 scripts/test_herdr_role_lifecycle.py -v` together with targeted diff inspection for malformed start/transport responses, mismatched cleanup identity, insecure authority material, relative executables, premature `READY=1`, receipt-triggered prompts, unbounded restart, and extension-side supervision. It is intentionally a different static/adversarial path over the same revision, not a repeated broad suite.

## Authority boundary

`herdr_role_lifecycle.py` is an external, one-shot lifecycle manager. A Pi extension may publish status and session identity, but it must never launch, restart, hibernate, or supervise Pi. The manager accepts only an explicit `assignment` or `report` activation artifact. It never watches Gate, mailbox, transport, settlement, status, or Todo records for wake-up signals. Consequently, a Gate or transport receipt cannot start a model turn.

Every role manifest binds all of these fields:

- durable role ID and role class;
- exact Herdr workspace, pane, terminal, working tree, and Pi session reference;
- canonical task reference;
- mailbox path;
- exact authorized issuer/sender and parent report routes, including full session identities;
- absolute Herdr, Pi, Python, `systemd-notify`, and `systemctl` executables;
- the exact owner-only Herdr server Unix socket path;
- durable state and activation paths below `/home` (a different durable root is accepted only for isolated tests);
- an explicit human-facing policy.

`humanFacing` defaults to false. A non-human-facing launch always adds `--exclude-tools ask_user_question`. Enabling it requires a separate owner-only grant file whose SHA-256 digest is pinned by the manifest and whose complete content is exactly bound to the authorized issuer, canonical task, role ID/class, recipient route, and parent report route. Only `user-facing-pm` and `human-facing-controller` role classes are eligible; TPMs, implementation owners, QA roles, and helpers are rejected. This is launch authority only. It is not evidence that a human-facing request was answered.

`herdrSocketPath` is mandatory and must resolve to an existing owner-owned Unix socket with no group/other permissions, a normalized bounded absolute path, and only non-symlink, non-group/world-writable owner/root-controlled parent directories. The rendered unit pins it as `HERDR_SOCKET_PATH`, and the manager overwrites any ambient value with the manifest binding for every Herdr CLI preflight, launch, prompt, poll, rollback, hibernate, and acknowledgement-authority call. User-manager or development defaults therefore cannot redirect a managed role to another Herdr server.

The manifest, activation, prompt, and human-facing grant must be regular non-symlink files owned by the effective user, owner-readable, inaccessible to group/other users, and located beneath owner-controlled non-group/world-writable parent directories under the durable root. The activation is immutable for an execution: it binds the authorized issuer, canonical task, exact sender and parent routes, execution ID, and prompt SHA-256 digest.

## Execution and hibernation

An external actor writes a versioned activation artifact and starts the role's user service. The manager takes a per-role lock, validates and records the pre-start identity, refuses to attach to any already-live process, and starts exactly one Pi under a unique activation-derived managed-agent generation name in the stored terminal/session. It waits for Herdr's actual `interactive_ready` observation, validates the exact generation, terminal, and full session identity, submits the activation prompt to that pane, and records only a Herdr runtime transport receipt. Gate admission, model execution, report acceptance, and Todo acceptance remain `unknown`.

Every parse, identity, readiness, or activation-transport failure after the start attempt invokes `agent send-keys` with atomic expected-terminal and expected-generation guards. The manager then records a durable rollback disposition of `completed`, `failed`, or `uncertain`. A failed or uncertain cleanup writes a durable relaunch inhibit; later service invocations exit without attaching or relaunching until an explicit recovery removes that condition. Terminal activation records likewise prevent automatic replay.

`READY=1` is sent only after exact interactive readiness and prompt transport acceptance. Durable JSON heartbeats and phase receipts are written atomically below the role's state directory. When execution returns to idle/done after a post-submission state change, the manager uses the same exact-identity guard to send `ctrl+d` and waits for that generation to leave. It never force-kills a mismatched or unresponsive process. The service then exits, leaving the durable role but no Pi process.

The manager does not poll while hibernated. Activation is therefore externally driven, including report delivery that needs a parent turn. Mailbox or Gate state alone is never an activation.

## Queued-input auto-release

`schedule-queued-input` implements `QueuedInputActivationRequestV1` outside Pi. It accepts the exact versioned request plus a separately supplied secure exact tasking-issuer route. The request is rejected unless its 128-bit activation ID, batch and item IDs, 1–32 item bound, priority, correlation chain, depth, UTF-8 payload size, payload SHA-256, `accepted_queued_input` cause, authorized issuer, and complete managed-role recipient identity all match policy and the role manifest.

Scheduling writes an owner-only payload, digest-bound explicit `queued_input` activation, and durable record beneath the role state directory. It never calls an editor API, writes raw PTY input, starts Pi, or submits a prompt. Only a later external lifecycle run that names that activation ID can consume the explicit activation. Empty settlement, receipt/import/status/heartbeat/supervisor events do not have the request shape or accepted cause and cannot schedule anything.

The durable key binds activation ID, batch ID, payload digest, and exact recipient. The complete request digest is also retained. Exact retries return `duplicate`; same-ID content changes return `rejected`. An ambiguous materialization remains `uncertain` and normal retries remain uncertain. `recover-queued-input` is the only resolution path: it uses the same request and issuer, verifies the existing activation and payload digests, marks the prior schedule proven, and returns `duplicate` without creating another artifact, process, or prompt.

Before materialization, the scheduler enforces per-role queue depth, an eight-per-minute rate limit, maximum depth eight, at most 32 correlation entries, no repeated namespace/key within a request, and monotonically increasing revisions for recently scheduled namespace/key chains. Lifecycle completion or inhibited rollback releases queue depth without changing the original scheduling receipt. Scheduling receipts claim only `scheduled`, `duplicate`, `uncertain`, or `rejected`; they do not claim Gate admission, model execution, report delivery, or Todo acceptance.

### Exact service-start boundary

Generate the role-specific instance template with:

```sh
/usr/bin/python3 /absolute/herdr_role_lifecycle.py render-queued-unit \
  --manifest /absolute/role.json \
  --manager /absolute/herdr_role_lifecycle.py
```

Install it separately under the exact `# UnitName=...` name on the rendered first line, such as `herdr-role-owner-1@.service`. The template has no `WantedBy` target and no generic wake command. Its only execution path is an explicit instance whose `%i` is passed to `run --activation-id %i`. The manager validates the canonical 128-bit ID and its exact secure scheduled record before launch. Paths are absolute and systemd-quoted; `%` in configured paths is escaped without escaping the intentional `%i` instance specifier.

After `schedule-queued-input` returns `scheduled`, or `recover-queued-input` proves the prior schedule and returns `duplicate`, the trusted tasking adapter may invoke exactly:

```sh
/usr/bin/python3 /absolute/herdr_role_lifecycle.py start-queued-input \
  --manifest /absolute/role.json \
  --activation-id 0123456789abcdef0123456789abcdef
```

The manager revalidates the scheduled activation and then executes exactly `SYSTEMCTL --user start herdr-role-ROLE@ACTIVATION.service`, where `SYSTEMCTL` is the absolute executable pinned by the secure role manifest. Because the unit is `Type=notify`, a successful systemctl return follows the lifecycle manager's truthful `READY=1`, which is still emitted only after interactive readiness and prompt transport. The JSON result is `herdr.queued-input-service-start-result` v1 with the exact activation ID and one of `accepted`, `duplicate`, `uncertain`, or `rejected`; its generated schema is `docs/next/queued-input-service-start-v1.schema.json`.

Before invoking systemctl, the manager durably records an uncertain exact start attempt. A successful acknowledgement becomes `accepted`; another call returns `duplicate` without invoking systemctl again. Timeout or lost acknowledgement remains `uncertain` and repeated calls do not retry. After external same-unit evidence, `recover-queued-start --disposition started` marks the attempt proven and returns `duplicate`; `--disposition not-started` terminates it as rejected and requires a new activation ID. Empty settlement, receipts, imports, status, heartbeat, supervisor observations, invalid IDs, and unscheduled IDs cannot reach systemctl.

## Exact-parent report acknowledgement transport

Herdr does have a production `handoff send` CLI, but it intentionally submits a normal prompt transaction and its receipt is nonterminal. It is therefore prohibited for report acknowledgement. Todo #20 instead uses a separate non-model Unix-socket transport owned by the child tasking adapter.

A role that can receive acknowledgements adds `reportAcknowledgement` to its secure manifest. This pins an owner-controlled socket path, canonical 22-character base64url delegation ID, positive parent task ID, the exact tasking `TaskAssignmentIdentityV1` object, and the exact parent route/session. The endpoint must be a regular Unix socket owned by the effective user with mode `0600`, beneath validated non-symlink/non-writable parents, and within the bounded Unix path length. The connected peer UID is verified with OS credentials. Neither side uses Pi input, an editor, a PTY, Gate admission, model APIs, Todo acceptance, status, heartbeat, or UI state.

The parent transport writes an owner-only request file matching `docs/next/report-parent-acknowledgement-v1.schema.json`, then calls:

```sh
/usr/bin/python3 /absolute/herdr_role_lifecycle.py send-parent-ack \
  --manifest /absolute/child-role.json \
  --request /absolute/parent-ack.json
```

Before delivery, Herdr validates the exact live parent issuer route/session; exact child workspace, pane, terminal, and full Pi session; acknowledgement ID; tasking's canonical 22-character base64url attempt, report, and delegation IDs; parent task; positive sequence; report digest; exact `TaskAssignmentIdentityV1`; parent and acknowledging routes; parent receipt; and timestamp. The assignment has exactly `paneId`, `workspaceId`, `agent`, `agentSession`, and `boundAt`, with optional `assignedByPaneId`. In line with tasking `report-delivery.ts`, required assignment and route strings—including `agentSession.value`—allow up to 512 UTF-8 bytes; only optional `assignedByPaneId` is capped at 128 bytes. Tasking routes have exactly `workspaceId`, `paneId`, `terminalId`, and `agentSession`: `name` and all other extras are rejected. Nested sessions likewise reject additional properties. It persists the complete request and its canonical digest under the child role state before the first socket connection. The child must durably append its authority and confirmed-send records before returning `confirmed`.

Exact retries after an accepted child result return `duplicate` without reconnecting. Same acknowledgement ID with different content is rejected. A timeout, malformed result, or lost parent-side persistence acknowledgement stays `uncertain`; normal calls never resend it. Explicit recovery sends only a bounded query for the same ID, content digest, and child target:

```sh
/usr/bin/python3 /absolute/herdr_role_lifecycle.py recover-parent-ack \
  --manifest /absolute/child-role.json \
  --request /absolute/parent-ack.json
```

A child durable duplicate reconciles the intent without a second confirmation append. Transport acceptance, report import, UI/status, heartbeat, supervisor observation, and Todo acceptance cannot produce a confirmed result. The child adapter listener is the only authority that can return the tasking receipt after its own durable confirmation.

## User-systemd design

Generate a role-specific unit with:

```sh
/usr/bin/python3 scripts/herdr_role_lifecycle.py render-unit \
  --manifest /home/alice/.local/share/herdr/roles/example/role.json \
  --manager /home/alice/.local/libexec/herdr/herdr_role_lifecycle.py
```

The generated unit contains absolute executable and manifest paths, `Type=notify`, `NotifyAccess=main`, watchdog heartbeats, `Restart=on-failure`, a finite start-rate limit, and restart delay. It does not claim readiness before Pi is interactive and the activation has been transported. Installation and enablement are deliberately separate, human-controlled steps; this repository does not install or enable the unit.

With `loginctl show-user "$USER" -p Linger` reporting `Linger=no`, user services stop at logout and cannot provide unattended activation while the user has no login session. That is an explicit operating limitation, not a condition the manager tries to repair. Either keep a user login session active or have an administrator deliberately enable linger after reviewing the local policy. The manager must not call `loginctl enable-linger` itself. After logout, an external actor must start the user manager and then start the role service; queued mailbox state still must not self-wake Pi.

## Collection helpers

`herdr collection helper-launch` creates the member with Pi as its initial process, so it never injects a launch command into an old or busy shell. The server selects the newly created member even when the Collection is nonempty. The CLI waits for `interactive_ready`, rechecks the returned pane/terminal/session identity, reads a bounded assignment file, and sends that exact text to the created pane using the normal prompt transaction. Its receipt says only that Herdr accepted the runtime transaction. Any startup, identity, assignment-read, or prompt-transport failure invokes identity-bound rollback; an uncertain rollback is reported as such.
