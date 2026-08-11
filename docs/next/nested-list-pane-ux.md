# Nested-list pane UX proposal

Status: product, interaction, and core runtime model proposal; detailed implementation design remains open.

## Summary

Herdr's core strength is that every agent and command runs as a real process in a real PTY. Humans and agents can inspect and control the same terminal through a common CLI and socket API. This makes agent delegation much more natural than in harness-specific orchestration interfaces.

The current layout model, however, makes terminal allocation and screen allocation the same decision. Every delegated agent normally requires another split or tab. Models that use subagents heavily can therefore turn an internal delegation tree into a large, flat collection of user-visible panes and tabs.

A **nested-list pane** addresses this without weakening Herdr's terminal-native model. In the TUI it occupies one leaf in a tab's layout while presenting a vertically scrollable collection of ordinary Herdr panes. In the shared runtime model that leaf is a typed **pane collection**, identified independently from panes. Child panes remain real, persistent, independently addressable PTYs; the collection changes their placement and organization, not their capabilities.

> A nested-list pane compresses terminal layout, not terminal capability.

## Coverage and required companion features

The nested-list pane solves the core **spatial-density** problem: a delegation tree can occupy one stable layout region instead of producing one top-level pane or tab per child. It is not sufficient by itself. A complete solution also requires:

- **Delegation metadata independent of layout:** explicit parent and purpose records, plus a derived queryable root, must survive moving or promoting a child.
- **Hierarchical Agents view:** primary agents and collapsed descendant groups should be emphasized so pane clutter does not become sidebar clutter.
- **Attention aggregation:** group rollups and, eventually, batched notifications must preserve urgent child identity without flooding the user.
- **Lifecycle controls:** live archival, bulk cleanup, orphan visibility, and age, count, or concurrency warnings make quiet resource and history growth visible without silently deleting terminals.
- **Agent-facing orchestration ergonomics:** the CLI and agent skill should make creating or reusing one helper group, recording parentage, and starting a child a coherent operation.
- **Careful completion semantics:** agent `done` or `idle` does not prove task success or result delivery. Archival is therefore an explicit organizational action, never an inference from detected agent state.

Together, nested-list panes, delegation metadata, hierarchical attention surfaces, and lifecycle policy address layout, attention, and retention density while leaving prompts, context transfer, and result exchange with the agent harness. Herdr does not need to become a task manager, message bus, workflow DAG, or scheduler.

## Problem

Herdr currently organizes work spatially:

```text
workspace -> tab -> pane -> process
```

Agent delegation is relational and temporal:

```text
parent task
├── research child
├── reviewer
└── implementation child
    └── test helper
```

Mapping every node in the delegation tree to a peer split or tab creates several UX problems:

- Pane and tab counts grow with an agent's delegation style.
- Short-lived helpers receive the same visual weight as primary agents.
- Parent-child provenance is lost in a flat layout.
- Completed helpers remain as persistent workspace furniture.
- Helper notifications and `done` states compete with primary work.
- Users cannot tell why a terminal exists or which task owns it.
- Agents can create lasting layout debt while performing otherwise useful orchestration.

The problem is not the use of PTYs. Real terminals are the correct execution primitive. The problem is requiring each PTY to consume permanent top-level layout space.

## Goals

- Preserve a real, interactive PTY for every child.
- Allow many concurrent terminals without pane or tab explosion.
- Represent delegation relationships without introducing a separate job runtime.
- Keep children available to Herdr's existing pane and agent APIs.
- Preserve agent state, notifications, direct attach, persistence, and restore behavior.
- Keep layout changes explicit and prevent background orchestration from stealing focus.
- Support agents, shells, tests, servers, logs, and other terminal processes equally.
- Allow terminals to move between grouped and ordinary presentation without restarting.
- Reuse Herdr's mouse-first and keyboard-first interaction language.

## Non-goals

- Replacing terminal agents with reconstructed chat views.
- Automatically grouping every agent or command.
- Making nested-list panes mandatory for delegation.
- Introducing recursive groups of groups.
- Redesigning notification delivery, batching, or policy beyond the visibility-aware seen-state changes required for grouped children.
- Automatically archiving or deleting terminals based on detected completion.
- Treating grouped children as reduced or non-interactive background jobs.

## Object model

A nested-list pane is the TUI presentation of a typed pane-collection leaf:

