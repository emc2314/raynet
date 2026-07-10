use raynet_core::ChannelId;

pub struct OutboundTransportPacket {
    pub channel_id: ChannelId,
    pub bytes: Vec<u8>,
}
