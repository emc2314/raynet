use log::{debug, error, info};
use rand::Rng;
use std::io;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{sleep, Duration};

use crate::connections::{assign_kcp, Connections};
use crate::packets::{TCPPacket, UDPPacket};

pub async fn endpoint_from(
    tcp_listener: TcpListener,
    udp_tx: mpsc::Sender<UDPPacket>,
    connections: Arc<RwLock<Connections>>,
    tcp_tx: mpsc::Sender<TCPPacket>,
) {
    loop {
        match tcp_listener.accept().await {
            Ok((tcp_stream, src)) => {
                let (tcp_read, tcp_write) = tcp_stream.into_split();
                info!("New TCP connection: {}", src);
                {
                    connections.write().await.cons.insert(src, tcp_write);
                }

                let connections = connections.clone();
                let udp_tx = udp_tx.clone();
                let tcp_tx = tcp_tx.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65535];
                    loop {
                        match tcp_read.try_read(&mut buf) {
                            Ok(0) => {
                                info!("TCP connection closed: {}", src);
                                let mut con = connections.write().await;
                                if let Some(conv) = con.convs.remove(&src) {
                                    con.kcps.remove(&conv);
                                }
                                // TODO: Inform the opposite end
                                break;
                            }
                            Ok(len) => {
                                debug!("Received {} bytes from TCP {}", len, src);
                                let session = {
                                    let con = connections.read().await;
                                    if let Some(conv) = con.convs.get(&src) {
                                        Some(con.kcps[conv].clone())
                                    } else {
                                        drop(con);
                                        let conv = rand::thread_rng().gen::<u32>();
                                        assign_kcp(&connections, conv, &udp_tx, &tcp_tx, true).await
                                    }
                                };
                                if let Some(session) = session {
                                    if let Err(e) = session.send(&buf[..len]).await {
                                        error!(
                                            "KCP session {} error when send: {}",
                                            session.conv(),
                                            e
                                        );
                                        let mut con = connections.write().await;
                                        if let Some(conv) = con.convs.remove(&src) {
                                            con.kcps.remove(&conv);
                                        }
                                    }
                                } else {
                                    error!("No spare connection");
                                }
                            }
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                sleep(Duration::from_millis(1)).await;
                            }
                            Err(e) => {
                                error!("Failed to read from TCP stream: {}", e);
                                let mut con = connections.write().await;
                                if let Some(conv) = con.convs.remove(&src) {
                                    con.kcps.remove(&conv);
                                }
                                // TODO: Inform the opposite end
                                break;
                            }
                        }
                    }
                    connections.write().await.cons.remove(&src);
                });
            }
            Err(e) => error!("Failed to accept TCP connection: {}", e),
        }
    }
}

pub async fn endpoint_to(
    connections: Arc<RwLock<Connections>>,
    tcp_rx: &mut mpsc::Receiver<TCPPacket>,
) {
    while let Some(packet) = tcp_rx.recv().await {
        let addr = packet.addr;
        if let Some(tcp_write) = connections.read().await.cons.get(&addr) {
            let mut data = packet.data.as_slice();
            while !data.is_empty() {
                match tcp_write.try_write(data) {
                    Ok(n) => {
                        debug!("Forwarded {} bytes to TCP stream {}", n, addr);
                        data = &data[n..];
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        sleep(Duration::from_millis(1)).await;
                    }
                    Err(e) => {
                        error!("Failed to write to TCP stream {}: {}", addr, e);
                        let mut con = connections.write().await;
                        if let Some(conv) = con.convs.remove(&addr) {
                            con.kcps.remove(&conv);
                        }
                        con.cons.remove(&addr);
                        break;
                    }
                }
            }
        } else {
            error!("No spare connection");
        }
    }
}
