use regex::Regex;

use crate::api::schema::{
    ErrorBody, ErrorResponse, EventKind, Method, PaneAgentStatusChangedEvent,
    PaneOutputMatchedEvent, PaneScrollChangedEvent, PaneScrollInfo, Request,
    SequencedEventEnvelope, StreamEventEnvelope, Subscription, SubscriptionEventData,
    SubscriptionEventEnvelope, SubscriptionEventKind,
};
use crate::api::server::{
    dispatch_observation_to_app_with_timeout, dispatch_to_app_with_timeout, APP_RESPONSE_TIMEOUT,
};
use crate::api::{ApiRequestSender, EventHub};

pub(super) fn output_match_read_source(
    source: &crate::api::schema::ReadSource,
) -> crate::api::schema::ReadSource {
    match source {
        crate::api::schema::ReadSource::Recent => crate::api::schema::ReadSource::RecentUnwrapped,
        other => *other,
    }
}

pub(super) fn match_output(
    text: &str,
    matcher: &crate::api::schema::OutputMatch,
    regex: Option<&Regex>,
) -> Option<String> {
    match matcher {
        crate::api::schema::OutputMatch::Substring { value } => text
            .lines()
            .find(|line| line.contains(value))
            .map(|line| line.to_string()),
        crate::api::schema::OutputMatch::Regex { .. } => regex.and_then(|re| {
            text.lines()
                .find(|line| re.is_match(line))
                .map(|line| line.to_string())
        }),
    }
}

pub(super) struct ActiveOutputMatchedSubscription {
    pane_id: String,
    source: crate::api::schema::ReadSource,
    lines: Option<u32>,
    matcher: crate::api::schema::OutputMatch,
    regex: Option<Regex>,
    strip_ansi: bool,
    currently_matching: bool,
    request_prefix: String,
}

pub(super) struct ActiveAgentStatusChangedSubscription {
    pane_id: String,
    status_filter: Option<crate::api::schema::AgentStatus>,
    last_status: Option<crate::api::schema::AgentStatus>,
    last_presentation: Option<PanePresentationSnapshot>,
    last_sequence: u64,
    initial_event: Option<PaneAgentStatusChangedEvent>,
    request_id: String,
    request_prefix: String,
}

pub(super) struct ActiveScrollChangedSubscription {
    pane_id: String,
    last_scroll: Option<PaneScrollInfo>,
    request_prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PanePresentationSnapshot {
    title: Option<String>,
    display_agent: Option<String>,
    state_labels: std::collections::HashMap<String, String>,
}

impl PanePresentationSnapshot {
    fn from(pane: &crate::api::schema::PaneInfo) -> Self {
        Self {
            title: pane.title.clone(),
            display_agent: pane.display_agent.clone(),
            state_labels: pane.state_labels.clone(),
        }
    }

    fn from_event(
        title: &Option<String>,
        display_agent: &Option<String>,
        state_labels: &std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            title: title.clone(),
            display_agent: display_agent.clone(),
            state_labels: state_labels.clone(),
        }
    }
}

pub(super) struct ActiveEventSubscription {
    event_kind: crate::api::schema::EventKind,
    last_sequence: u64,
    request_id: String,
}

pub(super) enum ActiveSubscription {
    Event(ActiveEventSubscription),
    OutputMatched(ActiveOutputMatchedSubscription),
    AgentStatusChanged(Box<ActiveAgentStatusChangedSubscription>),
    ScrollChanged(ActiveScrollChangedSubscription),
}

