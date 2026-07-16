use aegis::aegis128l::Key;
use log::info;
use rand::RngExt;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

use crate::channels::{ChannelSenders, InboundTransportPacket};
use crate::remote::{forward_in, forward_out};
use crate::transport::OutboundTransportPacket;
use crate::utils::CoreClock;
use raynet_core::{RelayConfig, RelayCore};

pub fn run(
    channels: Arc<ChannelSenders>,
    channel_receiver: mpsc::Receiver<InboundTransportPacket>,
    envelope_key: Key,
) {
    let (ray_tx, ray_rx) = mpsc::channel::<OutboundTransportPacket>(65536);
    let clock = Arc::new(CoreClock::new());
    let random_seed = rand::rng().random();
    let relay_core = Arc::new(Mutex::new(RelayCore::new(RelayConfig {
        envelope_key,
        random_seed,
        boot_time_ms: clock.boot_time_ms(),
        local_channels: channels.channel_ids(),
        local_min_mtu: 1200,
    })));

    {
        let relay_core = relay_core.clone();
        let clock = clock.clone();
        let ray_tx = ray_tx.clone();
        tokio::spawn(async move {
            forward_in(channel_receiver, ray_tx, relay_core, clock).await;
        });
    }

    tokio::spawn(async move {
        forward_out(ray_rx, channels, move |packet| {
            let relay_core = relay_core.clone();
            let clock = clock.clone();
            async move {
                let mut actions = Vec::new();
                let _ = relay_core.lock().await.handle_event(
                    clock.elapsed_ms(),
                    raynet_core::CoreEvent::TransportPacketSendFailed {
                        channel_id: packet.channel_id,
                        bytes: packet.bytes,
                    },
                    &mut actions,
                );
            }
        })
        .await;
    });

    info!("Started RayNet Relay");
}
