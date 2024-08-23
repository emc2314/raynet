use aegis::aegis128l::Key;
use log::{debug, error, warn};
use rand::seq::IteratorRandom;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, RwLock};
use tokio::time::Duration;

use crate::connections::Connections;
use crate::packets::{KCPPacket, RayPacket, RayPacketError, TCPPacket};
use crate::utils::NonceFilter;

pub async fn forward_in(udp_socket: UdpSocket, ray_tx: mpsc::Sender<RayPacket>, key: &Key) {
    let mut buf = vec![0u8; 65535];
    let filter = NonceFilter::new(1 << 24, 0.00001, Duration::from_millis(1 << 16));
    loop {
        match udp_socket.recv_from(&mut buf).await {
            Ok((size, src)) => {
                debug!("Received {} bytes from {}", size, src);
                match RayPacket::decrypt(key, &mut buf[..size], filter.clone()).await {
                    Ok(p) => {
                        if let Err(e) = ray_tx.send(p).await {
                            error!("Failed to send to channel: {}", e);
                        }
                    }
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

pub async fn forward_out(
    udp_socket_outs: Vec<UdpSocket>,
    ray_rx: &mut mpsc::Receiver<RayPacket>,
    send_addr: Vec<SocketAddr>,
    key: &Key,
) {
    let mut buf = vec![0u8; 65535];
    while let Some(packet) = ray_rx.recv().await {
        let rsize = packet.encrypt(key, &mut buf);
        let remote = send_addr.iter().choose(&mut rand::thread_rng()).unwrap();
        let udp_socket_out = udp_socket_outs
            .iter()
            .choose(&mut rand::thread_rng())
            .unwrap();
        match udp_socket_out.send_to(&buf[..rsize], remote).await {
            Ok(sent_size) => debug!("Sent {} bytes to {}", sent_size, remote),
            Err(e) => error!("Failed to send to UDP: {}", e),
        }
    }
}

pub async fn endpoint_in(
    udp_socket: UdpSocket,
    kcp_tx: mpsc::Sender<KCPPacket>,
    connections: Arc<RwLock<Connections>>,
    tcp_tx: mpsc::Sender<TCPPacket>,
    key: &Key,
) {
    let mut buf = vec![0u8; 65535];
    let filter = NonceFilter::new(1 << 24, 0.00001, Duration::from_millis(1 << 16));
    loop {
        match udp_socket.recv_from(&mut buf).await {
            Ok((size, src)) => {
                debug!("Received {} bytes from UDP {}", size, src);
                match RayPacket::decrypt(key, &mut buf[..size], filter.clone()).await {
                    Ok(packet) => {
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
                    }
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

pub async fn endpoint_out(
    udp_socket_outs: Vec<UdpSocket>,
    kcp_rx: &mut mpsc::Receiver<KCPPacket>,
    send_addr: Vec<SocketAddr>,
    key: &Key,
) {
    let mut buf = vec![0u8; 65535];
    while let Some(packet) = kcp_rx.recv().await {
        let packet = RayPacket::new(0, 0, 0, packet);
        let rsize = packet.encrypt(key, &mut buf);
        let remote = send_addr.iter().choose(&mut rand::thread_rng()).unwrap();
        let udp_socket_out = udp_socket_outs
            .iter()
            .choose(&mut rand::thread_rng())
            .unwrap();
        let size = udp_socket_out
            .send_to(&buf[..rsize], remote)
            .await
            .expect("Failed to send to UDP: {}");
        debug!("Sent {} bytes to UDP {}", size, remote);
    }
}
