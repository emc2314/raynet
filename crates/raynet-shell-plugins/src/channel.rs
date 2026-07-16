use raynet_core::ChannelId;
use tokio::sync::{mpsc, oneshot};

pub(crate) struct SendPacket {
    pub bytes: Vec<u8>,
    pub result: oneshot::Sender<Result<(), Vec<u8>>>,
}

#[derive(Clone)]
pub struct ChannelSender {
    channel_id: ChannelId,
    tx: mpsc::Sender<SendPacket>,
}

impl ChannelSender {
    pub(crate) fn new(channel_id: ChannelId, tx: mpsc::Sender<SendPacket>) -> Self {
        Self { channel_id, tx }
    }

    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    pub async fn send(&self, bytes: Vec<u8>) -> Result<(), Vec<u8>> {
        let (result, result_rx) = oneshot::channel();
        let packet = SendPacket { bytes, result };
        if let Err(error) = self.tx.send(packet).await {
            return Err(error.0.bytes);
        }
        result_rx.await.unwrap()
    }
}

pub struct ChannelReceiver {
    channel_id: ChannelId,
    rx: mpsc::Receiver<Vec<u8>>,
}

impl ChannelReceiver {
    pub(crate) fn new(channel_id: ChannelId, rx: mpsc::Receiver<Vec<u8>>) -> Self {
        Self { channel_id, rx }
    }

    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }
}
