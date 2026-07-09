use aegis::aegis128l::Key;
use log::info;
use std::io;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, mpsc};

use crate::channels::UdpChannels;
use crate::remote::{forward_in, forward_out};
use crate::transport::OutboundTransportPacket;
use raynet_core::{ChannelRouter, RelayConfig, RelayCore};

pub async fn run(
    udp_socket: Arc<UdpSocket>,
    channels: Arc<UdpChannels>,
    router: Arc<Mutex<ChannelRouter>>,
    key: Key,
) -> io::Result<()> {
    let (ray_tx, ray_rx) = mpsc::channel::<OutboundTransportPacket>(65536);
    let relay_core = Arc::new(Mutex::new(
        RelayCore::new(RelayConfig {
            node_id: 1,
            hop_key: key,
            destination_routes: Vec::new(),
            channels: channels.channel_configs(),
        })
        .expect("relay core config should be valid"),
    ));

    {
        let channels = channels.clone();
        let router = router.clone();
        let relay_core = relay_core.clone();
        tokio::spawn(async move {
            forward_in(udp_socket, ray_tx, channels, router, relay_core).await;
        });
    }

    tokio::spawn(async move {
        forward_out(ray_rx, channels, router).await;
    });

    info!("Started RayNet Relay");
    Ok(())
}
