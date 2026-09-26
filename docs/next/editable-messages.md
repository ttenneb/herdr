# Editable Messages for ordinary agent sends

`herdr agent prompt`, structured prompts (`[[pi-input-gate:ingress:v3:…]]` payloads) and `herdr handoff send` now go into a Pi recipient's Messages queue when that Pi has a live Messages connection. Otherwise they are typed into the pane exactly as before. `--transport auto|mailbox|pty` forces either path.

A queued message stays editable while it waits. It runs when the recipient picks it up, which is immediately for an idle agent and at the end of the current turn for a busy one.

## One waiting message per sender
If you already have unclaimed messages waiting for the same recipient, the send is refused with exit code 4 and `pending_exists`. The JSON on stdout carries `error.pending`: an array of your waiting messages, newest first, each with `stableId`, `revision`, `digest`, `subject`, `priority`, `enqueuedAt` and `ageSeconds`. Rerun with one of:
- `--edit-pending <stableId>`: replace that message's text. Add `--expect-revision N` to guard against a concurrent change. If the message is no longer waiting at that revision, the result is `pending_claimed` (also exit 4), with the current waiting list.
- `--send-new`: queue another message.

A structured prompt with `supersession.mode = replace_pending` edits its own waiting message automatically. Resending the same messageId or correlation returns the existing message instead of a duplicate. Sends from outside Herdr panes are not checked.

## What the queued message looks like
Handoffs keep their exact `HERDR HANDOFF v1` text as the body, and plain prompts use the subject "Message from <sender>". Senders are attributed from the calling process's pane; that attribution is never authority.

## Every Pi pane has a queue
Messages are addressed to the pane, not to a Pi process or session: `recipient = pane:<queueKey>`. The queue key is minted with the pane, saved in session.json and restored unchanged, so queued messages survive Pi restarts, sleep and Herdr server restarts.
- Once a pane's Pi has attached Messages, the pane keeps queueing while no Pi is attached. When a Pi attaches again (managed or receive-only), the backlog runs in normal priority order.
- A Pi that never attached (no Messages support, or receive-only switched off) and panes running another agent keep typed input. `--transport` overrides.
- Claims belong to the Pi execution that took them. A claim left behind by an exited Pi never blocks the pane; it shows as `claimExecution: "other"` and the human can Drop it.
- The session a message was sent to is shown (`recipientSession`, `previousSession`) but does not gate delivery.
- Queuing for a pane with no attached Pi emits `pane.wake_requested` for the sleep/wake owner.

## Pis without a trusted launch
Receive-only Messages for a typed `pi` or a Collection helper are on by default (`[experimental] unmanaged_pi_messages = false` turns them off). Such a Pi may read, claim, edit and drop only its pane's inbox; it has no sender, report or route authority. Every pane shell carries `HERDR_MAILBOX_BOOTSTRAP_ADDRESS` while it is on.

## Edited messages keep history (F3)
Editing a waiting message now records an admitted receipt for the new revision, so the recipient's current list and settled history stay valid.

## For Pi integrators
See the wire reference: descriptor `binding` and `messages` (`watchMethod`, `watchMaxWaitMs`, `inbox`, `dropMethod`), `mailbox.watch {afterCursor, waitMs}` → `{changed, cursor}`, `headStates[].recipientSession / previousSession / claimExecution`, and `mailbox.drop {stableId, expectedRevision}`. Use a dedicated accepted stream for watch; streams never revoke each other.
