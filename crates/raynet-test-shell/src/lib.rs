use std::collections::BTreeMap;

use raynet_core::{
    ChannelId, ChannelState, ConvId, CoreAction, CoreEvent, EndpointConfig, EndpointCore, Metadata,
    NodeId, RelayConfig, RelayCore, RouteChannel, RouteNode, RouteTopology, Target,
    TransportMetrics,
};

#[derive(Debug, Clone)]
pub struct SimLink {
    pub from_node: NodeId,
    pub from_channel_id: ChannelId,
    pub to_node: NodeId,
    pub to_channel_id: ChannelId,
    pub latency_ms: u64,
    pub bandwidth_bytes_per_ms: u64,
    pub loss_every: Option<u64>,
    pub down_windows: Vec<DownWindow>,
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
    nodes: BTreeMap<NodeId, SimNode>,
    links: Vec<SimLink>,
    packets: Vec<ScheduledPacket>,
    metrics: TestMetrics,
    delivered_by_outbound_channel: BTreeMap<(NodeId, ChannelId), u64>,
}

enum SimNode {
    Endpoint {
        core: EndpointCore,
        sessions: BTreeMap<ConvId, Vec<u8>>,
        echo_exit: bool,
    },
    Relay {
        core: RelayCore,
    },
}

struct ScheduledPacket {
    due_ms: u64,
    from_node: NodeId,
    from_channel_id: ChannelId,
    to_node: NodeId,
    to_channel_id: ChannelId,
    bytes: Vec<u8>,
}

