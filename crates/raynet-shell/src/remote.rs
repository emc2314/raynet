use futures::future::join_all;
use log::{debug, error, warn};
use rand::seq::IteratorRandom;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::time::{Duration, sleep};

use crate::channels::UdpChannels;
use crate::connections::Connections;
use crate::transport::OutboundTransportPacket;
use crate::utils::now_millis;
use raynet_core::core::{CoreAction, CoreEvent, EndpointCore};
use raynet_core::{ChannelId, ChannelRouter, ChannelState, RelayCore, TransportMetrics};

async fn send_udp(socket: &UdpSocket, remote: SocketAddr, bytes: &[u8], buf: &mut [u8]) -> bool {
    if bytes.len() > buf.len() {
        error!("UDP packet is too large: {}", bytes.len());
        return false;
    }
    buf[..bytes.len()].copy_from_slice(bytes);
    match socket.send_to(&buf[..bytes.len()], remote).await {
        Ok(sent_size) => {
            debug!("Sent {} bytes to {}", sent_size, remote);
            if bytes.len() != sent_size {
                error!("Sent partial {} of {} bytes", sent_size, bytes.len());
                return false;
            }
            true
        }
        Err(e) => {
            error!("UDP Failed to send to {}: {}", remote, e);
            false
        }
    }
}

async fn udp_in<F, Fut>(
    udp_socket: Arc<UdpSocket>,
    channels: Arc<UdpChannels>,
    router: Arc<Mutex<ChannelRouter>>,
    mut process_packet: F,
) where
    F: FnMut(ChannelId, Vec<u8>) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut buf = vec![0u8; 65535];

    loop {
        match udp_socket.recv_from(&mut buf).await {
            Ok((size, src)) => {
                debug!("Received {} bytes from {}", size, src);
                let channel_id = channels.channel_id_for_addr(src).unwrap_or(0);
                if channel_id != 0 {
                    router.lock().await.record_receive(channel_id);
                }
                process_packet(channel_id, buf[..size].to_vec()).await;
            }
            Err(e) => error!("Failed to receive from UDP: {}", e),
        }
    }
}

pub async fn forward_in(
    udp_socket: Arc<UdpSocket>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    channels: Arc<UdpChannels>,
    router: Arc<Mutex<ChannelRouter>>,
    relay_core: Arc<Mutex<RelayCore>>,
) {
    let process_forward = |channel_id: ChannelId, bytes: Vec<u8>| {
        let ray_tx = ray_tx.clone();
        let relay_core = relay_core.clone();
        async move {
            let mut actions = Vec::new();
            let event = CoreEvent::TransportPacketReceived { channel_id, bytes };

            let result = relay_core
                .lock()
                .await
                .handle_event(now_millis(), event, &mut actions);

            match result {
                Ok(()) => {
                    for action in actions {
                        if let CoreAction::SendTransportPacket { channel_id, bytes } = action {
                            if let Err(e) = ray_tx
                                .send(OutboundTransportPacket {
                                    channel_id: Some(channel_id),
                                    bytes,
                                })
                                .await
                            {
                                error!("Failed to send core action to channel: {}", e);
                            }
                        }
                    }
                }
                Err(error) => warn!("RelayCore rejected transport packet: {}", error),
            }
        }
    };

    udp_in(udp_socket, channels, router, process_forward).await;
}

pub async fn endpoint_in(
    udp_socket: Arc<UdpSocket>,
    connections: Arc<RwLock<Connections>>,
    channels: Arc<UdpChannels>,
    router: Arc<Mutex<ChannelRouter>>,
    endpoint_core: Arc<Mutex<EndpointCore>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
) {
    let process_endpoint = |channel_id: ChannelId, bytes: Vec<u8>| {
        let connections = connections.clone();
        let endpoint_core = endpoint_core.clone();
        let ray_tx = ray_tx.clone();
        async move {
            let handled = handle_endpoint_envelope(
                connections.clone(),
                endpoint_core,
                ray_tx,
                channel_id,
                bytes,
            )
            .await;
            if !handled {
                warn!("Dropping non-envelope endpoint packet");
            }
        }
    };

    udp_in(udp_socket, channels, router, process_endpoint).await;
}

