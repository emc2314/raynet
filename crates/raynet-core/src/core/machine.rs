use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use aegis::aegis128l::{Aegis128L, Key, Nonce, Tag};

use crate::kcp::{Error as KcpError, KCP_OVERHEAD, Kcp, get_conv};
use crate::routing::{LocalChannelTable, RoutePlanner, RouteTopology};

use super::nonce::NonceFilter;
use super::packet::{HopPacketError, RandomStream, open_hop_payload, seal_hop_payload};
use super::wire::{Envelope, RoutePlan, SessionFrame, WireError};

pub type NodeId = u64;
pub type ChannelId = u64;
pub type ConvId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

pub type Metadata = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointConfig {
    pub local_node_id: NodeId,
    pub envelope_key: [u8; 16],
    pub message_key: [u8; 16],
    pub random_seed: [u8; 16],
    pub local_channels: Vec<ChannelId>,
    pub route_topology: RouteTopology,
    pub transport_mtu: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayConfig {
    pub local_node_id: NodeId,
    pub envelope_key: [u8; 16],
    pub random_seed: [u8; 16],
    pub local_channels: Vec<ChannelId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    Up,
    Down,
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransportMetrics {
    pub queue_pressure: f32,
    pub send_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    LocalClosed,
    RemoteClosed,
    Reset,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenFailureReason {
    Refused,
    Timeout,
    UnsupportedTarget,
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Metric {
    TransportChannelUpdated {
        channel_id: ChannelId,
        state: ChannelState,
        metrics: TransportMetrics,
    },
    IngressSessionCreated {
        conv_id: ConvId,
    },
    SessionClosed {
        conv_id: ConvId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreStructuredEvent {
    IngressSessionCreated { conv_id: ConvId },
    SessionClosed { conv_id: ConvId },
}

#[derive(Debug, Clone, PartialEq)]
pub enum CoreEvent {
    TransportPacketReceived {
        channel_id: ChannelId,
        bytes: Vec<u8>,
    },
    TransportChannelUpdated {
        channel_id: ChannelId,
        state: ChannelState,
        metrics: TransportMetrics,
    },
    IngressSessionRequested {
        target: Target,
        metadata: Metadata,
    },
    ExitConnectionOpened {
        conv_id: ConvId,
    },
    ExitConnectionOpenFailed {
        conv_id: ConvId,
        reason: OpenFailureReason,
    },
    SessionBytes {
        conv_id: ConvId,
        bytes: Vec<u8>,
    },
    SessionClosed {
        conv_id: ConvId,
        reason: CloseReason,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum CoreAction {
    SendTransportPacket {
        channel_id: ChannelId,
        bytes: Vec<u8>,
    },
    IngressSessionCreated {
        conv_id: ConvId,
    },
    OpenExitConnection {
        conv_id: ConvId,
        target: Target,
        metadata: Metadata,
    },
    WriteSession {
        conv_id: ConvId,
        bytes: Vec<u8>,
    },
    CloseSession {
        conv_id: ConvId,
        reason: CloseReason,
    },
    EmitMetric(Metric),
    EmitEvent(CoreStructuredEvent),
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("node id must be non-zero")]
    EmptyNodeId,
    #[error("transport MTU must be non-zero")]
    ZeroMtu,
    #[error("unknown local channel {0}")]
    UnknownLocalChannel(ChannelId),
    #[error("route topology has no usable route from local node")]
    NoRoute,
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("event is not supported by this core role")]
    UnsupportedEvent,
    #[error("unknown conv {0}")]
    UnknownConv(ConvId),
    #[error("no route is available")]
    NoRoute,
    #[error("wire error: {0}")]
    Wire(#[from] WireError),
    #[error("hop packet error: {0}")]
    HopPacket(#[from] HopPacketError),
    #[error("session error: {0}")]
    Session(#[from] KcpError),
    #[error("endpoint message authentication failed")]
    EndpointMessageAuthFailed,
    #[error("invalid KCP segment length {0}")]
    InvalidKcpSegment(usize),
}

#[derive(Debug)]
pub struct EndpointCore {
    config: EndpointConfig,
    local_channels: LocalChannelTable,
    route_planner: RoutePlanner,
    envelope_nonce_filter: NonceFilter,
    envelope_random: RandomStream,
    message_random: RandomStream,
    sessions: BTreeMap<ConvId, EndpointSession>,
    next_conv_id: ConvId,
}

impl EndpointCore {
    pub fn new(config: EndpointConfig) -> Result<Self, ConfigError> {
        validate_endpoint_config(&config)?;
        let local_channels = LocalChannelTable::new(config.local_channels.iter().copied());
        let route_planner = RoutePlanner::new(config.route_topology.clone());

        if route_planner
            .build_route_plan(config.local_node_id, &local_channels)
            .is_none()
        {
            return Err(ConfigError::NoRoute);
        }

        let next_conv_id = initial_conv_id(config.random_seed);

        Ok(Self {
            envelope_nonce_filter: NonceFilter::new(1 << 24, 0.00001, 1 << 16, 0),
            envelope_random: RandomStream::new(config.random_seed, b"endpoint envelope"),
            message_random: RandomStream::new(config.random_seed, b"endpoint message"),
            config,
            local_channels,
            route_planner,
            sessions: BTreeMap::new(),
            next_conv_id,
        })
    }

    pub fn handle_event(
        &mut self,
        now_ms: u64,
        event: CoreEvent,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        match event {
            CoreEvent::IngressSessionRequested { target, metadata } => {
                let conv_id = self.allocate_conv_id();
                self.ensure_session(conv_id)?;
                actions.push(CoreAction::IngressSessionCreated { conv_id });
                actions.push(CoreAction::EmitMetric(Metric::IngressSessionCreated {
                    conv_id,
                }));
                actions.push(CoreAction::EmitEvent(
                    CoreStructuredEvent::IngressSessionCreated { conv_id },
                ));
                self.send_session_frame(
                    now_ms,
                    conv_id,
                    SessionFrame::OpenConnection { target, metadata },
                    actions,
                )
            }
            CoreEvent::ExitConnectionOpened { conv_id } => {
                self.ensure_session(conv_id)?;
                Ok(())
            }
            CoreEvent::ExitConnectionOpenFailed { conv_id, reason } => self.send_session_frame(
                now_ms,
                conv_id,
                SessionFrame::ResetConnection {
                    reason: CloseReason::Error(format!("{reason:?}")),
                },
                actions,
            ),
            CoreEvent::SessionBytes { conv_id, bytes } => self.send_session_frame(
                now_ms,
                conv_id,
                SessionFrame::ConnectionBytes { bytes },
                actions,
            ),
            CoreEvent::SessionClosed { conv_id, reason } => {
                self.send_session_frame(
                    now_ms,
                    conv_id,
                    SessionFrame::CloseConnection {
                        reason: reason.clone(),
                    },
                    actions,
                )?;
                self.sessions.remove(&conv_id);
                actions.push(CoreAction::EmitMetric(Metric::SessionClosed { conv_id }));
                actions.push(CoreAction::EmitEvent(CoreStructuredEvent::SessionClosed {
                    conv_id,
                }));
                Ok(())
            }
            CoreEvent::TransportChannelUpdated {
                channel_id,
                state,
                metrics,
            } => {
                self.local_channels
                    .update_channel(channel_id, state, metrics);
                self.route_planner.update_edge(
                    self.config.local_node_id,
                    channel_id,
                    state,
                    metrics,
                    now_ms,
                );
                actions.push(CoreAction::EmitMetric(Metric::TransportChannelUpdated {
                    channel_id,
                    state,
                    metrics,
                }));
                Ok(())
            }
            CoreEvent::TransportPacketReceived { channel_id, bytes } => {
                self.local_channels.record_receive(channel_id);
                self.handle_transport_packet(now_ms, bytes, actions)
            }
        }
    }

    pub fn poll(&mut self, now_ms: u64, actions: &mut Vec<CoreAction>) -> Result<(), CoreError> {
        let conv_ids: Vec<_> = self.sessions.keys().copied().collect();
        for conv_id in conv_ids {
            let packets = self
                .sessions
                .get_mut(&conv_id)
                .expect("conv id collected from sessions")
                .poll(now_ms)?;
            self.send_kcp_segments(now_ms, packets, actions)?;
        }
        Ok(())
    }

    pub fn next_deadline(&self, now_ms: u64) -> Option<u64> {
        self.sessions
            .values()
            .filter_map(|session| session.next_deadline(now_ms))
            .min()
    }

    pub fn channel_count(&self) -> usize {
        self.local_channels.channel_ids().count()
    }

    fn allocate_conv_id(&mut self) -> ConvId {
        let conv_id = self.next_conv_id;
        self.next_conv_id = self.next_conv_id.wrapping_add(1);
        conv_id
    }

    fn ensure_session(&mut self, conv_id: ConvId) -> Result<(), CoreError> {
        if !self.sessions.contains_key(&conv_id) {
            self.sessions.insert(
                conv_id,
                EndpointSession::new(conv_id, self.config.transport_mtu),
            );
        }
        Ok(())
    }

    fn send_session_frame(
        &mut self,
        now_ms: u64,
        conv_id: ConvId,
        frame: SessionFrame,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        self.ensure_session(conv_id)?;
        let packets = self
            .sessions
            .get_mut(&conv_id)
            .expect("ensure_session inserted session")
            .send_frame(now_ms, frame)?;
        self.send_kcp_segments(now_ms, packets, actions)
    }

    fn send_kcp_segments(
        &mut self,
        now_ms: u64,
        packets: Vec<Vec<u8>>,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        for packet in packets {
            let payload = self.seal_endpoint_message(packet)?;
            self.send_envelope_payload(now_ms, payload, actions)?;
        }
        Ok(())
    }

    fn send_envelope_payload(
        &mut self,
        now_ms: u64,
        payload: Vec<u8>,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        let (channel_id, route_plan) = self.next_outbound_plan()?;
        let envelope = Envelope {
            route_plan,
            payload,
        };
        actions.push(CoreAction::SendTransportPacket {
            channel_id,
            bytes: encode_hop_packet(
                now_ms,
                &self.config.envelope_key,
                &mut self.envelope_random,
                envelope,
            )?,
        });
        self.local_channels.record_send(channel_id);
        self.route_planner
            .record_success(self.config.local_node_id, channel_id, now_ms);
        Ok(())
    }

    fn next_outbound_plan(&self) -> Result<(ChannelId, RoutePlan), CoreError> {
        let mut route_plan = self
            .route_planner
            .build_route_plan(self.config.local_node_id, &self.local_channels)
            .ok_or(CoreError::NoRoute)?;
        if route_plan.is_empty() {
            return Err(CoreError::NoRoute);
        }
        let channel_id = route_plan.remove(0);
        Ok((channel_id, route_plan))
    }

    fn handle_transport_packet(
        &mut self,
        now_ms: u64,
        bytes: Vec<u8>,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        let envelope = decode_hop_packet(
            now_ms,
            &self.config.envelope_key,
            &mut self.envelope_nonce_filter,
            &bytes,
        )?;
        if !envelope.route_plan.is_empty() {
            return Err(CoreError::NoRoute);
        }

        let kcp_segment = self.open_endpoint_message(&envelope.payload)?;
        if kcp_segment.len() < KCP_OVERHEAD {
            return Err(CoreError::InvalidKcpSegment(kcp_segment.len()));
        }
        let conv_id = get_conv(&kcp_segment);
        self.ensure_session(conv_id)?;

        let session_output = self
            .sessions
            .get_mut(&conv_id)
            .expect("ensure_session inserted session")
            .input_packet(now_ms, &kcp_segment)?;
        self.send_kcp_segments(now_ms, session_output.packets, actions)?;

        for frame in session_output.frames {
            self.handle_session_frame(conv_id, frame, actions)?;
        }
        Ok(())
    }

    fn handle_session_frame(
        &mut self,
        conv_id: ConvId,
        frame: SessionFrame,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        match frame {
            SessionFrame::OpenConnection { target, metadata } => {
                actions.push(CoreAction::OpenExitConnection {
                    conv_id,
                    target,
                    metadata,
                });
                Ok(())
            }
            SessionFrame::ConnectionBytes { bytes } => {
                actions.push(CoreAction::WriteSession { conv_id, bytes });
                Ok(())
            }
            SessionFrame::CloseConnection { reason } | SessionFrame::ResetConnection { reason } => {
                self.sessions.remove(&conv_id);
                actions.push(CoreAction::CloseSession { conv_id, reason });
                actions.push(CoreAction::EmitMetric(Metric::SessionClosed { conv_id }));
                actions.push(CoreAction::EmitEvent(CoreStructuredEvent::SessionClosed {
                    conv_id,
                }));
                Ok(())
            }
            SessionFrame::KeepAlive => Ok(()),
        }
    }

    fn seal_endpoint_message(&mut self, kcp_segment: Vec<u8>) -> Result<Vec<u8>, CoreError> {
        let nonce = self.message_random.nonce();
        let mut plaintext = kcp_segment;
        let tag: Tag<16> = Aegis128L::new(&self.config.message_key, &nonce)
            .encrypt_in_place(&mut plaintext, &message_ad());
        let mut out = Vec::with_capacity(32 + plaintext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&tag);
        out.extend_from_slice(&plaintext);
        Ok(out)
    }

    fn open_endpoint_message(&self, packet: &[u8]) -> Result<Vec<u8>, CoreError> {
        if packet.len() < 32 {
            return Err(CoreError::EndpointMessageAuthFailed);
        }
        let nonce: Nonce = packet[0..16]
            .try_into()
            .map_err(|_| CoreError::EndpointMessageAuthFailed)?;
        let tag: Tag<16> = packet[16..32]
            .try_into()
            .map_err(|_| CoreError::EndpointMessageAuthFailed)?;
        Aegis128L::new(&self.config.message_key, &nonce)
            .decrypt(&packet[32..], &tag, &message_ad())
            .map_err(|_| CoreError::EndpointMessageAuthFailed)
    }
}

fn encode_hop_packet(
    now_ms: u64,
    envelope_key: &Key,
    random: &mut RandomStream,
    envelope: Envelope,
) -> Result<Vec<u8>, CoreError> {
    let payload = envelope.encode()?;
    Ok(seal_hop_payload(
        now_ms,
        envelope_key,
        random.nonce(),
        &payload,
    ))
}

fn decode_hop_packet(
    now_ms: u64,
    envelope_key: &Key,
    nonce_filter: &mut NonceFilter,
    bytes: &[u8],
) -> Result<Envelope, CoreError> {
    let payload = open_hop_payload(now_ms, envelope_key, bytes, nonce_filter)?;
    Envelope::decode(&payload).map_err(CoreError::from)
}

fn message_ad() -> [u8; 16] {
    *b"RNMSG1\0\0\0\0\0\0\0\0\0\0"
}

fn initial_conv_id(seed: [u8; 16]) -> ConvId {
    let hash = blake3::derive_key("RayNet conv id counter v1", &seed);
    u64::from_le_bytes(hash[0..8].try_into().expect("eight bytes"))
}

#[derive(Debug)]
struct EndpointSession {
    kcp: Kcp<SessionOutputBuffer>,
    output: SessionOutputBuffer,
}

#[derive(Debug)]
struct SessionOutput {
    frames: Vec<SessionFrame>,
    packets: Vec<Vec<u8>>,
}

impl EndpointSession {
    fn new(conv_id: ConvId, transport_mtu: usize) -> Self {
        let output = SessionOutputBuffer::default();
        let mut kcp = Kcp::new(conv_id, output.clone());
        kcp.set_mtu(session_mtu(transport_mtu))
            .expect("session_mtu returns a valid KCP MTU");
        kcp.set_nodelay(true, 20, 2, true);
        kcp.set_wndsize(128, 128);
        Self { kcp, output }
    }

    fn send_frame(&mut self, now_ms: u64, frame: SessionFrame) -> Result<Vec<Vec<u8>>, CoreError> {
        let payload = frame.encode()?;
        self.kcp.send(&payload)?;
        self.flush_now(now_ms)
    }

    fn input_packet(&mut self, now_ms: u64, packet: &[u8]) -> Result<SessionOutput, CoreError> {
        self.kcp.input(packet)?;
        let packets = self.flush_now(now_ms)?;
        let frames = self.recv_frames()?;
        Ok(SessionOutput { frames, packets })
    }

    fn poll(&mut self, now_ms: u64) -> Result<Vec<Vec<u8>>, CoreError> {
        self.kcp.update(now_ms as u32)?;
        Ok(self.output.take())
    }

    fn flush_now(&mut self, now_ms: u64) -> Result<Vec<Vec<u8>>, CoreError> {
        self.kcp.update(now_ms as u32)?;
        self.kcp.flush()?;
        Ok(self.output.take())
    }

    fn next_deadline(&self, now_ms: u64) -> Option<u64> {
        if self.kcp.wait_snd() == 0 {
            return None;
        }
        Some(now_ms.saturating_add(self.kcp.check(now_ms as u32) as u64))
    }

    fn recv_frames(&mut self) -> Result<Vec<SessionFrame>, CoreError> {
        let mut frames = Vec::new();
        loop {
            let size = match self.kcp.peeksize() {
                Ok(size) => size,
                Err(KcpError::RecvQueueEmpty | KcpError::ExpectingFragment) => break,
                Err(error) => return Err(error.into()),
            };
            let mut payload = vec![0; size];
            let read = self.kcp.recv(&mut payload)?;
            payload.truncate(read);
            frames.push(SessionFrame::decode(&payload)?);
        }
        Ok(frames)
    }
}

#[derive(Clone, Debug, Default)]
struct SessionOutputBuffer(Arc<Mutex<Vec<Vec<u8>>>>);

impl SessionOutputBuffer {
    fn take(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.0.lock().expect("session output mutex poisoned"))
    }
}

impl Write for SessionOutputBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("session output mutex poisoned"))?
            .push(bytes.to_vec());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn session_mtu(transport_mtu: usize) -> usize {
    transport_mtu.saturating_sub(128).max(50)
}

#[derive(Debug)]
pub struct RelayCore {
    config: RelayConfig,
    local_channels: LocalChannelTable,
    envelope_nonce_filter: NonceFilter,
    envelope_random: RandomStream,
}

impl RelayCore {
    pub fn new(config: RelayConfig) -> Result<Self, ConfigError> {
        validate_relay_config(&config)?;
        Ok(Self {
            local_channels: LocalChannelTable::new(config.local_channels.iter().copied()),
            envelope_nonce_filter: NonceFilter::new(1 << 24, 0.00001, 1 << 16, 0),
            envelope_random: RandomStream::new(config.random_seed, b"relay envelope"),
            config,
        })
    }

    pub fn handle_event(
        &mut self,
        now_ms: u64,
        event: CoreEvent,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        match event {
            CoreEvent::TransportPacketReceived { channel_id, bytes } => {
                self.local_channels.record_receive(channel_id);
                let envelope = decode_hop_packet(
                    now_ms,
                    &self.config.envelope_key,
                    &mut self.envelope_nonce_filter,
                    &bytes,
                )?;
                self.forward_envelope(now_ms, envelope, actions)
            }
            CoreEvent::TransportChannelUpdated {
                channel_id,
                state,
                metrics,
            } => {
                self.local_channels
                    .update_channel(channel_id, state, metrics);
                actions.push(CoreAction::EmitMetric(Metric::TransportChannelUpdated {
                    channel_id,
                    state,
                    metrics,
                }));
                Ok(())
            }
            CoreEvent::IngressSessionRequested { .. }
            | CoreEvent::ExitConnectionOpened { .. }
            | CoreEvent::ExitConnectionOpenFailed { .. }
            | CoreEvent::SessionBytes { .. }
            | CoreEvent::SessionClosed { .. } => Err(CoreError::UnsupportedEvent),
        }
    }

    pub fn poll(&mut self, _now_ms: u64, _actions: &mut Vec<CoreAction>) -> Result<(), CoreError> {
        Ok(())
    }

    pub fn next_deadline(&self, _now_ms: u64) -> Option<u64> {
        None
    }

    pub fn channel_count(&self) -> usize {
        self.local_channels.channel_ids().count()
    }

    fn forward_envelope(
        &mut self,
        now_ms: u64,
        mut envelope: Envelope,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        if envelope.route_plan.is_empty() {
            return Err(CoreError::NoRoute);
        }
        let channel_id = envelope.route_plan.remove(0);
        if !self.local_channels.is_usable(channel_id) {
            return Err(CoreError::NoRoute);
        }
        actions.push(CoreAction::SendTransportPacket {
            channel_id,
            bytes: encode_hop_packet(
                now_ms,
                &self.config.envelope_key,
                &mut self.envelope_random,
                envelope,
            )?,
        });
        self.local_channels.record_send(channel_id);
        Ok(())
    }
}

fn validate_endpoint_config(config: &EndpointConfig) -> Result<(), ConfigError> {
    if config.local_node_id == 0 {
        return Err(ConfigError::EmptyNodeId);
    }
    if config.transport_mtu == 0 {
        return Err(ConfigError::ZeroMtu);
    }
    let local_channels = LocalChannelTable::new(config.local_channels.iter().copied());
    for node in &config.route_topology.nodes {
        if node.node_id == config.local_node_id {
            for channel in &node.channels {
                if !local_channels.contains(channel.channel_id) {
                    return Err(ConfigError::UnknownLocalChannel(channel.channel_id));
                }
            }
        }
    }
    Ok(())
}

fn validate_relay_config(config: &RelayConfig) -> Result<(), ConfigError> {
    if config.local_node_id == 0 {
        return Err(ConfigError::EmptyNodeId);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{RouteChannel, RouteNode};

    const TEST_ENVELOPE_KEY: [u8; 16] = [2; 16];
    const TEST_MESSAGE_KEY: [u8; 16] = [3; 16];
    const TEST_SEED: [u8; 16] = [4; 16];

    fn topology(local: NodeId, route: &[(ChannelId, NodeId)]) -> RouteTopology {
        let mut nodes = vec![RouteNode {
            node_id: local,
            channels: route
                .iter()
                .map(|(channel_id, peer_node_id)| RouteChannel {
                    channel_id: *channel_id,
                    peer_node_id: *peer_node_id,
                })
                .collect(),
        }];
        if let Some((_, destination)) = route.last() {
            nodes.push(RouteNode {
                node_id: *destination,
                channels: Vec::new(),
            });
        }
        RouteTopology { nodes }
    }

    fn endpoint(local_node_id: NodeId, channel_id: ChannelId, peer: NodeId) -> EndpointCore {
        EndpointCore::new(EndpointConfig {
            local_node_id,
            envelope_key: TEST_ENVELOPE_KEY,
            message_key: TEST_MESSAGE_KEY,
            random_seed: TEST_SEED,
            local_channels: vec![channel_id],
            route_topology: topology(local_node_id, &[(channel_id, peer)]),
            transport_mtu: 1200,
        })
        .unwrap()
    }

    fn relay(local_node_id: NodeId, channels: Vec<ChannelId>) -> RelayCore {
        RelayCore::new(RelayConfig {
            local_node_id,
            envelope_key: TEST_ENVELOPE_KEY,
            random_seed: TEST_SEED,
            local_channels: channels,
        })
        .unwrap()
    }

    fn encode_test_packet(now_ms: u64, envelope: Envelope) -> Vec<u8> {
        let mut random = RandomStream::new(TEST_SEED, b"test");
        encode_hop_packet(now_ms, &TEST_ENVELOPE_KEY, &mut random, envelope).unwrap()
    }

    fn decode_test_packet(now_ms: u64, bytes: &[u8]) -> Envelope {
        let mut nonce_filter = NonceFilter::new(1 << 24, 0.00001, 1 << 16, 0);
        decode_hop_packet(now_ms, &TEST_ENVELOPE_KEY, &mut nonce_filter, bytes).unwrap()
    }

    fn first_created_conv(actions: &[CoreAction]) -> ConvId {
        actions
            .iter()
            .find_map(|action| match action {
                CoreAction::IngressSessionCreated { conv_id } => Some(*conv_id),
                _ => None,
            })
            .expect("ingress session created action")
    }

    #[test]
    fn allocation_wraps_through_zero() {
        let mut core = endpoint(1, 2, 9);
        core.next_conv_id = u64::MAX;

        assert_eq!(core.allocate_conv_id(), u64::MAX);
        assert_eq!(core.allocate_conv_id(), 0);
    }

    #[test]
    fn relay_consumes_next_channel_from_route_plan() {
        let mut relay = relay(2, vec![2, 3]);
        let envelope = Envelope {
            route_plan: vec![2, 3],
            payload: b"kcp".to_vec(),
        };
        let mut actions = Vec::new();

        relay
            .handle_event(
                100,
                CoreEvent::TransportPacketReceived {
                    channel_id: 1,
                    bytes: encode_test_packet(100, envelope),
                },
                &mut actions,
            )
            .unwrap();

        let [CoreAction::SendTransportPacket { channel_id, bytes }] = actions.as_slice() else {
            panic!("expected exactly one send action");
        };
        assert_eq!(*channel_id, 2);

        let forwarded = decode_test_packet(100, bytes);
        assert_eq!(forwarded.route_plan, vec![3]);
    }

    #[test]
    fn relay_rejects_empty_route_plan() {
        let mut relay = relay(2, vec![2, 3]);
        let envelope = Envelope {
            route_plan: Vec::new(),
            payload: b"kcp".to_vec(),
        };
        let mut actions = Vec::new();

        let result = relay.handle_event(
            100,
            CoreEvent::TransportPacketReceived {
                channel_id: 1,
                bytes: encode_test_packet(100, envelope),
            },
            &mut actions,
        );

        assert!(matches!(result, Err(CoreError::NoRoute)));
        assert!(actions.is_empty());
    }

    #[test]
    fn endpoint_ingress_open_reaches_remote_endpoint() {
        let mut entry = endpoint(1, 2, 9);
        let mut exit = endpoint(9, 4, 1);
        let mut actions = Vec::new();

        entry
            .handle_event(
                100,
                CoreEvent::IngressSessionRequested {
                    target: Target {
                        host: "example.com".to_string(),
                        port: 443,
                    },
                    metadata: Metadata::new(),
                },
                &mut actions,
            )
            .unwrap();

        let conv_id = first_created_conv(&actions);
        let Some(CoreAction::SendTransportPacket { channel_id, bytes }) = actions
            .iter()
            .find(|action| matches!(action, CoreAction::SendTransportPacket { .. }))
        else {
            panic!("expected send transport packet action");
        };
        assert_eq!(*channel_id, 2);
        assert!(decode_test_packet(100, bytes).route_plan.is_empty());

        let mut exit_actions = Vec::new();
        exit.handle_event(
            110,
            CoreEvent::TransportPacketReceived {
                channel_id: 4,
                bytes: bytes.clone(),
            },
            &mut exit_actions,
        )
        .unwrap();

        assert!(exit_actions.iter().any(|action| matches!(
            action,
            CoreAction::OpenExitConnection {
                conv_id: opened,
                target: Target { port: 443, .. },
                ..
            } if *opened == conv_id
        )));
    }

    #[test]
    fn endpoint_session_bytes_reach_remote_session() {
        let mut entry = endpoint(1, 2, 9);
        let mut exit = endpoint(9, 4, 1);
        let mut actions = Vec::new();

        entry
            .handle_event(
                100,
                CoreEvent::IngressSessionRequested {
                    target: Target {
                        host: "example.com".to_string(),
                        port: 443,
                    },
                    metadata: Metadata::new(),
                },
                &mut actions,
            )
            .unwrap();
        let conv_id = first_created_conv(&actions);
        let open_packet = actions
            .iter()
            .find_map(|action| match action {
                CoreAction::SendTransportPacket { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("open session packet");

        let mut exit_actions = Vec::new();
        exit.handle_event(
            110,
            CoreEvent::TransportPacketReceived {
                channel_id: 4,
                bytes: open_packet,
            },
            &mut exit_actions,
        )
        .unwrap();
        assert!(exit_actions.iter().any(|action| matches!(
            action,
            CoreAction::OpenExitConnection { conv_id: opened, .. } if *opened == conv_id
        )));
        exit.handle_event(
            111,
            CoreEvent::ExitConnectionOpened { conv_id },
            &mut Vec::new(),
        )
        .unwrap();

        actions.clear();
        entry
            .handle_event(
                101,
                CoreEvent::SessionBytes {
                    conv_id,
                    bytes: b"hello".to_vec(),
                },
                &mut actions,
            )
            .unwrap();

        let data_packet = actions
            .iter()
            .find_map(|action| match action {
                CoreAction::SendTransportPacket { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("connection bytes packet");

        let mut data_actions = Vec::new();
        exit.handle_event(
            120,
            CoreEvent::TransportPacketReceived {
                channel_id: 4,
                bytes: data_packet,
            },
            &mut data_actions,
        )
        .unwrap();

        assert!(data_actions.iter().any(|action| matches!(
            action,
            CoreAction::WriteSession {
                conv_id: written,
                bytes,
            } if *written == conv_id && bytes == b"hello"
        )));
    }
}
