use crate::limits::{LOCAL_COOLDOWN_INITIAL_MS, LOCAL_COOLDOWN_MAX_MS};
use crate::machine::ChannelId;

const DEFAULT_ROUTE_CAPACITY_KBPS: u32 = 50_000;
const DEFAULT_ROUTE_LATENCY_MS: u32 = 50;
const FIXED_ONE: u64 = 1 << 16;
const LOSS_ONE: u32 = u16::MAX as u32;

#[derive(Clone, PartialEq, Eq)]
pub struct RouteConfig {
    pub graph: RouteGraph,
    pub min_mtu: u32,
    pub feedback_interval_ms: u32,
    pub feedback_timeout_ms: u32,
}

#[derive(Clone, PartialEq, Eq)]
pub struct RouteGraph {
    pub nodes: Vec<RouteNode>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct RouteNode {
    pub edges: Vec<RouteEdge>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct RouteEdge {
    pub channel_id: ChannelId,
    pub next: u32,
    pub capacity_hint_kbps: u32,
    pub latency_hint_ms: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct EdgeId {
    node: usize,
    edge: usize,
}

struct Edge {
    channel_id: ChannelId,
    next: usize,
    capacity_kbps: u32,
    latency_ms: u32,
    virtual_free_at: u64,
    loss: u32,
    cooldown_until: u64,
    retry_delay_ms: u64,
    last_sampled: Option<u64>,
    last_observation_at: Option<u64>,
}

pub(crate) struct RoutePlanner {
    nodes: Vec<Vec<Edge>>,
}

impl RoutePlanner {
    pub(crate) fn new(config: &RouteConfig) -> Self {
        let nodes = config
            .graph
            .nodes
            .iter()
            .map(|node| {
                node.edges
                    .iter()
                    .map(|edge| Edge {
                        channel_id: edge.channel_id,
                        next: edge.next as usize,
                        capacity_kbps: if edge.capacity_hint_kbps == 0 {
                            DEFAULT_ROUTE_CAPACITY_KBPS
                        } else {
                            edge.capacity_hint_kbps
                        },
                        latency_ms: if edge.latency_hint_ms == 0 {
                            DEFAULT_ROUTE_LATENCY_MS
                        } else {
                            edge.latency_hint_ms
                        },
                        virtual_free_at: 0,
                        loss: 0,
                        cooldown_until: 0,
                        retry_delay_ms: LOCAL_COOLDOWN_INITIAL_MS,
                        last_sampled: None,
                        last_observation_at: None,
                    })
                    .collect()
            })
            .collect();
        Self { nodes }
    }

    pub(crate) fn build_route_plan(
        &mut self,
        elapsed_ms: u64,
        scheduled_bytes: usize,
        sample: bool,
    ) -> Option<Vec<ChannelId>> {
        let forced = if sample {
            Some(self.sampling_edge(elapsed_ms)?)
        } else {
            None
        };
        let edge_ids = self.calculate_path(elapsed_ms, scheduled_bytes, forced)?;

        self.reserve(&edge_ids, elapsed_ms, scheduled_bytes, sample);
        Some(
            edge_ids
                .into_iter()
                .map(|edge| self.nodes[edge.node][edge.edge].channel_id)
                .collect(),
        )
    }

    fn sampling_edge(&self, elapsed_ms: u64) -> Option<EdgeId> {
        let destination = self.nodes.len() - 1;
        let mut reachable = vec![false; self.nodes.len()];
        reachable[0] = true;
        let mut selected = None;
        let mut node = 0;
        while node < destination {
            if !reachable[node] {
                node += 1;
                continue;
            }
            for edge in 0..self.nodes[node].len() {
                let edge_id = EdgeId { node, edge };
                if !self.edge_allowed(edge_id, elapsed_ms) {
                    continue;
                }
                reachable[self.nodes[node][edge].next] = true;
                if selected.is_none_or(|selected: EdgeId| {
                    self.nodes[node][edge].last_sampled
                        < self.nodes[selected.node][selected.edge].last_sampled
                }) {
                    selected = Some(edge_id);
                }
            }
            node += 1;
        }
        selected
    }

    pub(crate) fn note_local_send_failure(&mut self, channel_id: ChannelId, elapsed_ms: u64) {
        let edge = self.nodes[0]
            .iter()
            .position(|edge| edge.channel_id == channel_id)
            .unwrap();
        let runtime = &mut self.nodes[0][edge];
        if runtime
            .last_observation_at
            .is_none_or(|last| elapsed_ms >= last)
        {
            runtime.loss += (LOSS_ONE - runtime.loss) / 2;
            runtime.last_observation_at = Some(elapsed_ms);
        }
        runtime.cooldown_until = elapsed_ms + runtime.retry_delay_ms;
        runtime.retry_delay_ms = (runtime.retry_delay_ms * 2).min(LOCAL_COOLDOWN_MAX_MS);
    }

    pub(crate) fn note_feedback(&mut self, plan: &[ChannelId], observation_at: u64, success: bool) {
        let mut node = 0;
        let mut first = None;
        let mut accepted_first = false;
        for channel in plan {
            let edge = self.nodes[node]
                .iter()
                .position(|edge| edge.channel_id == *channel)
                .unwrap();
            let edge_id = EdgeId { node, edge };
            let is_first = first.is_none();
            first.get_or_insert(edge_id);
            node = self.nodes[node][edge].next;

            let runtime = &mut self.nodes[edge_id.node][edge_id.edge];
            if runtime
                .last_observation_at
                .is_some_and(|last| observation_at < last)
            {
                continue;
            }
            runtime.loss = if success {
                runtime.loss * 3 / 4
            } else {
                runtime.loss + (LOSS_ONE - runtime.loss) / 16
            };
            runtime.last_observation_at = Some(observation_at);
            accepted_first |= is_first;
        }
        if success && accepted_first {
            let first = first.unwrap();
            let runtime = &mut self.nodes[first.node][first.edge];
            runtime.cooldown_until = 0;
            runtime.retry_delay_ms = LOCAL_COOLDOWN_INITIAL_MS;
        }
    }

    fn calculate_path(
        &self,
        elapsed_ms: u64,
        scheduled_bytes: usize,
        forced: Option<EdgeId>,
    ) -> Option<Vec<EdgeId>> {
        let destination = self.nodes.len() - 1;
        let mut arrival = vec![[u64::MAX; 2]; self.nodes.len()];
        let mut predecessor = vec![[None; 2]; self.nodes.len()];
        arrival[0][0] = elapsed_ms * FIXED_ONE;

        for node in 0..destination {
            for state in 0..=usize::from(forced.is_some()) {
                let node_arrival = arrival[node][state];
                if node_arrival == u64::MAX {
                    continue;
                }
                for edge_index in 0..self.nodes[node].len() {
                    let edge_id = EdgeId {
                        node,
                        edge: edge_index,
                    };
                    if !self.edge_allowed(edge_id, elapsed_ms) {
                        continue;
                    }
                    let edge = &self.nodes[node][edge_index];
                    let next_state = state | usize::from(forced == Some(edge_id));
                    let start = node_arrival.max(edge.virtual_free_at);
                    let candidate = start
                        + self.serialization_time(edge_id, scheduled_bytes)
                        + self.latency(edge_id);
                    let next = edge.next;
                    if candidate < arrival[next][next_state] {
                        arrival[next][next_state] = candidate;
                        predecessor[next][next_state] = Some((state, edge_id));
                    }
                }
            }
        }

        let mut state = usize::from(forced.is_some());
        if arrival[destination][state] == u64::MAX {
            return None;
        }
        let mut node = destination;
        let mut path = Vec::new();
        while node != 0 {
            let (previous_state, edge) = predecessor[node][state]?;
            path.push(edge);
            node = edge.node;
            state = previous_state;
        }
        path.reverse();
        Some(path)
    }

    fn edge_allowed(&self, edge_id: EdgeId, elapsed_ms: u64) -> bool {
        elapsed_ms >= self.nodes[edge_id.node][edge_id.edge].cooldown_until
    }

    fn reserve(&mut self, path: &[EdgeId], elapsed_ms: u64, scheduled_bytes: usize, sample: bool) {
        let mut cursor = elapsed_ms * FIXED_ONE;
        for &edge in path {
            let start = cursor.max(self.nodes[edge.node][edge.edge].virtual_free_at);
            let finished = start + self.serialization_time(edge, scheduled_bytes);
            let latency = self.latency(edge);
            let runtime = &mut self.nodes[edge.node][edge.edge];
            runtime.virtual_free_at = finished;
            if sample {
                runtime.last_sampled = Some(elapsed_ms);
            }
            cursor = finished + latency;
        }
    }

    fn serialization_time(&self, edge_id: EdgeId, bytes: usize) -> u64 {
        let edge = &self.nodes[edge_id.node][edge_id.edge];
        let capacity = u64::from(edge.capacity_kbps);
        let loss = u64::from(edge.loss);
        let penalty = FIXED_ONE + 15 * loss * loss * FIXED_ONE / u64::from(LOSS_ONE).pow(2);
        let effective_capacity = (capacity * FIXED_ONE / penalty).max(1);
        (bytes as u64 * 8 * FIXED_ONE).div_ceil(effective_capacity)
    }

    fn latency(&self, edge_id: EdgeId) -> u64 {
        let latency = u64::from(self.nodes[edge_id.node][edge_id.edge].latency_ms);
        latency * FIXED_ONE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_two_local_edges() -> RouteConfig {
        RouteConfig {
            graph: RouteGraph {
                nodes: vec![
                    RouteNode {
                        edges: vec![
                            RouteEdge {
                                channel_id: 2,
                                next: 1,
                                capacity_hint_kbps: 0,
                                latency_hint_ms: 0,
                            },
                            RouteEdge {
                                channel_id: 1,
                                next: 1,
                                capacity_hint_kbps: 0,
                                latency_hint_ms: 0,
                            },
                        ],
                    },
                    RouteNode { edges: Vec::new() },
                ],
            },
            min_mtu: 1200,
            feedback_interval_ms: 1_000,
            feedback_timeout_ms: 5_000,
        }
    }

    #[test]
    fn equal_routes_use_graph_order() {
        let config = graph_two_local_edges();
        let mut planner = RoutePlanner::new(&config);
        assert_eq!(planner.build_route_plan(0, 100, false), Some(vec![2]));
    }

    #[test]
    fn failed_local_edge_enters_exponential_cooldown() {
        let config = graph_two_local_edges();
        let mut planner = RoutePlanner::new(&config);
        planner.note_local_send_failure(2, 0);
        assert_eq!(planner.build_route_plan(1, 100, true), Some(vec![1]));
        assert_eq!(planner.build_route_plan(49, 100, false), Some(vec![1]));
        assert_eq!(planner.build_route_plan(50, 100, false), Some(vec![1]));
    }

    #[test]
    fn sampling_rotates_across_edges() {
        let config = graph_two_local_edges();
        let mut planner = RoutePlanner::new(&config);
        assert_eq!(planner.build_route_plan(0, 100, true), Some(vec![2]));
        assert_eq!(planner.build_route_plan(1, 100, true), Some(vec![1]));
    }

    #[test]
    fn virtual_service_time_spreads_work() {
        let mut config = graph_two_local_edges();
        config.graph.nodes[0].edges[0].capacity_hint_kbps = 1_000;
        config.graph.nodes[0].edges[0].latency_hint_ms = 1;
        config.graph.nodes[0].edges[1].capacity_hint_kbps = 100_000;
        config.graph.nodes[0].edges[1].latency_hint_ms = 10;
        let mut planner = RoutePlanner::new(&config);
        assert_eq!(planner.build_route_plan(0, 1_000, false), Some(vec![2]));
        let mut used_fast = false;
        for _ in 0..20 {
            used_fast |= planner.build_route_plan(0, 1_000, false) == Some(vec![1]);
        }
        assert!(used_fast);
    }
}
