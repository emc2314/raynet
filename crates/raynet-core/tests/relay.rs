mod common;

use raynet_core::{CoreAction, CoreEvent, RelayConfig, RelayCore, RouteNode};

use common::{edge, endpoint_config, take_send};

fn packet_for_relay(next_channel: u16) -> Vec<u8> {
    let mut sender = raynet_core::EndpointCore::new(endpoint_config(
        3,
        vec![10],
        vec![
            RouteNode {
                edges: vec![edge(10, 1)],
            },
            RouteNode {
                edges: vec![edge(next_channel, 2)],
            },
            RouteNode { edges: Vec::new() },
        ],
    ));
    let mut actions = Vec::new();
    sender.handle_event(0, CoreEvent::SessionOpen, &mut actions);
    take_send(&mut actions).1
}

fn relay() -> RelayCore {
    RelayCore::new(RelayConfig {
        envelope_key: [1; 16],
        random_seed: [4; 16],
        boot_time_ms: 0,
        local_channels: vec![20],
        local_min_mtu: 1200,
    })
}

#[test]
fn forwards_once_and_drops_a_replay() {
    let bytes = packet_for_relay(20);
    let mut relay = relay();
    let mut actions = Vec::new();
    relay.handle_event(
        0,
        CoreEvent::TransportPacketReceived {
            channel_id: 1,
            bytes: bytes.clone(),
        },
        &mut actions,
    );
    let CoreAction::SendTransportPacket {
        channel_id,
        bytes: forwarded,
    } = &actions[0]
    else {
        panic!("expected forwarded packet");
    };
    assert_eq!(*channel_id, 20);
    assert_ne!(forwarded, &bytes);

    actions.clear();
    relay.handle_event(
        0,
        CoreEvent::TransportPacketReceived {
            channel_id: 1,
            bytes,
        },
        &mut actions,
    );
    assert!(actions.is_empty());
    assert_eq!(relay.metrics().replay_drops, 1);
}

#[test]
fn drops_an_unknown_outbound_channel() {
    let mut relay = relay();
    let mut actions = Vec::new();
    relay.handle_event(
        0,
        CoreEvent::TransportPacketReceived {
            channel_id: 1,
            bytes: packet_for_relay(21),
        },
        &mut actions,
    );
    assert!(actions.is_empty());
    assert_eq!(relay.metrics().packets_dropped, 1);
}
