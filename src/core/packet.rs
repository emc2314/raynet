use aegis::aegis128l::{Aegis128L, Key, Nonce, Tag};
use num_enum::TryFromPrimitive;
use rand::Rng;
use std::net::SocketAddr;

use crate::core::nonce::NonceFilter;
use crate::utils::now_millis;

#[derive(Debug, Clone, Copy, TryFromPrimitive)]
#[repr(u8)]
pub enum RayPacketType {
    DataPacket,
    StatRequest,
    StatResponse,
}

#[derive(Debug)]
pub struct RayPacket {
    pub ptype: RayPacketType,
    pub data: DataPacket,
}

impl RayPacket {
    const HEADER_SIZE: usize = 33;

    pub fn new(ptype: RayPacketType, data: DataPacket) -> Self {
        RayPacket { ptype, data }
    }

    pub fn encrypt(&self, key: &Key, out: &mut [u8]) -> usize {
        let nonce: Nonce = rand::thread_rng().gen();
        let cipher = Aegis128L::new(key, &nonce);
        let rsize = self.data.data.len() + Self::HEADER_SIZE;
        out[0..16].copy_from_slice(&nonce);
        out[32] = self.ptype as u8;
        out[Self::HEADER_SIZE..rsize].copy_from_slice(&self.data.data);
        let ad = (now_millis() >> 15).to_le_bytes();
        let tag: Tag<16> = cipher.encrypt_in_place(&mut out[32..rsize], &ad);
        out[16..32].copy_from_slice(&tag);
        rsize
    }

    pub fn decrypt(
        key: &Key,
        data: &[u8],
        filter: &mut NonceFilter,
    ) -> Result<Self, RayPacketError> {
        if data.len() < Self::HEADER_SIZE || data.len() > 4096 {
            return Err(RayPacketError::BufLengthError);
        }

        let nonce: Nonce = data[0..16].try_into().unwrap();
        if !filter.check_and_set(&nonce) {
            return Err(RayPacketError::NonceReuseError);
        }

        let tag: Tag<16> = data[16..32].try_into().unwrap();
        let cipher = Aegis128L::new(key, &nonce);
        let ts = now_millis();
        let ad = (ts >> 15).to_le_bytes();

        cipher
            .decrypt(&data[32..], &tag, &ad)
            .or_else(|_| {
                let adjusted_ts = if (ts & 0x4000) == 0 {
                    ts - 0x4000
                } else {
                    ts + 0x4000
                };
                let adjusted_ad = (adjusted_ts >> 15).to_le_bytes();
                cipher.decrypt(&data[32..], &tag, &adjusted_ad)
            })
            .map(|m| RayPacket {
                ptype: m[0].try_into().unwrap(),
                data: DataPacket {
                    data: m[1..].to_vec(),
                },
            })
            .map_err(RayPacketError::DecryptError)
    }
}

#[derive(Debug)]
pub struct DataPacket {
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub struct TCPPacket {
    pub data: Vec<u8>,
    pub addr: SocketAddr,
}

pub enum RayPacketError {
    BufLengthError,
    NonceReuseError,
    DecryptError(aegis::Error),
}
