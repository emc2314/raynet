use aegis::aegis128l::{Aegis128L, Key, Nonce, Tag};
use num_enum::TryFromPrimitive;
use rand::Rng;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{io, net::SocketAddr};
use tokio::sync::mpsc::{error, Sender};

use crate::utils::{now_millis, NonceFilter};

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
    pub async fn decrypt(
        key: &Key,
        data: &[u8],
        filter: &NonceFilter,
    ) -> Result<Self, RayPacketError> {
        if data.len() < Self::HEADER_SIZE || data.len() > 4096 {
            return Err(RayPacketError::BufLengthError);
        }
        let nonce = data[0..16].try_into().unwrap();
        if !filter.check_and_set(nonce).await {
            return Err(RayPacketError::NonceReuseError);
        }
        let tag: Tag<16> = data[16..32].try_into().unwrap();
        let cipher = Aegis128L::new(key, nonce);
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

#[derive(Debug)]
pub struct KcpOutput {
    tx: Sender<DataPacket>,
}
impl KcpOutput {
    pub fn new(tx: Sender<DataPacket>) -> Self {
        KcpOutput { tx }
    }
}
impl io::Write for KcpOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match self.tx.try_send(DataPacket { data: buf.to_vec() }) {
                Ok(_) => return Ok(buf.len()),
                Err(error::TrySendError::Full(_)) => {
                    std::thread::yield_now();
                }
                Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e)),
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl tokio::io::AsyncWrite for KcpOutput {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Poll::Ready(<Self as std::io::Write>::write(self.get_mut(), buf))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

#[derive(Debug)]
pub struct KcpRecv {
    tx: Sender<TCPPacket>,
    pub addr: SocketAddr,
}
impl KcpRecv {
    pub fn new(tx: Sender<TCPPacket>, addr: SocketAddr) -> Self {
        KcpRecv { tx, addr }
    }
    pub async fn send(&self, value: &[u8]) -> Result<(), error::SendError<TCPPacket>> {
        self.tx
            .send(TCPPacket {
                data: value.to_vec(),
                addr: self.addr,
            })
            .await
    }
    pub fn try_send(&self, value: &[u8]) -> Result<(), error::TrySendError<TCPPacket>> {
        self.tx.try_send(TCPPacket {
            data: value.to_vec(),
            addr: self.addr,
        })
    }
}

pub enum RayPacketError {
    BufLengthError,
    NonceReuseError,
    DecryptError(aegis::Error),
}