```text
Tab layout
├── Pane leaf: parent agent pane
└── Collection leaf: helpers
    ├── Child pane
    ├── Child pane
    │   └── Grandchild relationship
    └── Child pane
```

A collection has a stable collection ID distinct from every pane ID. It occupies one leaf in the tab's top-level BSP layout and owns no PTY. Its member panes are owned by the same tab but are placed inside the collection rather than appearing as peer BSP leaves.

Every pane has exactly one placement in a tab:

- an ordinary tiled pane leaf, or
- membership in one pane collection.

A pane cannot be both tiled and collected, belong to multiple collections, or disappear from placement. Empty collection leaves are valid persistent objects so a group can be created before its first child. If collection state is invalid during recovery, surviving member panes are promoted rather than discarded.

The delegating parent normally remains in an adjacent ordinary pane. The nested-list pane is created explicitly by a user or agent when grouped execution is useful. Herdr does not pre-create a helper group for every parent and does not automatically group every agent launched by another agent.

Each child remains an ordinary Herdr pane with its own:

- pane and terminal identity
- PTY and running process
- cwd and environment
- terminal state and scrollback
- semantic agent state when applicable
- native agent session identity when reported
- CLI and socket API access
- direct attach and observation support
- persistence and restore behavior

The collection adds stable membership, ordering, archive state, typed focus, and lifecycle events. Expansion, preview sizing, scroll position, and list-versus-terminal interaction mode belong to client presentation unless explicitly noted below.

## Default presentation

The normal view is a vertically scrolling list of child cards.

A collapsed row shows:

- process or agent identity
- semantic state and attention indicator
- reported task title or terminal title when available

Example:

```text
Helpers
├─ ● researcher        investigating restore behavior
├─ ◉ reviewer          waiting for input
└─ ✓ implementation    API changes complete
   └─ ● test-helper    running integration tests
```

Delegation descendants are flattened into the same list and indented to preserve the task tree. There are no recursively nested list panes.

Ordering is stored as a sibling rank under each delegation parent. Rendering performs a stable depth-first preorder traversal. New nodes append after their siblings. Reordering is limited to siblings and moves a parent's displayed subtree with it; changing parentage is a separate explicit reparent operation. State changes, archival, and filtering do not rewrite canonical sibling order.

If a recorded parent is outside the collection, archived into another section, closed, or otherwise absent from the local projection, its child appears as a local display root with a compact provenance cue. The underlying parent relationship remains unchanged.

## Inline terminals

Any child can be expanded inline beneath its row. Several child terminals may remain expanded simultaneously.

- A newly expanded child automatically receives at least half of the current collection height, making terminal interaction useful without requiring an immediate resize.
- Expanded previews can be resized independently; an explicit size remains stable across collapse, re-expansion, and collection resizing.
- The list scrolls vertically, so rows and expanded terminals can exist above and below the viewport.
- Collapsing a child does not stop its process.
- A collapsed live terminal retains its last authoritative PTY dimensions.
- Before first expansion, a child keeps the compact standard preview geometry used during creation; expansion applies the adaptive height.

The foreground full-app client owns geometry for expanded previews. It submits neutral per-terminal geometry claims derived from visible terminal rectangles. A writable direct attach has higher priority and temporarily owns that terminal's dimensions. Non-foreground app clients and observers render by clipping or padding and never resize shared PTYs.

Geometry precedence is:

```text
writable direct attach
  > foreground full-app client geometry
  > last accepted terminal size
  > standard initial preview size
```

Collapsing a child withdraws the foreground preview claim without forcing a resize. Maximizing claims the full collection rectangle. Preview resize claims should be debounced so dragging does not flood applications with resize events. Cold restore may use standard preview dimensions; live handoff preserves current PTY dimensions.

This presentation allows a user to compare or monitor several terminals without creating additional top-level splits.

## Maximize and restore

A child can be maximized within the nested-list pane.

- The selected child terminal fills the entire container rectangle.
- The list and container chrome are hidden while maximized.
- A dedicated action returns to the scrolling list at the previous position and expansion state.
- Maximizing within the group is distinct from zooming the containing pane across the entire Herdr tab.

## Creation and focus

Nested-list panes are explicit rather than ambient:

1. A user or parent agent creates a pane collection at a chosen layout location.
2. The creator starts a new child in the collection or moves an existing pane into it.
3. Later children can be added to the same collection.

Creating a collection or adding a child never steals the user's focus by default. Attention is communicated through state indicators and visibility-aware notifications.

