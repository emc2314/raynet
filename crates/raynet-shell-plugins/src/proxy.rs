use std::net::SocketAddr;

use raynet_core::CloseReason;
use tokio::sync::mpsc;

pub enum ProxyMessage {
    Write(Vec<u8>),
    Close(CloseReason),
}

pub struct ProxySession {
    input: mpsc::Sender<ProxyMessage>,
    output: mpsc::Receiver<ProxyMessage>,
}

impl ProxySession {
    pub(crate) fn new(
        input: mpsc::Sender<ProxyMessage>,
        output: mpsc::Receiver<ProxyMessage>,
    ) -> Self {
        Self { input, output }
    }

    pub fn split(self) -> (mpsc::Sender<ProxyMessage>, mpsc::Receiver<ProxyMessage>) {
        (self.input, self.output)
    }
}

pub struct ProxyListener {
    local_addr: SocketAddr,
    sessions: mpsc::Receiver<ProxySession>,
}

impl ProxyListener {
    pub(crate) fn new(local_addr: SocketAddr, sessions: mpsc::Receiver<ProxySession>) -> Self {
        Self {
            local_addr,
            sessions,
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn accept(&mut self) -> Option<ProxySession> {
        self.sessions.recv().await
    }
}

pub trait ProxyPlugin: Send + Sync {
    fn open(&self) -> ProxySession;
}
