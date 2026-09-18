# Bounded handoff and staged run

## `herdr handoff`

`herdr handoff validate <JSON|PATH|->` validates a version-1 bounded envelope without contacting the server. `herdr handoff send <JSON|PATH|->` atomically checks the exact current sender and recipient identities, including terminal and full `agentSession`, then submits the encoded envelope through the same normal prompt transaction used by `agent prompt`.

The transport receipt reports Herdr runtime admission only. It does not claim Pi input-gate admission, model execution, todo completion, or parent acceptance. Herdr does not retry or store handoffs.

Limits in v1 are 16 KiB encoded, 2 KiB summary, eight artifact references, 1 KiB per artifact value, and 128 bytes for IDs/digests. Structural terminal controls, ANSI escape characters, and bidi controls are rejected.

## Strict worktree creation

`herdr worktree create --new-branch-only ...` refuses an existing local branch and uses an atomic `git worktree add -b` operation so a branch appearing after preflight is not silently reused. Omitting the flag preserves prior reuse-or-create behavior.

## `herdr run`

`herdr run` is a fail-fast local wrapper. It validates an exact repository/base and one launch mode, creates or opens a resource, starts one Pi agent with an explicit model/thinking selection, captures its full session identity, applies a named live profile through an external Pi-owned interface, records model/thinking verification honestly, and submits one bounded assignment using native handoff.

New-branch mode follows `~/Projects/.worktrees/<repo>/<slash-separated-branch>` and always requests strict new-branch creation. Existing-checkout mode opens/reuses that checkout. `--collection ID` is a separate workspace-local launch mode: it requires `--existing`, creates a member in that existing collection, and cannot be combined with `--branch`. Cross-workspace collection membership is not attempted.

Every invocation prints a compact receipt, including partial failures and created-versus-reused facts. Existing checkouts must have `HEAD` equal to the requested base; dirty state is preserved but explicitly marks reproducibility unverified and is represented by a bounded status digest. Cleanup is off by default. `--cleanup-on-failure` remains conservative and retains resources whenever unchanged identity and safe removal cannot be proved.

### Required external profile interface

Herdr does not own Pi profile content or Pane Prompt Override records. Set `--profile-helper PATH` or `HERDR_PANE_PROFILE_HELPER` to an executable implementing:

```text
HELPER use --workspace WORKSPACE_ID --pane PANE_ID --terminal TERMINAL_ID \
  --agent AGENT --session-source SOURCE --session-kind KIND \
  --session-value VALUE --profile PROFILE.md --json
```

It must perform the Pane Prompt Overrides `use` operation, read the live pane record back, and print exactly attributable JSON with at least:

```json
{
  "version": 1,
  "applied": true,
  "verified": true,
  "workspaceId": "w1",
  "paneId": "w1:p1",
  "terminalId": "term1",
  "agentSession": {
    "source": "herdr:pi",
    "agent": "pi",
    "kind": "id",
    "value": "session1"
  },
  "profile": "implementation-workspace-owner.md"
}
```

Herdr checks the child identity before and after the helper, requires the helper receipt to repeat the exact terminal and full agent session, and terminates helpers that exceed 10 seconds. The profile stage is therefore recorded as helper-attested rather than as independent Herdr inspection. A nonzero exit, timeout, malformed JSON, missing verification, or any identity/profile mismatch fails `apply_profile` before assignment submission. No such helper is currently shipped by this repository; it must be supplied by the Pi Pane Prompt Overrides component. Herdr does not copy profile text as a fallback.

### Example

```text
herdr run \
  --repo ~/Projects/example \
  --base origin/master \
  --branch feature/example \
  --role implementation-workspace-owner.md \
  --provider openai --model gpt-5 --thinking medium \
  --complexity-reason 'multi-file lifecycle change' \
  --assignment-file /tmp/example-assignment.md \
  --parent /tmp/parent-identity.json \
  --name example-owner \
  --profile-helper ~/.local/bin/pi-pane-profile
```
