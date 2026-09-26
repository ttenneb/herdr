# Pane sleep and wake

Herdr can put a managed agent to sleep and wake it again, in the same pane and terminal, when a message is queued for it.

## Launch recipe

Every managed launch (`herdr agent start`, and `herdr collection helper-launch` with `--session`) records a launch recipe on the pane when it commits, and the recipe is persisted with the session:

- the agent name, kind and arguments exactly as launched;
- only allowlisted `--env NAME=value` pairs the caller passed explicitly: `PI_*` (except credential-like names such as `*TOKEN*`, `*SECRET*`, `*API_KEY*`, `*AUTH*`, `*PASSWORD*`), `HERDR_LIFECYCLE_ROLE`, `TERM`, `LANG`, `LC_ALL` and `TZ`. Anything else, and any value with `scheme://user:pass@` credentials, is dropped. The inherited environment is never stored;
- no recipe at all for a launch with a secret on its command line (`--api-key`, `--token`, URL credentials), or with an initial prompt that a wake would re-send: a positional message, an `@file`, `-p`/`--print`, or anything after `--`.

The recipe, the end of a Herdr sleep and any route-carry change are applied only after the launch actually started, so a failed start loses none of them.

A launch that passes the reserved `--env HERDR_LIFECYCLE_ROLE=<roleId>` belongs to a lifecycle role manager. Herdr never sleeps or wakes it.

## Sleep

`herdr agent sleep <agent>` (API `agent.sleep`) records that Herdr put the agent to sleep, then sends it a ctrl+d guarded by the exact terminal and agent name. It refuses:

- agents without a recipe, including a hand-typed `pi`;
- lifecycle-owned agents;
- the parent of a ready delegation route or of a live child delegation (`parent of active delegation routes; not sleeping`): a slept parent has no live generation or trusted session, so its children's bound report routes would stop working;
- Collection helpers: their agent is the pane's first process, so the pane closes when it exits, and there is nothing left to wake;
- agents that are working or blocked.

A sleeping agent keeps its name: another agent cannot take it, while a relaunch in its own pane can. Any other live agent appearing in the slept pane (for example a hand-typed `pi`) ends the sleep, so prompts route normally again.

The sleep record is persisted. After a server restart, a slept pane stays asleep, and it is not resumed.

## Wake

The mailbox calls `App::wake_pane(pane_key, trigger_head)` in two situations. The trigger's `cause` records which:

- `head_appended`: a message is queued for a slept pane that has no attached Pi;
- `restore_backlog`: after a server restart, the mailbox sweeps slept panes that still have unsettled heads.

Both are level-triggered. The wake:

- relaunches the recipe through the managed launch path (`start_agent`), in the same pane and terminal, so the mailbox recipient stays the same. The Pi gets a new sender generation and resumes its `--session`. No prompt is sent: the Gate drains the queue;
- acts only if the pane has a sleep record, a recipe and an idle shell, has no live agent, and is not lifecycle-owned;
- is single-flight per pane. Further calls while a wake is outstanding return `Duplicate`;
- resolves when the woken generation attaches to Messages (becomes Active), which also ends the sleep. If it does not attach within 60 s, the wake counts as failed and one retry runs with a fresh wake ID. After a second failure, the pane cools down for 30 s;
- writes one owner-only record per wake ID under `<data dir>/pane-wakes/`: `requested`, then `started`, `duplicate`, `refused` (with a reason) or `failed`. A wake ID never launches twice.

Never woken:

- a Pi the human quit by hand. There is no sleep record, so its messages wait until someone starts it;
- a hand-typed `pi`, which has no recipe;
- Collection helpers;
- lifecycle-owned panes.

## Restart resume

With `resume_agents_on_restore`, a pane that has a recipe and is not asleep resumes through the same managed path, so the agent comes back `managed`, with a sender generation and Messages. Panes without a recipe keep the previous plain `pi --session` resume.

## Delegation routes across a relaunch

A delegation report route is tied to the child Pi's process generation. When `herdr delegation route-ready <child> --expected-parent <parent>` succeeds, Herdr also remembers the route on the child's pane: child and parent delegation, the child's session file and its generation. This record is persisted with the pane.

When a recipe relaunch starts a new generation, Herdr re-establishes `route_ready` for it with the same expected parent, once the new Pi is Active with a trusted session. A recipe relaunch here means `wake_pane` or the restart resume. The relaunch must be in the same pane, whose terminal is still bound to the child delegation, and on the same session file. Each outcome gets a durable record under `<data dir>/route-carries/<child>-g<generation>.json`: `established`, `refused` (different session or pane) or `expired` (not ready within 60 s).

The carry also pins the parent execution: the parent terminal and parent session the route was ready with. If the parent pane now runs another terminal or another session, the carry is refused, the child's old route is removed (the child shows not ready), and the refusal is recorded.

A hand start (`agent start`, helper launch) never inherits a route; it drops the remembered route. A different session file or a different pane drops it too.

## Policy notes and known issues

- **Launch-time session checks are policy, not a security boundary.** The directory a `--session` file must sit in is resolved from the launch's own `--env` (`PI_CODING_AGENT_SESSION_DIR`, `PI_CODING_AGENT_DIR`) before the server environment, so a caller can steer it. The checks are path based: the file is verified at launch and again at every identity read, but not held open, so a same-user process can swap it in between (TOCTOU). Both are accepted: the caller already runs as the same user.
- **Later:** a parent-side route carry across a wake. Today a parent of active routes refuses to sleep; carrying the parent side would let it sleep and re-bind its children's routes to its next generation.
- **Known issue:** two panes launched as managed on the same `--session` file are out of scope. Both would present the same trusted session identity. Wake, restart resume and route carry assume one pane per session file.
