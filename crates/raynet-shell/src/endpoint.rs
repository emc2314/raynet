use aegis::aegis128l::Key;
use log::info;
use rand::RngExt;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

use crate::channels::Channels;
use crate::config::EndpointCoreTuning;
use crate::remote::{EndpointRuntime, forward_out};
use crate::transport::OutboundTransportPacket;
use crate::utils::CoreClock;
use raynet_core::{EndpointConfig, EndpointCore, RouteEdge, RouteGraph, RouteNode};
use raynet_shell_plugins::{ProxyListener, ProxyPlugin};

pub fn run(
    channels: Channels,
    envelope_key: Key,
    message_key: Key,
    route: Option<RouteGraph>,
    tuning: EndpointCoreTuning,
    mut proxy_listener: ProxyListener,
    proxy: Arc<dyn ProxyPlugin>,
) {
    let Channels {
        senders: channels,
        receiver: channel_receiver,
        failures,
    } = channels;
    let clock = Arc::new(CoreClock::new());
    let (ray_tx, ray_rx) = mpsc::channel::<OutboundTransportPacket>(65536);
    let local_channels = channels.channel_ids();
    let random_seed = rand::rng().random();
    let endpoint_core = Arc::new(Mutex::new(EndpointCore::new(EndpointConfig {
        envelope_key,
        message_key,
        random_seed,
        boot_time_ms: clock.boot_time_ms(),
        local_channels: local_channels.clone(),
        route: tuning.route_config(route_graph(&local_channels, route)),
        kcp: tuning.kcp,
        padding_reserve: tuning.padding_reserve,
        keepalive_interval_ms: tuning.keepalive_interval_ms,
    })));

    let runtime = EndpointRuntime::new(proxy, endpoint_core, ray_tx, clock);
    tokio::spawn(runtime.clone().receive_packets(channel_receiver));
    let local_runtime = runtime.clone();
    tokio::spawn(async move {
        while let Some(session) = proxy_listener.accept().await {
            local_runtime.open(session).await;
        }
    });
    tokio::spawn(runtime.clone().poll());

    tokio::spawn(async move {
        forward_out(ray_rx, failures, channels, move |packet| {
            let runtime = runtime.clone();
            async move { runtime.send_failed(packet).await }
        })
        .await;
    });

    info!("Started RayNet Endpoint");
}

fn route_graph(local_channels: &[raynet_core::ChannelId], route: Option<RouteGraph>) -> RouteGraph {
    let graph = route.unwrap_or_else(|| RouteGraph {
        nodes: vec![
            RouteNode {
                edges: local_channels
                    .iter()
                    .map(|channel_id| RouteEdge {
                        channel_id: *channel_id,
                        next: 1,
                        capacity_hint_kbps: 0,
                        latency_hint_ms: 0,
                    })
                    .collect(),
            },
            RouteNode { edges: Vec::new() },
        ],
    });
    assert!(
        graph.nodes[0]
            .edges
            .iter()
            .all(|edge| local_channels.contains(&edge.channel_id))
    );
    graph
}
