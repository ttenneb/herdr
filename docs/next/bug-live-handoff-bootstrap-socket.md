# Bug: live handoff fails on every build with the Messages bootstrap socket

**Status:** open. It needs a fix in the next build. It is not fixed in rc3 (`8a32da36`, `17b0f49a`).

**Impact:** `server.live_handoff` (and `herdr update --handoff`) always fails, including a handoff to the same binary. It fails safely: the handoff rolls back, the old server keeps running, and panes are untouched.

**Repro:** start any rc3 server, then call `server.live_handoff` with `import_exe` set to the same or another rc3 binary. The error is:

```
handoff_failed: handoff replacement server did not become ready: handoff stream closed while reading line
```

The server log shows the import server reach `api server listening` and `client protocol socket listening`, then exit. The old server then logs `handoff import server reaped during rollback`.

## Cause

`run_handoff_import_server` builds a `HeadlessServer` (`src/server/headless.rs`). `HeadlessServer::new` binds `herdr-mailbox-bootstrap.sock` through `MailboxBootstrapListener::bind` → `prepare_socket_path`. The old server still owns that socket, because:

1. **Export:** the exporting server does not release its `mailbox_bootstrap_listener` before the import binds. Only the API and client sockets are handed over or closed.
2. **Import:** `wait_for_old_public_sockets_to_close` waits only for the API and client sockets. The import therefore finds a live bootstrap socket, `prepare_socket_path` reports "herdr server is already running", and the import exits.

The 0.8.4 ↔ rc1 rehearsal passed because rc1 had no bootstrap socket.

## Fix for the next build (both sides, so either peer can be an old build)

- **Export:** drop, or stop accepting on, the bootstrap listener before signalling the import. Old accepted Pi streams end with the old server either way; Pi re-bootstraps.
- **Import:** when the bootstrap socket path is still served by the handing-off server, unlink the path and bind a fresh socket. Only the old listener's owner identity (inode) remains live. The old server's `Drop` already removes the path only if it still owns it.
- **Test:** a live handoff round trip with the bootstrap listener bound, in both directions, including against an export that does not release it.

## Workaround in use (C2 update runbook)

Unlink `~/.config/herdr/herdr-mailbox-bootstrap.sock` immediately before `server.live_handoff`. The import then binds a fresh socket, and the handoff completes in both directions between rc3 builds. This was verified with 8a32da36 → 8a32da36 and 8a32da36 ↔ 17b0f49a on a fake HOME.

Side effect: for the few seconds between the unlink and the new server's bind, a newly started Pi cannot attach to Messages until `/reload`. If the handoff then fails, the old server keeps running without a reachable bootstrap path until Herdr is restarted.
