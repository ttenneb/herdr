# Pane sleep and wake

Herdr can put a managed agent to sleep and wake it again, in the same pane and terminal, when a message is queued for it.

## Launch recipe

Every managed launch (`herdr agent start`, and `herdr collection helper-launch` with `--session`) records a launch recipe on the pane when it commits, and the recipe is persisted with the session:

- the agent name, kind and arguments exactly as launched;
- only the `--env NAME=value` pairs the caller passed explicitly, minus credential-like names (`*TOKEN*`, `*SECRET*`, `*KEY*`, `*AUTH*`, `*PASSWORD*`, …). The inherited environment is never stored;
- a launch with a secret on its command line (`--api-key`, `--token`) gets no recipe.

A launch that passes the reserved `--env HERDR_LIFECYCLE_ROLE=<roleId>` belongs to a lifecycle role manager. Herdr never sleeps or wakes it.

## Sleep

`herdr agent sleep <agent>` (API `agent.sleep`) records that Herdr put the agent to sleep, then sends it a ctrl+d guarded by the exact terminal and agent name. It refuses:

- agents without a recipe, including a hand-typed `pi`;
- lifecycle-owned agents;
- Collection helpers: their agent is the pane's first process, so the pane closes when it exits, and there is nothing left to wake;
- agents that are working or blocked.

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

A hand start (`agent start`, helper launch) never inherits a route; it drops the remembered route. A different session file or a different pane drops it too.
