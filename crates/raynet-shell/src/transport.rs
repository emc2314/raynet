use raynet_core::ChannelId;

pub struct OutboundTransportPacket {
    pub channel_id: Option<ChannelId>,
    pub bytes: Vec<u8>,
}
