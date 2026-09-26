# Pi mailbox direct transport

`scripts/herdr_mailbox_transport.py` is the single external routing decision for a durable Pi mailbox. It does not edit a terminal, inject PTY input, admit a model turn, install a unit, or wait for a newly started role manager.

The caller supplies owner-only JSON files below one durable root:

- a v1 transport manifest binding the exact Pi recipient, stable `mailboxPath`, runtime registration path, state directory, lifecycle-manager lock, wake unit, and absolute `systemctl` executable;
- a v1 delivery request binding a random 128-bit delivery ID to that same recipient and mailbox path;
- a recipient-owned runtime registration with a monotonic generation, `live`, `sleeping`, or `stopped` state, the mailbox protocol, and the running candidate's structured build identity.

For a compatible `live` registration, Herdr sends the delivery request directly to the registered owner-only Unix socket and returns the recipient's accepted, duplicate, uncertain, or rejected result. It never falls back from an incompatible or failed live recipient to systemd, because doing so could create a second owner.

A `sleeping` or `stopped` registration is rejected with `wake_disabled`, and nothing is started. The earlier systemd wake replayed a static role activation and could never deliver the message. Herdr now wakes a pane it put to sleep in-process (`herdr agent sleep`, `App::wake_pane`; see `pane-sleep-wake.md`). Lifecycle roles are activated only through `start-queued-input`.

The server ping and `herdr status` surfaces now include structured build identity: version, build channel, optional build ID, and optional source commit. Legacy servers without this field remain readable as unknown build identity.
