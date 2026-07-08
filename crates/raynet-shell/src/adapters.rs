use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::sync::mpsc::{Sender, error};

use raynet_core::core::packet::{DataPacket, TCPPacket};

/// Tokio mpsc adapter used as KCP output target.
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
                Err(e) => return Err(io::Error::other(e)),
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