impl TestShell {
    pub fn new() -> Self {
        Self {
            now_ms: 0,
            nodes: BTreeMap::new(),
            links: Vec::new(),
            packets: Vec::new(),
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

    pub fn add_endpoint(&mut self, config: EndpointConfig, echo_exit: bool) {
        self.nodes.insert(
            config.local_node_id,
            SimNode::Endpoint {
                core: EndpointCore::new(config).expect("valid endpoint config"),
                sessions: BTreeMap::new(),
                echo_exit,
            },
        );
    }

    pub fn add_relay(&mut self, config: RelayConfig) {
        self.nodes.insert(
            config.local_node_id,
            SimNode::Relay {
                core: RelayCore::new(config).expect("valid relay config"),
            },
        );
    }

    pub fn add_link(&mut self, link: SimLink) {
        self.links.push(link);
    }

    pub fn set_channel_state(
        &mut self,
        node_id: NodeId,
        channel_id: ChannelId,
        state: ChannelState,
    ) {
        let metrics = TransportMetrics {
            queue_pressure: 0.0,
            send_error: state != ChannelState::Up,
        };
        self.handle_event(
            node_id,
            CoreEvent::TransportChannelUpdated {
                channel_id,
                state,
                metrics,
            },
        );
    }

    pub fn open_ingress(&mut self, node_id: NodeId, target: Target) -> ConvId {
        let mut actions = Vec::new();
        let result = match self.nodes.get_mut(&node_id) {
            Some(SimNode::Endpoint { core, .. }) => core.handle_event(
                self.now_ms,
                CoreEvent::IngressSessionRequested {
                    target,
                    metadata: Metadata::new(),
                },
                &mut actions,
            ),
            _ => panic!("node {node_id} is not an endpoint"),
        };
        result.expect("ingress session opens");
        let conv_id = actions
            .iter()
            .find_map(|action| match action {
                CoreAction::IngressSessionCreated { conv_id } => Some(*conv_id),
                _ => None,
            })
            .expect("core creates ingress session");
        if let Some(SimNode::Endpoint { sessions, .. }) = self.nodes.get_mut(&node_id) {
            sessions.insert(conv_id, Vec::new());
        }
        self.handle_actions(node_id, actions);
        conv_id
    }

    pub fn send_session_bytes(
        &mut self,
        node_id: NodeId,
        conv_id: ConvId,
        bytes: impl Into<Vec<u8>>,
    ) {
        self.handle_event(
            node_id,
            CoreEvent::SessionBytes {
                conv_id,
                bytes: bytes.into(),
            },
        );
    }

    pub fn session_bytes(&self, node_id: NodeId, conv_id: ConvId) -> Option<&[u8]> {
        let Some(SimNode::Endpoint { sessions, .. }) = self.nodes.get(&node_id) else {
            return None;
        };
        sessions.get(&conv_id).map(Vec::as_slice)
    }

    pub fn delivered_on(&self, node_id: NodeId, channel_id: ChannelId) -> u64 {
        self.delivered_by_outbound_channel
            .get(&(node_id, channel_id))
            .copied()
            .unwrap_or(0)
    }

    pub fn run_until_idle(&mut self, max_steps: usize) {
        for _ in 0..max_steps {
            let next_packet = self
                .next_packet_index()
                .map(|index| (index, self.packets[index].due_ms));
            let next_poll = self.next_poll_deadline();

            match (next_packet, next_poll) {
                (None, None) => return,
                (Some((_index, packet_due)), Some((node_id, poll_due)))
                    if poll_due < packet_due =>
                {
                    self.now_ms = self.now_ms.max(poll_due);
                    self.poll_node(node_id);
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
                (None, Some((node_id, poll_due))) => {
                    self.now_ms = self.now_ms.max(poll_due);
                    self.poll_node(node_id);
                }
            }
        }
    }

    fn handle_event(&mut self, node_id: NodeId, event: CoreEvent) {
        let mut actions = Vec::new();
        let result = match self.nodes.get_mut(&node_id) {
            Some(SimNode::Endpoint { core, .. }) => {
                core.handle_event(self.now_ms, event, &mut actions)
            }
            Some(SimNode::Relay { core }) => core.handle_event(self.now_ms, event, &mut actions),
            None => return,
        };
        if result.is_ok() {
            self.handle_actions(node_id, actions);
        }
    }

    fn handle_actions(&mut self, node_id: NodeId, actions: Vec<CoreAction>) {
        for action in actions {
            match action {
                CoreAction::SendTransportPacket { channel_id, bytes } => {
                    self.send_transport(node_id, channel_id, bytes);
                }
                CoreAction::IngressSessionCreated { conv_id } => {
                    if let Some(SimNode::Endpoint { sessions, .. }) = self.nodes.get_mut(&node_id) {
                        sessions.entry(conv_id).or_default();
                    }
                }
                CoreAction::OpenExitConnection {
                    conv_id, target, ..
                } => {
                    if let Some(SimNode::Endpoint { sessions, .. }) = self.nodes.get_mut(&node_id) {
                        sessions.entry(conv_id).or_default();
                    }
                    let _ = target;
                    self.handle_event(node_id, CoreEvent::ExitConnectionOpened { conv_id });
                }
                CoreAction::WriteSession { conv_id, bytes } => {
                    self.write_session(node_id, conv_id, bytes);
                }
                CoreAction::CloseSession { conv_id, .. } => {
                    if let Some(SimNode::Endpoint { sessions, .. }) = self.nodes.get_mut(&node_id) {
                        sessions.remove(&conv_id);
                    }
                }
                CoreAction::EmitMetric(_) | CoreAction::EmitEvent(_) => {}
            }
        }
    }

    fn write_session(&mut self, node_id: NodeId, conv_id: ConvId, bytes: Vec<u8>) {
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
            self.handle_event(node_id, CoreEvent::SessionBytes { conv_id, bytes });
        } else {
            self.metrics.delivered_local_bytes = self
                .metrics
                .delivered_local_bytes
                .saturating_add(bytes.len() as u64);
            self.metrics.last_delivery_ms = self.now_ms;
        }
    }

    fn send_transport(&mut self, node_id: NodeId, channel_id: ChannelId, bytes: Vec<u8>) {
        self.metrics.sent_packets = self.metrics.sent_packets.saturating_add(1);
        let Some(link) = self
            .links
            .iter()
            .find(|link| link.from_node == node_id && link.from_channel_id == channel_id)
            .cloned()
        else {
            self.metrics.dropped_packets = self.metrics.dropped_packets.saturating_add(1);
            self.channel_failed(node_id, channel_id);
            return;
        };

        if link.is_down(self.now_ms) || link.should_drop(self.metrics.sent_packets) {
            self.metrics.dropped_packets = self.metrics.dropped_packets.saturating_add(1);
            self.channel_failed(node_id, channel_id);
            return;
        }

        let bandwidth = link.bandwidth_bytes_per_ms.max(1);
        let transmit_ms = (bytes.len() as u64).div_ceil(bandwidth);
        self.packets.push(ScheduledPacket {
            due_ms: self.now_ms + link.latency_ms + transmit_ms,
            from_node: node_id,
            from_channel_id: channel_id,
            to_node: link.to_node,
            to_channel_id: link.to_channel_id,
            bytes,
        });
    }

    fn channel_failed(&mut self, node_id: NodeId, channel_id: ChannelId) {
        self.handle_event(
            node_id,
            CoreEvent::TransportChannelUpdated {
                channel_id,
                state: ChannelState::Degraded,
                metrics: TransportMetrics {
                    queue_pressure: 1.0,
                    send_error: true,
                },
            },
        );
    }

    fn next_packet_index(&self) -> Option<usize> {
        self.packets
            .iter()
            .enumerate()
            .min_by_key(|(_, packet)| packet.due_ms)
            .map(|(index, _)| index)
    }

    fn next_poll_deadline(&self) -> Option<(NodeId, u64)> {
        self.nodes
            .iter()
            .filter_map(|(node_id, node)| {
                let deadline = match node {
                    SimNode::Endpoint { core, .. } => core.next_deadline(self.now_ms),
                    SimNode::Relay { core } => core.next_deadline(self.now_ms),
                }?;
                Some((*node_id, deadline))
            })
            .min_by_key(|(_, deadline)| *deadline)
    }

    fn poll_node(&mut self, node_id: NodeId) {
        let mut actions = Vec::new();
        let result = match self.nodes.get_mut(&node_id) {
            Some(SimNode::Endpoint { core, .. }) => core.poll(self.now_ms, &mut actions),
            Some(SimNode::Relay { core }) => core.poll(self.now_ms, &mut actions),
            None => return,
        };
        if result.is_ok() {
            self.handle_actions(node_id, actions);
        }
    }
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
        self.loss_every
            .is_some_and(|loss_every| loss_every > 0 && sent_packets % loss_every == 0)
    }
}

pub fn route_channel(channel_id: ChannelId, peer_node_id: NodeId) -> RouteChannel {
    RouteChannel {
        channel_id,
        peer_node_id,
    }
}

pub fn topology(nodes: Vec<(NodeId, Vec<RouteChannel>)>) -> RouteTopology {
    RouteTopology {
        nodes: nodes
            .into_iter()
            .map(|(node_id, channels)| RouteNode { node_id, channels })
            .collect(),
    }
}

pub fn endpoint_config(
    local_node_id: NodeId,
    envelope_key: [u8; 16],
    message_key: [u8; 16],
    random_seed: [u8; 16],
    local_channels: Vec<ChannelId>,
    route_topology: RouteTopology,
) -> EndpointConfig {
    EndpointConfig {
        local_node_id,
        envelope_key,
        message_key,
        random_seed,
        local_channels,
        route_topology,
        transport_mtu: 1200,
    }
}

pub fn relay_config(
    local_node_id: NodeId,
    envelope_key: [u8; 16],
    random_seed: [u8; 16],
    local_channels: Vec<ChannelId>,
) -> RelayConfig {
    RelayConfig {
        local_node_id,
        envelope_key,
        random_seed,
        local_channels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENVELOPE_KEY: [u8; 16] = [5; 16];
    const MESSAGE_KEY: [u8; 16] = [7; 16];

    #[test]
    fn test_shell_delivers_echo_through_relay_with_virtual_time() {
        let mut shell = TestShell::new();
        shell.add_endpoint(
            endpoint_config(
                1,
                ENVELOPE_KEY,
                MESSAGE_KEY,
                [1; 16],
                vec![11],
                topology(vec![
                    (1, vec![route_channel(11, 2)]),
                    (2, vec![route_channel(21, 3)]),
                    (3, Vec::new()),
                ]),
            ),
            false,
        );
        shell.add_relay(relay_config(2, ENVELOPE_KEY, [2; 16], vec![21, 22]));
        shell.add_endpoint(
            endpoint_config(
                3,
                ENVELOPE_KEY,
                MESSAGE_KEY,
                [3; 16],
                vec![31],
                topology(vec![
                    (3, vec![route_channel(31, 2)]),
                    (2, vec![route_channel(22, 1)]),
                    (1, Vec::new()),
                ]),
            ),
            true,
        );
        shell.add_link(link(1, 11, 2, 201, 10, 100));
        shell.add_link(link(2, 21, 3, 301, 20, 100));
        shell.add_link(link(3, 31, 2, 202, 30, 100));
        shell.add_link(link(2, 22, 1, 101, 40, 100));

        let conv_id = shell.open_ingress(
            1,
            Target {
                host: "echo.invalid".to_string(),
                port: 7,
            },
        );
        shell.run_until_idle(16);
        shell.send_session_bytes(1, conv_id, b"hello".to_vec());
        shell.run_until_idle(128);

        assert_eq!(shell.session_bytes(1, conv_id), Some(&b"hello"[..]));
        assert_eq!(shell.metrics().delivered_local_bytes, 5);
        assert!(shell.metrics().last_delivery_ms >= 100);
    }

    #[test]
    fn test_shell_tracks_loss_and_temporary_disconnect() {
        let mut shell = TestShell::new();
        shell.add_relay(relay_config(2, ENVELOPE_KEY, [2; 16], vec![21, 22]));
        shell.add_link(SimLink {
            from_node: 2,
            from_channel_id: 21,
            to_node: 9,
            to_channel_id: 91,
            latency_ms: 5,
            bandwidth_bytes_per_ms: 10,
            loss_every: Some(1),
            down_windows: Vec::new(),
        });
        shell.add_link(SimLink {
            from_node: 2,
            from_channel_id: 22,
            to_node: 9,
            to_channel_id: 92,
            latency_ms: 50,
            bandwidth_bytes_per_ms: 10,
            loss_every: None,
            down_windows: vec![DownWindow {
                start_ms: 0,
                end_ms: 20,
            }],
        });

        shell.set_channel_state(2, 21, ChannelState::Down);
        shell.set_channel_state(2, 22, ChannelState::Up);
        shell.send_transport(2, 22, vec![1, 2, 3]);
        shell.run_until_idle(4);

        assert_eq!(shell.metrics().sent_packets, 1);
        assert_eq!(shell.metrics().dropped_packets, 1);
    }

    #[test]
    fn test_shell_uses_full_route_plan_for_relay_hops() {
        let mut shell = TestShell::new();
        shell.add_endpoint(
            endpoint_config(
                1,
                ENVELOPE_KEY,
                MESSAGE_KEY,
                [4; 16],
                vec![11],
                topology(vec![
                    (1, vec![route_channel(11, 2)]),
                    (2, vec![route_channel(22, 3)]),
                    (3, Vec::new()),
                ]),
            ),
            false,
        );
        shell.add_relay(relay_config(2, ENVELOPE_KEY, [2; 16], vec![21, 22]));
        shell.add_endpoint(
            endpoint_config(
                3,
                ENVELOPE_KEY,
                MESSAGE_KEY,
                [5; 16],
                vec![31],
                topology(vec![(3, vec![route_channel(31, 1)]), (1, Vec::new())]),
            ),
            true,
        );
        shell.add_link(link(1, 11, 2, 201, 5, 100));
        shell.add_link(link(2, 22, 3, 302, 35, 100));
        shell.add_link(link(3, 31, 1, 101, 5, 100));

        let conv_id = shell.open_ingress(
            1,
            Target {
                host: "echo.invalid".to_string(),
                port: 7,
            },
        );
        shell.run_until_idle(64);
        shell.send_session_bytes(1, conv_id, b"route".to_vec());
        shell.run_until_idle(128);

        assert_eq!(shell.session_bytes(1, conv_id), Some(&b"route"[..]));
        assert_eq!(shell.delivered_on(2, 21), 0);
        assert!(shell.delivered_on(2, 22) > 0);
    }

    #[test]
    fn test_shell_retransmits_with_virtual_time_after_disconnect() {
        let mut shell = TestShell::new();
        shell.add_endpoint(
            endpoint_config(
                1,
                ENVELOPE_KEY,
                [9; 16],
                [6; 16],
                vec![11],
                topology(vec![(1, vec![route_channel(11, 2)]), (2, Vec::new())]),
            ),
            false,
        );
        shell.add_endpoint(
            endpoint_config(
                2,
                ENVELOPE_KEY,
                [9; 16],
                [7; 16],
                vec![21],
                topology(vec![(2, vec![route_channel(21, 1)]), (1, Vec::new())]),
            ),
            true,
        );
        shell.add_link(SimLink {
            from_node: 1,
            from_channel_id: 11,
            to_node: 2,
            to_channel_id: 21,
            latency_ms: 5,
            bandwidth_bytes_per_ms: 100,
            loss_every: None,
            down_windows: vec![DownWindow {
                start_ms: 0,
                end_ms: 55,
            }],
        });
        shell.add_link(link(2, 21, 1, 11, 5, 100));

        let conv_id = shell.open_ingress(
            1,
            Target {
                host: "echo.invalid".to_string(),
                port: 7,
            },
        );
        shell.run_until_idle(128);
        shell.send_session_bytes(1, conv_id, b"retry".to_vec());
        shell.run_until_idle(128);

        assert_eq!(shell.session_bytes(1, conv_id), Some(&b"retry"[..]));
        assert!(shell.metrics().dropped_packets > 0);
        assert!(shell.now_ms() >= 55);
    }

    fn link(
        from_node: NodeId,
        from_channel_id: ChannelId,
        to_node: NodeId,
        to_channel_id: ChannelId,
        latency_ms: u64,
        bandwidth_bytes_per_ms: u64,
    ) -> SimLink {
        SimLink {
            from_node,
            from_channel_id,
            to_node,
            to_channel_id,
            latency_ms,
            bandwidth_bytes_per_ms,
            loss_every: None,
            down_windows: Vec::new(),
        }
    }
}