Focus is typed and two-level:

- The top-level layout focuses either a pane leaf or a collection leaf.
- A focused collection maintains a selected child for list navigation.
- Entering terminal mode routes input to that selected child without replacing the collection's top-level placement.
- Focusing a grouped child through the pane API focuses its containing collection and selects the child. API focus alone does not mark it seen.
- Empty collections can be focused in list mode without inventing a synthetic pane or PTY.

A split action targeting the collection leaf creates a peer top-level split. Adding another member is a distinct collection operation. Because grouping is optional, an agent can still create an ordinary split or tab when the child deserves durable top-level placement.

## Delegation provenance

Delegation is session-wide runtime metadata independent of layout and agent detection. Each delegation record has:

- a stable delegation ID distinct from pane, terminal, and native agent-session IDs
- an optional associated pane ID
- an optional parent delegation ID
- a purpose or short role description when provided
- sibling order under its parent

The root is derived by following parent records rather than stored as a second source of truth. Assignments reject self-parenting and cycles atomically. Caller context may provide a convenient default when an agent creates a child, but the resulting relationship is persisted explicitly rather than inferred during rendering.

Delegation relationships survive moves between collections, tabs, and workspaces within the same Herdr session. When a parent pane closes, its delegation record remains as a lightweight provenance tombstone while descendants refer to it. Tombstones own no process or terminal and may be garbage-collected only after they have no live or retained descendants.

This enables:

- stable indentation and purpose labels
- clear ownership
- parent, root, and descendant queries
- preserving relationships across reordering and reparenting
- preserving provenance when panes move or parents close
- future parent-level coordination without relying on labels

Terminals placed in a collection without delegation metadata appear as top-level children. A nested-list pane is therefore useful as a general terminal stack even when no agent delegation is involved. Collection membership never changes delegation parentage implicitly.

## Keyboard interaction

The container has two explicit interaction modes.

### List mode

List mode controls the group rather than sending input to a child terminal. It supports:

- moving to the previous or next child
- expanding and collapsing a child
- entering a child terminal
- maximizing a child
- reordering children
- moving a child out of the group
- closing or archiving a child
- returning from archived children to active children

### Terminal mode

Terminal mode sends input directly to the selected child PTY. The configured Herdr prefix followed by Esc returns to list mode without terminating or interrupting the child. Bare Esc remains child input, and prefix followed by prefix retains literal-prefix passthrough.

Exact bindings should follow Herdr's existing prefix, navigate, resize, copy, and zoom interaction language rather than introducing an unrelated keymap.

## Mouse interaction

Mouse behavior mirrors keyboard behavior:

- click a row to select it
- click its disclosure control to expand or collapse it
- click an expanded terminal to enter terminal interaction
- drag the terminal preview boundary to resize it
- drag rows among siblings or use a context action to reorder subtrees
- use an explicit reparent action to change delegation parentage
- use row context actions to maximize, move, archive, or close
- scroll rows and collection chrome to move through child cards

While the pointer is over an entered expanded terminal, mouse events—including wheel events—follow normal terminal mouse-reporting and scrollback rules. List scrolling resumes over rows or collection chrome, avoiding competing wheel-event owners.

## Attention and seen state

A hidden or collapsed child changing state does not alter the user's expansion, scroll position, or keyboard focus.

- Blocked and newly completed children are highlighted in their rows.
- Tab, Workspace, and Repository activity follows top-level or locally promoted agents rather than allowing a completed delegated descendant to impersonate its parent.
- Unseen completed descendants appear as a separate teal badge/count beside the primary state icon, while a blocked descendant still makes that primary icon red.
- Viewing or focusing the collection does not mark children as seen.
- Seen remains session-global initially.
- Input successfully delivered to a top-level agent terminal marks that agent and every surviving pane in its delegated subtree as seen.
- Entering or typing in an individual delegated child with a live parent does not acknowledge completion attention; users do not need to visit child terminals merely to clear the rollup.
- If a parent pane has been closed, its surviving child becomes an effective root and input to that child can acknowledge its remaining subtree.
- API focus, API reads, and passive observation do not mark descendants seen. API and direct-attach terminal input follow the same top-level-parent rule as full-app interactive input.

This keeps attention reliable without allowing background work to rearrange the interface or requiring users to clear descendants one by one.

Notification batching policies can be considered separately from the nested-list pane.

## Completion and archival

