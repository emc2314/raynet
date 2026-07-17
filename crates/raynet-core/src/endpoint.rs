use std::collections::BTreeMap;
use std::num::NonZeroU32;

use aegis::aegis128l::{Aegis128L, Tag};
use bytes::Bytes;

use crate::kcp::{Error as KcpError, KCP_OVERHEAD, Kcp, KcpParams, get_conv};
use crate::limits::{
    ENDPOINT_MESSAGE_OVERHEAD, SESSION_TOMBSTONE_MS, TRANSPORT_PACKET_OVERHEAD,
    effective_transport_mtu,
};
use crate::machine::{
    ChannelId, CloseReason, ConvId, CoreAction, CoreEvent, CoreEventResult, CoreMetrics,
    check_sequence, note_latest_sequence, receive_transport_packet,
};
use crate::packet::{decode_transport_packet, encode_transport_packet};
use crate::random::RandomStream;
use crate::routing::{RouteConfig, RoutePlanner};
use crate::sequence::{SequenceFilter, TS_SHIFT};
use crate::wire::{
    CHANNEL_ID_SIZE, ENVELOPE_FIXED_OVERHEAD, EndpointMessage, Envelope, EnvelopeStamp, RoutePlan,
    SessionMessage, TIME_MASK, endpoint_nonce,
};

const SEQUENCE_MASK: u64 = (1 << 48) - 1;
const SEQUENCE_SOFT_CAP: usize = 1;

#[derive(Clone, PartialEq, Eq)]
pub struct KcpConfig {
    pub send_window: u16,
    pub receive_window: u16,
    pub time_scale: u8,
    /// Fast-retransmit threshold; `0` disables fast retransmit.
    pub fast_resend: u32,
}

