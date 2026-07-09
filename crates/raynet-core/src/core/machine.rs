use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use crate::kcp::{Error as KcpError, Kcp};
use crate::routing::ChannelRouter;
use aegis::aegis128l::{Aegis128L, Key, Nonce, Tag};

use super::nonce::NonceFilter;
use super::packet::{HopPacketError, open_hop_payload, seal_hop_payload};
use super::wire::{EndpointFrame, Envelope, PacketType, TraceEntry, WireError};

pub type NodeId = u64;
pub type ChannelId = u64;
pub type EndpointId = u64;
pub type StreamId = u64;
pub type LocalConnectionId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

pub type Metadata = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelConfig {
    pub channel_id: ChannelId,
    pub peer_node_id: NodeId,
    pub mtu: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationRoute {
    pub destination_node_id: NodeId,
    pub channel_ids: Vec<ChannelId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointConfig {
    pub node_id: NodeId,
    pub endpoint_id: EndpointId,
    pub hop_key: [u8; 16],
    pub endpoint_key: [u8; 16],
    pub default_destination_node_id: NodeId,
    pub default_channel_plan: Vec<ChannelId>,
    pub destination_routes: Vec<DestinationRoute>,
    pub channels: Vec<ChannelConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayConfig {
    pub node_id: NodeId,
    pub hop_key: [u8; 16],
    pub destination_routes: Vec<DestinationRoute>,
    pub channels: Vec<ChannelConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConfigDelta;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Metric {
    TransportChannelUpdated {
        channel_id: ChannelId,
        state: ChannelState,
        metrics: TransportMetrics,
    },
    LocalConnectionOpened {
        local_connection_id: LocalConnectionId,
    },
    LocalConnectionClosed {
        local_connection_id: LocalConnectionId,
    },
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
    IngressConnectionOpened {
        local_connection_id: LocalConnectionId,
        target: Target,
        metadata: Metadata,
    },
    ExitConnectionOpened {
        stream_id: StreamId,
        local_connection_id: LocalConnectionId,
    },
    ExitConnectionOpenFailed {
        stream_id: StreamId,
        reason: OpenFailureReason,
    },
    LocalConnectionBytes {
        local_connection_id: LocalConnectionId,
        bytes: Vec<u8>,
    },
    LocalConnectionClosed {
        local_connection_id: LocalConnectionId,
        reason: CloseReason,
    },
    ConfigUpdated(ConfigDelta),
}

#[derive(Debug, Clone, PartialEq)]
pub enum CoreAction {
    SendTransportPacket {
        channel_id: ChannelId,
        bytes: Vec<u8>,
    },
    OpenExitConnection {
        stream_id: StreamId,
        target: Target,
        metadata: Metadata,
    },
    WriteLocalConnection {
        local_connection_id: LocalConnectionId,
        bytes: Vec<u8>,
    },
    CloseLocalConnection {
        local_connection_id: LocalConnectionId,
        reason: CloseReason,
    },
    CloseRemoteStream {
        stream_id: StreamId,
        reason: CloseReason,
    },
    EmitMetric(Metric),
    EmitLog {
        level: LogLevel,
        event: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("node id must be non-zero")]
    EmptyNodeId,
    #[error("endpoint id must be non-zero")]
    EmptyEndpointId,
    #[error("channel {0} has zero MTU")]
    ZeroMtu(ChannelId),
    #[error("unknown channel {0}")]
    UnknownChannel(ChannelId),
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("event is not supported by this core role")]
    UnsupportedEvent,
    #[error("unknown local connection {0}")]
    UnknownLocalConnection(LocalConnectionId),
    #[error("unknown stream {0}")]
    UnknownStream(StreamId),
    #[error("no route is available")]
    NoRoute,
    #[error("wire error: {0}")]
    Wire(#[from] WireError),
    #[error("hop packet error: {0}")]
    HopPacket(#[from] HopPacketError),
    #[error("session error: {0}")]
    Session(#[from] KcpError),
    #[error("endpoint payload authentication failed")]
    EndpointPayloadAuthFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalStream {
    stream_id: StreamId,
    local_connection_id: LocalConnectionId,
}

#[derive(Debug)]
pub struct EndpointCore {
    config: EndpointConfig,
    router: ChannelRouter,
    session: EndpointSession,
    hop_nonce_filter: NonceFilter,
    next_stream_id: u64,
    local_streams: BTreeMap<LocalConnectionId, LocalStream>,
    remote_streams: BTreeMap<StreamId, LocalConnectionId>,
}

impl EndpointCore {
    pub fn new(config: EndpointConfig) -> Result<Self, ConfigError> {
        validate_endpoint_config(&config)?;
        let session_mtu = session_mtu(&config.channels);

        Ok(Self {
            router: ChannelRouter::from_configs(&config.channels),
            session: EndpointSession::new(
                config.node_id,
                config.endpoint_id,
                config.endpoint_key,
                session_mtu,
            ),
            hop_nonce_filter: NonceFilter::new(1 << 24, 0.00001, 1 << 16, 0),
            config,
            next_stream_id: 1,
            local_streams: BTreeMap::new(),
            remote_streams: BTreeMap::new(),
        })
    }

    pub fn handle_event(
        &mut self,
        now_ms: u64,
        event: CoreEvent,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        match event {
            CoreEvent::IngressConnectionOpened {
                local_connection_id,
                target,
                metadata,
            } => {
                let stream_id = self.allocate_stream_id();
                self.local_streams.insert(
                    local_connection_id,
                    LocalStream {
                        stream_id,
                        local_connection_id,
                    },
                );
                self.send_endpoint_frame(
                    now_ms,
                    EndpointFrame::OpenStream {
                        stream_id,
                        target,
                        metadata,
                    },
                    actions,
                )?;
                actions.push(CoreAction::EmitMetric(Metric::LocalConnectionOpened {
                    local_connection_id,
                }));
                Ok(())
            }
            CoreEvent::ExitConnectionOpened {
                stream_id,
                local_connection_id,
            } => {
                self.remote_streams.insert(stream_id, local_connection_id);
                Ok(())
            }
            CoreEvent::LocalConnectionBytes {
                local_connection_id,
                bytes,
            } => {
                let stream_id = self
                    .stream_for_local_connection(local_connection_id)
                    .ok_or(CoreError::UnknownLocalConnection(local_connection_id))?;
                self.send_endpoint_frame(
                    now_ms,
                    EndpointFrame::StreamBytes { stream_id, bytes },
                    actions,
                )
            }
            CoreEvent::LocalConnectionClosed {
                local_connection_id,
                reason,
            } => {
                if let Some(stream) = self.local_streams.remove(&local_connection_id) {
                    self.send_endpoint_frame(
                        now_ms,
                        EndpointFrame::CloseStream {
                            stream_id: stream.stream_id,
                            reason,
                        },
                        actions,
                    )?;
                    actions.push(CoreAction::EmitMetric(Metric::LocalConnectionClosed {
                        local_connection_id,
                    }));
                    return Ok(());
                }

                if let Some(stream_id) = self
                    .remote_streams
                    .iter()
                    .find_map(|(stream_id, id)| (*id == local_connection_id).then_some(*stream_id))
                {
                    self.remote_streams.remove(&stream_id);
                    self.send_endpoint_frame(
                        now_ms,
                        EndpointFrame::CloseStream { stream_id, reason },
                        actions,
                    )?;
                    actions.push(CoreAction::EmitMetric(Metric::LocalConnectionClosed {
                        local_connection_id,
                    }));
                    return Ok(());
                }

                Err(CoreError::UnknownLocalConnection(local_connection_id))
            }
            CoreEvent::TransportChannelUpdated {
                channel_id,
                state,
                metrics,
            } => {
                self.router.update_channel(channel_id, state, metrics);
                actions.push(CoreAction::EmitMetric(Metric::TransportChannelUpdated {
                    channel_id,
                    state,
                    metrics,
                }));
                Ok(())
            }
            CoreEvent::ConfigUpdated(_) => Ok(()),
            CoreEvent::TransportPacketReceived { channel_id, bytes } => {
                self.router.record_receive(channel_id);
                self.handle_transport_packet(now_ms, bytes, actions)
            }
            CoreEvent::ExitConnectionOpenFailed { stream_id, reason } => self.send_endpoint_frame(
                now_ms,
                EndpointFrame::ResetStream {
                    stream_id,
                    reason: CloseReason::Error(format!("{reason:?}")),
                },
                actions,
            ),
        }
    }

    pub fn poll(&mut self, now_ms: u64, actions: &mut Vec<CoreAction>) -> Result<(), CoreError> {
        let packets = self.session.poll(now_ms)?;
        self.send_session_packets(now_ms, packets, actions)
    }

    pub fn next_deadline(&self, now_ms: u64) -> Option<u64> {
        self.session.next_deadline(now_ms)
    }

    pub fn channel_count(&self) -> usize {
        self.config.channels.len()
    }

    fn allocate_stream_id(&mut self) -> StreamId {
        let stream_id = self.next_stream_id;
        self.next_stream_id = self.next_stream_id.saturating_add(1).max(1);
        stream_id
    }

    fn stream_for_local_connection(
        &self,
        local_connection_id: LocalConnectionId,
    ) -> Option<StreamId> {
        self.local_streams
            .get(&local_connection_id)
            .map(|stream| stream.stream_id)
            .or_else(|| {
                self.remote_streams
                    .iter()
                    .find_map(|(stream_id, id)| (*id == local_connection_id).then_some(*stream_id))
            })
    }

    fn local_connection_for_stream(&self, stream_id: StreamId) -> Option<LocalConnectionId> {
        self.local_streams
            .values()
            .find_map(|stream| {
                (stream.stream_id == stream_id).then_some(stream.local_connection_id)
            })
            .or_else(|| self.remote_streams.get(&stream_id).copied())
    }

    fn send_endpoint_frame(
        &mut self,
        now_ms: u64,
        frame: EndpointFrame,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        let packets = self.session.send_frame(now_ms, frame)?;
        self.send_session_packets(now_ms, packets, actions)
    }

    fn send_session_packets(
        &mut self,
        now_ms: u64,
        packets: Vec<Vec<u8>>,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        for packet in packets {
            self.send_envelope_payload(now_ms, packet, actions)?;
        }
        Ok(())
    }

    fn send_envelope_payload(
        &mut self,
        now_ms: u64,
        payload: Vec<u8>,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        let (channel_id, channel_plan) = self.next_outbound_plan()?;
        let envelope = Envelope {
            packet_type: PacketType::Data,
            source_node_id: self.config.node_id,
            destination_node_id: self.config.default_destination_node_id,
            channel_plan,
            return_trace: Vec::new(),
            payload,
        };
        actions.push(CoreAction::SendTransportPacket {
            channel_id,
            bytes: encode_hop_packet(now_ms, &self.config.hop_key, envelope)?,
        });
        self.router.record_send(channel_id);
        Ok(())
    }

    fn next_outbound_plan(&mut self) -> Result<(ChannelId, Vec<ChannelId>), CoreError> {
        if let Some((first, rest)) = self.config.default_channel_plan.split_first() {
            if !self.router.is_usable(*first) {
                return Err(CoreError::NoRoute);
            }
            return Ok((*first, rest.to_vec()));
        }
        let channel_id = self.router.select_channel().ok_or(CoreError::NoRoute)?;
        Ok((channel_id, Vec::new()))
    }

    fn handle_transport_packet(
        &mut self,
        now_ms: u64,
        bytes: Vec<u8>,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        let envelope = decode_hop_packet(
            now_ms,
            &self.config.hop_key,
            &mut self.hop_nonce_filter,
            &bytes,
        )?;
        if envelope.destination_node_id != self.config.node_id || !envelope.channel_plan.is_empty()
        {
            return self.forward_envelope(now_ms, envelope, actions);
        }

        let session_output = self.session.input_packet(now_ms, &envelope.payload)?;
        self.send_session_packets(now_ms, session_output.packets, actions)?;

        for frame in session_output.frames {
            self.handle_endpoint_frame(frame, actions)?;
        }
        Ok(())
    }

    fn handle_endpoint_frame(
        &mut self,
        frame: EndpointFrame,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        match frame {
            EndpointFrame::OpenStream {
                stream_id,
                target,
                metadata,
            } => {
                actions.push(CoreAction::OpenExitConnection {
                    stream_id,
                    target,
                    metadata,
                });
                Ok(())
            }
            EndpointFrame::StreamBytes { stream_id, bytes } => {
                let local_connection_id = self
                    .local_connection_for_stream(stream_id)
                    .ok_or(CoreError::UnknownStream(stream_id))?;
                actions.push(CoreAction::WriteLocalConnection {
                    local_connection_id,
                    bytes,
                });
                Ok(())
            }
            EndpointFrame::CloseStream { stream_id, reason }
            | EndpointFrame::ResetStream { stream_id, reason } => {
                let local_connection_id = self
                    .local_connection_for_stream(stream_id)
                    .ok_or(CoreError::UnknownStream(stream_id))?;
                actions.push(CoreAction::CloseLocalConnection {
                    local_connection_id,
                    reason,
                });
                Ok(())
            }
            EndpointFrame::KeepAlive => Ok(()),
        }
    }

    fn forward_envelope(
        &mut self,
        now_ms: u64,
        mut envelope: Envelope,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        let channel_id = if !envelope.channel_plan.is_empty() {
            let channel_id = envelope.channel_plan.remove(0);
            if !self.router.is_usable(channel_id) {
                return Err(CoreError::NoRoute);
            }
            channel_id
        } else {
            self.select_channel_for_destination(envelope.destination_node_id)?
        };
        envelope.return_trace.push(TraceEntry {
            node_id: self.config.node_id,
            channel_id,
        });
        actions.push(CoreAction::SendTransportPacket {
            channel_id,
            bytes: encode_hop_packet(now_ms, &self.config.hop_key, envelope)?,
        });
        self.router.record_send(channel_id);
        Ok(())
    }

    fn select_channel_for_destination(
        &mut self,
        destination_node_id: NodeId,
    ) -> Result<ChannelId, CoreError> {
        if let Some(route) = self
            .config
            .destination_routes
            .iter()
            .find(|route| route.destination_node_id == destination_node_id)
        {
            return self
                .router
                .select_candidate(route.channel_ids.iter().copied())
                .ok_or(CoreError::NoRoute);
        }
        self.router.select_channel().ok_or(CoreError::NoRoute)
    }
}

fn encode_hop_packet(now_ms: u64, hop_key: &Key, envelope: Envelope) -> Result<Vec<u8>, CoreError> {
    let payload = envelope.encode()?;
    Ok(seal_hop_payload(now_ms, hop_key, &payload))
}

fn decode_hop_packet(
    now_ms: u64,
    hop_key: &Key,
    nonce_filter: &mut NonceFilter,
    bytes: &[u8],
) -> Result<Envelope, CoreError> {
    let payload = open_hop_payload(now_ms, hop_key, bytes, nonce_filter)?;
    Envelope::decode(&payload).map_err(CoreError::from)
}

#[derive(Debug)]
struct EndpointSession {
    kcp: Kcp<SessionOutputBuffer>,
    output: SessionOutputBuffer,
    local_node_id: NodeId,
    endpoint_id: EndpointId,
    endpoint_key: Key,
    next_nonce_counter: u64,
}

#[derive(Debug)]
struct SessionOutput {
    frames: Vec<EndpointFrame>,
    packets: Vec<Vec<u8>>,
}

impl EndpointSession {
    fn new(local_node_id: NodeId, endpoint_id: EndpointId, endpoint_key: Key, mtu: usize) -> Self {
        let output = SessionOutputBuffer::default();
        let mut kcp = Kcp::new(endpoint_id as u32, output.clone());
        kcp.set_mtu(mtu)
            .expect("session_mtu returns a valid KCP MTU");
        kcp.set_nodelay(true, 20, 2, true);
        kcp.set_wndsize(128, 128);
        Self {
            kcp,
            output,
            local_node_id,
            endpoint_id,
            endpoint_key,
            next_nonce_counter: 1,
        }
    }

    fn send_frame(&mut self, now_ms: u64, frame: EndpointFrame) -> Result<Vec<Vec<u8>>, CoreError> {
        let payload = frame.encode()?;
        self.kcp.send(&payload)?;
        self.flush_now(now_ms)
    }

    fn input_packet(&mut self, now_ms: u64, packet: &[u8]) -> Result<SessionOutput, CoreError> {
        let packet = self.decrypt_packet(packet)?;
        self.kcp.input(&packet)?;
        let packets = self.flush_now(now_ms)?;
        let frames = self.recv_frames()?;
        Ok(SessionOutput { frames, packets })
    }

    fn poll(&mut self, now_ms: u64) -> Result<Vec<Vec<u8>>, CoreError> {
        self.kcp.update(now_ms as u32)?;
        self.encrypt_packets(self.output.take())
    }

    fn flush_now(&mut self, now_ms: u64) -> Result<Vec<Vec<u8>>, CoreError> {
        self.kcp.update(now_ms as u32)?;
        self.kcp.flush()?;
        self.encrypt_packets(self.output.take())
    }

    fn next_deadline(&self, now_ms: u64) -> Option<u64> {
        if self.kcp.wait_snd() == 0 {
            return None;
        }
        Some(now_ms.saturating_add(self.kcp.check(now_ms as u32) as u64))
    }

    fn recv_frames(&mut self) -> Result<Vec<EndpointFrame>, CoreError> {
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
            frames.push(EndpointFrame::decode(&payload)?);
        }
        Ok(frames)
    }

    fn encrypt_packets(&mut self, packets: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>, CoreError> {
        packets
            .into_iter()
            .map(|packet| self.encrypt_packet(packet))
            .collect()
    }

    fn encrypt_packet(&mut self, mut packet: Vec<u8>) -> Result<Vec<u8>, CoreError> {
        let nonce = self.next_nonce();
        let tag: Tag<16> =
            Aegis128L::new(&self.endpoint_key, &nonce).encrypt_in_place(&mut packet, &self.ad());
        let mut out = Vec::with_capacity(32 + packet.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&tag);
        out.extend_from_slice(&packet);
        Ok(out)
    }

    fn decrypt_packet(&self, packet: &[u8]) -> Result<Vec<u8>, CoreError> {
        if packet.len() < 32 {
            return Err(CoreError::EndpointPayloadAuthFailed);
        }
        let nonce: Nonce = packet[0..16]
            .try_into()
            .map_err(|_| CoreError::EndpointPayloadAuthFailed)?;
        let tag: Tag<16> = packet[16..32]
            .try_into()
            .map_err(|_| CoreError::EndpointPayloadAuthFailed)?;
        Aegis128L::new(&self.endpoint_key, &nonce)
            .decrypt(&packet[32..], &tag, &self.ad())
            .map_err(|_| CoreError::EndpointPayloadAuthFailed)
    }

    fn next_nonce(&mut self) -> Nonce {
        let mut nonce = [0; 16];
        nonce[0..8].copy_from_slice(&self.local_node_id.to_le_bytes());
        nonce[8..16].copy_from_slice(&self.next_nonce_counter.to_le_bytes());
        self.next_nonce_counter = self.next_nonce_counter.saturating_add(1).max(1);
        nonce
    }

    fn ad(&self) -> [u8; 16] {
        let mut ad = [0; 16];
        ad[0..8].copy_from_slice(&self.endpoint_id.to_le_bytes());
        ad[8..16].copy_from_slice(b"RNEP1\0\0\0");
        ad
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

fn session_mtu(channels: &[ChannelConfig]) -> usize {
    channels
        .iter()
        .map(|channel| channel.mtu)
        .min()
        .unwrap_or(1200)
        .saturating_sub(128)
        .max(50)
}

#[derive(Debug)]
pub struct RelayCore {
    config: RelayConfig,
    router: ChannelRouter,
    hop_nonce_filter: NonceFilter,
}

impl RelayCore {
    pub fn new(config: RelayConfig) -> Result<Self, ConfigError> {
        validate_relay_config(&config)?;
        Ok(Self {
            router: ChannelRouter::from_configs(&config.channels),
            hop_nonce_filter: NonceFilter::new(1 << 24, 0.00001, 1 << 16, 0),
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
                self.router.record_receive(channel_id);
                let envelope = decode_hop_packet(
                    now_ms,
                    &self.config.hop_key,
                    &mut self.hop_nonce_filter,
                    &bytes,
                )?;
                self.forward_envelope(now_ms, envelope, actions)
            }
            CoreEvent::TransportChannelUpdated {
                channel_id,
                state,
                metrics,
            } => {
                self.router.update_channel(channel_id, state, metrics);
                actions.push(CoreAction::EmitMetric(Metric::TransportChannelUpdated {
                    channel_id,
                    state,
                    metrics,
                }));
                Ok(())
            }
            CoreEvent::ConfigUpdated(_) => Ok(()),
            CoreEvent::IngressConnectionOpened { .. }
            | CoreEvent::ExitConnectionOpened { .. }
            | CoreEvent::ExitConnectionOpenFailed { .. }
            | CoreEvent::LocalConnectionBytes { .. }
            | CoreEvent::LocalConnectionClosed { .. } => Err(CoreError::UnsupportedEvent),
        }
    }

    pub fn poll(&mut self, _now_ms: u64, _actions: &mut Vec<CoreAction>) -> Result<(), CoreError> {
        Ok(())
    }

    pub fn next_deadline(&self, _now_ms: u64) -> Option<u64> {
        None
    }

    pub fn channel_count(&self) -> usize {
        self.config.channels.len()
    }

    fn forward_envelope(
        &mut self,
        now_ms: u64,
        mut envelope: Envelope,
        actions: &mut Vec<CoreAction>,
    ) -> Result<(), CoreError> {
        let channel_id = if !envelope.channel_plan.is_empty() {
            let channel_id = envelope.channel_plan.remove(0);
            if !self.router.is_usable(channel_id) {
                return Err(CoreError::NoRoute);
            }
            channel_id
        } else {
            self.select_channel_for_destination(envelope.destination_node_id)?
        };
        envelope.return_trace.push(TraceEntry {
            node_id: self.config.node_id,
            channel_id,
        });
        actions.push(CoreAction::SendTransportPacket {
            channel_id,
            bytes: encode_hop_packet(now_ms, &self.config.hop_key, envelope)?,
        });
        self.router.record_send(channel_id);
        Ok(())
    }

    fn select_channel_for_destination(
        &mut self,
        destination_node_id: NodeId,
    ) -> Result<ChannelId, CoreError> {
        if let Some(route) = self
            .config
            .destination_routes
            .iter()
            .find(|route| route.destination_node_id == destination_node_id)
        {
            return self
                .router
                .select_candidate(route.channel_ids.iter().copied())
                .ok_or(CoreError::NoRoute);
        }
        self.router.select_channel().ok_or(CoreError::NoRoute)
    }
}

fn validate_endpoint_config(config: &EndpointConfig) -> Result<(), ConfigError> {
    if config.node_id == 0 {
        return Err(ConfigError::EmptyNodeId);
    }
    if config.endpoint_id == 0 {
        return Err(ConfigError::EmptyEndpointId);
    }
    if config.default_destination_node_id == 0 {
        return Err(ConfigError::EmptyNodeId);
    }
    validate_destination_routes(&config.destination_routes, &config.channels)?;
    validate_channels(&config.channels)
}

fn validate_relay_config(config: &RelayConfig) -> Result<(), ConfigError> {
    if config.node_id == 0 {
        return Err(ConfigError::EmptyNodeId);
    }
    validate_destination_routes(&config.destination_routes, &config.channels)?;
    validate_channels(&config.channels)
}

fn validate_channels(channels: &[ChannelConfig]) -> Result<(), ConfigError> {
    for channel in channels {
        if channel.mtu == 0 {
            return Err(ConfigError::ZeroMtu(channel.channel_id));
        }
    }
    Ok(())
}

fn validate_destination_routes(
    routes: &[DestinationRoute],
    channels: &[ChannelConfig],
) -> Result<(), ConfigError> {
    for route in routes {
        if route.destination_node_id == 0 {
            return Err(ConfigError::EmptyNodeId);
        }
        for channel_id in &route.channel_ids {
            if !channels
                .iter()
                .any(|channel| channel.channel_id == *channel_id)
            {
                return Err(ConfigError::UnknownChannel(*channel_id));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_HOP_KEY: [u8; 16] = [2; 16];
    const TEST_ENDPOINT_KEY: [u8; 16] = [3; 16];

    fn channel(channel_id: ChannelId) -> ChannelConfig {
        ChannelConfig {
            channel_id,
            peer_node_id: channel_id + 10,
            mtu: 1200,
        }
    }

    fn encode_test_packet(now_ms: u64, envelope: Envelope) -> Vec<u8> {
        encode_hop_packet(now_ms, &TEST_HOP_KEY, envelope).unwrap()
    }

    fn decode_test_packet(now_ms: u64, bytes: &[u8]) -> Envelope {
        let mut nonce_filter = NonceFilter::new(1 << 24, 0.00001, 1 << 16, 0);
        decode_hop_packet(now_ms, &TEST_HOP_KEY, &mut nonce_filter, bytes).unwrap()
    }

    #[test]
    fn relay_consumes_next_channel_from_envelope_plan() {
        let mut relay = RelayCore::new(RelayConfig {
            node_id: 2,
            hop_key: TEST_HOP_KEY,
            destination_routes: Vec::new(),
            channels: vec![channel(2), channel(3)],
        })
        .unwrap();
        let envelope = Envelope {
            packet_type: PacketType::Data,
            source_node_id: 1,
            destination_node_id: 9,
            channel_plan: vec![2, 3],
            return_trace: Vec::new(),
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
        assert_eq!(forwarded.channel_plan, vec![3]);
        assert_eq!(
            forwarded.return_trace,
            vec![TraceEntry {
                node_id: 2,
                channel_id: 2,
            }]
        );
    }

    #[test]
    fn relay_rejects_down_channel_from_envelope_plan() {
        let mut relay = RelayCore::new(RelayConfig {
            node_id: 2,
            hop_key: TEST_HOP_KEY,
            destination_routes: Vec::new(),
            channels: vec![channel(2), channel(3)],
        })
        .unwrap();
        relay
            .handle_event(
                90,
                CoreEvent::TransportChannelUpdated {
                    channel_id: 2,
                    state: ChannelState::Down,
                    metrics: TransportMetrics {
                        queue_pressure: 0.0,
                        send_error: true,
                    },
                },
                &mut Vec::new(),
            )
            .unwrap();
        let envelope = Envelope {
            packet_type: PacketType::Data,
            source_node_id: 1,
            destination_node_id: 9,
            channel_plan: vec![2, 3],
            return_trace: Vec::new(),
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
    fn relay_uses_destination_route_when_plan_is_empty() {
        let mut relay = RelayCore::new(RelayConfig {
            node_id: 2,
            hop_key: TEST_HOP_KEY,
            destination_routes: vec![DestinationRoute {
                destination_node_id: 9,
                channel_ids: vec![3],
            }],
            channels: vec![channel(2), channel(3)],
        })
        .unwrap();
        let envelope = Envelope {
            packet_type: PacketType::Data,
            source_node_id: 1,
            destination_node_id: 9,
            channel_plan: Vec::new(),
            return_trace: Vec::new(),
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
        assert_eq!(*channel_id, 3);

        let forwarded = decode_test_packet(100, bytes);
        assert_eq!(
            forwarded.return_trace,
            vec![TraceEntry {
                node_id: 2,
                channel_id: 3,
            }]
        );
    }

    #[test]
    fn endpoint_ingress_open_reaches_remote_endpoint() {
        let mut endpoint = EndpointCore::new(EndpointConfig {
            node_id: 1,
            endpoint_id: 1,
            hop_key: TEST_HOP_KEY,
            endpoint_key: TEST_ENDPOINT_KEY,
            default_destination_node_id: 9,
            default_channel_plan: vec![2],
            destination_routes: Vec::new(),
            channels: vec![channel(2), channel(3)],
        })
        .unwrap();
        let mut remote = EndpointCore::new(EndpointConfig {
            node_id: 9,
            endpoint_id: 1,
            hop_key: TEST_HOP_KEY,
            endpoint_key: TEST_ENDPOINT_KEY,
            default_destination_node_id: 1,
            default_channel_plan: vec![4],
            destination_routes: Vec::new(),
            channels: vec![channel(4)],
        })
        .unwrap();
        let mut actions = Vec::new();

        endpoint
            .handle_event(
                100,
                CoreEvent::IngressConnectionOpened {
                    local_connection_id: 77,
                    target: Target {
                        host: "example.com".to_string(),
                        port: 443,
                    },
                    metadata: Metadata::new(),
                },
                &mut actions,
            )
            .unwrap();

        let Some(CoreAction::SendTransportPacket { channel_id, bytes }) = actions.first() else {
            panic!("expected send transport packet action");
        };
        assert_eq!(*channel_id, 2);

        let envelope = decode_test_packet(100, bytes);
        assert_eq!(envelope.source_node_id, 1);
        assert_eq!(envelope.destination_node_id, 9);
        assert!(envelope.channel_plan.is_empty());
        assert!(EndpointFrame::decode(&envelope.payload).is_err());

        let mut remote_actions = Vec::new();
        remote
            .handle_event(
                110,
                CoreEvent::TransportPacketReceived {
                    channel_id: 4,
                    bytes: bytes.clone(),
                },
                &mut remote_actions,
            )
            .unwrap();

        assert!(remote_actions.iter().any(|action| matches!(
            action,
            CoreAction::OpenExitConnection {
                stream_id: 1,
                target: Target { port: 443, .. },
                ..
            }
        )));
    }

    #[test]
    fn endpoint_local_bytes_reach_remote_stream() {
        let mut endpoint = EndpointCore::new(EndpointConfig {
            node_id: 1,
            endpoint_id: 1,
            hop_key: TEST_HOP_KEY,
            endpoint_key: TEST_ENDPOINT_KEY,
            default_destination_node_id: 9,
            default_channel_plan: vec![2],
            destination_routes: Vec::new(),
            channels: vec![channel(2)],
        })
        .unwrap();
        let mut remote = EndpointCore::new(EndpointConfig {
            node_id: 9,
            endpoint_id: 1,
            hop_key: TEST_HOP_KEY,
            endpoint_key: TEST_ENDPOINT_KEY,
            default_destination_node_id: 1,
            default_channel_plan: vec![4],
            destination_routes: Vec::new(),
            channels: vec![channel(4)],
        })
        .unwrap();
        let mut actions = Vec::new();

        endpoint
            .handle_event(
                100,
                CoreEvent::IngressConnectionOpened {
                    local_connection_id: 77,
                    target: Target {
                        host: "example.com".to_string(),
                        port: 443,
                    },
                    metadata: Metadata::new(),
                },
                &mut actions,
            )
            .unwrap();
        let open_packet = actions
            .iter()
            .find_map(|action| match action {
                CoreAction::SendTransportPacket { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("open stream packet");

        let mut remote_actions = Vec::new();
        remote
            .handle_event(
                110,
                CoreEvent::TransportPacketReceived {
                    channel_id: 4,
                    bytes: open_packet,
                },
                &mut remote_actions,
            )
            .unwrap();
        assert!(
            remote_actions.iter().any(|action| matches!(
                action,
                CoreAction::OpenExitConnection { stream_id: 1, .. }
            ))
        );
        remote
            .handle_event(
                111,
                CoreEvent::ExitConnectionOpened {
                    stream_id: 1,
                    local_connection_id: 88,
                },
                &mut Vec::new(),
            )
            .unwrap();

        actions.clear();
        endpoint
            .handle_event(
                101,
                CoreEvent::LocalConnectionBytes {
                    local_connection_id: 77,
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
            .expect("stream bytes packet");

        let mut data_actions = Vec::new();
        remote
            .handle_event(
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
            CoreAction::WriteLocalConnection {
                local_connection_id: 88,
                bytes,
            } if bytes == b"hello"
        )));
    }
}
