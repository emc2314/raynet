use aegis::aegis128l::Key;
use log::info;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{RwLock, mpsc};

use crate::connections::Connections;
use crate::local::{endpoint_from, endpoint_to};
use crate::remote::{endpoint_in, endpoint_out};
use raynet_core::routing::Nodes;
use raynet_core::{DataPacket, TCPPacket};

pub async fn run(
    listen_addr: SocketAddr,
    udp_socket: Arc<UdpSocket>,
    nodes: Arc<Nodes>,
    key: Key,
) -> io::Result<Arc<RwLock<Connections>>> {
    let connections = Arc::new(RwLock::new(Connections::new()));
    let (kcp_tx, kcp_rx) = mpsc::channel::<DataPacket>(65536);
    let (tcp_tx, tcp_rx) = mpsc::channel::<TCPPacket>(65536);

    {
        let connections = connections.clone();
        let kcp_tx = kcp_tx.clone();
        let tcp_tx = tcp_tx.clone();
        let nodes = nodes.clone();
        tokio::spawn(async move {
            endpoint_in(udp_socket, kcp_tx, connections, tcp_tx, &key, nodes).await;
        });
    }

    {
        let tcp_listener = TcpListener::bind(listen_addr).await?;
        let connections = connections.clone();
        tokio::spawn(async move {
            endpoint_from(tcp_listener, kcp_tx, connections, tcp_tx).await;
        });
    }

    {
        let connections = connections.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                tokio::spawn(async move {
                    endpoint_to(connections, tcp_rx).await;
                });
                tokio::spawn(async move {
                    endpoint_out(kcp_rx, nodes, &key).await;
                });
                let _ = tokio::signal::ctrl_c().await;
            });
        });
    }

    info!("Started RayNet Endpoint");
    Ok(connections)
}
