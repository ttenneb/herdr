# External Pi role lifecycle

## Frozen verification plan

The owner verification for this change is `python3 -m unittest scripts.test_herdr_role_lifecycle` plus the focused Rust collection tests. It covers manifest/identity fail-closed behavior, durable receipts and heartbeats, truthful systemd readiness, bounded restart policy, default-deny `ask_user_question`, and exact helper assignment transport. The TPM consumes this evidence during integration.

The distinct failure-seeking check is `python3 scripts/test_herdr_role_lifecycle.py -v` together with a targeted diff inspection for relative executables, premature `READY=1`, receipt-triggered prompts, unbounded restart, and extension-side supervision. It is intentionally a different static/adversarial path over the same revision, not a repeated broad suite.

## Authority boundary

`herdr_role_lifecycle.py` is an external, one-shot lifecycle manager. A Pi extension may publish status and session identity, but it must never launch, restart, hibernate, or supervise Pi. The manager accepts only an explicit `assignment` or `report` activation artifact. It never watches Gate, mailbox, transport, settlement, status, or Todo records for wake-up signals. Consequently, a Gate or transport receipt cannot start a model turn.

Every role manifest binds all of these fields:

- durable role ID and role class;
- exact Herdr workspace, pane, terminal, working tree, and Pi session reference;
- canonical task reference;
- mailbox path;
- exact parent report route, including the full parent session identity;
- absolute Herdr, Pi, Python, and `systemd-notify` executables;
- durable state and activation paths below `/home` (a different durable root is accepted only for isolated tests);
- an explicit human-facing policy.

`humanFacing` defaults to false. A non-human-facing launch always adds `--exclude-tools ask_user_question`. Enabling it requires an explicit grant object and a role class of `user-facing-pm` or `human-facing-controller`; TPMs, implementation owners, QA roles, and helpers are rejected. This is launch authority only. It is not evidence that a human-facing request was answered.

## Execution and hibernation

An external actor writes a versioned activation artifact and starts the role's user service. The manager takes a per-role lock, validates the exact live pane identity, and either attaches to the already-running exact Pi generation or starts exactly one Pi with the stored session reference. It waits for Herdr's actual `interactive_ready` observation, validates the returned full session identity, submits the activation prompt to the exact pane, and records only a Herdr runtime transport receipt. Gate admission, model execution, report acceptance, and Todo acceptance remain `unknown`.

`READY=1` is sent only after exact interactive readiness and prompt transport acceptance. Durable JSON heartbeats and phase receipts are written atomically below the role's state directory. When execution returns to idle/done after a post-submission state change, the manager sends `ctrl+d` to the exact pane and waits for Pi to leave. It never force-kills a mismatched or unresponsive process. The service then exits, leaving the durable role but no Pi process. Restart recovery first inspects the exact pane and attaches to an existing matching process instead of launching a second one.

The manager does not poll while hibernated. Activation is therefore externally driven, including report delivery that needs a parent turn. Mailbox or Gate state alone is never an activation.

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
