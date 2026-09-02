use std::collections::{HashMap, HashSet};

use crate::delegation::Delegations;
use crate::detect::{Agent, AgentState};
use crate::layout::{PaneId, PanePlacement};
use crate::terminal::{TerminalId, TerminalState};

use super::{Tab, Workspace};

/// Detail info for a single pane, used by the agent detail panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttentionSummary {
    primary_state: AgentState,
    primary_seen: bool,
    primary_working: bool,
    unseen_descendant_done: usize,
    blocked_descendants: usize,
}

impl AttentionSummary {
    pub(crate) fn empty() -> Self {
        Self {
            primary_state: AgentState::Unknown,
            primary_seen: true,
            primary_working: false,
            unseen_descendant_done: 0,
            blocked_descendants: 0,
        }
    }

    pub(crate) fn for_tabs<'a, I>(
        tabs: I,
        terminals: &HashMap<TerminalId, TerminalState>,
        delegations: &Delegations,
    ) -> Self
    where
        I: IntoIterator<Item = &'a Tab>,
    {
        let panes = tabs
            .into_iter()
            .flat_map(|tab| {
                tab.panes
                    .iter()
                    .map(|(&pane_id, pane)| (pane_id, pane, tab.pane_placement(pane_id)))
            })
            .collect::<Vec<_>>();
        let scope_panes = panes
            .iter()
            .map(|(pane_id, _, _)| *pane_id)
            .collect::<HashSet<_>>();
        panes
            .into_iter()
            .fold(Self::empty(), |mut summary, (pane_id, pane, placement)| {
                if let Some(terminal) = terminals.get(&pane.attached_terminal_id) {
                    summary.include_pane(
                        pane_id,
                        terminal.state,
                        pane.seen,
                        placement,
                        delegations,
                        &scope_panes,
                    );
                }
                summary
            })
    }

    fn include_primary(&mut self, state: AgentState, seen: bool) {
        if pane_attention_priority(state, seen)
            > pane_attention_priority(self.primary_state, self.primary_seen)
        {
            self.primary_state = state;
            self.primary_seen = seen;
        }
    }

    fn include_pane(
        &mut self,
        pane_id: PaneId,
        state: AgentState,
        seen: bool,
        placement: Option<PanePlacement>,
        delegations: &Delegations,
        scope_panes: &HashSet<PaneId>,
    ) {
        // A collection is the notification boundary for completed members.
        // Keep the pane's unseen completion local to the collection without
        // changing its underlying state or seen flag. Other states, especially
        // blocked, continue to participate in ancestor rollups.
        if matches!(placement, Some(PanePlacement::Collection(_)))
            && state == AgentState::Idle
            && !seen
        {
            return;
        }

        let delegated_descendant = delegations
            .delegation_for_pane(pane_id)
            .and_then(|record| record.parent_id)
            .is_some_and(|parent_id| {
                let mut ancestor_id = Some(parent_id);
                let mut visited = HashSet::new();
                while let Some(id) = ancestor_id {
                    if !visited.insert(id) {
                        break;
                    }
                    let Some(ancestor) = delegations.get(id) else {
                        break;
                    };
                    if ancestor
                        .pane_id
                        .is_some_and(|ancestor_pane| scope_panes.contains(&ancestor_pane))
                    {
                        return true;
                    }
                    ancestor_id = ancestor.parent_id;
                }
                false
            });
        if delegated_descendant {
            self.unseen_descendant_done += usize::from(state == AgentState::Idle && !seen);
            self.blocked_descendants += usize::from(state == AgentState::Blocked);
        } else {
            self.primary_working |= state == AgentState::Working;
            self.include_primary(state, seen);
        }
    }

    pub fn display_state(self) -> (AgentState, bool) {
        if self.primary_state == AgentState::Blocked || self.blocked_descendants > 0 {
            (AgentState::Blocked, false)
        } else if self.primary_state == AgentState::Idle && !self.primary_seen {
            (AgentState::Idle, false)
        } else if self.primary_working {
            (AgentState::Working, true)
        } else {
            (self.primary_state, self.primary_seen)
        }
    }

    #[cfg(test)]
    pub fn unseen_descendant_done(self) -> usize {
        self.unseen_descendant_done
    }

    #[cfg(test)]
    pub fn blocked_descendants(self) -> usize {
        self.blocked_descendants
    }

    pub fn descendant_attention_count(self) -> usize {
        self.unseen_descendant_done
    }
}

