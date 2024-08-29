use log::{debug, error, info};
use std::io;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{sleep, Duration};

use crate::connections::Connections;
use crate::packets::{KCPPacket, TCPPacket};

pub async fn endpoint_from(
    tcp_listener: TcpListener,
    kcp_tx: mpsc::Sender<KCPPacket>,
    connections: Arc<RwLock<Connections>>,
    tcp_tx: mpsc::Sender<TCPPacket>,
) {
    loop {
        match tcp_listener.accept().await {
            Ok((tcp_stream, src)) => {
                let (tcp_read, tcp_write) = tcp_stream.into_split();
                info!("New TCP connection: {}", src);
                {
                    connections.write().await.insert(src, tcp_write);
                }

                let connections = connections.clone();
                let kcp_tx = kcp_tx.clone();
                let tcp_tx = tcp_tx.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65535];
                    loop {
                        match tcp_read.try_read(&mut buf) {
                            Ok(0) => {
                                info!("TCP connection closed: {}", src);
                                connections.write().await.close(&src);
                                break;
                            }
                            Ok(len) => {
                                debug!("Received {} bytes from TCP {}", len, src);
                                let session = {
                                    let con = connections.read().await;
                                    if let Some(kcp) = con.get_from_addr(&src) {
                                        kcp
                                    } else {
                                        drop(con);
                                        connections
                                            .write()
                                            .await
                                            .assign_from_addr(&src, &kcp_tx, &tcp_tx)
                                            .await
                                    }
                                };
                                if let Err(e) = session.send(&buf[..len]).await {
                                    error!("KCP session {} error when send: {}", session.conv(), e);
                                    connections.write().await.close(&src);
                                }
                            }
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                sleep(Duration::from_millis(1)).await;
                            }
                            Err(e) => {
                                error!("Failed to read from TCP stream: {}", e);
                                connections.write().await.close(&src);
                                break;
                            }
                        }
                    }
                    connections.write().await.remove(&src);
                });
            }
            Err(e) => error!("Failed to accept TCP connection: {}", e),
        }
    }
}

pub async fn endpoint_to(
    connections: Arc<RwLock<Connections>>,
    mut tcp_rx: mpsc::Receiver<TCPPacket>,
) {
    while let Some(packet) = tcp_rx.recv().await {
        let addr = packet.addr;
        if let Some(tcp_write) = connections.read().await.get(&addr) {
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
                        con.close(&addr);
                        con.remove(&addr);
                        break;
                    }
                }
            }
        } else {
            error!("No spare connection");
        }
    }
}
