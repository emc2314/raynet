use raynet_core::ChannelId;
use std::net::{SocketAddr, ToSocketAddrs};

#[derive(Debug, Clone)]
pub struct UdpChannel {
    pub channel_id: ChannelId,
    pub addr: SocketAddr,
}

#[derive(Debug, Clone)]
pub struct UdpChannels {
    channels: Vec<UdpChannel>,
}

impl UdpChannels {
    pub fn from_names(names: Vec<String>) -> Self {
        let channels = names
            .into_iter()
            .enumerate()
            .map(|(index, name)| UdpChannel {
                channel_id: index as ChannelId + 1,
                addr: name
                    .to_socket_addrs()
                    .expect("Unable to resolve send address")
                    .next()
                    .unwrap(),
            })
            .collect();

        Self { channels }
    }

    pub fn addr(&self, channel_id: ChannelId) -> Option<SocketAddr> {
        self.channels
            .iter()
            .find(|channel| channel.channel_id == channel_id)
            .map(|channel| channel.addr)
    }

    pub fn channel_id_for_addr(&self, addr: SocketAddr) -> Option<ChannelId> {
        let ip = addr.ip().to_canonical();
        self.channels
            .iter()
            .find(|channel| channel.addr == addr || channel.addr.ip().to_canonical() == ip)
            .map(|channel| channel.channel_id)
    }

    pub fn channel_ids(&self) -> impl Iterator<Item = ChannelId> + '_ {
        self.channels.iter().map(|channel| channel.channel_id)
    }
}
