use log::info;
use rand::Rng;
use std::collections::HashMap;
use std::fmt::{self, Debug};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

use crate::packets::{KCPPacket, KcpOutput, KcpRecv, TCPPacket};
use crate::rkcp::session::KcpSession;
use crate::rkcp::socket::KcpSocket;
use crate::utils::UnwrapNone;

pub struct Connections {
    cons: HashMap<SocketAddr, tokio::net::tcp::OwnedWriteHalf>,
    convs: HashMap<SocketAddr, u32>,
    kcps: HashMap<u32, Arc<KcpSession>>,
}

impl Connections {
    pub fn new() -> Connections {
        let cons = HashMap::new();
        let convs = HashMap::new();
        let kcps = HashMap::new();
        Connections { cons, convs, kcps }
    }

    pub async fn assign_from_conv(
        self: &mut Connections,
        conv: u32,
        kcp_tx: &Sender<KCPPacket>,
        tcp_tx: &Sender<TCPPacket>,
    ) -> Option<Arc<KcpSession>> {
        self.cons
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .iter()
            .find_map(|addr| {
                if let Some(old_conv) = self.convs.get(addr) {
                    let session = &self.kcps[old_conv];
                    if session.closed() {
                        self.close(addr);
                    } else {
                        return None;
                    }
                }
                let config = Default::default();
                let kcp =
                    KcpSocket::new(&config, conv, KcpOutput::new(kcp_tx.clone()), true).unwrap();
                let session = KcpSession::new_shared(
                    kcp,
                    KcpRecv::new(tcp_tx.clone(), *addr),
                    config.session_expire,
                    false,
                );
                self.convs.insert(*addr, conv).unwrap_none();
                self.kcps.insert(conv, session.clone()).unwrap_none();
                info!("Assign TCP {} as {}", addr, conv);
                Some(session)
            })
    }

    pub async fn assign_from_addr(
        self: &mut Connections,
        addr: &SocketAddr,
        kcp_tx: &Sender<KCPPacket>,
        tcp_tx: &Sender<TCPPacket>,
    ) -> Arc<KcpSession> {
        let conv = rand::thread_rng().gen::<u32>();
        let config = Default::default();
        let kcp = KcpSocket::new(&config, conv, KcpOutput::new(kcp_tx.clone()), true).unwrap();
        let session = KcpSession::new_shared(
            kcp,
            KcpRecv::new(tcp_tx.clone(), *addr),
            config.session_expire,
            false,
        );
        self.convs.insert(*addr, conv).unwrap_none();
        self.kcps.insert(conv, session.clone()).unwrap_none();
        info!("Assign TCP {} as {}", addr, conv);
        session
    }

    pub fn close(self: &mut Connections, addr: &SocketAddr) {
        if let Some(conv) = self.convs.remove(addr) {
            info!("Removed KCP session {} from connections", conv);
            self.kcps.remove(&conv).unwrap().close();
        }
    }

    pub fn remove(self: &mut Connections, addr: &SocketAddr) {
        info!("Remove TCP session {} from connections", addr);
        self.cons.remove(addr).unwrap();
    }

    pub fn insert(
        self: &mut Connections,
        addr: SocketAddr,
        write: tokio::net::tcp::OwnedWriteHalf,
    ) {
        self.cons.insert(addr, write).unwrap_none();
    }

    pub fn get(self: &Connections, addr: &SocketAddr) -> Option<&tokio::net::tcp::OwnedWriteHalf> {
        self.cons.get(addr)
    }

    pub fn get_from_conv(self: &Connections, conv: u32) -> Option<Arc<KcpSession>> {
        self.kcps.get(&conv).cloned()
    }

    pub fn get_from_addr(self: &Connections, addr: &SocketAddr) -> Option<Arc<KcpSession>> {
        self.convs
            .get(addr)
            .and_then(|conv| self.kcps.get(conv).cloned())
    }
}

impl Debug for Connections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connections")
            .field("cons", &self.cons.keys())
            .field("convs", &self.convs)
            .field("kcps", &self.kcps)
            .finish()
    }
}
