use crate::{ChannelConfig, ChannelId, ChannelState, TransportMetrics};

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelRouteState {
    pub channel_id: ChannelId,
    pub state: ChannelState,
    pub sent_packets: u64,
    pub received_packets: u64,
    pub send_errors: u64,
    pub queue_pressure: f32,
}

impl ChannelRouteState {
    fn new(channel_id: ChannelId) -> Self {
        Self {
            channel_id,
            state: ChannelState::Up,
            sent_packets: 0,
            received_packets: 0,
            send_errors: 0,
            queue_pressure: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelRouter {
    channels: Vec<ChannelRouteState>,
    cursor: usize,
}

impl ChannelRouter {
    pub fn new(channel_ids: impl IntoIterator<Item = ChannelId>) -> Self {
        let channels = channel_ids
            .into_iter()
            .map(ChannelRouteState::new)
            .collect();
        Self {
            channels,
            cursor: 0,
        }
    }

    pub fn from_configs(configs: &[ChannelConfig]) -> Self {
        Self::new(configs.iter().map(|channel| channel.channel_id))
    }

    pub fn select_channel(&mut self) -> Option<ChannelId> {
        self.select_by_state(ChannelState::Up)
            .or_else(|| self.select_by_state(ChannelState::Degraded))
    }

    pub fn select_candidate(
        &mut self,
        candidates: impl IntoIterator<Item = ChannelId>,
    ) -> Option<ChannelId> {
        let candidates: Vec<_> = candidates.into_iter().collect();
        self.select_candidate_by_state(&candidates, ChannelState::Up)
            .or_else(|| self.select_candidate_by_state(&candidates, ChannelState::Degraded))
    }

    pub fn record_send(&mut self, channel_id: ChannelId) {
        if let Some(channel) = self.channel_mut(channel_id) {
            channel.sent_packets = channel.sent_packets.saturating_add(1);
        }
    }

    pub fn record_receive(&mut self, channel_id: ChannelId) {
        if let Some(channel) = self.channel_mut(channel_id) {
            channel.received_packets = channel.received_packets.saturating_add(1);
        }
    }

    pub fn update_channel(
        &mut self,
        channel_id: ChannelId,
        state: ChannelState,
        metrics: TransportMetrics,
    ) {
        if let Some(channel) = self.channel_mut(channel_id) {
            channel.state = state;
            channel.queue_pressure = metrics.queue_pressure;
            if metrics.send_error {
                channel.send_errors = channel.send_errors.saturating_add(1);
            }
        }
    }

    pub fn channel_ids(&self) -> impl Iterator<Item = ChannelId> + '_ {
        self.channels.iter().map(|channel| channel.channel_id)
    }

    pub fn is_usable(&self, channel_id: ChannelId) -> bool {
        self.channels.iter().any(|channel| {
            channel.channel_id == channel_id
                && matches!(channel.state, ChannelState::Up | ChannelState::Degraded)
        })
    }

    pub fn channels(&self) -> &[ChannelRouteState] {
        &self.channels
    }

    fn select_by_state(&mut self, state: ChannelState) -> Option<ChannelId> {
        if self.channels.is_empty() {
            return None;
        }

        for offset in 0..self.channels.len() {
            let index = (self.cursor + offset) % self.channels.len();
            if self.channels[index].state == state {
                self.cursor = (index + 1) % self.channels.len();
                return Some(self.channels[index].channel_id);
            }
        }

        None
    }

    fn select_candidate_by_state(
        &mut self,
        candidates: &[ChannelId],
        state: ChannelState,
    ) -> Option<ChannelId> {
        if candidates.is_empty() || self.channels.is_empty() {
            return None;
        }

        for offset in 0..self.channels.len() {
            let index = (self.cursor + offset) % self.channels.len();
            let channel = &self.channels[index];
            if channel.state == state && candidates.contains(&channel.channel_id) {
                self.cursor = (index + 1) % self.channels.len();
                return Some(channel.channel_id);
            }
        }

        None
    }

    fn channel_mut(&mut self, channel_id: ChannelId) -> Option<&mut ChannelRouteState> {
        self.channels
            .iter_mut()
            .find(|channel| channel.channel_id == channel_id)
    }
}
