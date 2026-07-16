mod common;

use raynet_core::{CoreAction, CoreEvent, CoreEventResult, RouteNode};

use common::{edge, endpoint_config, take_send};

fn direct_config(seed: u8) -> raynet_core::EndpointConfig {
    endpoint_config(
        seed,
        vec![10],
        vec![
            RouteNode {
                edges: vec![edge(10, 1)],
            },
            RouteNode { edges: Vec::new() },
        ],
    )
}

fn direct_endpoint(seed: u8) -> raynet_core::EndpointCore {
    raynet_core::EndpointCore::new(direct_config(seed))
}

fn deliver_sends(
    target: &mut raynet_core::EndpointCore,
    source_actions: &mut Vec<CoreAction>,
    target_actions: &mut Vec<CoreAction>,
    elapsed_ms: u64,
) {
    let actions = std::mem::take(source_actions);
    for action in actions {
        match action {
            CoreAction::SendTransportPacket { bytes, .. } => {
                target.handle_event(
                    elapsed_ms,
                    CoreEvent::TransportPacketReceived {
                        channel_id: 10,
                        bytes,
                    },
                    target_actions,
                );
            }
            action => source_actions.push(action),
        }
    }
}

#[test]
fn next_deadline_is_max_when_idle() {
    assert_eq!(direct_endpoint(3).next_deadline(0), u64::MAX);
}

#[test]
fn session_write_blocked_is_atomic() {
    let mut config = direct_config(3);
    config.kcp.send_window = 1;
    let mut endpoint = raynet_core::EndpointCore::new(config);
    let mut actions = Vec::new();
    let CoreEventResult::SessionCreated { conv_id } =
        endpoint.handle_event(0, CoreEvent::SessionOpen, &mut actions)
    else {
        panic!("expected session");
    };

    let mut blocked_actions = Vec::new();
    let result = endpoint.handle_event(
        0,
        CoreEvent::SessionWrite {
            conv_id,
            bytes: vec![0; 1],
        },
        &mut blocked_actions,
    );
    assert!(matches!(result, CoreEventResult::SessionWriteBlocked));
    assert!(blocked_actions.is_empty());
}

#[test]
fn reroutes_a_failed_transport_packet() {
    let mut endpoint = raynet_core::EndpointCore::new(endpoint_config(
        3,
        vec![10, 20],
        vec![
            RouteNode {
                edges: vec![edge(10, 1), edge(20, 1)],
            },
            RouteNode { edges: Vec::new() },
        ],
    ));
    let mut actions = Vec::new();
    endpoint.handle_event(0, CoreEvent::SessionOpen, &mut actions);
    let (failed_channel, bytes) = take_send(&mut actions);

    endpoint.handle_event(
        0,
        CoreEvent::TransportPacketSendFailed {
            channel_id: failed_channel,
            bytes,
        },
        &mut actions,
    );
    let (retry_channel, _) = take_send(&mut actions);
    assert_ne!(retry_channel, failed_channel);
}

#[test]
fn endpoint_authentication_failure_creates_no_state() {
    let mut sender = direct_endpoint(3);
    let mut receiver_config = direct_config(4);
    receiver_config.message_key = [9; 16];
    let mut receiver = raynet_core::EndpointCore::new(receiver_config);

    let mut actions = Vec::new();
    sender.handle_event(0, CoreEvent::SessionOpen, &mut actions);
    let (_, bytes) = take_send(&mut actions);
    receiver.handle_event(
        0,
        CoreEvent::TransportPacketReceived {
            channel_id: 10,
            bytes,
        },
        &mut actions,
    );
    assert_eq!(receiver.metrics().authentication_failures, 1);
    assert_eq!(receiver.metrics().active_sequences, 0);
}

#[test]
fn open_and_write_cross_the_public_state_machine_boundary() {
    let mut left = direct_endpoint(3);
    let mut right = direct_endpoint(4);
    let mut left_actions = Vec::new();
    let mut right_actions = Vec::new();
    let CoreEventResult::SessionCreated { conv_id } =
        left.handle_event(0, CoreEvent::SessionOpen, &mut left_actions)
    else {
        panic!("expected session");
    };

    deliver_sends(&mut right, &mut left_actions, &mut right_actions, 1);
    deliver_sends(&mut left, &mut right_actions, &mut left_actions, 2);
    assert!(
        right_actions.iter().any(
            |action| matches!(action, CoreAction::OpenSession { conv_id: id } if *id == conv_id)
        )
    );

    left.handle_event(
        3,
        CoreEvent::SessionWrite {
            conv_id,
            bytes: b"payload".to_vec(),
        },
        &mut left_actions,
    );
    deliver_sends(&mut right, &mut left_actions, &mut right_actions, 4);
    assert!(right_actions.iter().any(|action| matches!(
        action,
        CoreAction::WriteSession { conv_id: id, bytes } if *id == conv_id && bytes == b"payload"
    )));

    left.handle_event(
        5,
        CoreEvent::SessionWrite {
            conv_id,
            bytes: Vec::new(),
        },
        &mut left_actions,
    );
    deliver_sends(&mut right, &mut left_actions, &mut right_actions, 6);
    assert!(right_actions.iter().any(|action| matches!(
        action,
        CoreAction::WriteSession { conv_id: id, bytes } if *id == conv_id && bytes.is_empty()
    )));
}

#[test]
fn malformed_transport_packet_creates_no_state() {
    let mut endpoint = direct_endpoint(3);
    endpoint.handle_event(
        0,
        CoreEvent::TransportPacketReceived {
            channel_id: 10,
            bytes: vec![0; 1],
        },
        &mut Vec::new(),
    );
    assert_eq!(endpoint.metrics().active_sequences, 0);
    assert_eq!(endpoint.metrics().packets_dropped, 1);
}
