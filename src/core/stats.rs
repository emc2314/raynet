use std::cmp::Ordering;
use std::net::SocketAddr;

use crate::core::packet::{DataPacket, RayPacket, RayPacketType};
use crate::routing::{Nodes, StatRequest, StatResponse};

pub fn build_stat_response(endpoint: bool, nodes: &Nodes, tc: f32, data: &[u8]) -> RayPacket {
    let max_next = if endpoint {
        1.0
    } else {
        nodes
            .nodes
            .iter()
            .map(|node| node.weight.load(std::sync::atomic::Ordering::Relaxed))
            .max_by(|a, b| {
                a.partial_cmp(b)
                    .unwrap_or_else(|| match (a.is_nan(), b.is_nan()) {
                        (true, true) => Ordering::Equal,
                        (true, false) => Ordering::Less,
                        (false, true) => Ordering::Greater,
                        (false, false) => Ordering::Equal,
                    })
            })
            .unwrap_or(1.0)
    };

    let request: StatRequest = bitcode::decode(data).unwrap();
    let weight = (tc / (request.tc + 2.0)) * max_next;

    RayPacket::new(
        RayPacketType::StatResponse,
        DataPacket {
            data: bitcode::encode(&StatResponse {
                index: request.index,
                weight,
            }),
        },
    )
}

pub fn apply_stat_update(nodes: &Nodes, addr: SocketAddr, weight: f32) {
    let mut flag = false;
    let ip = addr.ip().to_canonical();
    for node in nodes.nodes.iter() {
        if node.addr.ip() == ip {
            node.weight
                .store(weight, std::sync::atomic::Ordering::Relaxed);
            nodes.build_dist();
            flag = true;
            break;
        }
    }
    if !flag {
        log::error!("Received weight from unknown node {}", addr);
    }
}