async fn handle_endpoint_envelope(
    connections: Arc<RwLock<Connections>>,
    endpoint_core: Arc<Mutex<EndpointCore>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    channel_id: ChannelId,
    bytes: Vec<u8>,
) -> bool {
    let mut actions = Vec::new();
    let event = CoreEvent::TransportPacketReceived { channel_id, bytes };
    let result = endpoint_core
        .lock()
        .await
        .handle_event(now_millis(), event, &mut actions);
    match result {
        Ok(()) => {
            handle_endpoint_core_actions(connections, endpoint_core, ray_tx, actions).await;
            true
        }
        Err(error) => {
            warn!("EndpointCore rejected transport packet: {}", error);
            false
        }
    }
}

async fn handle_endpoint_core_actions(
    connections: Arc<RwLock<Connections>>,
    endpoint_core: Arc<Mutex<EndpointCore>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    actions: Vec<CoreAction>,
) {
    for action in actions {
        match action {
            CoreAction::OpenExitConnection {
                stream_id, target, ..
            } => {
                open_exit_connection(
                    connections.clone(),
                    endpoint_core.clone(),
                    ray_tx.clone(),
                    stream_id,
                    target,
                )
                .await;
            }
            CoreAction::WriteLocalConnection {
                local_connection_id,
                bytes,
            } => {
                write_local_connection(connections.clone(), local_connection_id, bytes).await;
            }
            CoreAction::CloseLocalConnection {
                local_connection_id,
                ..
            } => {
                connections
                    .write()
                    .await
                    .remove_by_local_connection_id(local_connection_id);
            }
            CoreAction::SendTransportPacket { channel_id, bytes } => {
                send_transport_action(ray_tx.clone(), channel_id, bytes).await;
            }
            CoreAction::CloseRemoteStream { .. }
            | CoreAction::EmitMetric(_)
            | CoreAction::EmitLog { .. } => {}
        }
    }
}

async fn open_exit_connection(
    connections: Arc<RwLock<Connections>>,
    endpoint_core: Arc<Mutex<EndpointCore>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    stream_id: u64,
    target: raynet_core::Target,
) {
    let target_addr = format!("{}:{}", target.host, target.port);
    let stream = match TcpStream::connect(&target_addr).await {
        Ok(stream) => stream,
        Err(error) => {
            error!(
                "Failed to open exit connection to {}: {}",
                target_addr, error
            );
            return;
        }
    };
    let addr = match stream.peer_addr() {
        Ok(addr) => addr,
        Err(error) => {
            error!("Failed to read peer address for {}: {}", target_addr, error);
            return;
        }
    };
    let (read, write) = stream.into_split();
    let local_connection_id = connections.write().await.insert(addr, write);

    emit_endpoint_core_event(
        endpoint_core.clone(),
        ray_tx.clone(),
        CoreEvent::ExitConnectionOpened {
            stream_id,
            local_connection_id,
        },
    )
    .await;

    tokio::spawn(read_exit_connection(
        read,
        local_connection_id,
        endpoint_core,
        ray_tx,
    ));
}

async fn read_exit_connection(
    read: tokio::net::tcp::OwnedReadHalf,
    local_connection_id: u64,
    endpoint_core: Arc<Mutex<EndpointCore>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
) {
    let mut buf = vec![0u8; 65535];
    loop {
        match read.try_read(&mut buf) {
            Ok(0) => {
                emit_endpoint_core_event(
                    endpoint_core,
                    ray_tx,
                    CoreEvent::LocalConnectionClosed {
                        local_connection_id,
                        reason: raynet_core::CloseReason::RemoteClosed,
                    },
                )
                .await;
                break;
            }
            Ok(len) => {
                emit_endpoint_core_event(
                    endpoint_core.clone(),
                    ray_tx.clone(),
                    CoreEvent::LocalConnectionBytes {
                        local_connection_id,
                        bytes: buf[..len].to_vec(),
                    },
                )
                .await;
            }
            Err(ref error) if error.kind() == io::ErrorKind::WouldBlock => {
                sleep(Duration::from_millis(1)).await;
            }
            Err(error) => {
                emit_endpoint_core_event(
                    endpoint_core,
                    ray_tx,
                    CoreEvent::LocalConnectionClosed {
                        local_connection_id,
                        reason: raynet_core::CloseReason::Error(error.to_string()),
                    },
                )
                .await;
                break;
            }
        }
    }
}

