use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use raynet_core::ChannelId;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Instant, interval};

use crate::channel::{ChannelSendFailure, SendPacket};
use crate::{ChannelReceiver, ChannelSender};

const QUEUE_SIZE: usize = 256;
const PROBE_INTERVAL: Duration = Duration::from_secs(15);
const PEER_TIMEOUT: Duration = Duration::from_secs(60);

pub struct ForwardUdp;

impl ForwardUdp {
    pub async fn sender(
        channel_id: ChannelId,
        binds: Vec<SocketAddr>,
        destinations: Vec<SocketAddr>,
        failures: mpsc::Sender<ChannelSendFailure>,
    ) -> io::Result<ChannelSender> {
        assert!(!destinations.is_empty());
        let sockets = bind_all(binds).await?;

        let (tx, mut rx) = mpsc::channel::<SendPacket>(QUEUE_SIZE);
        tokio::spawn(async move {
            while let Some(packet) = rx.recv().await {
                let socket = &sockets[rand::random_range(..sockets.len())];
                let destination = destinations[rand::random_range(..destinations.len())];
                if socket.send_to(&packet.bytes, destination).await.is_err() {
                    let _ = failures.send((channel_id, packet.bytes)).await;
                }
            }
        });
        Ok(ChannelSender::new(channel_id, tx))
    }

    pub async fn receiver(
        channel_id: ChannelId,
        binds: Vec<SocketAddr>,
    ) -> io::Result<ChannelReceiver> {
        Ok(receive_on(channel_id, bind_all(binds).await?))
    }
}

pub struct ReverseUdp;

impl ReverseUdp {
    pub async fn sender(
        channel_id: ChannelId,
        binds: Vec<SocketAddr>,
        channel_key: [u8; 32],
        failures: mpsc::Sender<ChannelSendFailure>,
    ) -> io::Result<ChannelSender> {
        let challenge_key = challenge_key(&channel_key);
        let sockets = bind_all(binds).await?;
        let peers = Arc::new(Mutex::new(HashMap::new()));

        for (socket_index, socket) in sockets.iter().enumerate() {
            let socket = socket.clone();
            let peers = peers.clone();
            tokio::spawn(async move {
                let mut bytes = [0; 65_535];
                while let Ok((length, peer)) = socket.recv_from(&mut bytes).await {
                    if valid_probe(&bytes[..length], &challenge_key) {
                        let response = make_response(&challenge_key, &bytes[..16]);
                        if socket.send_to(&response, peer).await.is_ok() {
                            peers
                                .lock()
                                .await
                                .insert((socket_index, peer), Instant::now());
                        }
                    }
                }
            });
        }

        let (tx, mut rx) = mpsc::channel::<SendPacket>(QUEUE_SIZE);
        tokio::spawn(async move {
            while let Some(packet) = rx.recv().await {
                let path = {
                    let mut peers = peers.lock().await;
                    peers.retain(|_, seen| seen.elapsed() < PEER_TIMEOUT);
                    if peers.is_empty() {
                        None
                    } else {
                        peers.keys().nth(rand::random_range(..peers.len())).copied()
                    }
                };
                let failed = match path {
                    Some((socket, peer)) => {
                        sockets[socket].send_to(&packet.bytes, peer).await.is_err()
                    }
                    None => true,
                };
                if failed {
                    let _ = failures.send((channel_id, packet.bytes)).await;
                }
            }
        });
        Ok(ChannelSender::new(channel_id, tx))
    }

    pub async fn receiver(
        channel_id: ChannelId,
        binds: Vec<SocketAddr>,
        destinations: Vec<SocketAddr>,
        channel_key: [u8; 32],
    ) -> io::Result<ChannelReceiver> {
        assert!(!destinations.is_empty());
        let challenge_key = challenge_key(&channel_key);
        let sockets = bind_all(binds).await?;
        let (tx, rx) = mpsc::channel(QUEUE_SIZE);
        for socket in sockets {
            let tx = tx.clone();
            let destinations = destinations.clone();
            tokio::spawn(async move {
                let mut timer = interval(PROBE_INTERVAL);
                let mut challenges = HashMap::new();
                let mut bytes = [0; 65_535];
                loop {
                    tokio::select! {
                        _ = timer.tick() => {
                            for destination in &destinations {
                                let probe = make_probe(&challenge_key);
                                challenges.insert(*destination, probe[..16].try_into().unwrap());
                                let _ = socket.send_to(&probe, destination).await;
                            }
                        }
                        result = socket.recv_from(&mut bytes) => {
                            let Ok((length, peer)) = result else { return };
                            if challenges.get(&peer).is_some_and(|nonce| {
                                valid_response(&bytes[..length], &challenge_key, nonce)
                            }) {
                                challenges.remove(&peer);
                            } else if tx.send(bytes[..length].to_vec()).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
        Ok(ChannelReceiver::new(channel_id, rx))
    }
}

async fn bind_all(binds: Vec<SocketAddr>) -> io::Result<Vec<Arc<UdpSocket>>> {
    assert!(!binds.is_empty());
    let mut sockets = Vec::with_capacity(binds.len());
    for bind in binds {
        sockets.push(Arc::new(UdpSocket::bind(bind).await?));
    }
    Ok(sockets)
}

fn receive_on(channel_id: ChannelId, sockets: Vec<Arc<UdpSocket>>) -> ChannelReceiver {
    let (tx, rx) = mpsc::channel(QUEUE_SIZE);
    for socket in sockets {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut bytes = [0; 65_535];
            while let Ok((length, _)) = socket.recv_from(&mut bytes).await {
                if tx.send(bytes[..length].to_vec()).await.is_err() {
                    return;
                }
            }
        });
    }
    ChannelReceiver::new(channel_id, rx)
}

fn make_probe(key: &[u8; 32]) -> [u8; 32] {
    let nonce = rand::random::<[u8; 16]>();
    let mut probe = [0; 32];
    probe[..16].copy_from_slice(&nonce);
    probe[16..].copy_from_slice(&tag(key, 0, &nonce));
    probe
}

fn challenge_key(channel_key: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("RayNet reverse UDP challenge key v1", channel_key)
}

fn valid_probe(probe: &[u8], key: &[u8; 32]) -> bool {
    probe.len() == 32 && probe[16..] == tag(key, 0, &probe[..16])
}

fn make_response(key: &[u8; 32], challenge: &[u8]) -> [u8; 32] {
    let nonce = rand::random::<[u8; 16]>();
    let mut authenticated = [0; 32];
    authenticated[..16].copy_from_slice(challenge);
    authenticated[16..].copy_from_slice(&nonce);
    let mut response = [0; 32];
    response[..16].copy_from_slice(&nonce);
    response[16..].copy_from_slice(&tag(key, 1, &authenticated));
    response
}

fn valid_response(response: &[u8], key: &[u8; 32], challenge: &[u8; 16]) -> bool {
    if response.len() != 32 {
        return false;
    }
    let mut authenticated = [0; 32];
    authenticated[..16].copy_from_slice(challenge);
    authenticated[16..].copy_from_slice(&response[..16]);
    response[16..] == tag(key, 1, &authenticated)
}

fn tag(key: &[u8; 32], domain: u8, bytes: &[u8]) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new_keyed(key);
    hasher.update(&[domain]);
    hasher.update(bytes);
    hasher.finalize().as_bytes()[..16].try_into().unwrap()
}