impl Default for KcpConfig {
    fn default() -> Self {
        Self {
            send_window: 128,
            receive_window: 128,
            time_scale: 10,
            fast_resend: 32,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct EndpointConfig {
    pub envelope_key: [u8; 16],
    pub message_key: [u8; 16],
    pub random_seed: [u8; 16],
    pub boot_time_ms: u64,
    pub kcp: KcpConfig,
    pub route: RouteConfig,
    pub local_channels: Vec<ChannelId>,
    pub padding_reserve: u8,
    pub keepalive_interval_ms: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionPhase {
    PendingRemote,
    Open,
    ClosingLocal,
    ClosingRemote,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PacketId {
    seq_id: u64,
    seq_no: u64,
}

struct FeedbackSample {
    route_plan: RoutePlan,
    sent_at: u64,
    deadline: u64,
}

pub struct EndpointCore {
    config: EndpointConfig,
    kcp_params: KcpParams,
    scheduled_packet_overhead: usize,
    close_deadline_ms: u64,
    route_planner: RoutePlanner,
    random: RandomStream,
    sequence_filter: SequenceFilter,
    seq_id: u64,
    next_seq_no: u64,
    next_feedback_at: u64,
    outstanding: BTreeMap<PacketId, FeedbackSample>,
    next_keepalive_at: u64,
    sessions: BTreeMap<ConvId, EndpointSession>,
    tombstones: BTreeMap<ConvId, u64>,
    next_conv_id: ConvId,
    metrics: CoreMetrics,
}

impl EndpointCore {
    pub fn new(mut config: EndpointConfig) -> Self {
        let defaults = KcpConfig::default();
        if config.kcp.send_window == 0 {
            config.kcp.send_window = defaults.send_window;
        }
        if config.kcp.receive_window == 0 {
            config.kcp.receive_window = defaults.receive_window;
        }
        if config.kcp.time_scale == 0 {
            config.kcp.time_scale = defaults.time_scale;
        }
        let scheduled_packet_overhead = scheduled_packet_overhead(&config);
        let mtu = effective_transport_mtu(config.route.min_mtu)
            - scheduled_packet_overhead
            - ENDPOINT_MESSAGE_OVERHEAD;
        let fast_resend = NonZeroU32::new(config.kcp.fast_resend);
        let kcp_params = KcpParams {
            mtu,
            send_window: config.kcp.send_window,
            receive_window: config.kcp.receive_window,
            nodelay: true,
            fast_resend,
            congestion_control: false,
            time_scale: config.kcp.time_scale,
        };
        let close_deadline_ms = 3 * 6_000u64 * u64::from(config.kcp.time_scale);
        let route_planner = RoutePlanner::new(&config.route);
        let mut random = RandomStream::new(config.random_seed);
        let next_conv_id = random.u64();
        let seq_id = random.u64() & SEQUENCE_MASK;
        let next_keepalive_at = u64::from(config.keepalive_interval_ms);
        Self {
            random,
            next_conv_id,
            config,
            kcp_params,
            scheduled_packet_overhead,
            close_deadline_ms,
            route_planner,
            sequence_filter: SequenceFilter::default(),
            seq_id,
            next_seq_no: 0,
            next_feedback_at: 0,
            outstanding: BTreeMap::new(),
            next_keepalive_at,
            sessions: BTreeMap::new(),
            tombstones: BTreeMap::new(),
            metrics: CoreMetrics::default(),
        }
    }

    pub fn metrics(&self) -> CoreMetrics {
        CoreMetrics {
            active_sessions: self.sessions.len(),
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
            CoreEvent::SessionOpen => {
                let conv_id = self.next_conv_id;
                self.next_conv_id += 1;
                self.sessions.insert(
                    conv_id,
                    EndpointSession::new(conv_id, self.kcp_params, SessionPhase::Open),
                );
                self.send_session_message(elapsed_ms, conv_id, SessionMessage::Open, actions);
                CoreEventResult::SessionCreated { conv_id }
            }
            CoreEvent::SessionWrite { conv_id, bytes } => {
                let session = self.sessions.get(&conv_id).unwrap();
                debug_assert!(session.phase == SessionPhase::Open);
                let message_len = 1 + bytes.len();
                let fragment_count = message_len.div_ceil(self.kcp_params.mtu - KCP_OVERHEAD);
                if session.kcp.wait_snd() + fragment_count > self.config.kcp.send_window as usize {
                    return CoreEventResult::SessionWriteBlocked;
                }
                self.send_session_message(
                    elapsed_ms,
                    conv_id,
                    SessionMessage::Data { bytes },
                    actions,
                );
                CoreEventResult::None
            }
            CoreEvent::SessionClose { conv_id, reason } => {
                let session = self.sessions.get_mut(&conv_id).unwrap();
                debug_assert!(session.phase == SessionPhase::Open);
                session.phase = SessionPhase::ClosingLocal;
                session.close_deadline_at = Some(elapsed_ms + self.close_deadline_ms);
                session.pending_close = Some(reason);
                self.queue_local_close(elapsed_ms, conv_id, actions);
                CoreEventResult::None
            }
            CoreEvent::TransportPacketSendFailed { channel_id, bytes } => {
                self.metrics.send_failures += 1;
                self.route_planner
                    .note_local_send_failure(channel_id, elapsed_ms);

                let Ok(envelope) = decode_transport_packet(&self.config.envelope_key, bytes) else {
                    panic!();
                };
                let stamp = EnvelopeStamp {
                    time: envelope.time,
                    seq_id: envelope.seq_id,
                    seq_no: envelope.seq_no,
                };
                let packet_id = PacketId {
                    seq_id: stamp.seq_id,
                    seq_no: stamp.seq_no,
                };
                let was_sample = self.outstanding.remove(&packet_id).is_some();
                if was_sample {
                    self.next_feedback_at = elapsed_ms;
                }
                if !matches!(
                    self.open_endpoint_message(stamp, envelope.payload.clone())
                        .unwrap(),
                    EndpointMessage::KcpPacket { .. }
                ) {
                    return CoreEventResult::None;
                }
                self.send_envelope(elapsed_ms, stamp, envelope.payload, was_sample, actions);
                CoreEventResult::None
            }
            CoreEvent::TransportPacketReceived { bytes, .. } => {
                self.metrics.packets_received += 1;
                self.handle_transport_packet(elapsed_ms, bytes, actions);
                CoreEventResult::None
            }
        }
    }

    pub fn poll(&mut self, elapsed_ms: u64, actions: &mut Vec<CoreAction>) {
        self.expire_feedback(elapsed_ms);
        self.expire_sequences(elapsed_ms, actions);
        self.tombstones
            .retain(|_, expire_at| elapsed_ms < *expire_at);
        let conv_ids: Vec<_> = self.sessions.keys().copied().collect();
        for conv_id in conv_ids {
            self.queue_local_close(elapsed_ms, conv_id, actions);
            let packets = {
                let session = self.sessions.get_mut(&conv_id).unwrap();
                session.poll(elapsed_ms)
            };
            self.send_kcp_packets(elapsed_ms, packets, actions);
            self.finish_closed_session(elapsed_ms, conv_id, actions);
        }
        if self.config.keepalive_interval_ms != 0 && elapsed_ms >= self.next_keepalive_at {
            let sample = self.feedback_due(elapsed_ms);
            self.send_endpoint_message(
                elapsed_ms,
                EndpointMessage::KeepAlive {
                    reply_depth: u8::from(sample),
                },
                sample,
                actions,
            );
            self.next_keepalive_at = elapsed_ms + u64::from(self.config.keepalive_interval_ms);
        }
    }

    pub fn next_deadline(&self, elapsed_ms: u64) -> u64 {
        let mut deadline = u64::MAX;
        for session in self.sessions.values() {
            deadline = deadline.min(session.next_deadline(elapsed_ms));
        }
        for &expire_at in self.tombstones.values() {
            deadline = deadline.min(expire_at);
        }
        for sample in self.outstanding.values() {
            deadline = deadline.min(sample.deadline);
        }
        if self.config.keepalive_interval_ms != 0 {
            deadline = deadline.min(self.next_keepalive_at);
        }
        if let Some(delay) = self
            .sequence_filter
            .next_expiry_delay_ms(self.current_time(elapsed_ms), SEQUENCE_SOFT_CAP)
        {
            let tick_offset = (self.config.boot_time_ms + elapsed_ms) & ((1 << TS_SHIFT) - 1);
            deadline = deadline.min(elapsed_ms + delay.saturating_sub(tick_offset));
        }
        deadline
    }

    fn send_session_message(
        &mut self,
        elapsed_ms: u64,
        conv_id: ConvId,
        message: SessionMessage,
        actions: &mut Vec<CoreAction>,
    ) {
        let session = self.sessions.get_mut(&conv_id).unwrap();
        let packets = session.send_message(elapsed_ms, message);
        self.send_kcp_packets(elapsed_ms, packets, actions);
        self.finish_closed_session(elapsed_ms, conv_id, actions);
    }

    fn queue_local_close(
        &mut self,
        elapsed_ms: u64,
        conv_id: ConvId,
        actions: &mut Vec<CoreAction>,
    ) {
        let message = {
            let Some(session) = self.sessions.get_mut(&conv_id) else {
                return;
            };
            if session.phase != SessionPhase::ClosingLocal
                || session.close_queued
                || elapsed_ms >= session.close_deadline_at.unwrap()
                || session.kcp.wait_snd() >= self.config.kcp.send_window as usize
            {
                return;
            }
            let reason = session.pending_close.unwrap();
            session.close_queued = true;
            SessionMessage::Close { reason }
        };
        self.send_session_message(elapsed_ms, conv_id, message, actions);
    }

    fn send_kcp_packets(
        &mut self,
        elapsed_ms: u64,
        packets: Vec<Bytes>,
        actions: &mut Vec<CoreAction>,
    ) {
        for packet in packets {
            let sample = self.feedback_due(elapsed_ms);
            self.send_endpoint_message(
                elapsed_ms,
                EndpointMessage::KcpPacket {
                    reply_depth: u8::from(sample),
                    packet,
                },
                sample,
                actions,
            );
        }
    }

    fn send_endpoint_message(
        &mut self,
        elapsed_ms: u64,
        message: EndpointMessage,
        sample: bool,
        actions: &mut Vec<CoreAction>,
    ) {
        let (seq_id, seq_no) = self.next_sequence();
        let stamp = EnvelopeStamp {
            time: self.current_time(elapsed_ms),
            seq_id,
            seq_no,
        };
        let payload = self.seal_endpoint_message(stamp, message);
        self.send_envelope(elapsed_ms, stamp, payload, sample, actions);
    }

    fn send_envelope(
        &mut self,
        elapsed_ms: u64,
        stamp: EnvelopeStamp,
        payload: Vec<u8>,
        sample: bool,
        actions: &mut Vec<CoreAction>,
    ) {
        let scheduled_bytes = payload.len() + self.scheduled_packet_overhead;
        let Some(full_plan) =
            self.route_planner
                .build_route_plan(elapsed_ms, scheduled_bytes, sample)
        else {
            return;
        };
        let channel_id = full_plan[0];
        let unpadded_len = TRANSPORT_PACKET_OVERHEAD
            + ENVELOPE_FIXED_OVERHEAD
            + (full_plan.len() - 1) * CHANNEL_ID_SIZE
            + payload.len();
        let available = (effective_transport_mtu(self.config.route.min_mtu) - unpadded_len)
            .min(u8::MAX as usize);
        let padding_len = (1 + self.random.u64() as usize % available) as u8;
        let envelope = Envelope {
            time: stamp.time,
            seq_id: stamp.seq_id,
            seq_no: stamp.seq_no,
            route_plan: full_plan[1..].to_vec(),
            payload,
            padding_len,
        };
        let bytes = encode_transport_packet(&self.config.envelope_key, &mut self.random, envelope);
        actions.push(CoreAction::SendTransportPacket { channel_id, bytes });
        self.metrics.packets_sent += 1;
        if self.config.keepalive_interval_ms != 0 {
            self.next_keepalive_at = elapsed_ms + u64::from(self.config.keepalive_interval_ms);
        }
        if sample {
            let packet_id = PacketId {
                seq_id: stamp.seq_id,
                seq_no: stamp.seq_no,
            };
            self.outstanding.insert(
                packet_id,
                FeedbackSample {
                    route_plan: full_plan,
                    sent_at: elapsed_ms,
                    deadline: elapsed_ms + u64::from(self.config.route.feedback_timeout_ms),
                },
            );
            let interval = u64::from(self.config.route.feedback_interval_ms);
            self.next_feedback_at = elapsed_ms + interval + self.random.u64() % interval;
        }
    }

    fn next_sequence(&mut self) -> (u64, u64) {
        if self.next_seq_no > SEQUENCE_MASK {
            self.seq_id = self.random.u64() & SEQUENCE_MASK;
            self.next_seq_no = 0;
        }
        let seq_no = self.next_seq_no;
        self.next_seq_no += 1;
        (self.seq_id, seq_no)
    }

    fn handle_transport_packet(
        &mut self,
        elapsed_ms: u64,
        bytes: Vec<u8>,
        actions: &mut Vec<CoreAction>,
    ) {
        let current_time = self.current_time(elapsed_ms);
        let first_sequence = self.sequence_filter.len() == 0;
        let Some(envelope) =
            receive_transport_packet(&self.config.envelope_key, &mut self.metrics, bytes)
        else {
            return;
        };
        if !envelope.route_plan.is_empty() {
            self.metrics.packets_dropped += 1;
            return;
        }
        let stamp = EnvelopeStamp {
            time: envelope.time,
            seq_id: envelope.seq_id,
            seq_no: envelope.seq_no,
        };
        let Some(message) = self.open_endpoint_message(stamp, envelope.payload) else {
            self.metrics.authentication_failures += 1;
            return;
        };
        if !check_sequence(
            &mut self.sequence_filter,
            &mut self.metrics,
            current_time,
            stamp,
        ) {
            return;
        }
        note_latest_sequence(
            &mut self.metrics,
            self.config.boot_time_ms,
            elapsed_ms,
            stamp.time,
            first_sequence,
        );
        let reply_depth = match &message {
            EndpointMessage::KcpPacket { reply_depth, .. }
            | EndpointMessage::KeepAlive { reply_depth }
            | EndpointMessage::RouteReply { reply_depth, .. } => *reply_depth,
        };
        match message {
            EndpointMessage::KcpPacket { packet, .. } => {
                self.handle_kcp_packet(elapsed_ms, stamp.seq_id, packet, actions);
            }
            EndpointMessage::KeepAlive { .. } => {}
            EndpointMessage::RouteReply {
                received_seq_id,
                received_seq_no,
                ..
            } => {
                let packet_id = PacketId {
                    seq_id: received_seq_id,
                    seq_no: received_seq_no,
                };
                if let Some(sample) = self.outstanding.remove(&packet_id) {
                    self.route_planner
                        .note_feedback(&sample.route_plan, sample.sent_at, true);
                }
            }
        }
        if reply_depth != 0 {
            let sample = reply_depth < 3 && self.feedback_due(elapsed_ms);
            self.send_endpoint_message(
                elapsed_ms,
                EndpointMessage::RouteReply {
                    reply_depth: if sample { reply_depth + 1 } else { 0 },
                    received_seq_id: stamp.seq_id,
                    received_seq_no: stamp.seq_no,
                },
                sample,
                actions,
            );
        }
    }

    fn handle_kcp_packet(
        &mut self,
        elapsed_ms: u64,
        peer_seq: u64,
        kcp_packet: Bytes,
        actions: &mut Vec<CoreAction>,
    ) {
        let conv_id = get_conv(&kcp_packet);
        if self.tombstones.contains_key(&conv_id) {
            self.metrics.packets_dropped += 1;
            return;
        }
        if !self.sessions.contains_key(&conv_id) {
            let mut session =
                EndpointSession::new(conv_id, self.kcp_params, SessionPhase::PendingRemote);
            session.peer_seq = Some(peer_seq);
            session.close_deadline_at = Some(elapsed_ms + self.close_deadline_ms);
            self.sessions.insert(conv_id, session);
        }
        let output = {
            let session = self.sessions.get_mut(&conv_id).unwrap();
            match session.peer_seq {
                Some(bound) if bound != peer_seq => {
                    self.metrics.packets_dropped += 1;
                    return;
                }
                None => session.peer_seq = Some(peer_seq),
                Some(_) => {}
            }
            session.input_packet(elapsed_ms, &kcp_packet)
        };
        self.send_kcp_packets(elapsed_ms, output.packets, actions);
        for message in output.messages {
            self.handle_session_message(elapsed_ms, conv_id, message, actions);
        }
        self.queue_local_close(elapsed_ms, conv_id, actions);
        self.finish_closed_session(elapsed_ms, conv_id, actions);
    }

    fn handle_session_message(
        &mut self,
        elapsed_ms: u64,
        conv_id: ConvId,
        message: SessionMessage,
        actions: &mut Vec<CoreAction>,
    ) {
        let Some(session) = self.sessions.get_mut(&conv_id) else {
            return;
        };
        match message {
            SessionMessage::Open => match session.phase {
                SessionPhase::PendingRemote => {
                    session.phase = SessionPhase::Open;
                    session.close_deadline_at = None;
                    actions.push(CoreAction::OpenSession { conv_id });
                }
                SessionPhase::Open => {}
                SessionPhase::ClosingLocal | SessionPhase::ClosingRemote => {
                    self.metrics.packets_dropped += 1;
                }
            },
            SessionMessage::Data { bytes } => match session.phase {
                SessionPhase::Open => {
                    actions.push(CoreAction::WriteSession { conv_id, bytes });
                }
                SessionPhase::PendingRemote => {
                    self.sessions.remove(&conv_id);
                    self.metrics.packets_dropped += 1;
                }
                SessionPhase::ClosingLocal | SessionPhase::ClosingRemote => {
                    self.metrics.packets_dropped += 1;
                }
            },
            SessionMessage::Close { reason } => match session.phase {
                SessionPhase::PendingRemote => {
                    self.sessions.remove(&conv_id);
                    self.metrics.packets_dropped += 1;
                }
                SessionPhase::Open => {
                    session.phase = SessionPhase::ClosingRemote;
                    session.close_deadline_at = Some(elapsed_ms + self.close_deadline_ms);
                    actions.push(CoreAction::CloseSession { conv_id, reason });
                }
                SessionPhase::ClosingLocal => {
                    // Peer also closed; remain ClosingLocal and keep ACK path.
                }
                SessionPhase::ClosingRemote => {
                    // Retransmitted Close: KCP already produced ACK packets.
                }
            },
        }
    }

    fn finish_closed_session(
        &mut self,
        elapsed_ms: u64,
        conv_id: ConvId,
        actions: &mut Vec<CoreAction>,
    ) {
        let Some(session) = self.sessions.get(&conv_id) else {
            return;
        };
        let deadline_hit = session
            .close_deadline_at
            .is_some_and(|deadline| elapsed_ms >= deadline);
        let done = match session.phase {
            SessionPhase::PendingRemote => deadline_hit,
            SessionPhase::ClosingLocal => session.kcp.wait_snd() == 0 || deadline_hit,
            SessionPhase::ClosingRemote => deadline_hit,
            SessionPhase::Open => false,
        };
        if !done {
            return;
        }
        let session = self.sessions.remove(&conv_id).unwrap();
        if session.phase == SessionPhase::PendingRemote {
            self.metrics.packets_dropped += 1;
            return;
        }
        if session.phase == SessionPhase::ClosingLocal {
            let reason = session.pending_close.unwrap();
            actions.push(CoreAction::CloseSession { conv_id, reason });
        }
        self.tombstones
            .insert(conv_id, elapsed_ms + SESSION_TOMBSTONE_MS);
    }

    fn feedback_due(&self, elapsed_ms: u64) -> bool {
        elapsed_ms >= self.next_feedback_at
    }

    fn expire_feedback(&mut self, elapsed_ms: u64) {
        let Self {
            outstanding,
            route_planner,
            ..
        } = self;
        outstanding.retain(|_, sample| {
            let keep = elapsed_ms < sample.deadline;
            if !keep {
                route_planner.note_feedback(&sample.route_plan, sample.sent_at, false);
            }
            keep
        });
    }

    fn expire_sequences(&mut self, elapsed_ms: u64, actions: &mut Vec<CoreAction>) {
        let expired = self
            .sequence_filter
            .remove_expired(self.current_time(elapsed_ms), SEQUENCE_SOFT_CAP);
        for seq_id in expired {
            let conv_ids: Vec<_> = self
                .sessions
                .iter()
                .filter(|(_, session)| session.peer_seq == Some(seq_id))
                .map(|(&conv_id, _)| conv_id)
                .collect();
            for conv_id in conv_ids {
                let session = self.sessions.remove(&conv_id).unwrap();
                if session.phase != SessionPhase::PendingRemote {
                    actions.push(CoreAction::CloseSession {
                        conv_id,
                        reason: CloseReason::Reset,
                    });
                }
                self.tombstones
                    .insert(conv_id, elapsed_ms + SESSION_TOMBSTONE_MS);
            }
        }
    }

    fn current_time(&self, elapsed_ms: u64) -> u32 {
        (((self.config.boot_time_ms + elapsed_ms) >> TS_SHIFT) as u32) & TIME_MASK
    }

    fn seal_endpoint_message(&self, stamp: EnvelopeStamp, message: EndpointMessage) -> Vec<u8> {
        let mut out = message.encode_with_prefix(16);
        let nonce = endpoint_nonce(stamp);
        let tag: Tag<16> =
            Aegis128L::new(&self.config.message_key, &nonce).encrypt_in_place(&mut out[16..], &[]);
        out[..16].copy_from_slice(&tag);
        out
    }

    fn open_endpoint_message(
        &self,
        stamp: EnvelopeStamp,
        mut packet: Vec<u8>,
    ) -> Option<EndpointMessage> {
        if packet.len() < 16 {
            return None;
        }
        let nonce = endpoint_nonce(stamp);
        let tag: Tag<16> = packet[..16].try_into().unwrap();
        Aegis128L::new(&self.config.message_key, &nonce)
            .decrypt_in_place(&mut packet[16..], &tag, &[])
            .ok()?;
        Some(EndpointMessage::decode(Bytes::from(packet).slice(16..)))
    }
}

struct EndpointSession {
    kcp: Kcp,
    phase: SessionPhase,
    peer_seq: Option<u64>,
    pending_close: Option<CloseReason>,
    close_queued: bool,
    close_deadline_at: Option<u64>,
}

struct SessionOutput {
    messages: Vec<SessionMessage>,
    packets: Vec<Bytes>,
}

impl EndpointSession {
    fn new(conv_id: ConvId, params: KcpParams, phase: SessionPhase) -> Self {
        Self {
            kcp: Kcp::new(conv_id, params),
            phase,
            peer_seq: None,
            pending_close: None,
            close_queued: false,
            close_deadline_at: None,
        }
    }

    fn send_message(&mut self, elapsed_ms: u64, message: SessionMessage) -> Vec<Bytes> {
        let message = message.encode();
        self.kcp.send(&message).ok().unwrap();
        self.flush_now(elapsed_ms)
    }

    fn input_packet(&mut self, elapsed_ms: u64, packet: &[u8]) -> SessionOutput {
        self.kcp.input(elapsed_ms as u32, packet).ok().unwrap();
        let packets = self.flush_now(elapsed_ms);
        SessionOutput {
            messages: self.recv_messages(),
            packets,
        }
    }

    fn poll(&mut self, elapsed_ms: u64) -> Vec<Bytes> {
        self.kcp.update(elapsed_ms as u32).ok().unwrap();
        self.kcp.take_output()
    }

    fn flush_now(&mut self, elapsed_ms: u64) -> Vec<Bytes> {
        self.kcp.update(elapsed_ms as u32).ok().unwrap();
        self.kcp.flush().ok().unwrap();
        self.kcp.take_output()
    }

    fn next_deadline(&self, elapsed_ms: u64) -> u64 {
        let mut deadline = u64::MAX;
        if self.kcp.wait_snd() != 0 {
            deadline = deadline.min(elapsed_ms + self.kcp.check(elapsed_ms as u32) as u64);
        }
        if let Some(close_deadline) = self.close_deadline_at {
            deadline = deadline.min(close_deadline);
        }
        deadline
    }

    fn recv_messages(&mut self) -> Vec<SessionMessage> {
        let mut messages = Vec::new();
        loop {
            let size = match self.kcp.peeksize() {
                Ok(size) => size,
                Err(KcpError::RecvQueueEmpty | KcpError::ExpectingFragment) => break,
                Err(_) => panic!(),
            };
            let mut payload = vec![0; size];
            self.kcp.recv(&mut payload).ok().unwrap();
            messages.push(SessionMessage::decode(&payload));
        }
        messages
    }
}

fn scheduled_packet_overhead(config: &EndpointConfig) -> usize {
    let graph = &config.route.graph;
    let destination = graph.nodes.len() - 1;
    let mut longest = vec![0usize; graph.nodes.len()];
    for node_index in (0..destination).rev() {
        for edge in &graph.nodes[node_index].edges {
            let next = edge.next as usize;
            longest[node_index] = longest[node_index].max(longest[next] + 1);
        }
    }
    TRANSPORT_PACKET_OVERHEAD
        + ENVELOPE_FIXED_OVERHEAD
        + config.padding_reserve as usize
        + (longest[0] - 1) * CHANNEL_ID_SIZE
}
