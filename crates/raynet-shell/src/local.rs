use log::{debug, error, info};
use std::io;
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
                let local_connection_id = { connections.write().await.insert(src, tcp_write) };
                emit_core_event(
                    endpoint_core.clone(),
                    ray_tx.clone(),
                    CoreEvent::IngressConnectionOpened {
                        local_connection_id,
                        target: Target {
                            host: src.ip().to_string(),
                            port: src.port(),
                        },
                        metadata: Metadata::new(),
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
                                emit_local_close(
                                    connections.clone(),
                                    endpoint_core.clone(),
                                    ray_tx.clone(),
                                    &src,
                                    CloseReason::LocalClosed,
                                )
                                .await;
                                break;
                            }
                            Ok(len) => {
                                debug!("Received {} bytes from TCP {}", len, src);
                                emit_local_bytes(
                                    connections.clone(),
                                    endpoint_core.clone(),
                                    ray_tx.clone(),
                                    &src,
                                    buf[..len].to_vec(),
                                )
                                .await;
                            }
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                sleep(Duration::from_millis(1)).await;
                            }
                            Err(e) => {
                                error!("Failed to read from TCP stream: {}", e);
                                emit_local_close(
                                    connections.clone(),
                                    endpoint_core.clone(),
                                    ray_tx.clone(),
                                    &src,
                                    CloseReason::Error(e.to_string()),
                                )
                                .await;
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

async fn emit_local_bytes(
    connections: Arc<RwLock<Connections>>,
    endpoint_core: Arc<Mutex<EndpointCore>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    src: &std::net::SocketAddr,
    bytes: Vec<u8>,
) {
    let Some(local_connection_id) = connections.read().await.local_connection_id(src) else {
        return;
    };
    emit_core_event(
        endpoint_core,
        ray_tx,
        CoreEvent::LocalConnectionBytes {
            local_connection_id,
            bytes,
        },
    )
    .await;
}

async fn emit_local_close(
    connections: Arc<RwLock<Connections>>,
    endpoint_core: Arc<Mutex<EndpointCore>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    src: &std::net::SocketAddr,
    reason: CloseReason,
) {
    let Some(local_connection_id) = connections.read().await.local_connection_id(src) else {
        return;
    };
    emit_core_event(
        endpoint_core,
        ray_tx,
        CoreEvent::LocalConnectionClosed {
            local_connection_id,
            reason,
        },
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

pub(crate) async fn send_core_actions(
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    actions: Vec<CoreAction>,
) {
    for action in actions {
        if let CoreAction::SendTransportPacket { channel_id, bytes } = action {
            if let Err(error) = ray_tx
                .send(OutboundTransportPacket {
                    channel_id: Some(channel_id),
                    bytes,
                })
                .await
            {
                error!("Failed to enqueue endpoint core packet: {}", error);
            }
        }
    }
}
