use raynet_core::{KcpConfig, RouteConfig, RouteEdge, RouteGraph, RouteNode};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};

/// Default transport packet MTU for UDP over standard Ethernet without IP
/// fragmentation: 1500 (Ethernet) - 40 (IPv6) - 8 (UDP) = 1452. IPv4 allows
/// 1472; 1400 leaves headroom for PPPoE and common tunnel overhead.
pub const DEFAULT_TRANSPORT_MTU: u32 = 1400;

#[derive(Deserialize, Default)]
pub struct FileConfig {
    pub endpoint: Option<bool>,
    pub listen: Option<String>,
    pub relay_key: Option<String>,
    pub endpoint_key: Option<String>,
    /// Mirrors `RelayConfig` / `EndpointConfig` fields that shell does not
    /// derive from keys, channels, or runtime clocks.
    #[serde(default)]
    pub core: CoreFileConfig,
    pub route: Option<Vec<RouteEdgeConfig>>,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
}

/// Shell-facing mirror of core tuning knobs. Keys, `random_seed`, `boot_time_ms`,
/// `local_channels`, and `route.graph` are filled in by the shell at startup.
#[derive(Deserialize, Default)]
pub struct CoreFileConfig {
    /// `RelayConfig.local_min_mtu`
    pub local_min_mtu: Option<u32>,
    /// `EndpointConfig.padding_reserve`
    pub padding_reserve: Option<u8>,
    /// `EndpointConfig.keepalive_interval_ms`
    pub keepalive_interval_ms: Option<u32>,
    #[serde(default)]
    pub kcp: KcpFileConfig,
    #[serde(default)]
    pub route: RouteFileConfig,
}

#[derive(Clone, Copy, Deserialize, Default)]
pub struct KcpFileConfig {
    pub send_window: Option<u16>,
    pub receive_window: Option<u16>,
    pub time_scale: Option<u8>,
    pub fast_resend: Option<u32>,
}

#[derive(Clone, Copy, Deserialize, Default)]
pub struct RouteFileConfig {
    /// `RouteConfig.min_mtu`
    pub min_mtu: Option<u32>,
    pub feedback_interval_ms: Option<u32>,
    pub feedback_timeout_ms: Option<u32>,
}

impl CoreFileConfig {
    pub fn relay_tuning(&self) -> RelayCoreTuning {
        RelayCoreTuning {
            local_min_mtu: self.local_min_mtu.unwrap_or(DEFAULT_TRANSPORT_MTU),
        }
    }

    pub fn endpoint_tuning(&self) -> EndpointCoreTuning {
        EndpointCoreTuning {
            kcp: self.kcp.to_kcp_config(),
            route: self.route.to_route_timing(),
            padding_reserve: self.padding_reserve.unwrap_or(128),
            keepalive_interval_ms: self.keepalive_interval_ms.unwrap_or(0),
        }
    }
}

pub struct RelayCoreTuning {
    pub local_min_mtu: u32,
}

pub struct EndpointCoreTuning {
    pub kcp: KcpConfig,
    pub route: RouteTiming,
    pub padding_reserve: u8,
    pub keepalive_interval_ms: u32,
}

pub struct RouteTiming {
    pub min_mtu: u32,
    pub feedback_interval_ms: u32,
    pub feedback_timeout_ms: u32,
}

impl KcpFileConfig {
    fn to_kcp_config(self) -> KcpConfig {
        let defaults = KcpConfig::default();
        KcpConfig {
            send_window: self.send_window.unwrap_or(defaults.send_window),
            receive_window: self.receive_window.unwrap_or(defaults.receive_window),
            time_scale: self.time_scale.unwrap_or(defaults.time_scale),
            fast_resend: self.fast_resend.unwrap_or(defaults.fast_resend),
        }
    }
}

impl RouteFileConfig {
    fn to_route_timing(self) -> RouteTiming {
        RouteTiming {
            min_mtu: self.min_mtu.unwrap_or(DEFAULT_TRANSPORT_MTU),
            feedback_interval_ms: self.feedback_interval_ms.unwrap_or(1_000),
            feedback_timeout_ms: self.feedback_timeout_ms.unwrap_or(5_000),
        }
    }
}

