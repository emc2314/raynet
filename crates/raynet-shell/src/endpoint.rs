use aegis::aegis128l::Key;
use log::{error, info};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::time::{Duration, sleep};

use crate::channels::UdpChannels;
use crate::connections::Connections;
use crate::local::{endpoint_from, send_core_actions};
use crate::remote::{endpoint_in, forward_out};
use crate::transport::OutboundTransportPacket;
use crate::utils::now_millis;
use raynet_core::{ChannelRouter, EndpointConfig, EndpointCore};

pub async fn run(
    listen_addr: SocketAddr,
    udp_socket: Arc<UdpSocket>,
    channels: Arc<UdpChannels>,
    router: Arc<Mutex<ChannelRouter>>,
    key: Key,
) -> io::Result<Arc<RwLock<Connections>>> {
    let connections = Arc::new(RwLock::new(Connections::new()));
    let (ray_tx, ray_rx) = mpsc::channel::<OutboundTransportPacket>(65536);
    let endpoint_core = Arc::new(Mutex::new(
        EndpointCore::new(EndpointConfig {
            node_id: 1,
            endpoint_id: 1,
            hop_key: key,
            endpoint_key: key,
            default_destination_node_id: 2,
            default_channel_plan: Vec::new(),
            destination_routes: Vec::new(),
            channels: channels.channel_configs(),
        })
        .expect("endpoint core config should be valid"),
    ));

    {
        let connections = connections.clone();
        let channels = channels.clone();
        let router = router.clone();
        let endpoint_core = endpoint_core.clone();
        let ray_tx = ray_tx.clone();
        tokio::spawn(async move {
            endpoint_in(
                udp_socket,
                connections,
                channels,
                router,
                endpoint_core,
                ray_tx,
            )
            .await;
        });
    }

    {
        let tcp_listener = TcpListener::bind(listen_addr).await?;
        let connections = connections.clone();
        let endpoint_core = endpoint_core.clone();
        let ray_tx = ray_tx.clone();
        tokio::spawn(async move {
            endpoint_from(tcp_listener, connections, endpoint_core, ray_tx).await;
        });
    }

    {
        let endpoint_core = endpoint_core.clone();
        let ray_tx = ray_tx.clone();
        tokio::spawn(async move {
            poll_endpoint_core(endpoint_core, ray_tx).await;
        });
    }

    {
        let channels = channels.clone();
        let router = router.clone();
        let core_channels = channels.clone();
        let core_router = router.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                tokio::spawn(async move {
                    forward_out(ray_rx, core_channels, core_router).await;
                });
                let _ = tokio::signal::ctrl_c().await;
            });
        });
    }

    info!("Started RayNet Endpoint");
    Ok(connections)
}

async fn poll_endpoint_core(
    endpoint_core: Arc<Mutex<EndpointCore>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
) {
    loop {
        let now = now_millis();
        let deadline = endpoint_core.lock().await.next_deadline(now);
        let Some(deadline) = deadline else {
            sleep(Duration::from_millis(20)).await;
            continue;
        };

        if deadline > now {
            sleep(Duration::from_millis((deadline - now).min(100))).await;
            continue;
        }

        let mut actions = Vec::new();
        let result = { endpoint_core.lock().await.poll(now_millis(), &mut actions) };
        match result {
            Ok(()) => send_core_actions(ray_tx.clone(), actions).await,
            Err(error) => error!("EndpointCore poll failed: {}", error),
        }
    }
}
