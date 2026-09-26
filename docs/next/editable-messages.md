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

## Restarting Pi
A message is tied to the recipient's Pi session at send time. After a restart, messages for the old session never run on their own. The recipient's Messages lists them through `mailbox.stranded`, and the human can Retry (`mailbox.adopt`) or Drop (`mailbox.drop`) each one.

## Pis without a trusted launch
With `[experimental] unmanaged_pi_messages = true` (default false), a typed `pi` or a Collection helper also gets Messages.
- It may read, claim and edit only its own inbox.
- It has no sender, report or route authority.
- Every pane shell then carries `HERDR_MAILBOX_BOOTSTRAP_ADDRESS`.

## Edited messages keep history (F3)
Editing a waiting message now records an admitted receipt for the new revision, so the recipient's current list and settled history stay valid.

## For Pi integrators
New stream methods: `mailbox.watch` (long-poll), `mailbox.stranded`, `mailbox.adopt` and `mailbox.drop`. They are advertised under `messages` in the bootstrap descriptor.
