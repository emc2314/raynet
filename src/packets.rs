use std::{io, net::SocketAddr};

#[derive(Debug)]
pub struct _RayPacket {
    flag: u8,
    src: u8,
    dst: u8,
    iv: [u8; 16],
    data: Vec<u8>,
}
impl _RayPacket {
    fn _from_buf(bytes: &Vec<u8>) -> Result<Self, &'static str> {
        if bytes.len() < 21 {
            return Err("Buf too short");
        }
        Ok(_RayPacket {
            flag: bytes[0],
            src: bytes[1],
            dst: bytes[2],
            iv: bytes[5..21].try_into().unwrap(),
            data: bytes[21..].to_vec(),
        })
    }
    fn _to_buf(&self) -> Vec<u8> {
        let mut buf = vec![0u8; 21 + self.data.len()];
        buf[0] = self.flag;
        buf[1] = self.src;
        buf[2] = self.dst;
        buf[5..21].copy_from_slice(&self.iv);
        buf[21..].copy_from_slice(&self.data);
        buf
    }
}

#[derive(Debug)]
pub struct UDPPacket {
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub struct TCPPacket {
    pub data: Vec<u8>,
    pub addr: SocketAddr,
}

#[derive(Debug)]
pub struct KcpOutput {
    tx: tokio::sync::mpsc::Sender<UDPPacket>,
}
impl KcpOutput {
    pub fn new(tx: tokio::sync::mpsc::Sender<UDPPacket>) -> Self {
        KcpOutput { tx }
    }
}
impl std::io::Write for KcpOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tx
            .try_send(UDPPacket { data: buf.to_vec() })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
            .map(|_| buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct KcpRecv {
    tx: tokio::sync::mpsc::Sender<TCPPacket>,
    pub addr: SocketAddr,
}
impl KcpRecv {
    pub fn new(tx: tokio::sync::mpsc::Sender<TCPPacket>, addr: SocketAddr) -> Self {
        KcpRecv { tx, addr }
    }
    pub async fn send(
        &self,
        value: &[u8],
    ) -> Result<(), tokio::sync::mpsc::error::SendError<TCPPacket>> {
        self.tx
            .send(TCPPacket {
                data: value.to_vec(),
                addr: self.addr,
            })
            .await
    }
}
