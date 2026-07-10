use aegis::aegis128l::Key;
use log::info;
use rand::RngExt;
use std::io;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, mpsc};

use crate::channels::UdpChannels;
use crate::remote::{forward_in, forward_out};
use crate::transport::OutboundTransportPacket;
use raynet_core::{RelayConfig, RelayCore};

pub async fn run(
    udp_socket: Arc<UdpSocket>,
    channels: Arc<UdpChannels>,
    key: Key,
) -> io::Result<()> {
    let (ray_tx, ray_rx) = mpsc::channel::<OutboundTransportPacket>(65536);
    let random_seed = rand::rng().random();
    let relay_core = Arc::new(Mutex::new(
        RelayCore::new(RelayConfig {
            local_node_id: 1,
            envelope_key: key,
            random_seed,
            local_channels: channels.channel_ids().collect(),
        })
        .expect("relay core config should be valid"),
    ));

    {
        let channels = channels.clone();
        let relay_core = relay_core.clone();
        tokio::spawn(async move {
            forward_in(udp_socket, ray_tx, channels, relay_core).await;
        });
    }

    tokio::spawn(async move {
        forward_out(ray_rx, channels).await;
    });

    info!("Started RayNet Relay");
    Ok(())
}
