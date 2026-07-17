use std::collections::BTreeMap;
use std::sync::Arc;

use raynet_core::ChannelId;
use raynet_shell_plugins::{ChannelReceiver, ChannelSendFailure, ChannelSender};
use tokio::sync::mpsc;

use crate::transport::OutboundTransportPacket;

pub struct InboundTransportPacket {
    pub channel_id: ChannelId,
    pub bytes: Vec<u8>,
}

pub struct ChannelSenders(BTreeMap<ChannelId, ChannelSender>);

pub struct Channels {
    pub senders: Arc<ChannelSenders>,
    pub receiver: mpsc::Receiver<InboundTransportPacket>,
    pub failures: mpsc::Receiver<ChannelSendFailure>,
}

impl ChannelSenders {
    pub fn channel_ids(&self) -> Vec<ChannelId> {
        self.0.keys().copied().collect()
    }

    pub async fn send(
        &self,
        packet: OutboundTransportPacket,
    ) -> Result<(), OutboundTransportPacket> {
        let Some(sender) = self.0.get(&packet.channel_id) else {
            return Err(packet);
        };
        sender
            .send(packet.bytes)
            .await
            .map_err(|bytes| OutboundTransportPacket {
                channel_id: packet.channel_id,
                bytes,
            })
    }
}

pub fn channels(
    senders: Vec<ChannelSender>,
    receivers: Vec<ChannelReceiver>,
    failures: mpsc::Receiver<ChannelSendFailure>,
) -> Channels {
    let mut by_id = BTreeMap::new();
    for sender in senders {
        assert!(by_id.insert(sender.channel_id(), sender).is_none());
    }
    let (tx, rx) = mpsc::channel(256);
    for mut receiver in receivers {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(bytes) = receiver.recv().await {
                if tx
                    .send(InboundTransportPacket {
                        channel_id: receiver.channel_id(),
                        bytes,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }
    Channels {
        senders: Arc::new(ChannelSenders(by_id)),
        receiver: rx,
        failures,
    }
}