impl EndpointCoreTuning {
    pub fn route_config(&self, graph: RouteGraph) -> RouteConfig {
        RouteConfig {
            graph,
            min_mtu: self.route.min_mtu,
            feedback_interval_ms: self.route.feedback_interval_ms,
            feedback_timeout_ms: self.route.feedback_timeout_ms,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub enum ChannelConfig {
    #[serde(rename = "udp-tx")]
    Tx {
        id: u16,
        bind: HostPorts,
        destination: HostPorts,
    },
    #[serde(rename = "udp-rx")]
    Rx { id: u16, bind: HostPorts },
    #[serde(rename = "udp-rev-tx")]
    RevTx { id: u16, bind: HostPorts },
    #[serde(rename = "udp-rev-rx")]
    RevRx {
        id: u16,
        bind: HostPorts,
        destination: HostPorts,
    },
}

#[derive(Deserialize)]
pub struct HostPorts {
    pub host: String,
    pub ports: Vec<u16>,
}

impl HostPorts {
    pub fn resolve(self) -> std::io::Result<Vec<std::net::SocketAddr>> {
        use std::net::ToSocketAddrs;
        assert!(!self.ports.is_empty());
        let host = (self.host.as_str(), 0)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::other("host resolved to no address"))?
            .ip();
        Ok(self
            .ports
            .into_iter()
            .map(|port| std::net::SocketAddr::new(host, port))
            .collect())
    }
}

#[derive(Deserialize)]
pub struct RouteEdgeConfig {
    pub from: String,
    pub to: String,
    pub channel: u16,
    #[serde(default)]
    pub capacity_hint_kbps: u32,
    #[serde(default)]
    pub latency_hint_ms: u32,
}

pub fn build_route(edges: Vec<RouteEdgeConfig>) -> RouteGraph {
    assert!(!edges.is_empty());
    let mut indexes = HashMap::new();
    for edge in &edges {
        let next = indexes.len();
        indexes.entry(edge.from.clone()).or_insert(next);
        let next = indexes.len();
        indexes.entry(edge.to.clone()).or_insert(next);
    }

    let mut nodes = vec![RouteNode { edges: Vec::new() }; indexes.len()];
    let mut indegree = vec![0; nodes.len()];
    for edge in edges {
        let from = indexes[&edge.from];
        let next = indexes[&edge.to];
        assert_ne!(from, next);
        indegree[next] += 1;
        nodes[from].edges.push(RouteEdge {
            channel_id: edge.channel,
            next: next as u32,
            capacity_hint_kbps: edge.capacity_hint_kbps,
            latency_hint_ms: edge.latency_hint_ms,
        });
    }

    let sources = indegree
        .iter()
        .enumerate()
        .filter(|(_, count)| **count == 0)
        .map(|(node, _)| node)
        .collect::<Vec<_>>();
    assert_eq!(sources.len(), 1);
    assert_eq!(nodes.iter().filter(|node| node.edges.is_empty()).count(), 1);

    let mut ready = VecDeque::from(sources);
    let mut order = Vec::with_capacity(nodes.len());
    while let Some(node) = ready.pop_front() {
        order.push(node);
        for edge in &nodes[node].edges {
            let next = edge.next as usize;
            indegree[next] -= 1;
            if indegree[next] == 0 {
                ready.push_back(next);
            }
        }
    }
    assert_eq!(order.len(), nodes.len());

    let mut positions = vec![0; nodes.len()];
    for (position, node) in order.iter().enumerate() {
        positions[*node] = position as u32;
    }
    RouteGraph {
        nodes: order
            .into_iter()
            .map(|node| RouteNode {
                edges: nodes[node]
                    .edges
                    .iter()
                    .map(|edge| RouteEdge {
                        next: positions[edge.next as usize],
                        ..edge.clone()
                    })
                    .collect(),
            })
            .collect(),
    }
}
