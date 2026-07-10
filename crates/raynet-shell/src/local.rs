use log::{debug, error, info, warn};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::time::{Duration, sleep};

use crate::connections::Connections;
use crate::transport::OutboundTransportPacket;
use crate::utils::now_millis;
use raynet_core::core::{CloseReason, CoreAction, CoreEvent, EndpointCore, Metadata, Target};

pub async fn endpoint_from(
    tcp_listener: TcpListener,
    connections: Arc<RwLock<Connections>>,
    endpoint_core: Arc<Mutex<EndpointCore>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
) {
    loop {
        match tcp_listener.accept().await {
            Ok((tcp_stream, src)) => {
                let (tcp_read, tcp_write) = tcp_stream.into_split();
                info!("New TCP connection: {}", src);
                connections.write().await.insert_pending(src, tcp_write);

                emit_ingress_request(
                    connections.clone(),
                    endpoint_core.clone(),
                    ray_tx.clone(),
                    src,
                    Target {
                        host: src.ip().to_string(),
                        port: src.port(),
                    },
                )
                .await;

                let connections = connections.clone();
                let endpoint_core = endpoint_core.clone();
                let ray_tx = ray_tx.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65535];
                    loop {
                        match tcp_read.try_read(&mut buf) {
                            Ok(0) => {
                                info!("TCP connection closed: {}", src);
                                emit_session_close(
                                    connections.clone(),
                                    endpoint_core.clone(),
                                    ray_tx.clone(),
                                    src,
                                    CloseReason::LocalClosed,
                                )
                                .await;
                                break;
                            }
                            Ok(len) => {
                                debug!("Received {} bytes from TCP {}", len, src);
                                emit_session_bytes(
                                    connections.clone(),
                                    endpoint_core.clone(),
                                    ray_tx.clone(),
                                    src,
                                    buf[..len].to_vec(),
                                )
                                .await;
                            }
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                sleep(Duration::from_millis(1)).await;
                            }
                            Err(e) => {
                                error!("Failed to read from TCP stream: {}", e);
                                emit_session_close(
                                    connections.clone(),
                                    endpoint_core.clone(),
                                    ray_tx.clone(),
                                    src,
                                    CloseReason::Error(e.to_string()),
                                )
                                .await;
                                break;
                            }
                        }
                    }
                    connections.write().await.remove_addr(&src);
                });
            }
            Err(e) => error!("Failed to accept TCP connection: {}", e),
        }
    }
}

async fn emit_ingress_request(
    connections: Arc<RwLock<Connections>>,
    endpoint_core: Arc<Mutex<EndpointCore>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    src: SocketAddr,
    target: Target,
) {
    let mut actions = Vec::new();
    let event = CoreEvent::IngressSessionRequested {
        target,
        metadata: Metadata::new(),
    };
    match endpoint_core
        .lock()
        .await
        .handle_event(now_millis(), event, &mut actions)
    {
        Ok(()) => handle_ingress_actions(connections, ray_tx, src, actions).await,
        Err(error) => {
            error!("EndpointCore rejected ingress request: {}", error);
            connections.write().await.remove_addr(&src);
        }
    }
}

async fn emit_session_bytes(
    connections: Arc<RwLock<Connections>>,
    endpoint_core: Arc<Mutex<EndpointCore>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    src: SocketAddr,
    bytes: Vec<u8>,
) {
    let Some(conv_id) = connections.read().await.conv_id(&src) else {
        return;
    };
    emit_core_event(
        endpoint_core,
        ray_tx,
        CoreEvent::SessionBytes { conv_id, bytes },
    )
    .await;
}

async fn emit_session_close(
    connections: Arc<RwLock<Connections>>,
    endpoint_core: Arc<Mutex<EndpointCore>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    src: SocketAddr,
    reason: CloseReason,
) {
    let Some(conv_id) = connections.read().await.conv_id(&src) else {
        return;
    };
    emit_core_event(
        endpoint_core,
        ray_tx,
        CoreEvent::SessionClosed { conv_id, reason },
    )
    .await;
}

async fn emit_core_event(
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
        Ok(()) => send_core_actions(ray_tx, actions).await,
        Err(error) => error!("EndpointCore rejected local event: {}", error),
    }
}

async fn handle_ingress_actions(
    connections: Arc<RwLock<Connections>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    src: SocketAddr,
    actions: Vec<CoreAction>,
) {
    for action in actions {
        match action {
            CoreAction::IngressSessionCreated { conv_id } => {
                if !connections.write().await.bind_pending(src, conv_id) {
                    warn!("No pending TCP connection for ingress conv {}", conv_id);
                }
            }
            CoreAction::SendTransportPacket { channel_id, bytes } => {
                send_transport_action(ray_tx.clone(), channel_id, bytes).await;
            }
            CoreAction::EmitMetric(_) | CoreAction::EmitEvent(_) => {}
            CoreAction::OpenExitConnection { .. }
            | CoreAction::WriteSession { .. }
            | CoreAction::CloseSession { .. } => {
                warn!("Unexpected endpoint core action while opening ingress")
            }
        }
    }
}

pub(crate) async fn send_core_actions(
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    actions: Vec<CoreAction>,
) {
    for action in actions {
        match action {
            CoreAction::SendTransportPacket { channel_id, bytes } => {
                send_transport_action(ray_tx.clone(), channel_id, bytes).await;
            }
            CoreAction::EmitMetric(_) | CoreAction::EmitEvent(_) => {}
            _ => warn!("Ignoring endpoint core action in local sender"),
        }
    }
}

async fn send_transport_action(
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    channel_id: raynet_core::ChannelId,
    bytes: Vec<u8>,
) {
    if let Err(error) = ray_tx
        .send(OutboundTransportPacket { channel_id, bytes })
        .await
    {
        error!("Failed to enqueue endpoint core packet: {}", error);
    }
}
