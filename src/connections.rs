use log::info;
use std::collections::HashMap;
use std::fmt::{self, Debug};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

use crate::packets::{KcpOutput, KcpRecv, TCPPacket, UDPPacket};
use crate::rkcp::session::KcpSession;
use crate::rkcp::socket::KcpSocket;

pub struct Connections {
    pub cons: HashMap<SocketAddr, tokio::net::tcp::OwnedWriteHalf>,
    pub convs: HashMap<SocketAddr, u32>,
    pub kcps: HashMap<u32, Arc<KcpSession>>,
}

impl Connections {
    pub fn new() -> Connections {
        let cons = HashMap::new();
        let convs = HashMap::new();
        let kcps = HashMap::new();
        Connections { cons, convs, kcps }
    }
}
impl Debug for Connections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connections")
            .field("cons", &self.cons)
            .field("convs", &self.convs)
            .field("kcps", &self.kcps)
            .finish()
    }
}

pub async fn assign_kcp(
    connections: &Arc<RwLock<Connections>>,
    conv: u32,
    udp_tx: &mpsc::Sender<UDPPacket>,
    tcp_tx: &mpsc::Sender<TCPPacket>,
    is_client: bool,
) -> Option<Arc<KcpSession>> {
    let mut con = connections.write().await;
    con.cons
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .iter()
        .find_map(|addr| {
            if !con.convs.contains_key(addr) {
                let config = Default::default();
                let kcp =
                    KcpSocket::new(&config, conv, KcpOutput::new(udp_tx.clone()), true).unwrap();
                let session = KcpSession::new_shared(
                    kcp,
                    KcpRecv::new(tcp_tx.clone(), addr.clone()),
                    config.session_expire,
                    is_client,
                );
                con.convs.insert(addr.clone(), conv);
                con.kcps.insert(conv, session.clone());
                info!("Assign TCP {} as {}", addr, conv);
                Some(session)
            } else {
                None
            }
        })
}