pub struct PaneDetail {
    pub pane_id: PaneId,
    pub tab_idx: usize,
    pub tab_label: String,
    pub label: String,
    pub pane_label: Option<String>,
    pub terminal_title: Option<String>,
    pub terminal_title_stripped: Option<String>,
    pub agent_label: String,
    pub agent_kind_label: Option<String>,
    pub agent: Option<Agent>,
    pub state: AgentState,
    pub seen: bool,
    pub last_agent_state_change_seq: Option<u64>,
    pub state_labels: HashMap<String, String>,
    pub tokens: HashMap<String, String>,
}

impl Tab {
    pub fn attention_summary(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        delegations: &Delegations,
    ) -> AttentionSummary {
        AttentionSummary::for_tabs(std::iter::once(self), terminals, delegations)
    }

    fn pane_details(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        tab_idx: usize,
        tab_label: &str,
    ) -> Vec<PaneDetail> {
        self.layout
            .pane_ids()
            .iter()
            .filter_map(|id| {
                let pane = self.panes.get(id)?;
                let terminal = terminals.get(&pane.attached_terminal_id)?;
                let agent_kind_label = terminal.effective_agent_label().map(str::to_string);
                let fallback_agent_label = terminal
                    .agent_name
                    .as_deref()
                    .or(agent_kind_label.as_deref())?
                    .to_string();
                let agent_label = terminal
                    .effective_display_agent()
                    .unwrap_or_else(|| fallback_agent_label.clone());
                let presentation = terminal.effective_presentation();
                Some(PaneDetail {
                    pane_id: *id,
                    tab_idx,
                    tab_label: tab_label.to_string(),
                    label: agent_label.clone(),
                    pane_label: terminal
                        .effective_title()
                        .or_else(|| terminal.manual_label.clone()),
                    terminal_title: terminal.terminal_title.clone(),
                    terminal_title_stripped: terminal.terminal_title_stripped(),
                    agent_label,
                    agent_kind_label,
                    agent: terminal.effective_known_agent(),
                    state: terminal.state,
                    seen: pane.seen,
                    last_agent_state_change_seq: terminal.last_agent_state_change_seq,
                    state_labels: presentation.state_labels,
                    tokens: terminal.metadata_tokens.values(),
                })
            })
            .collect()
    }
}

fn pane_attention_priority(state: AgentState, seen: bool) -> u8 {
    match (state, seen) {
        (AgentState::Blocked, _) => 4,
        (AgentState::Idle, false) => 3,
        (AgentState::Working, _) => 2,
        (AgentState::Idle, true) => 1,
        (AgentState::Unknown, _) => 0,
    }
}

impl Workspace {
    pub fn attention_summary(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        delegations: &Delegations,
    ) -> AttentionSummary {
        AttentionSummary::for_tabs(self.tabs.iter(), terminals, delegations)
    }

    #[cfg(test)]
    pub fn aggregate_state(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
    ) -> (AgentState, bool) {
        self.tabs
            .iter()
            .flat_map(|tab| tab.panes.values())
            .filter_map(|pane| {
                terminals
                    .get(&pane.attached_terminal_id)
                    .map(|terminal| (terminal.state, pane.seen))
            })
            .max_by_key(|(state, seen)| pane_attention_priority(*state, *seen))
            .unwrap_or((AgentState::Unknown, true))
    }

