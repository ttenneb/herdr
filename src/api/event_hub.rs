#[derive(Clone, Default)]
pub struct EventHub {
    inner: std::sync::Arc<std::sync::Mutex<EventHubState>>,
}

#[derive(Default)]
struct EventHubState {
    next_sequence: u64,
    events: Vec<(u64, crate::api::schema::EventEnvelope)>,
    dropped_lifecycle_through: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventHistoryGap {
    pub requested_sequence: u64,
    pub oldest_available_sequence: u64,
    pub current_sequence: u64,
}

impl EventHub {
    const MAX_EVENTS: usize = 512;

    pub fn push(&self, event: crate::api::schema::EventEnvelope) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        state.next_sequence += 1;
        let sequence = state.next_sequence;
        state.events.push((sequence, event));
        let overflow = state.events.len().saturating_sub(Self::MAX_EVENTS);
        if overflow > 0 {
            let dropped_through = state.events[overflow - 1].0;
            state.events.drain(0..overflow);
            state.dropped_lifecycle_through = Some(dropped_through);
        }
    }

    /// Reserve the next session-global sequence for an observation that is not
    /// retained in lifecycle history.
    pub fn reserve_sequence(&self) -> u64 {
        let Ok(mut state) = self.inner.lock() else {
            return 0;
        };
        state.next_sequence += 1;
        state.next_sequence
    }

    /// Test-only accessor for assertions over retained history. Runtime cursor
    /// consumers must use `checked_events_after`, whose type requires handling
    /// truncation explicitly.
    #[cfg(test)]
    pub fn events_after(&self, sequence: u64) -> Vec<(u64, crate::api::schema::EventEnvelope)> {
        self.checked_events_after(sequence).unwrap_or_default()
    }

    pub fn checked_events_after(
        &self,
        sequence: u64,
    ) -> Result<Vec<(u64, crate::api::schema::EventEnvelope)>, EventHistoryGap> {
        self.checked_events_after_through(sequence, u64::MAX)
    }

    pub fn checked_events_after_through(
        &self,
        sequence: u64,
        watermark: u64,
    ) -> Result<Vec<(u64, crate::api::schema::EventEnvelope)>, EventHistoryGap> {
        let Ok(state) = self.inner.lock() else {
            return Ok(Vec::new());
        };
        let oldest_available_sequence = state
            .events
            .first()
            .map(|(sequence, _)| *sequence)
            .unwrap_or_else(|| state.next_sequence.saturating_add(1));
        if state
            .dropped_lifecycle_through
            .is_some_and(|dropped_through| sequence < dropped_through)
        {
            return Err(EventHistoryGap {
                requested_sequence: sequence,
                oldest_available_sequence,
                current_sequence: state.next_sequence,
            });
        }
        Ok(state
            .events
            .iter()
            .filter(|(event_sequence, _)| {
                *event_sequence > sequence && *event_sequence <= watermark
            })
            .cloned()
            .collect())
    }

    pub fn current_sequence(&self) -> u64 {
        let Ok(state) = self.inner.lock() else {
            return 0;
        };
        state.next_sequence
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{EventData, EventEnvelope, EventKind};

    fn event(index: usize) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::WorkspaceFocused,
            data: EventData::WorkspaceFocused {
                workspace_id: format!("workspace_{index}"),
            },
        }
    }

    #[test]
    fn history_overflow_is_an_explicit_gap() {
        let hub = EventHub::default();
        for index in 0..=EventHub::MAX_EVENTS {
            hub.push(event(index));
        }

        let gap = hub
            .checked_events_after(0)
            .expect_err("overflow must require resync");
        assert_eq!(gap.requested_sequence, 0);
        assert_eq!(gap.oldest_available_sequence, 2);
        assert_eq!(gap.current_sequence, 513);
        assert_eq!(
            hub.checked_events_after(1).unwrap().len(),
            EventHub::MAX_EVENTS
        );
    }

    #[test]
    fn reserved_observation_sequences_do_not_create_history_gaps() {
        let hub = EventHub::default();
        for _ in 0..600 {
            hub.reserve_sequence();
        }
        for index in 0..EventHub::MAX_EVENTS {
            hub.push(event(index));
        }

        assert_eq!(
            hub.checked_events_after(0).unwrap().len(),
            EventHub::MAX_EVENTS
        );

        hub.push(event(EventHub::MAX_EVENTS));
        let gap = hub
            .checked_events_after(600)
            .expect_err("the dropped lifecycle event requires resync");
        assert_eq!(gap.oldest_available_sequence, 602);
        assert_eq!(hub.checked_events_after(601).unwrap().len(), 512);
    }
}
