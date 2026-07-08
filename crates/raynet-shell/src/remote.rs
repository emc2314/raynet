use aegis::aegis128l::Key;
use futures::future::join_all;
use log::{debug, error, warn};
use rand::seq::IteratorRandom;
use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use tokio::net::UdpSocket;
use tokio::sync::{RwLock, mpsc};
use tokio::time::Duration;

use crate::connections::Connections;
use crate::utils::now_millis;
use raynet_core::core::{
    DataPacket, NonceFilter, RayPacket, RayPacketError, RayPacketType, TCPPacket,
    apply_stat_update, build_stat_response,
};
use raynet_core::kcp;
use raynet_core::routing::{Nodes, StatRequest, StatResponse, TimedCounter};

async fn encrypt_and_send_udp(
    socket: &UdpSocket,
    remote: SocketAddr,
    packet: &RayPacket,
    key: &Key,
    buf: &mut [u8],
) {
    let rsize = packet.encrypt(now_millis(), key, buf);
    match socket.send_to(&buf[..rsize], remote).await {
        Ok(sent_size) => {
            debug!("Sent {} bytes to {}", sent_size, remote);
            if rsize != sent_size {
                error!("Sent partial {} of {} bytes", sent_size, rsize);
            }
        }
        Err(e) => error!("UDP Failed to send to {}: {}", remote, e),
    }
}

pub async fn stat_request(nodes: Arc<Nodes>, key: &Key, socket: Arc<UdpSocket>) {
    let mut buf = vec![0u8; 65535];
    let mut index = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let now = now_millis();
        debug!("Total: {}, Nodes: {:?}", nodes.sum(now), nodes.nodes);
        for node in nodes.nodes.iter() {
            let weight = node.weight.load(Relaxed);
            node.weight.store(weight * 0.8, Relaxed);
            if node.weight.load(Relaxed) < 0.01 {
                warn!("Lost route: {:?}", node.name);
            }
        }
        for node in nodes.nodes.iter() {
            node.tc.inc(now);
            let packet = RayPacket::new(
                RayPacketType::StatRequest,
                DataPacket {
                    data: bitcode::encode(&StatRequest {
                        index,
                        tc: node.tc.get(now) as f32,
                    }),
                },
            );
            encrypt_and_send_udp(socket.as_ref(), node.addr, &packet, key, &mut buf).await;
        }
        index += 1;
    }
}

async fn udp_in<F, Fut>(
    udp_socket: Arc<UdpSocket>,
    key: &Key,
    nodes: Arc<Nodes>,
    endpoint: bool,
    mut process_packet: F,
) where
    F: FnMut(RayPacket) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut buf = vec![0u8; 65535];
    let mut filter = NonceFilter::new(1 << 24, 0.00001, 1 << 16, now_millis());
    let mut imap = HashMap::<IpAddr, TimedCounter>::new();
    let mut curr_index = 0;

    loop {
        match udp_socket.recv_from(&mut buf).await {
            Ok((size, src)) => {
                let now = now_millis();
                debug!("Received {} bytes from {}", size, src);
                match RayPacket::decrypt(now, key, &buf[..size], &mut filter) {
                    Ok(packet) => match packet.ptype {
                        RayPacketType::DataPacket => {
                            imap.entry(src.ip())
                                .or_insert_with(|| TimedCounter::new(now))
                                .inc(now);
                            process_packet(packet).await;
                        }
                        RayPacketType::StatRequest => {
                            let tc = imap
                                .entry(src.ip())
                                .or_insert_with(|| TimedCounter::new(now));
                            tc.inc(now);
                            let response = &build_stat_response(
                                endpoint,
                                nodes.as_ref(),
                                tc.get(now) as f32,
                                &packet.data.data,
                            );
                            encrypt_and_send_udp(&udp_socket, src, response, key, &mut buf).await;
                        }
                        RayPacketType::StatResponse => {
                            let response: StatResponse =
                                bitcode::decode(&packet.data.data).unwrap();
                            if response.index >= curr_index {
                                apply_stat_update(nodes.as_ref(), src, response.weight);
                                curr_index = response.index;
                            }
                        }
                    },
                    Err(RayPacketError::NonceReuseError) => {
                        warn!("Duplicated nonce from {}", src);
                    }
                    Err(_) => {
                        warn!("Invalid packet received from {}", src);
                    }
                }
            }
            Err(e) => error!("Failed to receive from UDP: {}", e),
        }
    }
}

pub async fn forward_in(
    udp_socket: Arc<UdpSocket>,
    ray_tx: mpsc::Sender<RayPacket>,
    key: &Key,
    nodes: Arc<Nodes>,
) {
    let process_forward = |packet: RayPacket| async {
        if let Err(e) = ray_tx.send(packet).await {
            error!("Failed to send to channel: {}", e);
        }
    };

    udp_in(udp_socket, key, nodes, false, process_forward).await;
}

pub async fn endpoint_in(
    udp_socket: Arc<UdpSocket>,
    kcp_tx: mpsc::Sender<DataPacket>,
    connections: Arc<RwLock<Connections>>,
    tcp_tx: mpsc::Sender<TCPPacket>,
    key: &Key,
    nodes: Arc<Nodes>,
) {
    let process_endpoint = |packet: RayPacket| async {
        let packet = packet.data;
        let conv = kcp::get_conv(&packet.data);
        let session = if let Some(session) = {
            let con = connections.read().await;
            con.get_from_conv(conv)
        } {
            Some(session)
        } else {
            connections
                .write()
                .await
                .assign_from_conv(conv, &kcp_tx, &tcp_tx)
                .await
        };
        if let Some(session) = session {
            if session.input(&packet.data).await.is_err() {
                error!("KCP session {} closed when input", conv);
                session.close();
                connections.write().await.close(&session.kcp_recv().addr);
            }
        } else {
            error!("No spare connection");
        }
    };

    udp_in(udp_socket, key, nodes, true, process_endpoint).await;
}

async fn udp_out<T, F>(mut rx: mpsc::Receiver<T>, nodes: Arc<Nodes>, key: &Key, process_packet: F)
where
    F: Fn(T) -> RayPacket,
{
    let udp_socket_outs: Vec<UdpSocket> = join_all(
        (0..32)
            .map(|_| UdpSocket::bind("[::0]:0"))
            .collect::<Vec<_>>(),
    )
    .await
    .into_iter()
    .collect::<Result<_, _>>()
    .unwrap();
    let mut buf = vec![0u8; 65535];
    while let Some(packet) = rx.recv().await {
        let (udp_socket_index, remote) = {
            let mut rng = rand::rng();
            (
                (0..udp_socket_outs.len()).choose(&mut rng).unwrap(),
                nodes.route(now_millis(), &mut rng),
            )
        };
        let packet = process_packet(packet);
        encrypt_and_send_udp(
            &udp_socket_outs[udp_socket_index],
            remote,
            &packet,
            key,
            &mut buf,
        )
        .await;
    }
}

pub async fn forward_out(ray_rx: mpsc::Receiver<RayPacket>, nodes: Arc<Nodes>, key: &Key) {
    udp_out(ray_rx, nodes, key, |packet| packet).await
}

pub async fn endpoint_out(kcp_rx: mpsc::Receiver<DataPacket>, nodes: Arc<Nodes>, key: &Key) {
    udp_out(kcp_rx, nodes, key, |packet| {
        RayPacket::new(RayPacketType::DataPacket, packet)
    })
    .await
}
