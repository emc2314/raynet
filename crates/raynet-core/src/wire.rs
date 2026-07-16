use bytes::Bytes;

use crate::machine::{ChannelId, CloseReason};
const U48_SIZE: usize = 6;
const ROUTE_BITS: u32 = 6;
pub(crate) const TIME_BITS: u32 = 32 - ROUTE_BITS;
pub(crate) const TIME_MASK: u32 = (1 << TIME_BITS) - 1;
const ROUTE_MASK: u32 = (1 << ROUTE_BITS) - 1;

pub(crate) const ENVELOPE_FIXED_OVERHEAD: usize = size_of::<u32>() + 2 * U48_SIZE;
pub(crate) const CHANNEL_ID_SIZE: usize = size_of::<ChannelId>();

const FLAG_KCP: u8 = 1 << 0;
const FLAG_KEEP_ALIVE: u8 = 1 << 1;
const FLAG_ROUTE_REPLY: u8 = 1 << 2;
const REPLY_DEPTH_SHIFT: u8 = 3;
const REPLY_DEPTH_MASK: u8 = 0b11;

pub(crate) type RoutePlan = Vec<ChannelId>;

#[derive(Clone, Copy)]
pub(crate) struct EnvelopeStamp {
    pub time: u32,
    pub seq_id: u64,
    pub seq_no: u64,
}

#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) struct Envelope {
    pub time: u32,
    pub seq_id: u64,
    pub seq_no: u64,
    pub route_plan: RoutePlan,
    pub payload: Vec<u8>,
    pub padding_len: u8,
}

#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) enum EndpointMessage {
    KcpPacket {
        reply_depth: u8,
        packet: Bytes,
    },
    KeepAlive {
        reply_depth: u8,
    },
    RouteReply {
        reply_depth: u8,
        received_seq_id: u64,
        received_seq_no: u64,
    },
}

#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) enum SessionMessage {
    Open,
    Data { bytes: Vec<u8> },
    Close { reason: CloseReason },
}

#[cfg_attr(test, derive(Debug))]
pub(crate) struct WireError;

impl Envelope {
    pub(crate) fn encode_with_prefix(&self, prefix_len: usize) -> Vec<u8> {
        let size = ENVELOPE_FIXED_OVERHEAD
            + self.payload.len()
            + self.route_plan.len() * CHANNEL_ID_SIZE
            + self.padding_len as usize;
        let mut out = Vec::with_capacity(prefix_len + size);
        out.resize(prefix_len, 0);
        let time_and_route = ((self.time & TIME_MASK) << ROUTE_BITS) | self.route_plan.len() as u32;
        out.extend_from_slice(&time_and_route.to_le_bytes());
        put_u48(&mut out, self.seq_id);
        put_u48(&mut out, self.seq_no);
        out.extend_from_slice(&self.payload);
        for channel_id in &self.route_plan {
            out.extend_from_slice(&channel_id.to_le_bytes());
        }
        out.resize(out.len() + self.padding_len as usize, self.padding_len);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let (stamp, route_plan_len) = envelope_prefix(bytes)?;
        let padding_len = *bytes.last().ok_or(WireError)? as usize;
        let route_bytes = route_plan_len * CHANNEL_ID_SIZE;
        let tail = padding_len + route_bytes;
        if padding_len == 0
            || bytes.len() < ENVELOPE_FIXED_OVERHEAD + tail
            || !bytes[bytes.len() - padding_len..]
                .iter()
                .all(|byte| *byte as usize == padding_len)
        {
            return Err(WireError);
        }

        let payload_end = bytes.len() - tail;
        let route_end = bytes.len() - padding_len;
        let payload = bytes[ENVELOPE_FIXED_OVERHEAD..payload_end].to_vec();
        let mut route_plan = Vec::with_capacity(route_plan_len);
        let mut offset = payload_end;
        while offset < route_end {
            route_plan.push(u16::from_le_bytes(
                bytes[offset..offset + 2].try_into().unwrap(),
            ));
            offset += 2;
        }

        Ok(Self {
            time: stamp.time,
            seq_id: stamp.seq_id,
            seq_no: stamp.seq_no,
            route_plan,
            payload,
            padding_len: padding_len as u8,
        })
    }
}