impl ActiveSubscription {
    pub(super) fn new(
        subscription: Subscription,
        request_id: &str,
        index: usize,
        api_tx: &ApiRequestSender,
        _event_hub: &EventHub,
        event_start_sequence: u64,
    ) -> Result<Self, ErrorResponse> {
        let event_subscription = |event_kind| {
            Self::Event(ActiveEventSubscription {
                event_kind,
                last_sequence: event_start_sequence,
                request_id: request_id.to_string(),
            })
        };

        match subscription {
            Subscription::WorkspaceCreated {} => {
                Ok(event_subscription(EventKind::WorkspaceCreated))
            }
            Subscription::WorkspaceUpdated {} => {
                Ok(event_subscription(EventKind::WorkspaceUpdated))
            }
            Subscription::WorkspaceMetadataUpdated {} => {
                Ok(event_subscription(EventKind::WorkspaceMetadataUpdated))
            }
            Subscription::WorkspaceResourcesUpdated {} => {
                Ok(event_subscription(EventKind::WorkspaceResourcesUpdated))
            }
            Subscription::WorkspaceRenamed {} => {
                Ok(event_subscription(EventKind::WorkspaceRenamed))
            }
            Subscription::WorkspaceMoved {} => Ok(event_subscription(EventKind::WorkspaceMoved)),
            Subscription::WorkspaceReordered {} => {
                Ok(event_subscription(EventKind::WorkspaceReordered))
            }
            Subscription::WorkspaceClosed {} => Ok(event_subscription(EventKind::WorkspaceClosed)),
            Subscription::WorkspaceFocused {} => {
                Ok(event_subscription(EventKind::WorkspaceFocused))
            }
            Subscription::CheckoutOpened {} => Ok(event_subscription(EventKind::CheckoutOpened)),
            Subscription::CheckoutRenamed {} => Ok(event_subscription(EventKind::CheckoutRenamed)),
            Subscription::CheckoutMoved {} => Ok(event_subscription(EventKind::CheckoutMoved)),
            Subscription::CheckoutClosed {} => Ok(event_subscription(EventKind::CheckoutClosed)),
            Subscription::CheckoutFocused {} => Ok(event_subscription(EventKind::CheckoutFocused)),
            Subscription::RepositoryCreated {} => {
                Ok(event_subscription(EventKind::RepositoryCreated))
            }
            Subscription::RepositoryRenamed {} => {
                Ok(event_subscription(EventKind::RepositoryRenamed))
            }
            Subscription::RepositoryMoved {} => Ok(event_subscription(EventKind::RepositoryMoved)),
            Subscription::RepositoryClosed {} => {
                Ok(event_subscription(EventKind::RepositoryClosed))
            }
            Subscription::RepositoryFocused {} => {
                Ok(event_subscription(EventKind::RepositoryFocused))
            }
            Subscription::WorktreeCreated {} => Ok(event_subscription(EventKind::WorktreeCreated)),
            Subscription::WorktreeOpened {} => Ok(event_subscription(EventKind::WorktreeOpened)),
            Subscription::WorktreeRemoved {} => Ok(event_subscription(EventKind::WorktreeRemoved)),
            Subscription::TabCreated {} => Ok(event_subscription(EventKind::TabCreated)),
            Subscription::TabClosed {} => Ok(event_subscription(EventKind::TabClosed)),
            Subscription::TabFocused {} => Ok(event_subscription(EventKind::TabFocused)),
            Subscription::TabRenamed {} => Ok(event_subscription(EventKind::TabRenamed)),
            Subscription::TabMoved {} => Ok(event_subscription(EventKind::TabMoved)),
            Subscription::CollectionCreated {} => {
                Ok(event_subscription(EventKind::CollectionCreated))
            }
            Subscription::CollectionMemberAdded {} => {
                Ok(event_subscription(EventKind::CollectionMemberAdded))
            }
            Subscription::CollectionMemberMoved {} => {
                Ok(event_subscription(EventKind::CollectionMemberMoved))
            }
            Subscription::CollectionMemberRemoved {} => {
                Ok(event_subscription(EventKind::CollectionMemberRemoved))
            }
            Subscription::CollectionMemberPromoted {} => {
                Ok(event_subscription(EventKind::CollectionMemberPromoted))
            }
            Subscription::CollectionMemberSelected {} => {
                Ok(event_subscription(EventKind::CollectionMemberSelected))
            }
            Subscription::CollectionMembersReordered {} => {
                Ok(event_subscription(EventKind::CollectionMembersReordered))
            }
            Subscription::CollectionMemberArchived {} => {
                Ok(event_subscription(EventKind::CollectionMemberArchived))
            }
            Subscription::CollectionMemberRestored {} => {
                Ok(event_subscription(EventKind::CollectionMemberRestored))
            }
            Subscription::CollectionClosed {} => {
                Ok(event_subscription(EventKind::CollectionClosed))
            }
            Subscription::DelegationCreated {} => {
                Ok(event_subscription(EventKind::DelegationCreated))
            }
            Subscription::DelegationReparented {} => {
                Ok(event_subscription(EventKind::DelegationReparented))
            }
            Subscription::DelegationReordered {} => {
                Ok(event_subscription(EventKind::DelegationReordered))
            }
            Subscription::DelegationTombstoned {} => {
                Ok(event_subscription(EventKind::DelegationTombstoned))
            }
            Subscription::DelegationGarbageCollected {} => {
                Ok(event_subscription(EventKind::DelegationGarbageCollected))
            }
            Subscription::PaneCreated {} => Ok(event_subscription(EventKind::PaneCreated)),
            Subscription::PaneClosed {} => Ok(event_subscription(EventKind::PaneClosed)),
            Subscription::PaneUpdated {} => Ok(event_subscription(EventKind::PaneUpdated)),
            Subscription::PaneFocused {} => Ok(event_subscription(EventKind::PaneFocused)),
            Subscription::PaneMoved {} => Ok(event_subscription(EventKind::PaneMoved)),
            Subscription::PaneExited {} => Ok(event_subscription(EventKind::PaneExited)),
            Subscription::PaneAgentDetected {} => {
                Ok(event_subscription(EventKind::PaneAgentDetected))
            }
            Subscription::LayoutUpdated {} => Ok(event_subscription(EventKind::LayoutUpdated)),
            Subscription::PaneOutputMatched {
                pane_id,
                source,
                lines,
                r#match,
                strip_ansi,
            } => {
                let regex = match &r#match {
                    crate::api::schema::OutputMatch::Regex { value } => match Regex::new(value) {
                        Ok(regex) => Some(regex),
                        Err(err) => {
                            return Err(ErrorResponse {
                                id: request_id.to_string(),
                                error: ErrorBody {
                                    code: "invalid_regex".into(),
                                    message: err.to_string(),
                                },
                            });
                        }
                    },
                    crate::api::schema::OutputMatch::Substring { .. } => None,
                };

                let probe = pane_read(
                    format!("{request_id}:sub:{index}:probe"),
                    &pane_id,
                    source,
                    lines,
                    strip_ansi,
                    api_tx,
                )
                .map_err(|response| with_request_id(response, request_id));
                probe?;

                Ok(Self::OutputMatched(ActiveOutputMatchedSubscription {
                    pane_id,
                    source,
                    lines,
                    matcher: r#match,
                    regex,
                    strip_ansi,
                    currently_matching: false,
                    request_prefix: format!("{request_id}:sub:{index}"),
                }))
            }
            Subscription::PaneAgentStatusChanged {
                pane_id,
                agent_status,
            } => {
                // Every subscription in one request starts from the cursor
                // accepted by stream_subscriptions. The probe may reflect a
                // later state, but replay from that common cursor wins so a
                // transient setup-window transition cannot be lost.
                let last_sequence = event_start_sequence;
                let probe = pane_get(format!("{request_id}:sub:{index}:probe"), &pane_id, api_tx)
                    .map_err(|response| with_request_id(response, request_id))?;
                let last_status = probe.agent_status;
                let last_presentation = PanePresentationSnapshot::from(&probe);
                let initial_event = agent_status
                    .is_some_and(|wanted| wanted == probe.agent_status)
                    .then_some(PaneAgentStatusChangedEvent {
                        pane_id: probe.pane_id.clone(),
                        workspace_id: probe.workspace_id,
                        agent_status: probe.agent_status,
                        agent: probe.agent,
                        title: probe.title,
                        display_agent: probe.display_agent,
                        state_labels: probe.state_labels,
                    });

                Ok(Self::AgentStatusChanged(Box::new(
                    ActiveAgentStatusChangedSubscription {
                        pane_id: probe.pane_id,
                        status_filter: agent_status,
                        last_status: Some(last_status),
                        last_presentation: Some(last_presentation),
                        last_sequence,
                        initial_event,
                        request_id: request_id.to_string(),
                        request_prefix: format!("{request_id}:sub:{index}"),
                    },
                )))
            }
            Subscription::PaneScrollChanged { pane_id } => {
                let probe = pane_get(format!("{request_id}:sub:{index}:probe"), &pane_id, api_tx)
                    .map_err(|response| with_request_id(response, request_id))?;

                Ok(Self::ScrollChanged(ActiveScrollChangedSubscription {
                    pane_id: probe.pane_id,
                    last_scroll: probe.scroll,
                    request_prefix: format!("{request_id}:sub:{index}"),
                }))
            }
        }
    }

    pub(super) fn poll_through(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
        watermark: u64,
    ) -> Result<Option<StreamEventEnvelope>, ErrorResponse> {
        match self {
            Self::Event(subscription) => subscription.poll_through(event_hub, watermark),
            Self::OutputMatched(subscription) => Ok(subscription
                .poll(api_tx, event_hub)
                .map(StreamEventEnvelope::Subscription)),
            Self::AgentStatusChanged(subscription) => {
                match subscription.poll_result(api_tx, event_hub, watermark) {
                    Ok(event) => Ok(event.map(StreamEventEnvelope::Subscription)),
                    Err(response) if response.error.code == "resync_required" => Err(response),
                    Err(_) => Ok(None),
                }
            }
            Self::ScrollChanged(subscription) => Ok(subscription
                .poll(api_tx, event_hub)
                .map(StreamEventEnvelope::Subscription)),
        }
    }

    #[cfg(test)]
    pub(super) fn poll(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Result<Option<StreamEventEnvelope>, ErrorResponse> {
        self.poll_through(api_tx, event_hub, event_hub.current_sequence())
    }

    pub(super) fn poll_for_wait(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Result<Option<serde_json::Value>, ErrorResponse> {
        match self {
            Self::AgentStatusChanged(subscription) => Ok(subscription
                .poll_result(api_tx, event_hub, event_hub.current_sequence())?
                .and_then(|event| serde_json::to_value(event).ok())),
            _ => Ok(self
                .poll_through(api_tx, event_hub, event_hub.current_sequence())?
                .and_then(|event| serde_json::to_value(event).ok())),
        }
    }
}

impl ActiveEventSubscription {
    fn poll_through(
        &mut self,
        event_hub: &EventHub,
        watermark: u64,
    ) -> Result<Option<StreamEventEnvelope>, ErrorResponse> {
        let events = event_hub
            .checked_events_after_through(self.last_sequence, watermark)
            .map_err(|gap| history_gap_error(&self.request_id, gap))?;
        for (sequence, event) in events {
            self.last_sequence = sequence;
            if event.event == self.event_kind {
                return Ok(Some(StreamEventEnvelope::Event(Box::new(
                    SequencedEventEnvelope { sequence, event },
                ))));
            }
        }
        self.last_sequence = self.last_sequence.max(watermark);
        Ok(None)
    }
}

impl ActiveOutputMatchedSubscription {
    fn poll(
        &mut self,
        api_tx: &ApiRequestSender,
        _event_hub: &EventHub,
    ) -> Option<SubscriptionEventEnvelope> {
        let read = pane_read(
            format!("{}:read", self.request_prefix),
            &self.pane_id,
            output_match_read_source(&self.source),
            self.lines,
            self.strip_ansi,
            api_tx,
        )
        .ok()?;

        let matched = match_output(&read.text, &self.matcher, self.regex.as_ref()).is_some();
        if !matched {
            self.currently_matching = false;
            return None;
        }
        if self.currently_matching {
            return None;
        }

        // Confirm the transition and reserve its sequence in the same app-thread
        // turn, so snapshots cannot interleave between the observation and cursor.
        let (read, sequence) = pane_read_observation(
            format!("{}:read:observation", self.request_prefix),
            &self.pane_id,
            output_match_read_source(&self.source),
            self.lines,
            self.strip_ansi,
            api_tx,
        )
        .ok()?;
        let Some(matched_line) = match_output(&read.text, &self.matcher, self.regex.as_ref())
        else {
            self.currently_matching = false;
            return None;
        };
        self.currently_matching = true;
        Some(SubscriptionEventEnvelope {
            sequence,
            event: SubscriptionEventKind::PaneOutputMatched,
            data: SubscriptionEventData::PaneOutputMatched(PaneOutputMatchedEvent {
                pane_id: read.pane_id.clone(),
                matched_line,
                read,
            }),
        })
    }
}

impl ActiveAgentStatusChangedSubscription {
    #[cfg(test)]
    fn poll(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Option<SubscriptionEventEnvelope> {
        self.poll_result(api_tx, event_hub, event_hub.current_sequence())
            .ok()
            .flatten()
    }

    fn poll_result(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
        watermark: u64,
    ) -> Result<Option<SubscriptionEventEnvelope>, ErrorResponse> {
        let mut saw_status_event = false;
        let events = event_hub
            .checked_events_after_through(self.last_sequence, watermark)
            .map_err(|gap| history_gap_error(&self.request_id, gap))?;
        for (sequence, event) in events {
            self.last_sequence = sequence;
            let crate::api::schema::EventData::PaneAgentStatusChanged {
                pane_id,
                workspace_id,
                agent_status,
                agent,
                title,
                display_agent,
                state_labels,
            } = event.data
            else {
                continue;
            };
            if event.event != crate::api::schema::EventKind::PaneAgentStatusChanged {
                continue;
            }
            if pane_id != self.pane_id {
                continue;
            }
            saw_status_event = true;

            let current_presentation =
                PanePresentationSnapshot::from_event(&title, &display_agent, &state_labels);
            self.last_status = Some(agent_status);
            self.last_presentation = Some(current_presentation);
            if self
                .status_filter
                .is_some_and(|wanted| wanted != agent_status)
            {
                continue;
            }

            self.initial_event = None;
            return Ok(Some(SubscriptionEventEnvelope {
                sequence,
                event: SubscriptionEventKind::PaneAgentStatusChanged,
                data: SubscriptionEventData::PaneAgentStatusChanged(PaneAgentStatusChangedEvent {
                    pane_id,
                    workspace_id,
                    agent_status,
                    agent,
                    title,
                    display_agent,
                    state_labels,
                }),
            }));
        }

        self.last_sequence = self.last_sequence.max(watermark);
        if saw_status_event {
            self.initial_event = None;
        } else if self.initial_event.is_some() {
            let (pane, sequence) = pane_get_observation(
                format!("{}:pane:initial", self.request_prefix),
                &self.pane_id,
                api_tx,
            )
            .map_err(|response| with_request_id(response, &self.request_id))?;
            if self.has_relevant_status_event_through(event_hub, sequence)? {
                return Ok(None);
            }
            self.initial_event = None;
            self.last_status = Some(pane.agent_status);
            self.last_presentation = Some(PanePresentationSnapshot::from(&pane));
            if self
                .status_filter
                .is_some_and(|wanted| wanted == pane.agent_status)
            {
                return Ok(Some(agent_status_observation(pane, sequence)));
            }
        }

        let pane = pane_get(
            format!("{}:pane", self.request_prefix),
            &self.pane_id,
            api_tx,
        )
        .map_err(|response| with_request_id(response, &self.request_id))?;
        if !self.snapshot_changed(&pane) {
            return Ok(None);
        }

        let (pane, sequence) = pane_get_observation(
            format!("{}:pane:observation", self.request_prefix),
            &self.pane_id,
            api_tx,
        )
        .map_err(|response| with_request_id(response, &self.request_id))?;
        if self.has_relevant_status_event_through(event_hub, sequence)? {
            return Ok(None);
        }
        Ok(self.event_from_snapshot(pane, sequence))
    }

    fn has_relevant_status_event_through(
        &self,
        event_hub: &EventHub,
        sequence: u64,
    ) -> Result<bool, ErrorResponse> {
        let events = event_hub
            .checked_events_after_through(self.last_sequence, sequence)
            .map_err(|gap| history_gap_error(&self.request_id, gap))?;
        Ok(events.into_iter().any(|(_, event)| {
            event.event == crate::api::schema::EventKind::PaneAgentStatusChanged
                && matches!(
                    event.data,
                    crate::api::schema::EventData::PaneAgentStatusChanged { pane_id, .. }
                        if pane_id == self.pane_id
                )
        }))
    }

    fn snapshot_changed(&self, pane: &crate::api::schema::PaneInfo) -> bool {
        let current_presentation = PanePresentationSnapshot::from(pane);
        self.last_status
            .is_some_and(|previous| previous != pane.agent_status)
            || self
                .last_presentation
                .as_ref()
                .is_some_and(|previous| previous != &current_presentation)
    }

    fn event_from_snapshot(
        &mut self,
        pane: crate::api::schema::PaneInfo,
        sequence: u64,
    ) -> Option<SubscriptionEventEnvelope> {
        let current_status = pane.agent_status;
        let current_presentation = PanePresentationSnapshot::from(&pane);
        let changed = self.snapshot_changed(&pane);
        self.last_status = Some(current_status);
        self.last_presentation = Some(current_presentation);
        if !changed
            || self
                .status_filter
                .is_some_and(|wanted| wanted != current_status)
        {
            return None;
        }

        Some(SubscriptionEventEnvelope {
            sequence,
            event: SubscriptionEventKind::PaneAgentStatusChanged,
            data: SubscriptionEventData::PaneAgentStatusChanged(PaneAgentStatusChangedEvent {
                pane_id: pane.pane_id,
                workspace_id: pane.workspace_id,
                agent_status: current_status,
                agent: pane.agent,
                title: pane.title,
                display_agent: pane.display_agent,
                state_labels: pane.state_labels,
            }),
        })
    }
}

fn agent_status_observation(
    pane: crate::api::schema::PaneInfo,
    sequence: u64,
) -> SubscriptionEventEnvelope {
    SubscriptionEventEnvelope {
        sequence,
        event: SubscriptionEventKind::PaneAgentStatusChanged,
        data: SubscriptionEventData::PaneAgentStatusChanged(PaneAgentStatusChangedEvent {
            pane_id: pane.pane_id,
            workspace_id: pane.workspace_id,
            agent_status: pane.agent_status,
            agent: pane.agent,
            title: pane.title,
            display_agent: pane.display_agent,
            state_labels: pane.state_labels,
        }),
    }
}

impl ActiveScrollChangedSubscription {
    fn poll(
        &mut self,
        api_tx: &ApiRequestSender,
        _event_hub: &EventHub,
    ) -> Option<SubscriptionEventEnvelope> {
        let pane = pane_get(
            format!("{}:pane", self.request_prefix),
            &self.pane_id,
            api_tx,
        )
        .ok()?;
        if self.last_scroll == pane.scroll {
            return None;
        }
        let (pane, sequence) = pane_get_observation(
            format!("{}:pane:observation", self.request_prefix),
            &self.pane_id,
            api_tx,
        )
        .ok()?;
        self.event_from_snapshot(pane, sequence)
    }

    fn event_from_snapshot(
        &mut self,
        pane: crate::api::schema::PaneInfo,
        sequence: u64,
    ) -> Option<SubscriptionEventEnvelope> {
        let scroll = pane.scroll;
        if self.last_scroll == scroll {
            return None;
        }
        self.last_scroll = scroll;
        let scroll = scroll?;

        Some(SubscriptionEventEnvelope {
            sequence,
            event: SubscriptionEventKind::ScrollChanged,
            data: SubscriptionEventData::ScrollChanged(PaneScrollChangedEvent {
                pane_id: pane.pane_id,
                workspace_id: pane.workspace_id,
                scroll,
            }),
        })
    }
}

fn with_request_id(mut response: ErrorResponse, request_id: &str) -> ErrorResponse {
    response.id = request_id.to_string();
    response
}

fn history_gap_error(
    request_id: &str,
    gap: crate::api::event_hub::EventHistoryGap,
) -> ErrorResponse {
    ErrorResponse {
        id: request_id.to_string(),
        error: ErrorBody {
            code: "resync_required".into(),
            message: format!(
                "event history after sequence {} is unavailable; oldest available is {} (current {})",
                gap.requested_sequence, gap.oldest_available_sequence, gap.current_sequence
            ),
        },
    }
}

fn pane_read(
    request_id: String,
    pane_id: &str,
    source: crate::api::schema::ReadSource,
    lines: Option<u32>,
    strip_ansi: bool,
    api_tx: &ApiRequestSender,
) -> Result<crate::api::schema::PaneReadResult, ErrorResponse> {
    pane_read_response(
        request_id, pane_id, source, lines, strip_ansi, api_tx, false,
    )
    .map(|(read, _)| read)
}

fn pane_read_observation(
    request_id: String,
    pane_id: &str,
    source: crate::api::schema::ReadSource,
    lines: Option<u32>,
    strip_ansi: bool,
    api_tx: &ApiRequestSender,
) -> Result<(crate::api::schema::PaneReadResult, u64), ErrorResponse> {
    let (read, sequence) = pane_read_response(
        request_id.clone(),
        pane_id,
        source,
        lines,
        strip_ansi,
        api_tx,
        true,
    )?;
    sequence
        .map(|sequence| (read, sequence))
        .ok_or(ErrorResponse {
            id: request_id,
            error: ErrorBody {
                code: "internal_error".into(),
                message: "pane read observation omitted sequence".into(),
            },
        })
}

#[allow(clippy::too_many_arguments)]
fn pane_read_response(
    request_id: String,
    pane_id: &str,
    source: crate::api::schema::ReadSource,
    lines: Option<u32>,
    strip_ansi: bool,
    api_tx: &ApiRequestSender,
    observation: bool,
) -> Result<(crate::api::schema::PaneReadResult, Option<u64>), ErrorResponse> {
    let request = Request {
        id: request_id.clone(),
        method: Method::PaneRead(crate::api::schema::PaneReadParams {
            pane_id: pane_id.to_string(),
            source,
            lines,
            format: crate::api::schema::ReadFormat::Text,
            strip_ansi,
            intent: crate::api::schema::ReadIntent::Passive,
        }),
    };
    let response = if observation {
        dispatch_observation_to_app_with_timeout(request, api_tx, Some(APP_RESPONSE_TIMEOUT))
    } else {
        dispatch_to_app_with_timeout(request, api_tx, Some(APP_RESPONSE_TIMEOUT))
    };
    let value: serde_json::Value = serde_json::from_str(&response).map_err(|_| ErrorResponse {
        id: request_id.clone(),
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane read response".into(),
        },
    })?;
    if value.get("error").is_some() {
        return serde_json::from_value(value).map_err(|_| ErrorResponse {
            id: request_id,
            error: ErrorBody {
                code: "internal_error".into(),
                message: "failed to decode pane read error".into(),
            },
        });
    }
    let sequence = value
        .get("observation_sequence")
        .and_then(|value| value.as_u64());
    let read =
        serde_json::from_value(value["result"]["read"].clone()).map_err(|_| ErrorResponse {
            id: request_id,
            error: ErrorBody {
                code: "internal_error".into(),
                message: "failed to decode pane read result".into(),
            },
        })?;
    Ok((read, sequence))
}

fn pane_get(
    request_id: String,
    pane_id: &str,
    api_tx: &ApiRequestSender,
) -> Result<crate::api::schema::PaneInfo, ErrorResponse> {
    pane_get_response(request_id, pane_id, api_tx, false).map(|(pane, _)| pane)
}

fn pane_get_observation(
    request_id: String,
    pane_id: &str,
    api_tx: &ApiRequestSender,
) -> Result<(crate::api::schema::PaneInfo, u64), ErrorResponse> {
    let (pane, sequence) = pane_get_response(request_id.clone(), pane_id, api_tx, true)?;
    sequence
        .map(|sequence| (pane, sequence))
        .ok_or(ErrorResponse {
            id: request_id,
            error: ErrorBody {
                code: "internal_error".into(),
                message: "pane get observation omitted sequence".into(),
            },
        })
}

fn pane_get_response(
    request_id: String,
    pane_id: &str,
    api_tx: &ApiRequestSender,
    observation: bool,
) -> Result<(crate::api::schema::PaneInfo, Option<u64>), ErrorResponse> {
    let request = Request {
        id: request_id.clone(),
        method: Method::PaneGet(crate::api::schema::PaneTarget {
            pane_id: pane_id.to_string(),
        }),
    };
    let response = if observation {
        dispatch_observation_to_app_with_timeout(request, api_tx, Some(APP_RESPONSE_TIMEOUT))
    } else {
        dispatch_to_app_with_timeout(request, api_tx, Some(APP_RESPONSE_TIMEOUT))
    };
    let value: serde_json::Value = serde_json::from_str(&response).map_err(|_| ErrorResponse {
        id: request_id.clone(),
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane get response".into(),
        },
    })?;
    if value.get("error").is_some() {
        let response =
            serde_json::from_value::<ErrorResponse>(value).map_err(|_| ErrorResponse {
                id: request_id,
                error: ErrorBody {
                    code: "internal_error".into(),
                    message: "failed to decode pane get error".into(),
                },
            })?;
        return Err(response);
    }
    let sequence = value
        .get("observation_sequence")
        .and_then(|value| value.as_u64());
    let pane =
        serde_json::from_value(value["result"]["pane"].clone()).map_err(|_| ErrorResponse {
            id: request_id,
            error: ErrorBody {
                code: "internal_error".into(),
                message: "failed to decode pane get result".into(),
            },
        })?;
    Ok((pane, sequence))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::api::schema::{AgentStatus, EventData, EventEnvelope, EventKind, PaneInfo};

    fn presentation_event(title: Option<&str>) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::PaneAgentStatusChanged,
            data: EventData::PaneAgentStatusChanged {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: title.map(str::to_string),
                display_agent: None,
                state_labels: HashMap::new(),
            },
        }
    }

    fn workspace_focused_event(workspace_id: &str) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::WorkspaceFocused,
            data: EventData::WorkspaceFocused {
                workspace_id: workspace_id.into(),
            },
        }
    }

    fn pane_info_with_scroll(scroll: Option<PaneScrollInfo>) -> PaneInfo {
        PaneInfo {
            pane_id: "pane_1".into(),
            terminal_id: "terminal_1".into(),
            workspace_id: "workspace_1".into(),
            tab_id: "tab_1".into(),
            focused: true,
            placement: crate::api::schema::PanePlacementInfo::Tiled,
            cwd: None,
            foreground_cwd: None,
            label: None,
            agent: None,
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status: AgentStatus::Unknown,
            state_labels: HashMap::new(),
            tokens: HashMap::new(),
            agent_session: None,
            scroll,
            revision: 0,
        }
    }

    #[test]
    fn lifecycle_subscription_skips_history_but_keeps_setup_window_events() {
        let event_hub = EventHub::default();
        event_hub.push(workspace_focused_event("before_subscription"));
        let event_start_sequence = event_hub.current_sequence();
        event_hub.push(workspace_focused_event("during_setup"));

        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscription = ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "test",
            0,
            &api_tx,
            &event_hub,
            event_start_sequence,
        )
        .expect("workspace focus subscription");

        let setup_event = subscription
            .poll(&api_tx, &event_hub)
            .expect("setup poll")
            .expect("setup-window event");
        let setup_event = serde_json::to_value(setup_event).unwrap();
        assert_eq!(setup_event["sequence"], 2);
        assert_eq!(setup_event["data"]["workspace_id"], "during_setup");
        assert!(subscription
            .poll(&api_tx, &event_hub)
            .expect("empty poll")
            .is_none());

        event_hub.push(workspace_focused_event("after_setup"));
        let live_event = subscription
            .poll(&api_tx, &event_hub)
            .expect("live poll")
            .expect("live event");
        let live_event = serde_json::to_value(live_event).unwrap();
        assert_eq!(live_event["sequence"], 3);
        assert_eq!(live_event["data"]["workspace_id"], "after_setup");
    }

    #[test]
    fn lifecycle_subscription_surfaces_history_overflow_as_resync_required() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscription = ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "overflow",
            0,
            &api_tx,
            &event_hub,
            0,
        )
        .expect("workspace focus subscription");

        for index in 0..513 {
            event_hub.push(workspace_focused_event(&format!("workspace_{index}")));
        }

        let error = subscription
            .poll(&api_tx, &event_hub)
            .expect_err("truncated history must not look empty");
        assert_eq!(error.id, "overflow");
        assert_eq!(error.error.code, "resync_required");
        assert!(error.error.message.contains("oldest available is 2"));
    }

    #[test]
    fn history_gap_error_preserves_request_ids_containing_internal_delimiter() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            pane_id: "pane_1".into(),
            status_filter: None,
            last_status: Some(AgentStatus::Working),
            last_presentation: None,
            last_sequence: 0,
            initial_event: None,
            request_id: "job:sub:retry".into(),
            request_prefix: "job:sub:retry:sub:0".into(),
        };
        for index in 0..513 {
            event_hub.push(workspace_focused_event(&format!("workspace_{index}")));
        }

        let error = subscription
            .poll_result(
                &tokio::sync::mpsc::unbounded_channel().0,
                &event_hub,
                event_hub.current_sequence(),
            )
            .expect_err("overflow must preserve the public request id");
        assert_eq!(error.id, "job:sub:retry");
        assert_eq!(error.error.code, "resync_required");
    }

    #[test]
    fn workspace_metadata_subscription_uses_dedicated_event_kind() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let subscription = ActiveSubscription::new(
            Subscription::WorkspaceMetadataUpdated {},
            "test",
            0,
            &api_tx,
            &event_hub,
            event_hub.current_sequence(),
        )
        .expect("workspace metadata subscription");

        assert!(matches!(
            subscription,
            ActiveSubscription::Event(ActiveEventSubscription {
                event_kind: EventKind::WorkspaceMetadataUpdated,
                ..
            })
        ));
    }

    #[test]
    fn post_snapshot_scroll_observation_reserves_new_sequence() {
        let at_bottom = PaneScrollInfo {
            offset_from_bottom: 0,
            max_offset_from_bottom: 40,
            viewport_rows: 20,
        };
        let scrolled_back = PaneScrollInfo {
            offset_from_bottom: 8,
            max_offset_from_bottom: 40,
            viewport_rows: 20,
        };
        let event_hub = EventHub::default();
        let mut subscription = ActiveScrollChangedSubscription {
            pane_id: "pane_1".into(),
            last_scroll: Some(at_bottom),
            request_prefix: "test".into(),
        };

        assert!(subscription
            .event_from_snapshot(
                pane_info_with_scroll(Some(at_bottom)),
                event_hub.current_sequence(),
            )
            .is_none());

        let snapshot_watermark = event_hub.current_sequence();
        let event = subscription
            .event_from_snapshot(
                pane_info_with_scroll(Some(scrolled_back)),
                event_hub.reserve_sequence(),
            )
            .expect("scroll event");
        assert!(event.sequence > snapshot_watermark);
        assert_eq!(event.event, SubscriptionEventKind::ScrollChanged);
        let SubscriptionEventData::ScrollChanged(data) = event.data else {
            panic!("wrong event data");
        };
        assert_eq!(data.pane_id, "pane_1");
        assert_eq!(data.workspace_id, "workspace_1");
        assert_eq!(data.scroll, scrolled_back);
    }

    #[test]
    fn stream_ignores_transient_live_pane_probe_errors() {
        let event_hub = EventHub::default();
        let (api_tx, mut api_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::api::ApiRequestMessage>();
        let responder = std::thread::spawn(move || {
            let setup = api_rx.blocking_recv().expect("setup probe");
            setup
                .respond_to
                .send(
                    serde_json::json!({
                        "id": setup.request.id,
                        "result": {
                            "type": "pane_info",
                            "pane": pane_info_with_scroll(None)
                        }
                    })
                    .to_string(),
                )
                .unwrap();
            let live = api_rx.blocking_recv().expect("live probe");
            live.respond_to
                .send(
                    serde_json::json!({
                        "id": live.request.id,
                        "error": {"code": "pane_not_found", "message": "transient"}
                    })
                    .to_string(),
                )
                .unwrap();
        });
        let mut subscription = ActiveSubscription::new(
            Subscription::PaneAgentStatusChanged {
                pane_id: "pane_1".into(),
                agent_status: None,
            },
            "probe:sub:id",
            0,
            &api_tx,
            &event_hub,
            0,
        )
        .unwrap();

        assert!(subscription
            .poll_through(&api_tx, &event_hub, 0)
            .expect("transient stream probe errors are ignored")
            .is_none());
        responder.join().unwrap();
    }

    #[test]
    fn agent_status_setup_replays_transient_changes_from_common_acceptance_cursor() {
        let event_hub = EventHub::default();
        let responder_hub = event_hub.clone();
        let (api_tx, mut api_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::api::ApiRequestMessage>();
        let responder = std::thread::spawn(move || {
            let request = api_rx.blocking_recv().expect("pane probe request");
            assert!(matches!(request.request.method, Method::PaneGet(_)));
            responder_hub.push(presentation_event(Some("transient")));
            responder_hub.push(presentation_event(None));
            let mut pane = pane_info_with_scroll(None);
            pane.agent = Some("pi".into());
            pane.agent_status = AgentStatus::Working;
            request
                .respond_to
                .send(
                    serde_json::json!({
                        "id": request.request.id,
                        "result": {"type": "pane_info", "pane": pane}
                    })
                    .to_string(),
                )
                .unwrap();
        });

        let mut subscription = ActiveSubscription::new(
            Subscription::PaneAgentStatusChanged {
                pane_id: "pane_1".into(),
                agent_status: None,
            },
            "setup",
            0,
            &api_tx,
            &event_hub,
            0,
        )
        .expect("agent status subscription");

        let first = subscription
            .poll(&api_tx, &event_hub)
            .unwrap()
            .expect("transient set");
        let second = subscription
            .poll(&api_tx, &event_hub)
            .unwrap()
            .expect("transient revert");
        assert_eq!(first.sequence(), 1);
        assert_eq!(second.sequence(), 2);
        let first = serde_json::to_value(first).unwrap();
        let second = serde_json::to_value(second).unwrap();
        assert_eq!(first["data"]["title"], "transient");
        assert!(second["data"].get("title").is_none());
        responder.join().unwrap();
    }

    #[test]
    fn agent_status_subscription_replays_queued_metadata_set_and_expiry_events() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            pane_id: "pane_1".into(),
            status_filter: None,
            last_status: Some(AgentStatus::Working),
            last_presentation: Some(PanePresentationSnapshot {
                title: None,
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            last_sequence: event_hub.current_sequence(),
            initial_event: None,
            request_id: "test".into(),
            request_prefix: "test".into(),
        };

        event_hub.push(presentation_event(Some("short lived")));
        event_hub.push(presentation_event(None));

        let set_event = subscription
            .poll(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("set event");
        let SubscriptionEventData::PaneAgentStatusChanged(set_data) = set_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(set_data.title.as_deref(), Some("short lived"));

        let expiry_event = subscription
            .poll(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("expiry event");
        let SubscriptionEventData::PaneAgentStatusChanged(expiry_data) = expiry_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(expiry_data.title, None);
    }

    #[test]
    fn agent_status_subscription_prefers_setup_window_events_over_initial_snapshot() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            pane_id: "pane_1".into(),
            status_filter: Some(AgentStatus::Working),
            last_status: Some(AgentStatus::Working),
            last_presentation: Some(PanePresentationSnapshot {
                title: None,
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            last_sequence: event_hub.current_sequence(),
            initial_event: Some(PaneAgentStatusChangedEvent {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: None,
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            request_id: "test".into(),
            request_prefix: "test".into(),
        };

        event_hub.push(presentation_event(Some("short lived")));
        event_hub.push(presentation_event(None));

        let set_event = subscription
            .poll(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("set event");
        let SubscriptionEventData::PaneAgentStatusChanged(set_data) = set_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(set_data.title.as_deref(), Some("short lived"));

        let expiry_event = subscription
            .poll(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("expiry event");
        let SubscriptionEventData::PaneAgentStatusChanged(expiry_data) = expiry_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(expiry_data.title, None);
    }

    #[test]
    fn agent_status_subscription_emits_setup_window_event_already_reflected_by_probe() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            pane_id: "pane_1".into(),
            status_filter: Some(AgentStatus::Working),
            last_status: Some(AgentStatus::Working),
            last_presentation: Some(PanePresentationSnapshot {
                title: Some("short lived".into()),
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            last_sequence: event_hub.current_sequence(),
            initial_event: Some(PaneAgentStatusChangedEvent {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: Some("short lived".into()),
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            request_id: "test".into(),
            request_prefix: "test".into(),
        };

        event_hub.push(presentation_event(Some("short lived")));

        let event = subscription
            .poll(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("setup-window event");
        let SubscriptionEventData::PaneAgentStatusChanged(data) = event.data else {
            panic!("wrong event data");
        };
        assert_eq!(data.title.as_deref(), Some("short lived"));
        assert!(subscription.initial_event.is_none());
    }
}
