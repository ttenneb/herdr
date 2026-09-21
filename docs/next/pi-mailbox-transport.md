# Pi mailbox direct transport and wake boundary

`scripts/herdr_mailbox_transport.py` is the single external routing decision for a durable Pi mailbox. It does not edit a terminal, inject PTY input, admit a model turn, install a unit, or wait for a newly started role manager.

The caller supplies owner-only JSON files below one durable root:

- a v1 transport manifest binding the exact Pi recipient, stable `mailboxPath`, runtime registration path, state directory, lifecycle-manager lock, wake unit, and absolute `systemctl` executable;
- a v1 delivery request binding a random 128-bit delivery ID to that same recipient and mailbox path;
- a recipient-owned runtime registration with a monotonic generation, `live`, `sleeping`, or `stopped` state, the mailbox protocol, and the running candidate's structured build identity.

For a compatible `live` registration, Herdr sends the delivery request directly to the registered owner-only Unix socket and returns the recipient's accepted, duplicate, uncertain, or rejected result. It never falls back from an incompatible or failed live recipient to systemd, because doing so could create a second owner.

For a compatible `sleeping` or `stopped` registration, Herdr serializes the wake decision and first proves the role-manager lock is free. A held lock returns `lifecycle_manager_lock_held` without invoking systemd. Otherwise it durably records one wake intent for that runtime generation and executes only:

```text
systemctl --user start --no-block <manifest wake unit>
```

Distinct or duplicate deliveries while that generation is still sleeping reuse one recipient/unit/generation-scoped wake receipt and do not issue another start; the receipt never embeds or falsely identifies the first delivery ID. The nonblocking call acknowledges only systemd job scheduling; it does not wait for service readiness, Pi startup, mailbox admission, or model execution. Once the recipient publishes a newer live generation, delivery uses the direct socket. Both paths carry the exact same manifest-bound mailbox path, so wake changes process state rather than queue identity.

The server ping and `herdr status` surfaces now include structured build identity: version, build channel, optional build ID, and optional source commit. Legacy servers without this field remain readable as unknown build identity.