async fn write_local_connection(
    connections: Arc<RwLock<Connections>>,
    local_connection_id: u64,
    bytes: Vec<u8>,
) {
    let connections = connections.read().await;
    let Some(write) = connections.get_by_local_connection_id(local_connection_id) else {
        warn!(
            "No local connection {} for write action",
            local_connection_id
        );
        return;
    };
    let mut remaining = bytes.as_slice();
    while !remaining.is_empty() {
        match write.try_write(remaining) {
            Ok(n) => remaining = &remaining[n..],
            Err(ref error) if error.kind() == io::ErrorKind::WouldBlock => {
                sleep(Duration::from_millis(1)).await;
            }
            Err(error) => {
                error!(
                    "Failed to write to local connection {}: {}",
                    local_connection_id, error
                );
                break;
            }
        }
    }
}

async fn emit_endpoint_core_event(
    endpoint_core: Arc<Mutex<EndpointCore>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    event: CoreEvent,
) {
    let mut actions = Vec::new();
    match endpoint_core
        .lock()
        .await
        .handle_event(now_millis(), event, &mut actions)
    {
        Ok(()) => {
            for action in actions {
                if let CoreAction::SendTransportPacket { channel_id, bytes } = action {
                    send_transport_action(ray_tx.clone(), channel_id, bytes).await;
                }
            }
        }
        Err(error) => error!("EndpointCore rejected exit event: {}", error),
    }
}

async fn send_transport_action(
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    channel_id: ChannelId,
    bytes: Vec<u8>,
) {
    if let Err(error) = ray_tx
        .send(OutboundTransportPacket {
            channel_id: Some(channel_id),
            bytes,
        })
        .await
    {
        error!(
            "Failed to enqueue endpoint core transport packet: {}",
            error
        );
    }
}

async fn udp_out<T, F>(
    mut rx: mpsc::Receiver<T>,
    channels: Arc<UdpChannels>,
    router: Arc<Mutex<ChannelRouter>>,
    process_packet: F,
) where
    F: Fn(T) -> (Option<ChannelId>, Vec<u8>),
{
    let udp_socket_outs: Vec<UdpSocket> = join_all(
        (0..32)
            .map(|_| UdpSocket::bind("[::0]:0"))
            .collect::<Vec<_>>(),
    )
    .await
    .into_iter()
    .collect::<Result<_, _>>()
    .unwrap();
    let mut buf = vec![0u8; 65535];
    while let Some(packet) = rx.recv().await {
        let udp_socket_index = {
            let mut rng = rand::rng();
            (0..udp_socket_outs.len()).choose(&mut rng).unwrap()
        };
        let (requested_channel_id, packet) = process_packet(packet);
        let channel_id = if let Some(channel_id) = requested_channel_id {
            channel_id
        } else {
            let Some(channel_id) = router.lock().await.select_channel() else {
                warn!("No available channel for outgoing packet");
                continue;
            };
            channel_id
        };
        let Some(remote) = channels.addr(channel_id) else {
            warn!("Selected channel {} has no UDP address", channel_id);
            continue;
        };

        let sent = send_udp(
            &udp_socket_outs[udp_socket_index],
            remote,
            &packet,
            &mut buf,
        )
        .await;
        let mut router = router.lock().await;
        router.record_send(channel_id);
        if !sent {
            router.update_channel(
                channel_id,
                ChannelState::Degraded,
                TransportMetrics {
                    queue_pressure: 0.0,
                    send_error: true,
                },
            );
        }
    }
}

pub async fn forward_out(
    ray_rx: mpsc::Receiver<OutboundTransportPacket>,
    channels: Arc<UdpChannels>,
    router: Arc<Mutex<ChannelRouter>>,
) {
    udp_out(ray_rx, channels, router, |packet| {
        (packet.channel_id, packet.bytes)
    })
    .await
}
