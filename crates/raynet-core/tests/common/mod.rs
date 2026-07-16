use raynet_core::{
    ChannelId, CoreAction, EndpointConfig, KcpConfig, RouteConfig, RouteEdge, RouteGraph, RouteNode,
};

pub fn edge(channel_id: ChannelId, next: u32) -> RouteEdge {
    RouteEdge {
        channel_id,
        next,
        capacity_hint_kbps: 0,
        latency_hint_ms: 0,
    }
}

pub fn endpoint_config(
    seed: u8,
    local_channels: Vec<ChannelId>,
    nodes: Vec<RouteNode>,
) -> EndpointConfig {
    EndpointConfig {
        envelope_key: [1; 16],
        message_key: [2; 16],
        random_seed: [seed; 16],
        boot_time_ms: 0,
        kcp: KcpConfig::default(),
        route: RouteConfig {
            graph: RouteGraph { nodes },
            min_mtu: 1200,
            feedback_interval_ms: 1_000,
            feedback_timeout_ms: 5_000,
        },
        local_channels,
        padding_reserve: 128,
        keepalive_interval_ms: 0,
    }
}

pub fn take_send(actions: &mut Vec<CoreAction>) -> (ChannelId, Vec<u8>) {
    let CoreAction::SendTransportPacket { channel_id, bytes } = actions.remove(0) else {
        panic!("expected transport packet");
    };
    (channel_id, bytes)
}