Archive membership is an explicit organizational state. Herdr never archives a child merely because it detects `done`, `idle`, process exit, or another completion-like transition. A user or API caller may archive a child without asserting task success or result delivery.

Archival initially preserves the live pane and PTY:

- the process is not stopped
- the terminal remains readable and interactive
- the agent can be reopened and prompted again
- native session state remains available
- delegation metadata and sibling order remain unchanged

Any input sent to an archived pane atomically returns it to the active section before delivery. A transition to `working` or `blocked` also returns it to active. Reads, observation, `idle`, and process exit do not change archive membership.

Cleanup is explicit:

- close one archived child
- close selected archived children with confirmation
- clear an archive with counts of live, working, blocked, and exited panes

Initial age and count limits are advisory: they warn or refuse further archival but never close a live PTY automatically. Destructive retention policy is deferred until it can distinguish safe cleanup from resumed work and require explicit opt-in.

Active and archived sections are filtered projections of one canonical delegation forest. If a parent and child appear in different sections, the child is shown as a local root with a provenance cue rather than misleading indentation.

## Moving and promotion

Collection placement is shared runtime organization rather than a property fixed at process launch.

Without restarting its process, a pane can move:

- from a nested-list pane to an ordinary split
- from a nested-list pane to a normal tab
- from an ordinary layout into a nested-list pane
- between nested-list panes

This allows a delegated helper to be promoted into a primary collaborator, or an ordinary terminal to be compacted after it becomes secondary.

The running terminal and agent identity must survive the move.

## Closing behavior

Removing a non-empty pane collection requires an explicit member disposition. CLI and socket API calls refuse an ambiguous close and require one of:

- **cascade-close:** close the collection and every member PTY
- **promote-members:** remove the collection and move every surviving member into its own standalone tab in the same workspace without restarting it

The TUI presents the same choices and shows at least:

- number of active children
- number of archived children
- number of live and exited processes
- whether any children are blocked or working

Cascade-close requires destructive confirmation. Promotion must be atomic and use a deterministic fallback placement, creating a normal tab when promoting all members into peer splits would not fit sensibly. Callers can also cancel and move selected children manually.

Closing an empty collection behaves like closing an ordinary empty layout leaf.

## Relationship to existing Herdr objects

### Tabs

A tab is an alternate layout for an entire workspace. A nested-list pane is a local collection of interchangeable or related terminals occupying one position inside a layout.

### Ordinary panes

An ordinary pane is immediately and continuously visible in the split tree. A grouped child remains an ordinary runtime pane but is presented through its container.

### Popups and overlays

Popups are temporary, modal terminals outside the normal pane and agent APIs. Nested-list children are persistent session objects and must remain fully addressable.

### Agents view

The Agents view remains the session-wide attention projection. A nested-list pane provides local task structure and terminal access within a tab. Neither replaces the other.

## Progressive disclosure

The feature should remain understandable without requiring users to learn agent orchestration terminology.

- Basic concept: one pane can hold a list of terminals.
- Normal use: select, expand, interact, and collapse.
- Advanced use: delegation trees, explicit delegation IDs, API creation, movement, retention policies, and programmatic reordering.

Terms such as parent, child, and task tree should appear only when delegation provenance exists. A general group of shells should still read naturally as a list of terminals.

## Failure and recovery behavior

The collection must make partial failures legible:

- A child process exit remains visible with its exit state and is not archived automatically.
- One failed child does not close or corrupt the collection.
- A failed resume restores that child according to normal Herdr pane restore behavior.
- Missing or external parentage does not make a child inaccessible; it becomes a local display root with provenance retained when possible.
- Cyclic parent updates are rejected before mutation.
- Stale sibling order is normalized deterministically.
- A stale focused-member reference falls back to the first visible child without losing panes.
- Invalid or missing collection placement promotes surviving panes instead of dropping them.
- A client detach preserves all children because their PTYs remain server-owned.
- Live handoff preserves collection, placement, delegation, order, and archive state even when individual child restores fail.

## Runtime and presentation boundary

The product model requires a clean distinction between shared runtime facts and TUI presentation state.

Server-owned facts include:

- stable pane-collection identity
- typed collection placement in the tab layout
- pane membership with exactly-one-placement invariants
- stable delegation records, parentage, purpose, and sibling order
- typed top-level focus and the focused member within a collection
- archive membership
- lifecycle events for creation, membership, movement, reparenting, reordering, archival, promotion, and closure
- accepted per-terminal geometry and its current ownership precedence

