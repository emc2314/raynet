use aegis::aegis128l::Key;
use futures::future::join_all;
use log::{debug, error, warn};
use rand::seq::IteratorRandom;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, RwLock};
use tokio::time::Duration;

use crate::connections::Connections;
use crate::packets::{DataPacket, RayPacket, RayPacketError, RayPacketType, TCPPacket};
use crate::routing::{Nodes, StatRequest, StatResponse, TimedCounter};
use crate::utils::NonceFilter;

async fn encrypt_and_send_udp(
    socket: &UdpSocket,
    remote: SocketAddr,
    packet: &RayPacket,
    key: &Key,
    buf: &mut [u8],
) {
    let rsize = packet.encrypt(key, buf);
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
        debug!("Total: {}, Nodes: {:?}", nodes.sum(), nodes.nodes);
        for node in nodes.nodes.iter() {
            let weight = node.weight.load(Relaxed);
            node.weight.store(weight * 0.8, Relaxed);
            if node.weight.load(Relaxed) < 0.01 {
                warn!("Lost route: {:?}", node.name);
            }
        }
        for node in nodes.nodes.iter() {
            node.tc.inc();
            let packet = RayPacket::new(
                RayPacketType::StatRequest,
                DataPacket {
                    data: bitcode::encode(&StatRequest {
                        index,
                        tc: node.tc.get() as f32,
                    }),
                },
            );
            encrypt_and_send_udp(socket.as_ref(), node.addr, &packet, key, &mut buf).await;
        }
        index += 1;
    }
}

fn stat_respond(endpoint: bool, nodes: &Nodes, tc: f32, data: &[u8]) -> RayPacket {
    let max_next = if endpoint {
        1.0
    } else {
        nodes
            .nodes
            .iter()
            .map(|node| node.weight.load(Relaxed))
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

fn stat_update(nodes: &Nodes, addr: SocketAddr, weight: f32) {
    let mut flag = false;
    let ip = addr.ip().to_canonical();
    for node in nodes.nodes.iter() {
        if node.addr.ip() == ip {
            node.weight.store(weight, Relaxed);
            nodes.build_dist();
            flag = true;
            break;
        }
    }
    if !flag {
        error!("Received weight from unknown node {}", addr);
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
    let filter = NonceFilter::new(1 << 24, 0.00001, Duration::from_millis(1 << 16));
    let mut imap = HashMap::<IpAddr, TimedCounter>::new();
    let mut curr_index = 0;

    loop {
        match udp_socket.recv_from(&mut buf).await {
            Ok((size, src)) => {
                debug!("Received {} bytes from {}", size, src);
                match RayPacket::decrypt(key, &buf[..size], filter.as_ref()).await {
                    Ok(packet) => match packet.ptype {
                        RayPacketType::DataPacket => {
                            imap.entry(src.ip()).or_insert_with(TimedCounter::new).inc();
                            process_packet(packet).await;
                        }
                        RayPacketType::StatRequest => {
                            let tc = imap.entry(src.ip()).or_insert_with(TimedCounter::new);
                            tc.inc();
                            let response = &stat_respond(
                                endpoint,
                                nodes.as_ref(),
                                tc.get() as f32,
                                &packet.data.data,
                            );
                            encrypt_and_send_udp(&udp_socket, src, response, key, &mut buf).await;
                        }
                        RayPacketType::StatResponse => {
                            let response: StatResponse =
                                bitcode::decode(&packet.data.data).unwrap();
                            if response.index >= curr_index {
                                stat_update(nodes.as_ref(), src, response.weight);
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
        let session = {
            let con = connections.read().await;
            if let Some(kcp) = con.get_from_conv(conv) {
                Some(kcp)
            } else {
                drop(con);
                connections
                    .write()
                    .await
                    .assign_from_conv(conv, &kcp_tx, &tcp_tx)
                    .await
            }
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
        let udp_socket_out = udp_socket_outs
            .iter()
            .choose(&mut rand::thread_rng())
            .unwrap();
        let remote = nodes.route(&mut rand::thread_rng());
        encrypt_and_send_udp(
            udp_socket_out,
            remote,
            &process_packet(packet),
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
