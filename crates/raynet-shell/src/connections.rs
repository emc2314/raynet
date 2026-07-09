use std::collections::HashMap;
use std::fmt::{self, Debug};
use std::net::SocketAddr;

use raynet_core::core::LocalConnectionId;
use raynet_core::utils::UnwrapNone;

pub struct Connections {
    cons: HashMap<SocketAddr, tokio::net::tcp::OwnedWriteHalf>,
    local_ids: HashMap<SocketAddr, LocalConnectionId>,
    local_addrs: HashMap<LocalConnectionId, SocketAddr>,
    next_local_id: LocalConnectionId,
}

impl Connections {
    pub fn new() -> Connections {
        Connections {
            cons: HashMap::new(),
            local_ids: HashMap::new(),
            local_addrs: HashMap::new(),
            next_local_id: 1,
        }
    }

    pub fn remove(self: &mut Connections, addr: &SocketAddr) {
        self.cons.remove(addr);
        if let Some(local_connection_id) = self.local_ids.remove(addr) {
            self.local_addrs.remove(&local_connection_id);
        }
    }

    pub fn insert(
        self: &mut Connections,
        addr: SocketAddr,
        write: tokio::net::tcp::OwnedWriteHalf,
    ) -> LocalConnectionId {
        self.cons.insert(addr, write).unwrap_none();
        let local_connection_id = self.next_local_id;
        self.next_local_id = self.next_local_id.saturating_add(1).max(1);
        self.local_ids.insert(addr, local_connection_id);
        self.local_addrs.insert(local_connection_id, addr);
        local_connection_id
    }

    pub fn get_by_local_connection_id(
        self: &Connections,
        local_connection_id: LocalConnectionId,
    ) -> Option<&tokio::net::tcp::OwnedWriteHalf> {
        self.local_addrs
            .get(&local_connection_id)
            .and_then(|addr| self.cons.get(addr))
    }

    pub fn local_connection_id(&self, addr: &SocketAddr) -> Option<LocalConnectionId> {
        self.local_ids.get(addr).copied()
    }

    pub fn remove_by_local_connection_id(&mut self, local_connection_id: LocalConnectionId) {
        if let Some(addr) = self.local_addrs.remove(&local_connection_id) {
            self.local_ids.remove(&addr);
            self.cons.remove(&addr);
        }
    }
}

impl Debug for Connections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connections")
            .field("cons", &self.cons.keys())
            .finish()
    }
}