pub(crate) fn endpoint_nonce(stamp: EnvelopeStamp) -> [u8; 16] {
    let mut nonce = [0; 16];
    let time_and_route = (stamp.time & TIME_MASK) << ROUTE_BITS;
    nonce[..4].copy_from_slice(&time_and_route.to_le_bytes());
    put_u48_slice(&mut nonce[4..10], stamp.seq_id);
    put_u48_slice(&mut nonce[10..16], stamp.seq_no);
    nonce
}

fn envelope_prefix(bytes: &[u8]) -> Result<(EnvelopeStamp, usize), WireError> {
    if bytes.len() < ENVELOPE_FIXED_OVERHEAD {
        return Err(WireError);
    }
    let time_and_route = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let time = time_and_route >> ROUTE_BITS;
    let route_plan_len = (time_and_route & ROUTE_MASK) as usize;
    let seq_id = get_u48(&bytes[4..10]);
    let seq_no = get_u48(&bytes[10..16]);
    Ok((
        EnvelopeStamp {
            time,
            seq_id,
            seq_no,
        },
        route_plan_len,
    ))
}

impl EndpointMessage {
    pub(crate) fn encode_with_prefix(&self, prefix_len: usize) -> Vec<u8> {
        let (type_flag, reply_depth, body) = match self {
            EndpointMessage::KcpPacket {
                reply_depth,
                packet,
            } => (FLAG_KCP, *reply_depth, packet.as_ref()),
            EndpointMessage::KeepAlive { reply_depth } => (FLAG_KEEP_ALIVE, *reply_depth, &[][..]),
            EndpointMessage::RouteReply {
                reply_depth,
                received_seq_id,
                received_seq_no,
            } => {
                let mut body = [0u8; 12];
                put_u48_slice(&mut body[..6], *received_seq_id);
                put_u48_slice(&mut body[6..], *received_seq_no);
                return encode_flags_body(prefix_len, FLAG_ROUTE_REPLY, *reply_depth, &body);
            }
        };
        encode_flags_body(prefix_len, type_flag, reply_depth, body)
    }

    pub fn decode(bytes: Bytes) -> Self {
        let flags = bytes[0];
        let reply_depth = (flags >> REPLY_DEPTH_SHIFT) & REPLY_DEPTH_MASK;
        let type_bits = flags & !(REPLY_DEPTH_MASK << REPLY_DEPTH_SHIFT);
        match type_bits {
            FLAG_KCP => EndpointMessage::KcpPacket {
                reply_depth,
                packet: bytes.slice(1..),
            },
            FLAG_KEEP_ALIVE => {
                debug_assert!(bytes.len() == 1);
                EndpointMessage::KeepAlive { reply_depth }
            }
            FLAG_ROUTE_REPLY => {
                debug_assert!(bytes.len() == 13);
                EndpointMessage::RouteReply {
                    reply_depth,
                    received_seq_id: get_u48(&bytes[1..7]),
                    received_seq_no: get_u48(&bytes[7..13]),
                }
            }
            _ => panic!(),
        }
    }
}

fn encode_flags_body(prefix_len: usize, type_flag: u8, reply_depth: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix_len + 1 + body.len());
    out.resize(prefix_len, 0);
    out.push(type_flag | (reply_depth << REPLY_DEPTH_SHIFT));
    out.extend_from_slice(body);
    out
}

fn put_u48_slice(out: &mut [u8], value: u64) {
    out.copy_from_slice(&value.to_le_bytes()[..6]);
}

fn put_u48(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes()[..U48_SIZE]);
}

fn get_u48(bytes: &[u8]) -> u64 {
    let mut value = [0; 8];
    value[..6].copy_from_slice(bytes);
    u64::from_le_bytes(value)
}

