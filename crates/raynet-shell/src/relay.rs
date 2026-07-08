use aegis::aegis128l::Key;
use log::info;
use std::io;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::remote::{forward_in, forward_out};
use raynet_core::RayPacket;
use raynet_core::routing::Nodes;

pub async fn run(udp_socket: Arc<UdpSocket>, nodes: Arc<Nodes>, key: Key) -> io::Result<()> {
    let (ray_tx, ray_rx) = mpsc::channel::<RayPacket>(65536);

    {
        let nodes = nodes.clone();
        tokio::spawn(async move {
            forward_in(udp_socket, ray_tx, &key, nodes).await;
        });
    }

    tokio::spawn(async move {
        forward_out(ray_rx, nodes, &key).await;
    });

    info!("Started RayNet Relay");
    Ok(())
}
