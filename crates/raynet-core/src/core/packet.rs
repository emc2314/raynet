use aegis::aegis128l::{Aegis128L, Key, Nonce, Tag};
use rand::RngExt;

use crate::core::nonce::NonceFilter;

const HEADER_SIZE: usize = 32;
const MAX_PACKET_SIZE: usize = 64 * 1024 + HEADER_SIZE;

pub fn seal_hop_payload(time_ms: u64, key: &Key, payload: &[u8]) -> Vec<u8> {
    let nonce: Nonce = rand::rng().random();
    let cipher = Aegis128L::new(key, &nonce);
    let mut out = Vec::with_capacity(HEADER_SIZE + payload.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&[0; 16]);
    out.extend_from_slice(payload);
    let ad = hop_ad(time_ms);
    let tag: Tag<16> = cipher.encrypt_in_place(&mut out[HEADER_SIZE..], &ad);
    out[16..32].copy_from_slice(&tag);
    out
}

pub fn open_hop_payload(
    time_ms: u64,
    key: &Key,
    data: &[u8],
    filter: &mut NonceFilter,
) -> Result<Vec<u8>, HopPacketError> {
    if data.len() < HEADER_SIZE || data.len() > MAX_PACKET_SIZE {
        return Err(HopPacketError::InvalidLength);
    }

    let nonce: Nonce = data[0..16]
        .try_into()
        .map_err(|_| HopPacketError::InvalidLength)?;
    let tag: Tag<16> = data[16..32]
        .try_into()
        .map_err(|_| HopPacketError::InvalidLength)?;
    let cipher = Aegis128L::new(key, &nonce);
    let ad = hop_ad(time_ms);

    let payload = cipher
        .decrypt(&data[HEADER_SIZE..], &tag, &ad)
        .or_else(|_| {
            let adjusted_time = if (time_ms & 0x4000) == 0 {
                time_ms.wrapping_sub(0x4000)
            } else {
                time_ms.wrapping_add(0x4000)
            };
            cipher.decrypt(&data[HEADER_SIZE..], &tag, &hop_ad(adjusted_time))
        })
        .map_err(HopPacketError::AuthFailed)?;

    if !filter.check_and_set(time_ms, &nonce) {
        return Err(HopPacketError::NonceReuse);
    }

    Ok(payload)
}

fn hop_ad(time_ms: u64) -> [u8; 8] {
    (time_ms >> 15).to_le_bytes()
}

#[derive(Debug, thiserror::Error)]
pub enum HopPacketError {
    #[error("invalid packet length")]
    InvalidLength,
    #[error("nonce was reused")]
    NonceReuse,
    #[error("packet authentication failed")]
    AuthFailed(aegis::Error),
}
