use std::collections::BTreeMap;

use raynet_core::{
    ChannelId, CloseReason, ConvId, CoreAction, CoreEvent, CoreEventResult, EndpointConfig,
    EndpointCore, KcpConfig, RelayConfig, RelayCore, RouteConfig, RouteEdge, RouteGraph, RouteNode,
};

pub type SimNodeId = u64;

#[derive(Debug, Clone)]
pub struct SimLink {
    pub from_node: SimNodeId,
    pub from_channel_id: ChannelId,
    pub to_node: SimNodeId,
    pub to_channel_id: ChannelId,
    pub latency_ms: u64,
    /// Zero means no serialization delay.
    pub bandwidth_bytes_per_ms: u64,
    pub loss: LossModel,
    pub extra_delay_ms: u64,
    pub down_windows: Vec<DownWindow>,
}

#[derive(Debug, Clone, Copy, Default)]
pub enum LossModel {
    #[default]
    None,
    Every {
        packets: u64,
    },
    Bernoulli {
        loss_ppm: u32,
        seed: u64,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct DownWindow {
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TestMetrics {
    pub sent_packets: u64,
    pub delivered_packets: u64,
    pub dropped_packets: u64,
    pub delivered_local_bytes: u64,
    pub last_delivery_ms: u64,
}

pub struct TestShell {
    now_ms: u64,
    nodes: BTreeMap<SimNodeId, SimNode>,
    links: Vec<SimLink>,
    link_attempts: Vec<u64>,
    packets: Vec<ScheduledPacket>,
    captured_packets: BTreeMap<(SimNodeId, ChannelId), ScheduledPacket>,
    link_next_tx: BTreeMap<(SimNodeId, ChannelId), u64>,
    metrics: TestMetrics,
    delivered_by_outbound_channel: BTreeMap<(SimNodeId, ChannelId), u64>,
}

enum SimNode {
    Endpoint {
        core: Box<EndpointCore>,
        config: EndpointConfig,
        started_at_sim_ms: u64,
        sessions: BTreeMap<ConvId, Vec<u8>>,
        echo_exit: bool,
        blocked_writes: BTreeMap<ConvId, Vec<u8>>,
    },
    Relay {
        core: Box<RelayCore>,
        config: RelayConfig,
        started_at_sim_ms: u64,
    },
}

#[derive(Clone)]
struct ScheduledPacket {
    due_ms: u64,
    from_node: SimNodeId,
    from_channel_id: ChannelId,
    to_node: SimNodeId,
    to_channel_id: ChannelId,
    bytes: Vec<u8>,
}

impl TestShell {
    pub fn new() -> Self {
        Self {
            now_ms: 0,
            nodes: BTreeMap::new(),
            links: Vec::new(),
            link_attempts: Vec::new(),
            packets: Vec::new(),
            captured_packets: BTreeMap::new(),
            link_next_tx: BTreeMap::new(),
            metrics: TestMetrics::default(),
            delivered_by_outbound_channel: BTreeMap::new(),
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }
    pub fn metrics(&self) -> TestMetrics {
        self.metrics
    }

    pub fn add_endpoint(&mut self, node_id: SimNodeId, config: EndpointConfig, echo_exit: bool) {
        self.nodes.insert(
            node_id,
            SimNode::Endpoint {
                core: Box::new(EndpointCore::new(config.clone())),
                config,
                started_at_sim_ms: self.now_ms,
                sessions: BTreeMap::new(),
                echo_exit,
                blocked_writes: BTreeMap::new(),
            },
        );
    }

    pub fn add_relay(&mut self, node_id: SimNodeId, config: RelayConfig) {
        self.nodes.insert(
            node_id,
            SimNode::Relay {
                core: Box::new(RelayCore::new(config.clone())),
                config,
                started_at_sim_ms: self.now_ms,
            },
        );
    }

    pub fn restart_node(&mut self, node_id: SimNodeId) {
        let Some(node) = self.nodes.get_mut(&node_id) else {
            return;
        };
        match node {
            SimNode::Endpoint {
                core,
                config,
                started_at_sim_ms,
                sessions,
                blocked_writes,
                ..
            } => {
                let uptime_ms = self.now_ms - *started_at_sim_ms;
                config.boot_time_ms = config.boot_time_ms.saturating_add(uptime_ms);
                advance_seed(&mut config.random_seed);
                **core = EndpointCore::new(config.clone());
                *started_at_sim_ms = self.now_ms;
                sessions.clear();
                blocked_writes.clear();
            }
            SimNode::Relay {
                core,
                config,
                started_at_sim_ms,
            } => {
                let uptime_ms = self.now_ms - *started_at_sim_ms;
                config.boot_time_ms = config.boot_time_ms.saturating_add(uptime_ms);
                advance_seed(&mut config.random_seed);
                **core = RelayCore::new(config.clone());
                *started_at_sim_ms = self.now_ms;
            }
        }
    }

    pub fn add_link(&mut self, link: SimLink) {
        self.links.push(link);
        self.link_attempts.push(0);
    }

    pub fn open_session(&mut self, node_id: SimNodeId) -> ConvId {
        let mut actions = Vec::new();
        let elapsed_ms = self.elapsed(node_id);
        let result = match self.nodes.get_mut(&node_id) {
            Some(SimNode::Endpoint { core, sessions, .. }) => {
                let result = core.handle_event(elapsed_ms, CoreEvent::SessionOpen, &mut actions);
                let CoreEventResult::SessionCreated { conv_id } = result else {
                    panic!("SessionOpen must return SessionCreated");
                };
                sessions.entry(conv_id).or_default();
                conv_id
            }
            _ => panic!("node {node_id} is not an endpoint"),
        };
        self.handle_actions(node_id, actions);
        result
    }

    /// Returns `true` when the write was fully accepted by core.
    pub fn send_session_bytes(
        &mut self,
        node_id: SimNodeId,
        conv_id: ConvId,
        bytes: impl Into<Vec<u8>>,
    ) -> bool {
        let bytes = bytes.into();
        let mut actions = Vec::new();
        let elapsed_ms = self.elapsed(node_id);
        let result = match self.nodes.get_mut(&node_id) {
            Some(SimNode::Endpoint {
                core,
                blocked_writes,
                ..
            }) => {
                let result = core.handle_event(
                    elapsed_ms,
                    CoreEvent::SessionWrite {
                        conv_id,
                        bytes: bytes.clone(),
                    },
                    &mut actions,
                );
                if matches!(result, CoreEventResult::SessionWriteBlocked) {
                    blocked_writes.insert(conv_id, bytes);
                } else {
                    blocked_writes.remove(&conv_id);
                }
                result
            }
            _ => panic!("node {node_id} is not an endpoint"),
        };
        assert!(
            matches!(
                result,
                CoreEventResult::None | CoreEventResult::SessionWriteBlocked
            ),
            "unexpected SessionWrite result"
        );
        self.handle_actions(node_id, actions);
        matches!(result, CoreEventResult::None)
    }

    pub fn close_session(&mut self, node_id: SimNodeId, conv_id: ConvId, reason: CloseReason) {
        self.handle_event(node_id, CoreEvent::SessionClose { conv_id, reason });
    }

    pub fn has_session(&self, node_id: SimNodeId, conv_id: ConvId) -> bool {
        match self.nodes.get(&node_id) {
            Some(SimNode::Endpoint { sessions, .. }) => sessions.contains_key(&conv_id),
            _ => false,
        }
    }

    pub fn active_sessions(&self, node_id: SimNodeId) -> usize {
        match self.nodes.get(&node_id) {
            Some(SimNode::Endpoint { core, .. }) => core.metrics().active_sessions,
            _ => 0,
        }
    }

    pub fn drop_queued_from(&mut self, from_node: SimNodeId, channel_id: ChannelId) -> usize {
        let before = self.packets.len();
        self.packets.retain(|packet| {
            !(packet.from_node == from_node && packet.from_channel_id == channel_id)
        });
        let dropped = before - self.packets.len();
        self.metrics.dropped_packets = self.metrics.dropped_packets.saturating_add(dropped as u64);
        dropped
    }

    pub fn session_bytes(&self, node_id: SimNodeId, conv_id: ConvId) -> Option<&[u8]> {
        match self.nodes.get(&node_id) {
            Some(SimNode::Endpoint { sessions, .. }) => sessions.get(&conv_id).map(Vec::as_slice),
            _ => None,
        }
    }

    pub fn delivered_on(&self, node_id: SimNodeId, channel_id: ChannelId) -> u64 {
        self.delivered_by_outbound_channel
            .get(&(node_id, channel_id))
            .copied()
            .unwrap_or(0)
    }

    pub fn attempted_on(&self, node_id: SimNodeId, channel_id: ChannelId) -> u64 {
        self.links
            .iter()
            .position(|link| link.from_node == node_id && link.from_channel_id == channel_id)
            .map(|index| self.link_attempts[index])
            .unwrap_or(0)
    }

    pub fn replay_last_packet(&mut self, from_node: SimNodeId, channel_id: ChannelId) -> bool {
        let Some(packet) = self.captured_packets.get(&(from_node, channel_id)).cloned() else {
            return false;
        };
        self.packets.push(ScheduledPacket {
            due_ms: self.now_ms,
            ..packet
        });
        true
    }

    pub fn run_until_idle(&mut self, max_steps: usize) {
        for _ in 0..max_steps {
            self.retry_blocked_writes();
            let packet = self
                .next_packet_index()
                .map(|index| (index, self.packets[index].due_ms));
            let poll = self.next_poll_deadline();
            match (packet, poll) {
                (None, None) => return,
                (Some((_index, due)), Some((node, poll_due))) if poll_due < due => {
                    self.now_ms = if poll_due <= self.now_ms {
                        self.now_ms.saturating_add(1)
                    } else {
                        poll_due
                    };
                    self.poll_node(node);
                }
                (Some((index, _)), _) => {
                    let packet = self.packets.remove(index);
                    self.now_ms = packet.due_ms;
                    self.metrics.delivered_packets =
                        self.metrics.delivered_packets.saturating_add(1);
                    *self
                        .delivered_by_outbound_channel
                        .entry((packet.from_node, packet.from_channel_id))
                        .or_default() += 1;
                    self.handle_event(
                        packet.to_node,
                        CoreEvent::TransportPacketReceived {
                            channel_id: packet.to_channel_id,
                            bytes: packet.bytes,
                        },
                    );
                }
                (None, Some((node, due))) => {
                    self.now_ms = if due <= self.now_ms {
                        self.now_ms.saturating_add(1)
                    } else {
                        due
                    };
                    self.poll_node(node);
                }
            }
        }
    }

    fn retry_blocked_writes(&mut self) {
        let retries: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(node_id, node)| match node {
                SimNode::Endpoint { blocked_writes, .. } if !blocked_writes.is_empty() => Some((
                    *node_id,
                    blocked_writes
                        .iter()
                        .map(|(conv_id, bytes)| (*conv_id, bytes.clone()))
                        .collect::<Vec<_>>(),
                )),
                _ => None,
            })
            .collect();
        for (node_id, writes) in retries {
            for (conv_id, bytes) in writes {
                self.send_session_bytes(node_id, conv_id, bytes);
            }
        }
    }

    fn elapsed(&self, node_id: SimNodeId) -> u64 {
        let started = match self.nodes.get(&node_id) {
            Some(
                SimNode::Endpoint {
                    started_at_sim_ms, ..
                }
                | SimNode::Relay {
                    started_at_sim_ms, ..
                },
            ) => *started_at_sim_ms,
            None => return 0,
        };
        self.now_ms.saturating_sub(started)
    }

    fn handle_event(&mut self, node_id: SimNodeId, event: CoreEvent) {
        let mut actions = Vec::new();
        let elapsed_ms = self.elapsed(node_id);
        match self.nodes.get_mut(&node_id) {
            Some(SimNode::Endpoint { core, .. }) => {
                let _ = core.handle_event(elapsed_ms, event, &mut actions);
            }
            Some(SimNode::Relay { core, .. }) => {
                let _ = core.handle_event(elapsed_ms, event, &mut actions);
            }
            None => return,
        }
        self.handle_actions(node_id, actions);
    }

    fn handle_actions(&mut self, node_id: SimNodeId, actions: Vec<CoreAction>) {
        for action in actions {
            match action {
                CoreAction::SendTransportPacket { channel_id, bytes } => {
                    self.send_transport(node_id, channel_id, bytes)
                }
                CoreAction::OpenSession { conv_id } => {
                    if let Some(SimNode::Endpoint { sessions, .. }) = self.nodes.get_mut(&node_id) {
                        sessions.entry(conv_id).or_default();
                    }
                }
                CoreAction::WriteSession { conv_id, bytes } => {
                    self.write_session(node_id, conv_id, bytes)
                }
                CoreAction::CloseSession { conv_id, .. } => {
                    if let Some(SimNode::Endpoint {
                        sessions,
                        blocked_writes,
                        ..
                    }) = self.nodes.get_mut(&node_id)
                    {
                        sessions.remove(&conv_id);
                        blocked_writes.remove(&conv_id);
                    }
                }
            }
        }
    }

    fn write_session(&mut self, node_id: SimNodeId, conv_id: ConvId, bytes: Vec<u8>) {
        let echo_exit = match self.nodes.get_mut(&node_id) {
            Some(SimNode::Endpoint {
                sessions,
                echo_exit,
                ..
            }) => {
                sessions
                    .entry(conv_id)
                    .or_default()
                    .extend_from_slice(&bytes);
                *echo_exit
            }
            _ => false,
        };
        if echo_exit {
            self.send_session_bytes(node_id, conv_id, bytes);
        } else {
            self.metrics.delivered_local_bytes = self
                .metrics
                .delivered_local_bytes
                .saturating_add(bytes.len() as u64);
            self.metrics.last_delivery_ms = self.now_ms;
        }
    }

    fn send_transport(&mut self, node_id: SimNodeId, channel_id: ChannelId, bytes: Vec<u8>) {
        self.metrics.sent_packets = self.metrics.sent_packets.saturating_add(1);
        let Some(link_index) = self
            .links
            .iter()
            .position(|link| link.from_node == node_id && link.from_channel_id == channel_id)
        else {
            self.transport_send_failed(node_id, channel_id, bytes);
            return;
        };
        let link = self.links[link_index].clone();
        self.link_attempts[link_index] = self.link_attempts[link_index].saturating_add(1);
        if link.is_down(self.now_ms) {
            self.transport_send_failed(node_id, channel_id, bytes);
            return;
        }
        if link.should_drop(self.link_attempts[link_index]) {
            self.metrics.dropped_packets = self.metrics.dropped_packets.saturating_add(1);
            return;
        }
        let next = self
            .link_next_tx
            .entry((node_id, channel_id))
            .or_insert(self.now_ms);
        let starts_at = (*next).max(self.now_ms);
        let transmit_ms = if link.bandwidth_bytes_per_ms == 0 {
            0
        } else {
            (bytes.len() as u64).div_ceil(link.bandwidth_bytes_per_ms)
        };
        *next = starts_at.saturating_add(transmit_ms);
        let packet = ScheduledPacket {
            due_ms: starts_at
                .saturating_add(transmit_ms)
                .saturating_add(link.latency_ms)
                .saturating_add(link.extra_delay_ms),
            from_node: node_id,
            from_channel_id: channel_id,
            to_node: link.to_node,
            to_channel_id: link.to_channel_id,
            bytes,
        };
        self.captured_packets
            .insert((node_id, channel_id), packet.clone());
        self.packets.push(packet);
    }

    fn transport_send_failed(&mut self, node_id: SimNodeId, channel_id: ChannelId, bytes: Vec<u8>) {
        self.metrics.dropped_packets = self.metrics.dropped_packets.saturating_add(1);
        self.handle_event(
            node_id,
            CoreEvent::TransportPacketSendFailed { channel_id, bytes },
        );
    }

    fn next_packet_index(&self) -> Option<usize> {
        self.packets
            .iter()
            .enumerate()
            .min_by_key(|(_, packet)| packet.due_ms)
            .map(|(index, _)| index)
    }

    fn next_poll_deadline(&self) -> Option<(SimNodeId, u64)> {
        self.nodes
            .iter()
            .filter_map(|(node_id, node)| {
                let elapsed = self.elapsed(*node_id);
                let deadline = match node {
                    SimNode::Endpoint {
                        core,
                        started_at_sim_ms,
                        ..
                    } => {
                        let deadline = core.next_deadline(elapsed);
                        if deadline == u64::MAX {
                            None
                        } else {
                            Some(deadline.saturating_add(*started_at_sim_ms))
                        }
                    }
                    SimNode::Relay {
                        core,
                        started_at_sim_ms,
                        ..
                    } => {
                        let deadline = core.next_deadline(elapsed);
                        if deadline == u64::MAX {
                            None
                        } else {
                            Some(deadline.saturating_add(*started_at_sim_ms))
                        }
                    }
                }?;
                Some((*node_id, deadline))
            })
            .min_by_key(|(_, deadline)| *deadline)
    }

    fn poll_node(&mut self, node_id: SimNodeId) {
        let mut actions = Vec::new();
        let elapsed = self.elapsed(node_id);
        match self.nodes.get_mut(&node_id) {
            Some(SimNode::Endpoint { core, .. }) => core.poll(elapsed, &mut actions),
            Some(SimNode::Relay { core, .. }) => core.poll(elapsed, &mut actions),
            None => return,
        }
        self.handle_actions(node_id, actions);
        self.retry_blocked_writes();
    }
}

fn advance_seed(seed: &mut [u8; 16]) {
    for byte in seed {
        let (next, carry) = byte.overflowing_add(1);
        *byte = next;
        if !carry {
            return;
        }
    }
    panic!("test shell restart seed exhausted");
}

impl Default for TestShell {
    fn default() -> Self {
        Self::new()
    }
}

impl SimLink {
    fn is_down(&self, now_ms: u64) -> bool {
        self.down_windows
            .iter()
            .any(|window| now_ms >= window.start_ms && now_ms < window.end_ms)
    }
    fn should_drop(&self, sent_packets: u64) -> bool {
        match self.loss {
            LossModel::None => false,
            LossModel::Every { packets } => sent_packets.is_multiple_of(packets),
            LossModel::Bernoulli { loss_ppm, seed } => {
                assert!(loss_ppm <= 1_000_000);
                let random = splitmix64(seed.wrapping_add(sent_packets));
                let sample = ((u128::from(random) * 1_000_000) >> 64) as u32;
                sample < loss_ppm
            }
        }
    }
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

pub fn route_edge(channel_id: ChannelId, next: u32) -> RouteEdge {
    RouteEdge {
        channel_id,
        next,
        capacity_hint_kbps: 0,
        latency_hint_ms: 0,
    }
}

pub fn route_graph(nodes: Vec<Vec<RouteEdge>>) -> RouteGraph {
    RouteGraph {
        nodes: nodes.into_iter().map(|edges| RouteNode { edges }).collect(),
    }
}

pub fn kcp_config() -> KcpConfig {
    KcpConfig::default()
}

pub fn endpoint_config(
    envelope_key: [u8; 16],
    message_key: [u8; 16],
    random_seed: [u8; 16],
    boot_time_ms: u64,
    local_channels: Vec<ChannelId>,
    route_graph: RouteGraph,
    kcp: KcpConfig,
) -> EndpointConfig {
    EndpointConfig {
        envelope_key,
        message_key,
        random_seed,
        boot_time_ms,
        kcp,
        local_channels,
        route: RouteConfig {
            graph: route_graph,
            min_mtu: 1200,
            feedback_interval_ms: 1_000,
            feedback_timeout_ms: 5_000,
        },
        padding_reserve: 128,
        keepalive_interval_ms: 0,
    }
}

pub fn relay_config(
    envelope_key: [u8; 16],
    random_seed: [u8; 16],
    boot_time_ms: u64,
    local_channels: Vec<ChannelId>,
) -> RelayConfig {
    RelayConfig {
        envelope_key,
        random_seed,
        boot_time_ms,
        local_channels,
        local_min_mtu: 1200,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENVELOPE_KEY: [u8; 16] = [5; 16];
    const MESSAGE_KEY: [u8; 16] = [7; 16];

    #[test]
    fn echo_uses_the_selected_multihop_route() {
        let mut shell = TestShell::new();
        shell.add_endpoint(
            1,
            endpoint_config(
                ENVELOPE_KEY,
                MESSAGE_KEY,
                [1; 16],
                0,
                vec![11, 12],
                route_graph(vec![
                    vec![route_edge(11, 1), route_edge(12, 2)],
                    vec![route_edge(21, 3)],
                    vec![route_edge(31, 3)],
                    vec![],
                ]),
                kcp_config(),
            ),
            false,
        );
        shell.add_relay(2, relay_config(ENVELOPE_KEY, [2; 16], 0, vec![21, 51]));
        shell.add_relay(3, relay_config(ENVELOPE_KEY, [3; 16], 0, vec![31]));
        shell.add_endpoint(
            4,
            endpoint_config(
                ENVELOPE_KEY,
                MESSAGE_KEY,
                [4; 16],
                0,
                vec![41],
                route_graph(vec![
                    vec![route_edge(41, 1)],
                    vec![route_edge(51, 2)],
                    vec![],
                ]),
                kcp_config(),
            ),
            true,
        );
        let mut broken = link(1, 11, 2, 201);
        broken.down_windows.push(DownWindow {
            start_ms: 0,
            end_ms: u64::MAX,
        });
        shell.add_link(broken);
        shell.add_link(link(2, 21, 4, 401));
        shell.add_link(link(1, 12, 3, 301));
        shell.add_link(link(3, 31, 4, 401));
        shell.add_link(link(4, 41, 2, 202));
        shell.add_link(link(2, 51, 1, 101));
        shell.add_link(link(4, 41, 3, 302));
        shell.add_link(link(3, 61, 1, 101));
        let conv = shell.open_session(1);
        shell.run_until_idle(128);
        shell.send_session_bytes(1, conv, b"route".to_vec());
        shell.run_until_idle(256);
        assert_eq!(shell.session_bytes(1, conv), Some(&b"route"[..]));
        assert_eq!(shell.delivered_on(1, 11), 0);
        assert!(shell.delivered_on(1, 12) > 0);
    }

    #[test]
    fn loss_recovers_and_replay_is_not_forwarded_twice() {
        let mut shell = TestShell::new();
        shell.add_endpoint(
            1,
            endpoint_config(
                ENVELOPE_KEY,
                MESSAGE_KEY,
                [1; 16],
                0,
                vec![11],
                route_graph(vec![
                    vec![route_edge(11, 1)],
                    vec![route_edge(21, 2)],
                    vec![],
                ]),
                kcp_config(),
            ),
            false,
        );
        shell.add_relay(2, relay_config(ENVELOPE_KEY, [2; 16], 0, vec![21, 41]));
        shell.add_endpoint(
            3,
            endpoint_config(
                ENVELOPE_KEY,
                MESSAGE_KEY,
                [3; 16],
                0,
                vec![31],
                route_graph(vec![
                    vec![route_edge(31, 1)],
                    vec![route_edge(41, 2)],
                    vec![],
                ]),
                kcp_config(),
            ),
            true,
        );
        shell.add_link(link(1, 11, 2, 201));
        shell.add_link(link(2, 21, 3, 301));
        shell.add_link(link(3, 31, 2, 202));
        shell.add_link(link(2, 41, 1, 101));
        let conv = shell.open_session(1);
        shell.run_until_idle(128);
        let forwarded = shell.delivered_on(2, 21);
        assert!(shell.replay_last_packet(1, 11));
        shell.run_until_idle(32);
        assert_eq!(shell.delivered_on(2, 21), forwarded);
        shell.send_session_bytes(1, conv, b"hello".to_vec());
        shell.run_until_idle(256);
        assert_eq!(shell.session_bytes(1, conv), Some(&b"hello"[..]));
    }

    #[test]
    fn scaled_kcp_recovers_after_a_slow_link_returns() {
        let mut shell = TestShell::new();
        let fast = kcp_config();
        let mut slow = kcp_config();
        slow.send_window = 96;
        slow.receive_window = 256;
        slow.time_scale = 5;
        shell.add_endpoint(
            1,
            endpoint_config(
                ENVELOPE_KEY,
                MESSAGE_KEY,
                [8; 16],
                0,
                vec![11],
                route_graph(vec![vec![route_edge(11, 1)], vec![]]),
                fast,
            ),
            false,
        );
        shell.add_endpoint(
            2,
            endpoint_config(
                ENVELOPE_KEY,
                MESSAGE_KEY,
                [9; 16],
                0,
                vec![21],
                route_graph(vec![vec![route_edge(21, 1)], vec![]]),
                slow,
            ),
            true,
        );
        let mut forward = link(1, 11, 2, 21);
        forward.latency_ms = 30;
        forward.bandwidth_bytes_per_ms = 10;
        forward.down_windows.push(DownWindow {
            start_ms: 0,
            end_ms: 55,
        });
        shell.add_link(forward);
        shell.add_link(link(2, 21, 1, 11));
        let conv = shell.open_session(1);
        shell.run_until_idle(1024);
        shell.send_session_bytes(1, conv, b"retry".to_vec());
        shell.run_until_idle(1024);
        assert_eq!(shell.session_bytes(1, conv), Some(&b"retry"[..]));
        assert!(shell.metrics().dropped_packets > 0);
    }

    #[test]
    fn restart_uses_fresh_seed_and_continuous_global_time() {
        let mut shell = TestShell::new();
        shell.add_relay(1, relay_config(ENVELOPE_KEY, [1; 16], 100, vec![11]));
        shell.now_ms = 5_000;
        shell.restart_node(1);

        let SimNode::Relay {
            config,
            started_at_sim_ms,
            ..
        } = shell.nodes.get(&1).unwrap()
        else {
            panic!("node remains a relay");
        };
        assert_eq!(config.boot_time_ms, 5_100);
        assert_ne!(config.random_seed, [1; 16]);
        assert_eq!(*started_at_sim_ms, 5_000);
        assert_eq!(shell.elapsed(1), 0);
    }

    #[test]
    fn close_retransmits_after_first_close_is_dropped() {
        let mut shell = TestShell::new();
        let mut kcp = kcp_config();
        kcp.time_scale = 1;
        shell.add_endpoint(
            1,
            endpoint_config(
                ENVELOPE_KEY,
                MESSAGE_KEY,
                [11; 16],
                0,
                vec![11],
                route_graph(vec![vec![route_edge(11, 1)], vec![]]),
                kcp,
            ),
            false,
        );
        shell.add_endpoint(
            2,
            endpoint_config(
                ENVELOPE_KEY,
                MESSAGE_KEY,
                [22; 16],
                0,
                vec![21],
                route_graph(vec![vec![route_edge(21, 1)], vec![]]),
                kcp_config(),
            ),
            true,
        );
        shell.add_link(link(1, 11, 2, 21));
        shell.add_link(link(2, 21, 1, 11));

        let conv = shell.open_session(1);
        shell.run_until_idle(256);
        assert!(shell.has_session(2, conv), "remote should open");
        assert_eq!(shell.active_sessions(1), 1);
        assert_eq!(shell.active_sessions(2), 1);

        shell.close_session(1, conv, CloseReason::LocalClosed);
        let dropped = shell.drop_queued_from(1, 11);
        assert!(dropped > 0, "close should produce at least one packet");
        assert_eq!(
            shell.active_sessions(1),
            1,
            "local must keep session while close is unacked"
        );

        shell.run_until_idle(10_000);

        assert_eq!(
            shell.active_sessions(1),
            0,
            "local should finish after retransmit/ack now={} dropped={}",
            shell.now_ms(),
            shell.metrics().dropped_packets
        );
        assert_eq!(shell.active_sessions(2), 0, "remote must not leak session");
        assert!(!shell.has_session(1, conv));
        assert!(!shell.has_session(2, conv));
    }

    #[test]
    fn same_seed_is_deterministic() {
        fn run() -> (ConvId, TestMetrics, Option<Vec<u8>>) {
            let mut shell = TestShell::new();
            shell.add_endpoint(
                1,
                endpoint_config(
                    ENVELOPE_KEY,
                    MESSAGE_KEY,
                    [42; 16],
                    0,
                    vec![11],
                    route_graph(vec![vec![route_edge(11, 1)], vec![]]),
                    kcp_config(),
                ),
                false,
            );
            shell.add_endpoint(
                2,
                endpoint_config(
                    ENVELOPE_KEY,
                    MESSAGE_KEY,
                    [43; 16],
                    0,
                    vec![21],
                    route_graph(vec![vec![route_edge(21, 1)], vec![]]),
                    kcp_config(),
                ),
                true,
            );
            shell.add_link(link(1, 11, 2, 21));
            shell.add_link(link(2, 21, 1, 11));
            let conv = shell.open_session(1);
            shell.run_until_idle(256);
            shell.send_session_bytes(1, conv, b"det".to_vec());
            shell.run_until_idle(256);
            (
                conv,
                shell.metrics(),
                shell.session_bytes(1, conv).map(|bytes| bytes.to_vec()),
            )
        }
        assert_eq!(run(), run());
    }

    #[test]
    fn seeded_bernoulli_loss_is_deterministic() {
        let mut first = link(1, 11, 2, 21);
        first.loss = LossModel::Bernoulli {
            loss_ppm: 100_000,
            seed: 42,
        };
        let second = first.clone();
        let first_outcomes = (1..=10_000)
            .map(|attempt| first.should_drop(attempt))
            .collect::<Vec<_>>();
        let second_outcomes = (1..=10_000)
            .map(|attempt| second.should_drop(attempt))
            .collect::<Vec<_>>();
        assert_eq!(first_outcomes, second_outcomes);
        let dropped = first_outcomes
            .into_iter()
            .filter(|dropped| *dropped)
            .count();
        assert!((900..=1_100).contains(&dropped));
    }

    #[test]
    fn malformed_packet_does_not_create_session_state() {
        let mut shell = TestShell::new();
        shell.add_endpoint(
            1,
            endpoint_config(
                ENVELOPE_KEY,
                MESSAGE_KEY,
                [1; 16],
                0,
                vec![11],
                route_graph(vec![vec![route_edge(11, 1)], vec![]]),
                kcp_config(),
            ),
            false,
        );
        shell.handle_event(
            1,
            CoreEvent::TransportPacketReceived {
                channel_id: 11,
                bytes: vec![0u8; 64],
            },
        );
        assert_eq!(shell.active_sessions(1), 0);
    }

    fn link(
        from_node: SimNodeId,
        from_channel_id: ChannelId,
        to_node: SimNodeId,
        to_channel_id: ChannelId,
    ) -> SimLink {
        SimLink {
            from_node,
            from_channel_id,
            to_node,
            to_channel_id,
            latency_ms: 5,
            bandwidth_bytes_per_ms: 100,
            loss: LossModel::None,
            extra_delay_ms: 0,
            down_windows: Vec::new(),
        }
    }
}
