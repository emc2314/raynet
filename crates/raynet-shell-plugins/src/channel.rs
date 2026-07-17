use raynet_core::ChannelId;
use tokio::sync::mpsc;

/// Outbound datagram. Delivery failures are reported asynchronously via the
/// `failures` channel passed into channel constructors — not via a per-packet
/// oneshot round-trip.
pub(crate) struct SendPacket {
    pub bytes: Vec<u8>,
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

    /// Enqueue a packet for sending. Returns `Err` only if the channel is closed
    /// or the queue is full after waiting (caller should treat as send failure).
    pub async fn send(&self, bytes: Vec<u8>) -> Result<(), Vec<u8>> {
        self.tx
            .send(SendPacket { bytes })
            .await
            .map_err(|error| error.0.bytes)
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

/// Local send failure reported by a channel worker after enqueue succeeded.
pub type ChannelSendFailure = (ChannelId, Vec<u8>);
