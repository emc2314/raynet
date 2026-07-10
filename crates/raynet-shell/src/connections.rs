use std::collections::HashMap;
use std::fmt::{self, Debug};
use std::net::SocketAddr;

use raynet_core::ConvId;

pub struct Connections {
    sessions: HashMap<ConvId, tokio::net::tcp::OwnedWriteHalf>,
    addrs: HashMap<SocketAddr, ConvId>,
    pending: HashMap<SocketAddr, tokio::net::tcp::OwnedWriteHalf>,
}

impl Connections {
    pub fn new() -> Connections {
        Connections {
            sessions: HashMap::new(),
            addrs: HashMap::new(),
            pending: HashMap::new(),
        }
    }

    pub fn insert_pending(&mut self, addr: SocketAddr, write: tokio::net::tcp::OwnedWriteHalf) {
        self.pending.insert(addr, write);
    }

    pub fn bind_pending(&mut self, addr: SocketAddr, conv_id: ConvId) -> bool {
        let Some(write) = self.pending.remove(&addr) else {
            return false;
        };
        self.sessions.insert(conv_id, write);
        self.addrs.insert(addr, conv_id);
        true
    }

    pub fn insert_session(
        &mut self,
        addr: SocketAddr,
        conv_id: ConvId,
        write: tokio::net::tcp::OwnedWriteHalf,
    ) {
        self.sessions.insert(conv_id, write);
        self.addrs.insert(addr, conv_id);
    }

    pub fn remove_addr(&mut self, addr: &SocketAddr) {
        self.pending.remove(addr);
        if let Some(conv_id) = self.addrs.remove(addr) {
            self.sessions.remove(&conv_id);
        }
    }

    pub fn remove_conv(&mut self, conv_id: ConvId) {
        self.sessions.remove(&conv_id);
        self.addrs.retain(|_, mapped| *mapped != conv_id);
    }

    pub fn conv_id(&self, addr: &SocketAddr) -> Option<ConvId> {
        self.addrs.get(addr).copied()
    }

    pub fn get(&self, conv_id: ConvId) -> Option<&tokio::net::tcp::OwnedWriteHalf> {
        self.sessions.get(&conv_id)
    }
}

impl Debug for Connections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connections")
            .field("sessions", &self.sessions.keys())
            .field("pending", &self.pending.keys())
            .finish()
    }
}