    pub fn pane_details(&self, terminals: &HashMap<TerminalId, TerminalState>) -> Vec<PaneDetail> {
        let multi_tab = self.tabs.len() > 1;
        self.tabs
            .iter()
            .enumerate()
            .flat_map(|(tab_idx, tab)| {
                let tab_label = self
                    .tab_display_name(tab_idx)
                    .unwrap_or_else(|| (tab_idx + 1).to_string());
                tab.pane_details(terminals, tab_idx, &tab_label).into_iter()
            })
            .map(|mut detail| {
                if multi_tab {
                    detail.label = format!("{}·{}", detail.tab_label, detail.agent_label);
                }
                detail
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Direction;

    use super::*;
    use crate::detect::Agent;
    use crate::layout::LayoutLeaf;

    fn terminal_for_pane(ws: &Workspace, pane_id: PaneId) -> TerminalState {
        TerminalState::new(ws.terminal_id(pane_id).unwrap().clone(), "/tmp".into())
    }

    #[test]
    fn aggregate_state_all_unknown() {
        let ws = Workspace::test_new("test");
        let mut terminals = HashMap::new();
        let root = ws.tabs[0].root_pane.expect("test tab has root pane");
        let terminal = terminal_for_pane(&ws, root);
        terminals.insert(terminal.id.clone(), terminal);
        let (state, seen) = ws.aggregate_state(&terminals);
        assert_eq!(state, AgentState::Unknown);
        assert!(seen);
    }

    #[test]
    fn aggregate_state_priority() {
        let mut ws = Workspace::test_new("test");
        let id2 = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != id2)
            .copied()
            .unwrap();
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Idle;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut second_terminal = terminal_for_pane(&ws, id2);
        second_terminal.state = AgentState::Working;
        terminals.insert(second_terminal.id.clone(), second_terminal);

        let (state, seen) = ws.aggregate_state(&terminals);

        assert_eq!(state, AgentState::Working);
        assert!(seen);
    }

    #[test]
    fn aggregate_state_done_unseen_beats_working() {
        let mut ws = Workspace::test_new("test");
        let id2 = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != id2)
            .copied()
            .unwrap();
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Idle;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut second_terminal = terminal_for_pane(&ws, id2);
        second_terminal.state = AgentState::Working;
        terminals.insert(second_terminal.id.clone(), second_terminal);
        let root = ws.tabs[0].panes.get_mut(&root_id).unwrap();
        root.seen = false;

        let (state, seen) = ws.aggregate_state(&terminals);

        assert_eq!(state, AgentState::Idle);
        assert!(!seen);
    }

    #[test]
    fn delegated_completion_is_attention_without_replacing_working_activity() {
        let mut ws = Workspace::test_new("test");
        let child_id = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != child_id)
            .copied()
            .unwrap();
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Working;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut child_terminal = terminal_for_pane(&ws, child_id);
        child_terminal.state = AgentState::Idle;
        terminals.insert(child_terminal.id.clone(), child_terminal);
        ws.tabs[0].panes.get_mut(&child_id).unwrap().seen = false;

        let mut delegations = Delegations::new();
        let root = delegations.create(Some(root_id), None, None).unwrap();
        delegations
            .create(Some(child_id), Some(root), Some("child".into()))
            .unwrap();

        let summary = ws.attention_summary(&terminals, &delegations);
        assert_eq!(summary.display_state(), (AgentState::Working, true));
        assert_eq!(summary.unseen_descendant_done(), 1);
        assert_eq!(summary.blocked_descendants(), 0);
    }

    #[test]
    fn delegated_collection_completion_stays_local() {
        let mut ws = Workspace::test_new("test");
        let child_id = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0].root_pane.expect("root");
        let collection = ws
            .create_collection_near(
                0,
                LayoutLeaf::Pane(root_id),
                Direction::Vertical,
                0.5,
                Some("helpers".into()),
            )
            .expect("create collection");
        ws.collect_pane(child_id, collection)
            .expect("collect child");

        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Working;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut child_terminal = terminal_for_pane(&ws, child_id);
        child_terminal.state = AgentState::Idle;
        terminals.insert(child_terminal.id.clone(), child_terminal);
        ws.tabs[0].panes.get_mut(&child_id).unwrap().seen = false;

        let mut delegations = Delegations::new();
        let root = delegations.create(Some(root_id), None, None).unwrap();
        delegations
            .create(Some(child_id), Some(root), Some("child".into()))
            .unwrap();

