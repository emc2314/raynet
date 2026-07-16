use aegis::aegis128l::{Aegis128L, Key, Nonce, Tag};

use crate::limits::{MAX_TRANSPORT_PACKET_SIZE, TRANSPORT_PACKET_OVERHEAD};
use crate::random::RandomStream;
use crate::wire::Envelope;

#[cfg_attr(test, derive(Debug))]
pub(crate) enum TransportPacketError {
    InvalidLength,
    Authentication,
    MalformedEnvelope,
}

pub(crate) fn encode_transport_packet(
    key: &Key,
    random: &mut RandomStream,
    envelope: Envelope,
) -> Vec<u8> {
    let mut nonce = [0; 16];
    random.fill(&mut nonce);
    let mut packet = envelope.encode_with_prefix(TRANSPORT_PACKET_OVERHEAD);
    packet[..16].copy_from_slice(&nonce);
    let tag: Tag<16> =
        Aegis128L::new(key, &nonce).encrypt_in_place(&mut packet[TRANSPORT_PACKET_OVERHEAD..], &[]);
    packet[16..TRANSPORT_PACKET_OVERHEAD].copy_from_slice(&tag);
    packet
}

pub(crate) fn decode_transport_packet(
    key: &Key,
    mut packet: Vec<u8>,
) -> Result<Envelope, TransportPacketError> {
    if packet.len() < TRANSPORT_PACKET_OVERHEAD || packet.len() > MAX_TRANSPORT_PACKET_SIZE {
        return Err(TransportPacketError::InvalidLength);
    }
    let nonce: Nonce = packet[..16].try_into().unwrap();
    let tag: Tag<16> = packet[16..TRANSPORT_PACKET_OVERHEAD].try_into().unwrap();
    Aegis128L::new(key, &nonce)
        .decrypt_in_place(&mut packet[TRANSPORT_PACKET_OVERHEAD..], &tag, &[])
        .map_err(|_| TransportPacketError::Authentication)?;
    Envelope::decode(&packet[TRANSPORT_PACKET_OVERHEAD..])
        .map_err(|_| TransportPacketError::MalformedEnvelope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_packet_roundtrip() {
        let key = [7; 16];
        let mut random = RandomStream::new([9; 16]);
        let envelope = Envelope {
            time: 1,
            seq_id: 2,
            seq_no: 3,
            route_plan: vec![4],
            payload: b"payload".to_vec(),
            padding_len: 1,
        };
        let expected_len = TRANSPORT_PACKET_OVERHEAD + envelope.encode_with_prefix(0).len();

        let packet = encode_transport_packet(&key, &mut random, envelope);

        assert_eq!(packet.len(), expected_len);
        assert_eq!(
            decode_transport_packet(&key, packet).unwrap().payload,
            b"payload"
        );
    }
}
