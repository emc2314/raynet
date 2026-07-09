use std::collections::BTreeMap;

use raynet_core::{
    ChannelConfig, ChannelId, ChannelState, CoreAction, CoreEvent, EndpointConfig, EndpointCore,
    LocalConnectionId, Metadata, NodeId, RelayConfig, RelayCore, Target, TransportMetrics,
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
    next_local_connection_id: LocalConnectionId,
    metrics: TestMetrics,
    delivered_by_outbound_channel: BTreeMap<(NodeId, ChannelId), u64>,
}

enum SimNode {
    Endpoint {
        core: EndpointCore,
        local_connections: BTreeMap<LocalConnectionId, Vec<u8>>,
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
            next_local_connection_id: 1,
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
            config.node_id,
            SimNode::Endpoint {
                core: EndpointCore::new(config).expect("valid endpoint config"),
                local_connections: BTreeMap::new(),
                echo_exit,
            },
        );
    }

    pub fn add_relay(&mut self, config: RelayConfig) {
        self.nodes.insert(
            config.node_id,
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
        let event = CoreEvent::TransportChannelUpdated {
            channel_id,
            state,
            metrics,
        };
        self.handle_event(node_id, event);
    }

    pub fn open_ingress(&mut self, node_id: NodeId, target: Target) -> LocalConnectionId {
        let local_connection_id = self.alloc_local_connection_id();
        if let Some(SimNode::Endpoint {
            local_connections, ..
        }) = self.nodes.get_mut(&node_id)
        {
            local_connections.insert(local_connection_id, Vec::new());
        }
        self.handle_event(
            node_id,
            CoreEvent::IngressConnectionOpened {
                local_connection_id,
                target,
                metadata: Metadata::new(),
            },
        );
        local_connection_id
    }

    pub fn send_local_bytes(
        &mut self,
        node_id: NodeId,
        local_connection_id: LocalConnectionId,
        bytes: impl Into<Vec<u8>>,
    ) {
        self.handle_event(
            node_id,
            CoreEvent::LocalConnectionBytes {
                local_connection_id,
                bytes: bytes.into(),
            },
        );
    }

    pub fn local_bytes(
        &self,
        node_id: NodeId,
        local_connection_id: LocalConnectionId,
    ) -> Option<&[u8]> {
        let Some(SimNode::Endpoint {
            local_connections, ..
        }) = self.nodes.get(&node_id)
        else {
            return None;
        };
        local_connections
            .get(&local_connection_id)
            .map(Vec::as_slice)
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
                CoreAction::OpenExitConnection {
                    stream_id, target, ..
                } => {
                    let local_connection_id = self.alloc_local_connection_id();
                    if let Some(SimNode::Endpoint {
                        local_connections, ..
                    }) = self.nodes.get_mut(&node_id)
                    {
                        local_connections.insert(local_connection_id, Vec::new());
                    }
                    let _ = target;
                    self.handle_event(
                        node_id,
                        CoreEvent::ExitConnectionOpened {
                            stream_id,
                            local_connection_id,
                        },
                    );
                }
                CoreAction::WriteLocalConnection {
                    local_connection_id,
                    bytes,
                } => self.write_local_connection(node_id, local_connection_id, bytes),
                CoreAction::CloseLocalConnection {
                    local_connection_id,
                    reason,
                } => {
                    if let Some(SimNode::Endpoint {
                        local_connections, ..
                    }) = self.nodes.get_mut(&node_id)
                    {
                        let _ = reason;
                        local_connections.remove(&local_connection_id);
                    }
                }
                CoreAction::CloseRemoteStream { .. }
                | CoreAction::EmitMetric(_)
                | CoreAction::EmitLog { .. } => {}
            }
        }
    }

    fn write_local_connection(
        &mut self,
        node_id: NodeId,
        local_connection_id: LocalConnectionId,
        bytes: Vec<u8>,
    ) {
        let echo_exit = match self.nodes.get_mut(&node_id) {
            Some(SimNode::Endpoint {
                local_connections,
                echo_exit,
                ..
            }) => {
                local_connections
                    .entry(local_connection_id)
                    .or_default()
                    .extend_from_slice(&bytes);
                *echo_exit
            }
            _ => false,
        };

        if echo_exit {
            self.handle_event(
                node_id,
                CoreEvent::LocalConnectionBytes {
                    local_connection_id,
                    bytes,
                },
            );
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

    fn alloc_local_connection_id(&mut self) -> LocalConnectionId {
        let id = self.next_local_connection_id;
        self.next_local_connection_id = self.next_local_connection_id.saturating_add(1).max(1);
        id
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

pub fn channel(channel_id: ChannelId, peer_node_id: NodeId) -> ChannelConfig {
    ChannelConfig {
        channel_id,
        peer_node_id,
        mtu: 1200,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use raynet_core::{DestinationRoute, EndpointConfig, RelayConfig};

    #[test]
    fn test_shell_delivers_echo_through_relay_with_virtual_time() {
        let mut shell = TestShell::new();
        shell.add_endpoint(
            EndpointConfig {
                node_id: 1,
                endpoint_id: 1,
                hop_key: [5; 16],
                endpoint_key: [7; 16],
                default_destination_node_id: 3,
                default_channel_plan: vec![11, 21],
                destination_routes: Vec::new(),
                channels: vec![channel(11, 2)],
            },
            false,
        );
        shell.add_relay(RelayConfig {
            node_id: 2,
            hop_key: [5; 16],
            destination_routes: Vec::new(),
            channels: vec![channel(21, 3), channel(22, 1)],
        });
        shell.add_endpoint(
            EndpointConfig {
                node_id: 3,
                endpoint_id: 1,
                hop_key: [5; 16],
                endpoint_key: [7; 16],
                default_destination_node_id: 1,
                default_channel_plan: vec![31, 22],
                destination_routes: Vec::new(),
                channels: vec![channel(31, 2)],
            },
            true,
        );
        shell.add_link(link(1, 11, 2, 201, 10, 100));
        shell.add_link(link(2, 21, 3, 301, 20, 100));
        shell.add_link(link(3, 31, 2, 202, 30, 100));
        shell.add_link(link(2, 22, 1, 101, 40, 100));

        let local_id = shell.open_ingress(
            1,
            Target {
                host: "echo.invalid".to_string(),
                port: 7,
            },
        );
        shell.run_until_idle(16);
        shell.send_local_bytes(1, local_id, b"hello".to_vec());
        shell.run_until_idle(64);

        assert_eq!(shell.local_bytes(1, local_id), Some(&b"hello"[..]));
        assert_eq!(shell.metrics().delivered_local_bytes, 5);
        assert!(shell.metrics().last_delivery_ms >= 100);
    }

    #[test]
    fn test_shell_tracks_loss_and_temporary_disconnect() {
        let mut shell = TestShell::new();
        shell.add_relay(RelayConfig {
            node_id: 2,
            hop_key: [5; 16],
            destination_routes: vec![DestinationRoute {
                destination_node_id: 9,
                channel_ids: vec![21, 22],
            }],
            channels: vec![channel(21, 9), channel(22, 9)],
        });
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
    fn test_shell_routes_around_down_relay_channel_end_to_end() {
        let mut shell = TestShell::new();
        shell.add_endpoint(
            EndpointConfig {
                node_id: 1,
                endpoint_id: 11,
                hop_key: [5; 16],
                endpoint_key: [11; 16],
                default_destination_node_id: 3,
                default_channel_plan: vec![11],
                destination_routes: Vec::new(),
                channels: vec![channel(11, 2)],
            },
            false,
        );
        shell.add_relay(RelayConfig {
            node_id: 2,
            hop_key: [5; 16],
            destination_routes: vec![DestinationRoute {
                destination_node_id: 3,
                channel_ids: vec![21, 22],
            }],
            channels: vec![channel(21, 3), channel(22, 3)],
        });
        shell.add_endpoint(
            EndpointConfig {
                node_id: 3,
                endpoint_id: 11,
                hop_key: [5; 16],
                endpoint_key: [11; 16],
                default_destination_node_id: 1,
                default_channel_plan: vec![31],
                destination_routes: Vec::new(),
                channels: vec![channel(31, 1)],
            },
            true,
        );
        shell.add_link(link(1, 11, 2, 201, 5, 100));
        shell.add_link(link(2, 21, 3, 301, 5, 100));
        shell.add_link(link(2, 22, 3, 302, 35, 100));
        shell.add_link(link(3, 31, 1, 101, 5, 100));
        shell.set_channel_state(2, 21, ChannelState::Down);

        let local_id = shell.open_ingress(
            1,
            Target {
                host: "echo.invalid".to_string(),
                port: 7,
            },
        );
        shell.run_until_idle(64);
        shell.send_local_bytes(1, local_id, b"route".to_vec());
        shell.run_until_idle(128);

        assert_eq!(shell.local_bytes(1, local_id), Some(&b"route"[..]));
        assert_eq!(shell.delivered_on(2, 21), 0);
        assert!(shell.delivered_on(2, 22) > 0);
    }

    #[test]
    fn test_shell_retransmits_with_virtual_time_after_disconnect() {
        let mut shell = TestShell::new();
        shell.add_endpoint(
            EndpointConfig {
                node_id: 1,
                endpoint_id: 7,
                hop_key: [5; 16],
                endpoint_key: [9; 16],
                default_destination_node_id: 2,
                default_channel_plan: vec![11],
                destination_routes: Vec::new(),
                channels: vec![channel(11, 2)],
            },
            false,
        );
        shell.add_endpoint(
            EndpointConfig {
                node_id: 2,
                endpoint_id: 7,
                hop_key: [5; 16],
                endpoint_key: [9; 16],
                default_destination_node_id: 1,
                default_channel_plan: vec![21],
                destination_routes: Vec::new(),
                channels: vec![channel(21, 1)],
            },
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

        let local_id = shell.open_ingress(
            1,
            Target {
                host: "echo.invalid".to_string(),
                port: 7,
            },
        );
        shell.run_until_idle(128);
        shell.send_local_bytes(1, local_id, b"retry".to_vec());
        shell.run_until_idle(128);

        assert_eq!(shell.local_bytes(1, local_id), Some(&b"retry"[..]));
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
