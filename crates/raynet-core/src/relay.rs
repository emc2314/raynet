use crate::limits::{MIN_PADDING, TRANSPORT_PACKET_OVERHEAD, effective_transport_mtu};
use crate::machine::{
    ChannelId, CoreAction, CoreEvent, CoreEventResult, CoreMetrics, check_sequence,
    note_latest_sequence, receive_transport_packet,
};
use crate::packet::encode_transport_packet;
use crate::random::RandomStream;
use crate::sequence::{SequenceFilter, TS_SHIFT};
use crate::wire::{CHANNEL_ID_SIZE, ENVELOPE_FIXED_OVERHEAD, EnvelopeStamp, TIME_MASK};

#[derive(Clone, PartialEq, Eq)]
pub struct RelayConfig {
    pub envelope_key: [u8; 16],
    pub random_seed: [u8; 16],
    pub boot_time_ms: u64,
    pub local_channels: Vec<ChannelId>,
    pub local_min_mtu: u32,
}

pub struct RelayCore {
    config: RelayConfig,
    random: RandomStream,
    sequence_filter: SequenceFilter,
    metrics: CoreMetrics,
}

impl RelayCore {
    pub fn new(config: RelayConfig) -> Self {
        Self {
            random: RandomStream::new(config.random_seed),
            sequence_filter: SequenceFilter::default(),
            metrics: CoreMetrics::default(),
            config,
        }
    }

    pub fn metrics(&self) -> CoreMetrics {
        CoreMetrics {
            active_sequences: self.sequence_filter.len(),
            ..self.metrics
        }
    }

    pub fn handle_event(
        &mut self,
        elapsed_ms: u64,
        event: CoreEvent,
        actions: &mut Vec<CoreAction>,
    ) -> CoreEventResult {
        match event {
            CoreEvent::TransportPacketReceived { bytes, .. } => {
                self.metrics.packets_received += 1;
                self.forward_packet(elapsed_ms, bytes, actions);
                CoreEventResult::None
            }
            CoreEvent::TransportPacketSendFailed { .. } => {
                self.metrics.send_failures += 1;
                CoreEventResult::None
            }
            CoreEvent::SessionOpen
            | CoreEvent::SessionWrite { .. }
            | CoreEvent::SessionClose { .. } => {
                panic!();
            }
        }
    }

    pub fn poll(&mut self, _elapsed_ms: u64, _actions: &mut Vec<CoreAction>) {}

    pub fn next_deadline(&self, _elapsed_ms: u64) -> u64 {
        u64::MAX
    }

    fn forward_packet(&mut self, elapsed_ms: u64, bytes: Vec<u8>, actions: &mut Vec<CoreAction>) {
        let current_time =
            (((self.config.boot_time_ms + elapsed_ms) >> TS_SHIFT) as u32) & TIME_MASK;
        let first_sequence = self.sequence_filter.len() == 0;
        let Some(mut envelope) =
            receive_transport_packet(&self.config.envelope_key, &mut self.metrics, bytes)
        else {
            return;
        };
        let stamp = EnvelopeStamp {
            time: envelope.time,
            seq_id: envelope.seq_id,
            seq_no: envelope.seq_no,
        };
        if !check_sequence(
            &mut self.sequence_filter,
            &mut self.metrics,
            current_time,
            stamp,
        ) {
            return;
        }
        self.sequence_filter.remove_expired(current_time, 0);
        note_latest_sequence(
            &mut self.metrics,
            self.config.boot_time_ms,
            elapsed_ms,
            stamp.time,
            first_sequence,
        );
        let Some(channel_id) = envelope.route_plan.first().copied() else {
            self.metrics.packets_dropped += 1;
            return;
        };
        if !self.config.local_channels.contains(&channel_id) {
            self.metrics.packets_dropped += 1;
            return;
        }
        envelope.route_plan.remove(0);
        let unpadded_len = TRANSPORT_PACKET_OVERHEAD
            + ENVELOPE_FIXED_OVERHEAD
            + envelope.route_plan.len() * CHANNEL_ID_SIZE
            + envelope.payload.len();
        let local_min_mtu = effective_transport_mtu(self.config.local_min_mtu);
        if unpadded_len + MIN_PADDING > local_min_mtu {
            self.metrics.packets_dropped += 1;
            return;
        }
        let available = (local_min_mtu - unpadded_len).min(u8::MAX as usize);
        envelope.padding_len = (1 + self.random.u64() as usize % available) as u8;
        let bytes = encode_transport_packet(&self.config.envelope_key, &mut self.random, envelope);
        self.metrics.packets_sent += 1;
        actions.push(CoreAction::SendTransportPacket { channel_id, bytes });
    }
}
