# #181: waiting messages follow their agent; no silent queue into a Pi without Messages

Consumers: the Pi owner, and herdr_beta_owner's harness. There are no new record kinds and no new wire methods.

## (b) "Has Messages" means "reads its inbox"

A Herdr-launched Pi opens its mailbox bootstrap stream whether or not Messages is on in its checkout. `pi-input-gate` bootstraps for reports and sender authority even without `.pi/pi-input-gate.json`, but then runs no inbox consumer. Until now Herdr counted any such stream as "Messages attached" and queued messages there that were never read. This was the reorg repro: the TPM and QA relaunched in a checkout without the managed-compat gate file, and the PM's messages sat unread.

Now a stream counts as a **Messages consumer** only after its first own-inbox read: `mailbox.watch`, `mailbox.snapshot` or `mailbox.claim`. Every Pi with Messages on makes one at start. Only a consumer:
- makes the pane Messages-capable (the durable `messages_capable` flag);
- exempts its Pi from the 30 s typed fallback;
- is returned by `attached_messages_recipient`.

For a live Pi that has bootstrapped but not consumed:
- **In a pane that never had a Messages Pi:** messages are typed through the draft guard (`typedReason:"no_messages"`), as for any Pi without Messages.
- **In a pane whose earlier Pi had Messages:** messages queue during the 30 s grace, as before, since a starting Pi may not have read its inbox yet.
  - After the grace, new messages are typed (`fallback_30s`).
  - The heads queued for this Pi during its grace are then typed too, in their original order and through the draft guard. These are unclaimed heads accepted after the Pi started, pinned to its session or not pinned.
  - Each is first closed and replaced by its typed-history row in one store lock section: a `withdrawn:` claim settled `closedBy:"typed"`, plus a `typed.<hash>` row with `typedReason:"fallback_30s"` and `delivery.movedFrom`. A head can never be both typed and later claimed.
  - The sender pane gets a note: "Typed into <pane> instead of queued". If typing then fails, for example because the human's draft never clears within 10 minutes, the existing failure note follows.
  - History shows each message once, as its typed row.
  - Heads queued before this Pi started stay held for a Messages Pi, and (a) below may move them.
- **`--transport mailbox`** to such a Pi is refused with `messages_unavailable`: "the Pi in the recipient pane has not turned Messages on (for example no .pi/pi-input-gate.json in its checkout); the message was not queued".

**Crash risk:** a server crash between closing a queued head and typing it loses that one typing. Its history row says it was typed. A graceful stop fails held typings with a sender note, as before.

## (a) A waiting message follows its agent's session

**Trigger:** a **managed** Pi on session S becomes a consumer in pane N. The Pi has a managed launch record for this exact generation with `--session S`, and the session it reports is S.

**What moves:** heads in any other pane O's queue (pane key or legacy terminal key) that are:
- pinned to S (`delivery.recipientSession == S`);
- held and unclaimed, and not typed history;

and O has no consuming Pi on S. They move to N's queue exactly once, in their original order.

**Records** (existing kinds, one store lock section per head):
1. The old head is closed with the Drop records, a `withdrawn:<stableId>` claim settled with `closedBy:"moved"`.
2. A copy is appended to N:
   - stable ID `moved.<sha256(old stableId, S)[..32]>`, revision 1;
   - the same subject, body, priority, sender, correlation and session pin;
   - `delivery.movedFrom` set to the old stable ID;
   - its own receipt.

It closes first, then appends. A repeat finds the deterministic copy and does nothing. A crash between the two writes is repaired on the session's next consumer attach, which appends the missing copy. The claim store uses the same lock, so a head is either claimed in O or moved, never both.

**The session pin.** A message queued for a pane is pinned to the session of the Pi running there. If no Pi runs in the pane, for example because its managed Pi exited, it is pinned to the `--session` of the pane's managed Pi launch recipe, so it still follows that agent. A live Pi's own session always wins. A pane with neither has no pin, and its messages stay with the pane.

**What does not move:**
- **Heads with no session pin** are pane-owned and stay.
- **Claimed heads**, including an ended execution's leftover claim (`recoveryNeeded`), stay in O. Moving them could run them twice. They are resolved there with Retry or Drop, as before.
- **Reported and hand-typed (`recipient_only`) Pis never pull.** A reported Pi could name any session, so a reported or hand-typed Pi relaunched elsewhere does not get its messages moved: it gets its new pane's own queue. Its earlier messages stay in the old pane, visible there as `previousSession`, to Drop or Retry.

**Where they went:**
- N gets one Herdr note, "Messages moved to this pane", listing how many messages moved from which panes, and any claimed messages left behind that need Retry or Drop there.
- O's history keeps each moved head as settled `closedBy:"moved"` with `movedTo` set to the new pane's public ID (or `a closed pane`).

## Wire additions (all optional; older clients ignore them)

- `ServerDelivery.movedFrom` on moved copies and on fallback typed rows.
- `headStates[].closedBy`, which can now also be `"moved"` or `"typed"` for a withdrawn head. A typed-replaced head is not listed, since its typed row is.
- `headStates[].movedTo` on `closedBy:"moved"`.

Installed Pi `ce8c4e6` shows an unknown `closedBy` as "(settled)"; the Pi owner may add a "moved to <pane>" label.

## Rollback (to rc3 8a32da36 / 17b0f49a, or rc2)

An older build reads:
- the `withdrawn:` claim as a Drop, which is hidden and never delivered;
- `closedBy:"moved"` or `"typed"` as a display string;
- the moved copy as an ordinary held head in N (it ignores `movedFrom`);
- the typed row as typed history.

Each moved message is therefore still delivered exactly once, in N. The only rollback loss is a move interrupted between its two writes: the older build has no repair and shows it dropped.

A (b) change of behaviour on rollback: the older build again counts a non-consuming Pi as attached. That is the old bug, not a new one.
