use aegis::aegis128l::Key;
use futures::future::join_all;
use log::{debug, error, warn};
use rand::seq::IteratorRandom;
use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, RwLock};
use tokio::time::Duration;

use crate::connections::Connections;
use crate::packets::{KCPPacket, RayPacket, RayPacketError, RayPacketType, TCPPacket};
use crate::routing::{Nodes, TimedCounter, WeightInfo};
use crate::utils::NonceFilter;

async fn udp_send(
    socket: &UdpSocket,
    remote: SocketAddr,
    packet: RayPacket,
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
    let factor = 0.8;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        for node in nodes.nodes.iter() {
            node.tc.inc();
            let packet = RayPacket::new(
                RayPacketType::StatRequest,
                KCPPacket {
                    data: (node.tc.get() as f32).to_le_bytes().to_vec(),
                },
            );
            udp_send(socket.as_ref(), node.addr, packet, key, &mut buf).await;
            let weight = node.weight.load(Ordering::Relaxed);
            node.weight.store(weight * factor, Ordering::Relaxed);
        }
    }
}

pub async fn stat_update(nodes: Arc<Nodes>, mut stat_rx: mpsc::Receiver<WeightInfo>) {
    loop {
        if let Some(info) = stat_rx.recv().await {
            let mut flag = false;
            let ip = info.addr.ip().to_canonical();
            for node in nodes.nodes.iter() {
                if node.addr.ip() == ip {
                    node.weight.store(info.weight, Ordering::Relaxed);
                    flag = true;
                    break;
                }
            }
            if !flag {
                error!("Received weight from unknown node {}", info.addr);
            }
            debug!("NodeInfo: {:?}", nodes.nodes);
        }
    }
}

async fn udp_in<F, Fut>(
    udp_socket: Arc<UdpSocket>,
    key: &Key,
    stat_tx: mpsc::Sender<WeightInfo>,
    mut process_packet: F,
) where
    F: FnMut(RayPacket) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut buf = vec![0u8; 65535];
    let filter = NonceFilter::new(1 << 24, 0.00001, Duration::from_millis(1 << 16));
    let mut imap = HashMap::<IpAddr, TimedCounter>::new();

    loop {
        match udp_socket.recv_from(&mut buf).await {
            Ok((size, src)) => {
                debug!("Received {} bytes from {}", size, src);
                match RayPacket::decrypt(key, &mut buf[..size], filter.clone()).await {
                    Ok(packet) => match packet.ptype {
                        RayPacketType::DataPacket => {
                            imap.entry(src.ip()).or_insert_with(TimedCounter::new).inc();
                            process_packet(packet).await;
                        }
                        RayPacketType::StatRequest => {
                            let tc = imap.entry(src.ip()).or_insert_with(TimedCounter::new);
                            tc.inc();
                            let weight = (tc.get() as f32
                                / (f32::from_le_bytes(packet.kcp.data[0..4].try_into().unwrap())
                                    + 10.0))
                                * 10.0;
                            let packet = RayPacket::new(
                                RayPacketType::StatResponse,
                                KCPPacket {
                                    data: weight.to_le_bytes().to_vec(),
                                },
                            );
                            udp_send(&udp_socket, src, packet, key, &mut buf).await;
                        }
                        RayPacketType::StatResponse => {
                            let weight =
                                f32::from_le_bytes(packet.kcp.data[0..4].try_into().unwrap());
                            if let Err(e) = stat_tx.send(WeightInfo { addr: src, weight }).await {
                                error!("Failed to send to channel: {}", e);
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
    stat_tx: mpsc::Sender<WeightInfo>,
) {
    let process_forward = |packet: RayPacket| async {
        if let Err(e) = ray_tx.send(packet).await {
            error!("Failed to send to channel: {}", e);
        }
    };

    udp_in(udp_socket, key, stat_tx, process_forward).await;
}

pub async fn endpoint_in(
    udp_socket: Arc<UdpSocket>,
    kcp_tx: mpsc::Sender<KCPPacket>,
    connections: Arc<RwLock<Connections>>,
    tcp_tx: mpsc::Sender<TCPPacket>,
    key: &Key,
    stat_tx: mpsc::Sender<WeightInfo>,
) {
    let process_endpoint = |packet: RayPacket| async {
        let packet = packet.kcp;
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
            if let Err(_) = session.input(&packet.data).await {
                error!("KCP session {} closed when input", conv);
                session.close();
                connections.write().await.close(&session.kcp_recv().addr);
            }
        } else {
            error!("No spare connection");
        }
    };

    udp_in(udp_socket, key, stat_tx, process_endpoint).await;
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
        let remote = nodes.route(&mut rand::thread_rng());
        let udp_socket_out = udp_socket_outs
            .iter()
            .choose(&mut rand::thread_rng())
            .unwrap();
        udp_send(
            &udp_socket_out,
            remote,
            process_packet(packet),
            key,
            &mut buf,
        )
        .await;
    }
}

pub async fn forward_out(ray_rx: mpsc::Receiver<RayPacket>, nodes: Arc<Nodes>, key: &Key) {
    udp_out(ray_rx, nodes, key, |packet| packet).await
}

pub async fn endpoint_out(kcp_rx: mpsc::Receiver<KCPPacket>, nodes: Arc<Nodes>, key: &Key) {
    udp_out(kcp_rx, nodes, key, |packet| {
        RayPacket::new(RayPacketType::DataPacket, packet)
    })
    .await
}
