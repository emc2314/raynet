use aegis::aegis128l::{Aegis128L, Key, Nonce, Tag};
use rand::Rng;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{io, net::SocketAddr};
use tokio::sync::mpsc::{error, Sender};

use crate::utils::now_millis;

#[derive(Debug)]
pub struct RayPacket {
    flag: u8,
    src: u8,
    dst: u8,
    pub kcp: KCPPacket,
}
impl RayPacket {
    pub fn new(flag: u8, src: u8, dst: u8, kcp: KCPPacket) -> Self {
        RayPacket {
            flag,
            src,
            dst,
            kcp,
        }
    }
    pub fn encrypt(&self, key: &Key, out: &mut [u8]) -> usize {
        let nonce: Nonce = rand::thread_rng().gen();
        let cipher = Aegis128L::new(key, &nonce);
        let rsize = self.kcp.data.len() + 35;
        out[0..16].copy_from_slice(&nonce);
        out[32] = self.flag;
        out[33] = self.src;
        out[34] = self.dst;
        out[35..rsize].copy_from_slice(&self.kcp.data);
        let ad = (now_millis() >> 16).to_le_bytes();
        let tag: Tag<16> = cipher.encrypt_in_place(&mut out[32..rsize], &ad);
        out[16..32].copy_from_slice(&tag);
        rsize
    }
    pub fn decrypt(key: &Key, data: &[u8]) -> Result<Self, RayPacketError> {
        if data.len() < 35 {
            return Err(RayPacketError::BufTooShortError);
        }
        let nonce = data[0..16].try_into().unwrap();
        let tag: Tag<16> = data[16..32].try_into().unwrap();
        let cipher = Aegis128L::new(key, &nonce);
        let ts = now_millis();
        let ad = (ts >> 16).to_le_bytes();
        cipher
            .decrypt(&data[32..], &tag, &ad)
            .or_else(|_| {
                let adjusted_ts = if (ts & 0x8000) == 0 {
                    ts - 0x8000
                } else {
                    ts + 0x8000
                };
                let adjusted_ad = (adjusted_ts >> 16).to_le_bytes();
                cipher.decrypt(&data[32..], &tag, &adjusted_ad)
            })
            .map(|m| RayPacket {
                flag: m[0],
                src: m[1],
                dst: m[2],
                kcp: KCPPacket {
                    data: m[3..].to_vec(),
                },
            })
            .map_err(|e| RayPacketError::DecryptError(e))
    }
}

#[derive(Debug)]
pub struct KCPPacket {
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub struct TCPPacket {
    pub data: Vec<u8>,
    pub addr: SocketAddr,
}

#[derive(Debug)]
pub struct KcpOutput {
    tx: Sender<KCPPacket>,
}
impl KcpOutput {
    pub fn new(tx: Sender<KCPPacket>) -> Self {
        KcpOutput { tx }
    }
}
impl io::Write for KcpOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match self.tx.try_send(KCPPacket { data: buf.to_vec() }) {
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
    BufTooShortError,
    DecryptError(aegis::Error),
}
