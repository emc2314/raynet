use log::{debug, error};
use rand::seq::IteratorRandom;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, RwLock};

use crate::connections::{assign_kcp, Connections};
use crate::packets::{TCPPacket, UDPPacket};

pub async fn forward_in(udp_socket: UdpSocket, buf: &mut [u8], udp_tx: mpsc::Sender<UDPPacket>) {
    loop {
        match udp_socket.recv_from(buf).await {
            Ok((size, src)) => {
                debug!("Received {} bytes from {}", size, src);
                if let Err(e) = udp_tx
                    .send(UDPPacket {
                        data: buf[..size].to_vec(),
                    })
                    .await
                {
                    error!("Failed to send to channel: {}", e);
                }
            }
            Err(e) => error!("Failed to receive from UDP: {}", e),
        }
    }
}

pub async fn forward_out(
    udp_socket_outs: Vec<UdpSocket>,
    udp_rx: &mut mpsc::Receiver<UDPPacket>,
    send_addr: Vec<SocketAddr>,
) {
    while let Some(packet) = udp_rx.recv().await {
        let remote = send_addr.iter().choose(&mut rand::thread_rng()).unwrap();
        let udp_socket_out = udp_socket_outs
            .iter()
            .choose(&mut rand::thread_rng())
            .unwrap();
        match udp_socket_out.send_to(&packet.data, remote).await {
            Ok(sent_size) => debug!("Sent {} bytes to {}", sent_size, remote),
            Err(e) => error!("Failed to send to UDP: {}", e),
        }
    }
}

pub async fn endpoint_in(
    udp_socket: UdpSocket,
    buf: &mut [u8],
    udp_tx: mpsc::Sender<UDPPacket>,
    connections: Arc<RwLock<Connections>>,
    tcp_tx: mpsc::Sender<TCPPacket>,
) {
    loop {
        match udp_socket.recv_from(buf).await {
            Ok((size, src)) => {
                debug!("Received {} bytes from UDP {}", size, src);
                let conv = kcp::get_conv(buf);
                let session = {
                    let con = connections.read().await;
                    if con.kcps.contains_key(&conv) {
                        Some(con.kcps[&conv].clone())
                    } else {
                        drop(con);
                        assign_kcp(&connections, conv, &udp_tx, &tcp_tx, false).await
                    }
                };
                if let Some(session) = session {
                    if let Err(_) = session.input(&buf[..size]).await {
                        error!("KCP session {} closed when input", conv);
                        session.close();
                        let mut con = connections.write().await;
                        if let Some(conv) = con.convs.remove(&session.kcp_recv().addr) {
                            con.kcps.remove(&conv);
                        }
                    }
                } else {
                    error!("No spare connection");
                }
            }
            Err(e) => error!("Failed to receive from UDP: {}", e),
        }
    }
}

pub async fn endpoint_out(
    udp_socket_outs: Vec<UdpSocket>,
    udp_rx: &mut mpsc::Receiver<UDPPacket>,
    send_addr: Vec<SocketAddr>,
) {
    while let Some(packet) = udp_rx.recv().await {
        let mut cur = 0;
        while packet.data.len() > cur {
            let remote = send_addr.iter().choose(&mut rand::thread_rng()).unwrap();
            let udp_socket_out = udp_socket_outs
                .iter()
                .choose(&mut rand::thread_rng())
                .unwrap();
            let size = udp_socket_out
                .send_to(
                    &packet.data[cur..std::cmp::min(packet.data.len(), cur + 1200)],
                    remote,
                )
                .await
                .expect("Failed to send to UDP: {}");
            debug!("Sent {} bytes to UDP {}", size, remote);
            cur += size;
        }
    }
}