impl SessionMessage {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            SessionMessage::Open => out.push(0),
            SessionMessage::Data { bytes } => {
                out.push(1);
                out.extend_from_slice(bytes);
            }
            SessionMessage::Close { reason } => {
                out.push(2);
                out.push(*reason as u8);
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Self {
        match bytes[0] {
            0 => {
                debug_assert!(bytes.len() == 1);
                SessionMessage::Open
            }
            1 => SessionMessage::Data {
                bytes: bytes[1..].to_vec(),
            },
            2 => {
                debug_assert!(bytes.len() == 2);
                SessionMessage::Close {
                    reason: match bytes[1] {
                        0 => CloseReason::LocalClosed,
                        1 => CloseReason::RemoteClosed,
                        2 => CloseReason::Reset,
                        3 => CloseReason::Error,
                        _ => panic!(),
                    },
                }
            }
            _ => panic!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_header_layout_and_decode() {
        let envelope = Envelope {
            time: 1,
            seq_id: 2,
            seq_no: 3,
            route_plan: vec![2, 3],
            payload: b"payload".to_vec(),
            padding_len: 3,
        };

        let encoded = envelope.encode_with_prefix(0);
        assert_eq!(
            encoded.len(),
            ENVELOPE_FIXED_OVERHEAD
                + envelope.payload.len()
                + 2 * CHANNEL_ID_SIZE
                + envelope.padding_len as usize
        );
        assert_eq!(u32::from_le_bytes(encoded[0..4].try_into().unwrap()), 66);
        assert_eq!(get_u48(&encoded[4..10]), 2);
        assert_eq!(get_u48(&encoded[10..16]), 3);
        assert_eq!(&encoded[16..23], b"payload");
        assert_eq!(u16::from_le_bytes(encoded[23..25].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(encoded[25..27].try_into().unwrap()), 3);
        assert_eq!(&encoded[encoded.len() - 3..], &[3, 3, 3]);
        assert_eq!(Envelope::decode(&encoded).unwrap(), envelope);
        assert_eq!(
            endpoint_nonce(EnvelopeStamp {
                time: 1,
                seq_id: 2,
                seq_no: 3,
            })[..4],
            ((1u32 << 6).to_le_bytes())
        );
    }

    #[test]
    fn endpoint_message_layouts() {
        for reply_depth in 0..=3 {
            let kcp = EndpointMessage::KcpPacket {
                reply_depth,
                packet: Bytes::from_static(b"abcd"),
            };
            let encoded = kcp.encode_with_prefix(0);
            assert_eq!(encoded[0], FLAG_KCP | (reply_depth << REPLY_DEPTH_SHIFT));
            assert_eq!(&encoded[1..], b"abcd");
            assert_eq!(EndpointMessage::decode(encoded.into()), kcp);

            let keep = EndpointMessage::KeepAlive { reply_depth };
            let encoded = keep.encode_with_prefix(0);
            assert_eq!(
                encoded,
                [FLAG_KEEP_ALIVE | (reply_depth << REPLY_DEPTH_SHIFT)]
            );
            assert_eq!(EndpointMessage::decode(encoded.into()), keep);

            let reply = EndpointMessage::RouteReply {
                reply_depth,
                received_seq_id: 0x3344_5566_7788,
                received_seq_no: 0x1122_99aa_bbcc,
            };
            let encoded = reply.encode_with_prefix(0);
            assert_eq!(encoded.len(), 13);
            assert_eq!(
                encoded[0],
                FLAG_ROUTE_REPLY | (reply_depth << REPLY_DEPTH_SHIFT)
            );
            assert_eq!(EndpointMessage::decode(encoded.into()), reply);
        }
    }

    #[test]
    fn session_message_layout_and_decode() {
        assert_eq!(SessionMessage::Open.encode(), [0]);
        assert_eq!(SessionMessage::decode(&[0]), SessionMessage::Open);

        let bytes = SessionMessage::Data {
            bytes: b"data".to_vec(),
        };
        assert_eq!(bytes.encode(), b"\x01data");
        assert_eq!(SessionMessage::decode(b"\x01data"), bytes);

        for reason in [
            CloseReason::LocalClosed,
            CloseReason::RemoteClosed,
            CloseReason::Reset,
            CloseReason::Error,
        ] {
            let message = SessionMessage::Close { reason };
            let encoded = message.encode();
            assert_eq!(encoded, [2, reason as u8]);
            assert_eq!(SessionMessage::decode(&encoded), message);
        }
    }
}
