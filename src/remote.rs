use aegis::aegis128l::Key;
use futures::future::join_all;
use log::{debug, error, warn};
use rand::seq::IteratorRandom;
use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, RwLock};
use tokio::time::Duration;

use crate::connections::Connections;
use crate::packets::{KCPPacket, RayPacket, RayPacketError, RayPacketType, TCPPacket};
use crate::utils::{NonceFilter, TimedCounter};

async fn udp_in<F, Fut>(udp_socket: UdpSocket, key: &Key, mut process_packet: F)
where
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
                        RayPacketType::StatRequest => {}
                        RayPacketType::StatResponse => {}
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

pub async fn forward_in(udp_socket: UdpSocket, ray_tx: mpsc::Sender<RayPacket>, key: &Key) {
    let process_forward = |packet: RayPacket| async {
        if let Err(e) = ray_tx.send(packet).await {
            error!("Failed to send to channel: {}", e);
        }
    };

    udp_in(udp_socket, key, process_forward).await;
}

pub async fn endpoint_in(
    udp_socket: UdpSocket,
    kcp_tx: mpsc::Sender<KCPPacket>,
    connections: Arc<RwLock<Connections>>,
    tcp_tx: mpsc::Sender<TCPPacket>,
    key: &Key,
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

    udp_in(udp_socket, key, process_endpoint).await;
}

async fn udp_out<T, F>(
    mut rx: mpsc::Receiver<T>,
    send_addr: Vec<SocketAddr>,
    key: &Key,
    process_packet: F,
) where
    F: Fn(T, &Key, &mut Vec<u8>) -> usize,
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
    let mut omap = HashMap::<IpAddr, TimedCounter>::new();
    while let Some(packet) = rx.recv().await {
        let rsize = process_packet(packet, key, &mut buf);
        let remote = send_addr.iter().choose(&mut rand::thread_rng()).unwrap();
        let udp_socket_out = udp_socket_outs
            .iter()
            .choose(&mut rand::thread_rng())
            .unwrap();
        match udp_socket_out.send_to(&buf[..rsize], remote).await {
            Ok(sent_size) => {
                omap.entry(remote.ip())
                    .or_insert_with(TimedCounter::new)
                    .inc();
                debug!("Sent {} bytes to {}", sent_size, remote);
                if rsize != sent_size {
                    error!("Sent partial {} of {} bytes", sent_size, rsize);
                }
            }
            Err(e) => error!("Failed to send to UDP: {}", e),
        }
    }
}

pub async fn forward_out(ray_rx: mpsc::Receiver<RayPacket>, send_addr: Vec<SocketAddr>, key: &Key) {
    udp_out(ray_rx, send_addr, key, |packet, key, buf| {
        packet.encrypt(key, buf)
    })
    .await
}

pub async fn endpoint_out(
    kcp_rx: mpsc::Receiver<KCPPacket>,
    send_addr: Vec<SocketAddr>,
    key: &Key,
) {
    udp_out(kcp_rx, send_addr, key, |packet, key, buf| {
        let ray_packet = RayPacket::new(RayPacketType::DataPacket, packet);
        ray_packet.encrypt(key, buf)
    })
    .await
}
