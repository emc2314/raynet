use clap::Parser;
use env_logger::Env;
use log::info;
use raynet_core::{RouteEdge, RouteGraph, RouteNode};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::{fs, io};

mod channels;
mod endpoint;
mod relay;
mod remote;
mod transport;
mod utils;

use channels::channels;
use raynet_shell_plugins::channel_udp::{ForwardUdp, ReverseUdp};
use raynet_shell_plugins::proxy_socks5::Socks5Proxy;
use raynet_shell_plugins::{ChannelReceiver, ChannelSender};

use mimalloc::MiMalloc;
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Deserialize, Default)]
struct FileConfig {
    endpoint: Option<bool>,
    listen: Option<String>,
    relay_key: Option<String>,
    endpoint_key: Option<String>,
    route: Option<Vec<RouteEdgeConfig>>,
    #[serde(default)]
    channels: Vec<ChannelConfig>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ChannelConfig {
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
struct HostPorts {
    host: String,
    ports: Vec<u16>,
}

impl HostPorts {
    fn resolve(self) -> io::Result<Vec<SocketAddr>> {
        assert!(!self.ports.is_empty());
        let host = (self.host.as_str(), 0)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::other("host resolved to no address"))?
            .ip();
        Ok(self
            .ports
            .into_iter()
            .map(|port| SocketAddr::new(host, port))
            .collect())
    }
}

#[derive(Deserialize)]
struct RouteEdgeConfig {
    from: String,
    to: String,
    channel: u16,
    #[serde(default)]
    capacity_hint_kbps: u32,
    #[serde(default)]
    latency_hint_ms: u32,
}

fn build_route(edges: Vec<RouteEdgeConfig>) -> RouteGraph {
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

#[derive(Parser)]
#[command(name = "RayNet")]
#[command(version = "0.1.0")]
#[command(about = "Cross the fire")]
struct CliConfig {
    #[arg(short, long)]
    endpoint: bool,
    #[arg(short, long, value_name = "ADDRESS")]
    listen: Option<String>,
    #[arg(long)]
    relay_key: Option<String>,
    #[arg(long)]
    endpoint_key: Option<String>,
    #[arg(short, long, value_name = "CONFIG_FILE")]
    config: Option<String>,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let env = Env::default()
        .filter_or("RAYNET_LOG_LEVEL", "trace")
        .write_style_or("RAYNET_LOG_STYLE", "always");
    env_logger::init_from_env(env);

    let cli = CliConfig::parse();
    let file_config = match cli.config.as_ref() {
        Some(path) => {
            let config_str =
                fs::read_to_string(Path::new(path)).expect("Failed to read config file");
            toml::from_str(&config_str).expect("Failed to parse config file")
        }
        None => FileConfig::default(),
    };

    let endpoint = cli.endpoint || file_config.endpoint.unwrap_or(false);
    let relay_psk = cli
        .relay_key
        .or(file_config.relay_key)
        .expect("relay key is required");
    let channel_key = blake3::derive_key("RayNet channel PSK v1", relay_psk.as_bytes());
    let envelope_key = blake3::derive_key("RayNet envelope PSK v1", relay_psk.as_bytes())[..16]
        .try_into()
        .unwrap();
    let (channel_senders, channel_receivers) =
        build_channels(file_config.channels, channel_key).await?;
    let (channel_senders, channel_receiver) = channels(channel_senders, channel_receivers);

    if !endpoint {
        relay::run(channel_senders, channel_receiver, envelope_key);
    } else {
        let route = file_config.route.map(build_route);
        let listen_addr: SocketAddr = cli
            .listen
            .or(file_config.listen)
            .unwrap_or_else(|| "[::0]:8443".to_string())
            .to_socket_addrs()
            .expect("Unable to resolve listen address")
            .next()
            .unwrap();
        let endpoint_psk = cli
            .endpoint_key
            .or(file_config.endpoint_key)
            .expect("endpoint key is required");
        let message_key = blake3::derive_key("RayNet endpoint PSK v1", endpoint_psk.as_bytes())
            [..16]
            .try_into()
            .unwrap();
        let (proxy_listener, proxy) = Socks5Proxy::bind(listen_addr).await?;
        endpoint::run(
            channel_senders,
            channel_receiver,
            envelope_key,
            message_key,
            route,
            proxy_listener,
            std::sync::Arc::new(proxy),
        )
    }

    tokio::signal::ctrl_c().await?;
    info!("Ctrl-C received, shutting down");
    Ok(())
}

async fn build_channels(
    configs: Vec<ChannelConfig>,
    channel_key: [u8; 32],
) -> io::Result<(Vec<ChannelSender>, Vec<ChannelReceiver>)> {
    let mut senders = Vec::new();
    let mut receivers = Vec::new();
    for config in configs {
        match config {
            ChannelConfig::Tx {
                id,
                bind,
                destination,
            } => {
                senders.push(ForwardUdp::sender(id, bind.resolve()?, destination.resolve()?).await?)
            }
            ChannelConfig::Rx { id, bind } => {
                receivers.push(ForwardUdp::receiver(id, bind.resolve()?).await?)
            }
            ChannelConfig::RevTx { id, bind } => {
                senders.push(ReverseUdp::sender(id, bind.resolve()?, channel_key).await?)
            }
            ChannelConfig::RevRx {
                id,
                bind,
                destination,
            } => receivers.push(
                ReverseUdp::receiver(id, bind.resolve()?, destination.resolve()?, channel_key)
                    .await?,
            ),
        }
    }
    Ok((senders, receivers))
}
