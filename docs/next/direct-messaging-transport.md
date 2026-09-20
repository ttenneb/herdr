# Direct messaging transport primitives

The `direct_transport` module contains the default-off Herdr side of the frozen `direct-editable-messaging/v1` contract. It is intentionally a pure authority/channel layer tested against a mock Gate; this release does **not** enable a live transport. Authorized delivery ends at the recipient's durable, visible, inspectable Gate/mailbox. Receipt, idle, report, supervisor, and transport events never wake a model turn, call a model-facing message API, or write to an editor or PTY.

## Authority

A delegation-v2 grant is issued only from a persisted, non-tombstoned canonical delegation edge with exact task, session generation, delegation, repository, worktree, branch, message-kind, effect, expiry, topology-revision, and grant-revision scope. Direct grants cannot skip levels or move laterally. Peer advisory grants require an exact common-ancestor generation, expire within five minutes, and permit advisory sends only. Agent stop uses a separate stop-only grant and one of the fixed reason codes.

Grant and revocation mutations are serialized by exclusive authority-state access. Starting revocation immediately blocks new tickets; revocation becomes complete only after the recipient Gate returns an exact fence receipt. Generation replacement, expiry, revision gaps or rollback, disconnect, and unconfirmed authority all fail closed.

## Channel and manifest

An anonymous inherited, non-child-inheritable channel is preferred. The fallback local socket requires supported-platform OS peer identity evidence plus exact foreground Pi process-tree and terminal-generation binding. Raw proof fields and constructors are not public; only a crate-owned descriptor or OS adapter can produce a binding. No bearer secret is placed in environment variables, arguments, messages, logs, session metadata, or child processes because the protocol defines no reusable secret.

A live manifest is registered and negotiated atomically on that authenticated channel. It binds the recipient generation, protocol range, frozen fixture digest, features, Gate/mailbox version, package revision, effective-config digest and provenance, epoch, expiry, and channel identity. A persistent high-water mark makes registration epochs strictly increasing even across disconnect and reload, and registry-generation exhaustion fails without mutation. Every grant and ticket carries the exact negotiated recipient generation, channel binding, manifest epoch, and registry generation; disconnect, reload, or replacement invalidates outstanding routes. Cached files and PTY input are never discovery fallbacks.

Wire frames must be UTF-8 NFC and exact RFC 8785 JCS encodings. Duplicate keys, unknown fields, invalid operation schemas, malformed 128-bit message IDs, over-limit text, excessive structural depth, bidi controls, and oversized frames are rejected before dispatch.

## Delivery receipts and reconciliation

Transport acceptance is never delivery. Before writing a send, the sender persists an unresolved record containing the immutable message ID, a fresh attempt correlation, payload digest, exact recipient generation, and creation time. Socket acceptance may be recorded, but it cannot resolve that record. Only an exact durable Gate receipt with an `admitted` or `rejected` outcome resolves sender state. Missing receipts—including stale-config rejection before admission, reload races, disconnects, and pre-admission drops—remain visible and recoverable across restart and retention.

Retries use a fresh correlation while preserving the message ID and payload digest. A Gate can therefore return the existing admission for a duplicate without enqueuing the body twice; that receipt reconciles every unresolved attempt for the same immutable delivery. Mismatched and conflicting receipts fail closed. For sessions that have not passed a fresh structured-ingress canary, use plain nonblocking ingress. After stale configuration or reload, reconcile with a fresh correlation and do not treat an earlier structured-sender success as delivery evidence.

## Configuration provenance

Direct messaging remains disabled by default. A linked worktree may use one validated local override. Otherwise, it inherits a primary-checkout mailbox only when the primary configuration explicitly enables `inheritMailboxToLinkedWorktrees`. On supported Linux hosts, ownership is derived internally from the running process and the configuration is opened once with no-follow semantics; metadata validation and bounded reading use that same descriptor, with namespace identity checked before and after reading. Malformed, symlinked, wrong-owner, wrong-repository, wrong-worktree, ambiguous, or unsupported-platform evidence is rejected.

Frozen fixture digest: `16d169e6c9b472b864c2c97e6d77c9268dc98d0d68c75154fa23ed87d2729129`.
