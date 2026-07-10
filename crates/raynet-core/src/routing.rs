use std::collections::{BTreeMap, BTreeSet};

use crate::{ChannelId, ChannelState, NodeId, TransportMetrics};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTopology {
    pub nodes: Vec<RouteNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteNode {
    pub node_id: NodeId,
    pub channels: Vec<RouteChannel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteChannel {
    pub channel_id: ChannelId,
    pub peer_node_id: NodeId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteEdgeState {
    pub node_id: NodeId,
    pub channel_id: ChannelId,
    pub peer_node_id: NodeId,
    pub channel_state: ChannelState,
    pub loss_ewma: f32,
    pub probe_budget: u32,
    pub last_update_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalChannelState {
    pub channel_id: ChannelId,
    pub state: ChannelState,
    pub sent_packets: u64,
    pub received_packets: u64,
    pub send_errors: u64,
    pub queue_pressure: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalChannelTable {
    channels: BTreeMap<ChannelId, LocalChannelState>,
}

impl LocalChannelTable {
    pub fn new(channel_ids: impl IntoIterator<Item = ChannelId>) -> Self {
        Self {
            channels: channel_ids
                .into_iter()
                .map(|channel_id| {
                    (
                        channel_id,
                        LocalChannelState {
                            channel_id,
                            state: ChannelState::Up,
                            sent_packets: 0,
                            received_packets: 0,
                            send_errors: 0,
                            queue_pressure: 0.0,
                        },
                    )
                })
                .collect(),
        }
    }

    pub fn contains(&self, channel_id: ChannelId) -> bool {
        self.channels.contains_key(&channel_id)
    }

    pub fn is_usable(&self, channel_id: ChannelId) -> bool {
        self.channels.get(&channel_id).is_some_and(|channel| {
            matches!(channel.state, ChannelState::Up | ChannelState::Degraded)
        })
    }

    pub fn record_send(&mut self, channel_id: ChannelId) {
        if let Some(channel) = self.channels.get_mut(&channel_id) {
            channel.sent_packets = channel.sent_packets.saturating_add(1);
        }
    }

    pub fn record_receive(&mut self, channel_id: ChannelId) {
        if let Some(channel) = self.channels.get_mut(&channel_id) {
            channel.received_packets = channel.received_packets.saturating_add(1);
        }
    }

    pub fn update_channel(
        &mut self,
        channel_id: ChannelId,
        state: ChannelState,
        metrics: TransportMetrics,
    ) {
        if let Some(channel) = self.channels.get_mut(&channel_id) {
            channel.state = state;
            channel.queue_pressure = metrics.queue_pressure;
            if metrics.send_error {
                channel.send_errors = channel.send_errors.saturating_add(1);
            }
        }
    }

    pub fn channel_ids(&self) -> impl Iterator<Item = ChannelId> + '_ {
        self.channels.keys().copied()
    }

    pub fn states(&self) -> impl Iterator<Item = &LocalChannelState> {
        self.channels.values()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoutePlanner {
    topology: RouteTopology,
    edge_states: BTreeMap<(NodeId, ChannelId), RouteEdgeState>,
}

impl RoutePlanner {
    pub fn new(topology: RouteTopology) -> Self {
        let edge_states = topology
            .nodes
            .iter()
            .flat_map(|node| {
                node.channels.iter().map(|channel| {
                    (
                        (node.node_id, channel.channel_id),
                        RouteEdgeState {
                            node_id: node.node_id,
                            channel_id: channel.channel_id,
                            peer_node_id: channel.peer_node_id,
                            channel_state: ChannelState::Up,
                            loss_ewma: 0.0,
                            probe_budget: 0,
                            last_update_ms: 0,
                        },
                    )
                })
            })
            .collect();

        Self {
            topology,
            edge_states,
        }
    }

    pub fn topology(&self) -> &RouteTopology {
        &self.topology
    }

    pub fn edge_states(&self) -> impl Iterator<Item = &RouteEdgeState> {
        self.edge_states.values()
    }

    pub fn update_edge(
        &mut self,
        node_id: NodeId,
        channel_id: ChannelId,
        state: ChannelState,
        metrics: TransportMetrics,
        now_ms: u64,
    ) {
        if let Some(edge) = self.edge_states.get_mut(&(node_id, channel_id)) {
            edge.channel_state = state;
            edge.last_update_ms = now_ms;
            if metrics.send_error || state == ChannelState::Down {
                edge.loss_ewma = edge.loss_ewma * 0.6 + 0.4;
                edge.probe_budget = edge.probe_budget.saturating_add(1).min(8);
            } else {
                edge.loss_ewma *= 0.9;
                edge.probe_budget = edge.probe_budget.saturating_sub(1);
            }
        }
    }

    pub fn record_success(&mut self, node_id: NodeId, channel_id: ChannelId, now_ms: u64) {
        if let Some(edge) = self.edge_states.get_mut(&(node_id, channel_id)) {
            edge.loss_ewma *= 0.95;
            edge.last_update_ms = now_ms;
        }
    }

    pub fn build_route_plan(
        &self,
        local_node_id: NodeId,
        local_channels: &LocalChannelTable,
    ) -> Option<Vec<ChannelId>> {
        let destination = self.destination_node_id(local_node_id)?;
        let mut best_cost: BTreeMap<NodeId, f32> = BTreeMap::new();
        let mut previous: BTreeMap<NodeId, (NodeId, ChannelId)> = BTreeMap::new();
        let mut unvisited: BTreeSet<NodeId> = self
            .topology
            .nodes
            .iter()
            .map(|node| node.node_id)
            .collect();

        best_cost.insert(local_node_id, 0.0);

        while !unvisited.is_empty() {
            let Some((&node_id, &cost)) = best_cost
                .iter()
                .filter(|(node_id, _)| unvisited.contains(node_id))
                .min_by(|(_, left), (_, right)| left.total_cmp(right))
            else {
                break;
            };
            unvisited.remove(&node_id);

            if node_id == destination {
                break;
            }

            let Some(node) = self.node(node_id) else {
                continue;
            };
            for channel in &node.channels {
                if node_id == local_node_id && !local_channels.is_usable(channel.channel_id) {
                    continue;
                }
                let next_cost = cost + self.edge_cost(node_id, channel.channel_id);
                if next_cost
                    < best_cost
                        .get(&channel.peer_node_id)
                        .copied()
                        .unwrap_or(f32::INFINITY)
                {
                    best_cost.insert(channel.peer_node_id, next_cost);
                    previous.insert(channel.peer_node_id, (node_id, channel.channel_id));
                }
            }
        }

        if !previous.contains_key(&destination) {
            return None;
        }

        let mut node_id = destination;
        let mut route_plan = Vec::new();
        while node_id != local_node_id {
            let (prev_node_id, channel_id) = previous.get(&node_id).copied()?;
            route_plan.push(channel_id);
            node_id = prev_node_id;
        }
        route_plan.reverse();
        Some(route_plan)
    }

    fn destination_node_id(&self, local_node_id: NodeId) -> Option<NodeId> {
        let source_nodes: BTreeSet<_> = self
            .topology
            .nodes
            .iter()
            .map(|node| node.node_id)
            .collect();
        let referenced_nodes: BTreeSet<_> = self
            .topology
            .nodes
            .iter()
            .flat_map(|node| node.channels.iter().map(|channel| channel.peer_node_id))
            .collect();

        let mut sinks: Vec<_> = referenced_nodes
            .into_iter()
            .filter(|node_id| *node_id != local_node_id)
            .filter(|node_id| {
                self.node(*node_id)
                    .is_none_or(|node| node.channels.is_empty())
                    || !source_nodes.contains(node_id)
            })
            .collect();
        sinks.sort_unstable();
        sinks.dedup();
        if sinks.len() == 1 { sinks.pop() } else { None }
    }

    fn edge_cost(&self, node_id: NodeId, channel_id: ChannelId) -> f32 {
        let Some(edge) = self.edge_states.get(&(node_id, channel_id)) else {
            return f32::INFINITY;
        };
        let down_penalty = match edge.channel_state {
            ChannelState::Up => 0.0,
            ChannelState::Degraded => 10.0,
            ChannelState::Down => 1_000_000.0,
        };
        1.0 + edge.loss_ewma * 100.0 + down_penalty
    }

    fn node(&self, node_id: NodeId) -> Option<&RouteNode> {
        self.topology
            .nodes
            .iter()
            .find(|node| node.node_id == node_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(channel_id: ChannelId, peer_node_id: NodeId) -> RouteChannel {
        RouteChannel {
            channel_id,
            peer_node_id,
        }
    }

    #[test]
    fn route_planner_builds_low_cost_route_plan_without_enumerating_routes() {
        let mut planner = RoutePlanner::new(RouteTopology {
            nodes: vec![
                RouteNode {
                    node_id: 1,
                    channels: vec![channel(11, 2), channel(12, 3)],
                },
                RouteNode {
                    node_id: 2,
                    channels: vec![channel(21, 4)],
                },
                RouteNode {
                    node_id: 3,
                    channels: vec![channel(31, 4)],
                },
            ],
        });
        planner.update_edge(
            1,
            11,
            ChannelState::Down,
            TransportMetrics {
                queue_pressure: 1.0,
                send_error: true,
            },
            10,
        );
        let local_channels = LocalChannelTable::new([11, 12]);

        assert_eq!(
            planner.build_route_plan(1, &local_channels),
            Some(vec![12, 31])
        );
    }
}