        let tab_summary = ws.tabs[0].attention_summary(&terminals, &delegations);
        let workspace_summary = ws.attention_summary(&terminals, &delegations);
        assert_eq!(tab_summary.display_state(), (AgentState::Working, true));
        assert_eq!(tab_summary.descendant_attention_count(), 0);
        assert_eq!(
            workspace_summary.display_state(),
            (AgentState::Working, true)
        );
        assert_eq!(workspace_summary.descendant_attention_count(), 0);
        assert!(!ws.tabs[0].panes[&child_id].seen);
    }

    #[test]
    fn nondelegated_collection_completion_does_not_replace_tiled_activity() {
        let mut ws = Workspace::test_new("test");
        let child_id = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0].root_pane.expect("root");
        let collection = ws
            .create_collection_near(0, LayoutLeaf::Pane(root_id), Direction::Vertical, 0.5, None)
            .expect("create collection");
        ws.collect_pane(child_id, collection)
            .expect("collect child");

        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Working;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut child_terminal = terminal_for_pane(&ws, child_id);
        child_terminal.state = AgentState::Idle;
        terminals.insert(child_terminal.id.clone(), child_terminal);
        ws.tabs[0].panes.get_mut(&child_id).unwrap().seen = false;

        let summary = ws.attention_summary(&terminals, &Delegations::new());
        assert_eq!(summary.display_state(), (AgentState::Working, true));
        assert_eq!(summary.descendant_attention_count(), 0);
    }

    #[test]
    fn blocked_archived_collection_member_remains_urgent() {
        let mut ws = Workspace::test_new("test");
        let child_id = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0].root_pane.expect("root");
        let collection = ws
            .create_collection_near(0, LayoutLeaf::Pane(root_id), Direction::Vertical, 0.5, None)
            .expect("create collection");
        ws.collect_pane(child_id, collection)
            .expect("collect child");
        ws.set_collection_member_archived(child_id, collection, true)
            .expect("archive child");

        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Working;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut child_terminal = terminal_for_pane(&ws, child_id);
        child_terminal.state = AgentState::Blocked;
        terminals.insert(child_terminal.id.clone(), child_terminal);

        let mut delegations = Delegations::new();
        let root = delegations.create(Some(root_id), None, None).unwrap();
        delegations
            .create(Some(child_id), Some(root), Some("child".into()))
            .unwrap();

        let summary = ws.attention_summary(&terminals, &delegations);
        assert_eq!(summary.display_state(), (AgentState::Blocked, false));
        assert_eq!(summary.blocked_descendants(), 1);
    }

    #[test]
    fn tombstoned_intermediate_keeps_grandchild_in_root_rollup() {
        let mut ws = Workspace::test_new("test");
        let child_id = ws.test_split(Direction::Horizontal);
        let grandchild_id = ws.test_split(Direction::Vertical);
        let root_id = ws.tabs[0].root_pane.expect("root");
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Working;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut grandchild_terminal = terminal_for_pane(&ws, grandchild_id);
        grandchild_terminal.state = AgentState::Idle;
        terminals.insert(grandchild_terminal.id.clone(), grandchild_terminal);
        ws.tabs[0].panes.get_mut(&grandchild_id).unwrap().seen = false;

        let mut delegations = Delegations::new();
        let root = delegations.create(Some(root_id), None, None).unwrap();
        let child = delegations
            .create(Some(child_id), Some(root), Some("child".into()))
            .unwrap();
        delegations
            .create(Some(grandchild_id), Some(child), Some("grandchild".into()))
            .unwrap();
        delegations.tombstone_pane(child_id);
        ws.tabs[0].panes.remove(&child_id);

        let summary = ws.attention_summary(&terminals, &delegations);
        assert_eq!(summary.display_state(), (AgentState::Working, true));
        assert_eq!(summary.unseen_descendant_done(), 1);
    }

    #[test]
    fn working_delegated_child_does_not_replace_idle_root_activity() {
        let mut ws = Workspace::test_new("test");
        let child_id = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != child_id)
            .copied()
            .unwrap();
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Idle;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut child_terminal = terminal_for_pane(&ws, child_id);
        child_terminal.state = AgentState::Working;
        terminals.insert(child_terminal.id.clone(), child_terminal);
        let mut delegations = Delegations::new();
        let root = delegations.create(Some(root_id), None, None).unwrap();
        delegations
            .create(Some(child_id), Some(root), None)
            .unwrap();

        let summary = ws.attention_summary(&terminals, &delegations);
        assert_eq!(summary.display_state(), (AgentState::Idle, true));
        assert_eq!(summary.descendant_attention_count(), 0);
    }

    #[test]
    fn external_parent_is_local_root_for_tab_but_descendant_for_workspace() {
        let mut ws = Workspace::test_new("test");
        let root_id = ws.tabs[0].root_pane.expect("root");
        let child_tab = ws.test_add_tab(Some("child"));
        let child_id = ws.tabs[child_tab].root_pane.expect("child");
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Idle;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut child_terminal = terminal_for_pane(&ws, child_id);
        child_terminal.state = AgentState::Working;
        terminals.insert(child_terminal.id.clone(), child_terminal);
        let mut delegations = Delegations::new();
        let root = delegations.create(Some(root_id), None, None).unwrap();
        delegations
            .create(Some(child_id), Some(root), None)
            .unwrap();

        assert_eq!(
            ws.tabs[child_tab]
                .attention_summary(&terminals, &delegations)
                .display_state(),
            (AgentState::Working, true)
        );
        assert_eq!(
            ws.attention_summary(&terminals, &delegations)
                .display_state(),
            (AgentState::Idle, true)
        );
    }

    #[test]
    fn blocked_delegated_child_remains_space_urgent() {
        let mut ws = Workspace::test_new("test");
        let child_id = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != child_id)
            .copied()
            .unwrap();
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Working;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut child_terminal = terminal_for_pane(&ws, child_id);
        child_terminal.state = AgentState::Blocked;
        terminals.insert(child_terminal.id.clone(), child_terminal);

        let mut delegations = Delegations::new();
        let root = delegations.create(Some(root_id), None, None).unwrap();
        delegations
            .create(Some(child_id), Some(root), None)
            .unwrap();

        let summary = ws.attention_summary(&terminals, &delegations);
        assert_eq!(summary.display_state(), (AgentState::Blocked, false));
        assert_eq!(summary.blocked_descendants(), 1);
    }

    #[test]
    fn pane_details_prefers_agent_name_over_detected_agent_label() {
        let ws = Workspace::test_new("test");
        let root_pane = ws.tabs[0].root_pane.expect("test tab has root pane");
        let mut terminals = HashMap::new();
        let mut terminal = terminal_for_pane(&ws, root_pane);
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Working);
        terminal.set_agent_name("planner".into());
        terminals.insert(terminal.id.clone(), terminal);

        let labels: Vec<_> = ws
            .pane_details(&terminals)
            .into_iter()
            .map(|detail| (detail.label, detail.agent_label, detail.agent))
            .collect();

        assert_eq!(
            labels,
            vec![("planner".into(), "planner".into(), Some(Agent::Pi))]
        );
    }

    #[test]
    fn pane_details_includes_tab_context_for_multi_tab_workspace() {
        let mut ws = Workspace::test_new("test");
        ws.tabs[0].custom_name = Some("main".into());
        let root_pane = ws.tabs[0].root_pane.expect("test tab has root pane");
        let second_tab = ws.test_add_tab(Some("review"));
        let review_pane = ws.tabs[second_tab]
            .root_pane
            .expect("test tab has root pane");
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_pane);
        root_terminal.set_hook_authority(
            "test".into(),
            "pi".into(),
            AgentState::Working,
            None,
            None,
        );
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut review_terminal = terminal_for_pane(&ws, review_pane);
        review_terminal.set_hook_authority(
            "test".into(),
            "claude".into(),
            AgentState::Idle,
            None,
            None,
        );
        terminals.insert(review_terminal.id.clone(), review_terminal);

        let labels: Vec<_> = ws
            .pane_details(&terminals)
            .into_iter()
            .map(|detail| (detail.label, detail.agent_label, detail.agent))
            .collect();

        assert_eq!(
            labels,
            vec![
                ("main·pi".into(), "pi".into(), Some(Agent::Pi)),
                ("review·claude".into(), "claude".into(), Some(Agent::Claude)),
            ]
        );
    }

    #[test]
    fn pane_details_use_tab_vector_index_not_stable_public_tab_number() {
        let mut ws = Workspace::test_new("test");
        let removed_tab = ws.test_add_tab(Some("removed"));
        let survivor_tab = ws.test_add_tab(Some("survivor"));
        let survivor_pane = ws.tabs[survivor_tab]
            .root_pane
            .expect("test tab has root pane");
        assert!(ws.close_tab(removed_tab));

        let mut terminals = HashMap::new();
        let mut terminal = terminal_for_pane(&ws, survivor_pane);
        terminal.detected_agent = Some(Agent::Codex);
        terminals.insert(terminal.id.clone(), terminal);

        let details = ws.pane_details(&terminals);
        let survivor = details
            .iter()
            .find(|detail| detail.pane_id == survivor_pane)
            .expect("surviving tab agent should be listed");

        assert_eq!(ws.tabs[1].number, 3);
        assert_eq!(survivor.tab_idx, 1);
    }
}