Client presentation state includes:

- scroll offset
- expanded rows
- inline preview heights before they become foreground geometry claims
- list mode versus terminal mode
- locally maximized child

Expansion, preview sizing, scroll position, and local maximize are client-local and need not synchronize between attached clients. The selected child that participates in typed focus is shared runtime state; a client may keep transient hover or pointer state locally. Only the foreground full-app client's resulting geometry claims affect PTYs. Client presentation state may be restored locally across detach as a convenience, but it is not a shared runtime fact.

The API and event model uses neutral runtime terminology such as collection and member rather than UI-specific names such as card, sidebar, or widget.

## Future runtime work

- **Delegation bootstrap:** `session.snapshot` currently omits delegation state. Clients must use
  `delegation.tree` as a workaround, but event history retains only 512 events, so it cannot
  provide a cursor-bounded atomic bootstrap. Add delegation state to snapshots together with a
  cursor-bounded atomic delegation bootstrap protocol in a future feature; do not treat the
  current workaround as lossless synchronization.

## Open implementation questions

The product semantics and core runtime boundaries are settled. Detailed implementation design still needs to determine:

1. The concrete typed layout representation and migration from the current pane-only BSP leaf and focus invariants.
2. The exact public ID format and API schema for pane collections and delegation records.
3. How portable layout export distinguishes collection structure from member launch commands, and how apply targets existing versus new panes.
4. The deterministic member order and focus behavior used when `promote-members` creates one standalone tab per member.
5. The event and command names required for atomic create, add, move, reorder, reparent, archive, promote, cascade-close, and remove operations.
6. Geometry-claim revisioning, drag debounce, minimum preview dimensions, pixel-cell metadata, and recovery when the foreground client disconnects.
7. Whether local expansion, scroll, preview-height, and maximize state is optionally persisted per client across detach.
8. How older protocol clients receive or reject collection layouts after the required protocol-version change.
9. How delegation tombstones are compacted after their last retained descendant disappears.
10. Whether a future explicit acknowledgment API is needed in addition to top-level terminal-input rollups.

## Acceptance criteria

A first complete UX should satisfy the following:

- A user or agent can explicitly create a nested-list pane without stealing focus.
- The pane collection has a stable ID distinct from panes and occupies one typed leaf in an ordinary Herdr tab layout.
- Every pane is placed exactly once as either an ordinary tiled leaf or a member of one collection.
- Any supported terminal pane can be created in or moved into a collection without changing its pane or terminal identity.
- Several children can run concurrently while consuming one top-level layout rectangle.
- Child panes retain normal IDs, PTYs, reads, input, agent state, waits, notifications, and direct attach.
- Focus distinguishes the collection leaf, selected child, and entered terminal without synthetic panes.
- Multiple children can be expanded inline and independently resized by the foreground client; direct attach has geometry precedence.
- One child can fill the collection and return to the prior list state.
- Delegation uses stable session-wide records with parent, derived root, purpose, cycle prevention, and parent-close tombstones.
- The list renders sibling order as a depth-first forest; reorder moves sibling subtrees and reparenting is explicit.
- Background state changes highlight children without changing focus, scroll, expansion, ordering, or archive membership.
- Successfully delivered terminal input to a top-level parent marks its surviving delegated subtree seen; entering or interacting directly with a delegated child, focus, reads, observers, and viewing the collection do not.
- Archival is explicit and does not claim completion or result delivery.
- Input or renewed working/blocked state atomically returns an archived child to active.
- Initial count and age limits warn but never close live PTYs automatically.
- A child can move between collected and ordinary layouts, collections, tabs, and workspaces without process restart or lost provenance.
- Removing a non-empty collection requires an explicit `cascade-close` or `promote-members` disposition.
- Invalid collection state promotes surviving panes rather than dropping them.
- Detach, reattach, snapshot restore, and live handoff preserve collection placement, delegation, order, archive state, and pane identity mappings.
- The UX remains useful for ordinary terminals without requiring agent-specific concepts.

## Product principle

Herdr should continue to standardize on the terminal as the common interface for humans, agents, commands, and remote operation. The nested-list pane does not replace that standard. It acknowledges that execution topology can grow much faster than the screen layout a human can comfortably maintain.

Agents should be free to allocate execution without forcing every allocation to become permanent top-level visual structure.
