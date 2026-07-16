use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use raynet_core::{CloseReason, MAX_SESSION_DATA_SIZE};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::{ProxyListener, ProxyMessage, ProxyPlugin, ProxySession};

pub struct Socks5Proxy;

impl Socks5Proxy {
    pub async fn bind(addr: SocketAddr) -> io::Result<(ProxyListener, Self)> {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        let (sessions, session_rx) = mpsc::channel(64);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let sessions = sessions.clone();
                tokio::spawn(async move {
                    let _ = accept_entry(stream, sessions).await;
                });
            }
        });
        Ok((ProxyListener::new(local_addr, session_rx), Self))
    }
}

impl ProxyPlugin for Socks5Proxy {
    fn open(&self) -> ProxySession {
        let (input, input_rx) = mpsc::channel(64);
        let (output, output_rx) = mpsc::channel(64);
        tokio::spawn(async move {
            if run_exit(input_rx, &output).await.is_err() {
                let _ = output.send(ProxyMessage::Close(CloseReason::Reset)).await;
            }
        });
        ProxySession::new(input, output_rx)
    }
}

async fn accept_entry(
    mut stream: TcpStream,
    sessions: mpsc::Sender<ProxySession>,
) -> Result<(), ()> {
    let request = handshake(&mut stream).await?;
    let (input, input_rx) = mpsc::channel(64);
    let (output, output_rx) = mpsc::channel(64);
    sessions
        .send(ProxySession::new(input, output_rx))
        .await
        .map_err(|_| ())?;
    if run_entry(stream, request, input_rx, &output).await.is_err() {
        let _ = output.send(ProxyMessage::Close(CloseReason::Reset)).await;
    }
    Ok(())
}

async fn handshake(stream: &mut TcpStream) -> Result<Vec<u8>, ()> {
    let mut greeting = [0; 2];
    stream.read_exact(&mut greeting).await.map_err(|_| ())?;
    let mut methods = vec![0; greeting[1] as usize];
    stream.read_exact(&mut methods).await.map_err(|_| ())?;
    if greeting[0] != 5 || !methods.contains(&0) {
        stream.write_all(&[5, 0xff]).await.map_err(|_| ())?;
        return Err(());
    }
    stream.write_all(&[5, 0]).await.map_err(|_| ())?;

    let mut header = [0; 4];
    stream.read_exact(&mut header).await.map_err(|_| ())?;
    if header[..3] != [5, 1, 0] {
        stream.write_all(&reply(7, None)).await.map_err(|_| ())?;
        return Err(());
    }
    let mut request = header.to_vec();
    let remaining = match header[3] {
        1 => 6,
        4 => 18,
        3 => {
            let length = stream.read_u8().await.map_err(|_| ())? as usize;
            request.push(length as u8);
            length + 2
        }
        _ => {
            stream.write_all(&reply(8, None)).await.map_err(|_| ())?;
            return Err(());
        }
    };
    let offset = request.len();
    request.resize(offset + remaining, 0);
    stream
        .read_exact(&mut request[offset..])
        .await
        .map_err(|_| ())?;
    Ok(request)
}

async fn run_entry(
    mut stream: TcpStream,
    request: Vec<u8>,
    mut input: mpsc::Receiver<ProxyMessage>,
    output: &mpsc::Sender<ProxyMessage>,
) -> Result<(), ()> {
    output
        .send(ProxyMessage::Write(request))
        .await
        .map_err(|_| ())?;
    let Some(ProxyMessage::Write(reply)) = input.recv().await else {
        return Ok(());
    };
    let status = *reply.get(1).ok_or(())?;
    stream.write_all(&reply).await.map_err(|_| ())?;
    if status != 0 {
        return Ok(());
    }
    transfer(stream, input, output, CloseReason::LocalClosed).await
}

async fn run_exit(
    mut input: mpsc::Receiver<ProxyMessage>,
    output: &mpsc::Sender<ProxyMessage>,
) -> Result<(), ()> {
    let Some(ProxyMessage::Write(request)) = input.recv().await else {
        return Ok(());
    };
    let stream = match TcpStream::connect(target(&request)?).await {
        Ok(stream) => stream,
        Err(_) => {
            output
                .send(ProxyMessage::Write(reply(5, None)))
                .await
                .map_err(|_| ())?;
            output
                .send(ProxyMessage::Close(CloseReason::Reset))
                .await
                .map_err(|_| ())?;
            return Ok(());
        }
    };
    let bound_addr = stream.local_addr().map_err(|_| ())?;
    output
        .send(ProxyMessage::Write(reply(0, Some(bound_addr))))
        .await
        .map_err(|_| ())?;
    transfer(stream, input, output, CloseReason::RemoteClosed).await
}

async fn transfer(
    stream: TcpStream,
    mut input: mpsc::Receiver<ProxyMessage>,
    output: &mpsc::Sender<ProxyMessage>,
    close_reason: CloseReason,
) -> Result<(), ()> {
    let (mut read, mut write) = stream.into_split();
    let mut read_open = true;
    let mut write_open = true;
    let mut bytes = vec![0; MAX_SESSION_DATA_SIZE];

    loop {
        tokio::select! {
            message = input.recv() => match message {
                Some(ProxyMessage::Write(bytes)) if bytes.is_empty() => {
                        write.shutdown().await.map_err(|_| ())?;
                        write_open = false;
                }
                Some(ProxyMessage::Write(bytes)) => write.write_all(&bytes).await.map_err(|_| ())?,
                Some(ProxyMessage::Close(_)) | None => return Ok(()),
            },
            result = read.read(&mut bytes), if read_open => match result {
                Ok(0) => {
                    read_open = false;
                    output.send(ProxyMessage::Write(Vec::new())).await.map_err(|_| ())?;
                }
                Ok(length) => output.send(ProxyMessage::Write(bytes[..length].to_vec())).await.map_err(|_| ())?,
                Err(_) => return Err(()),
            }
        }
        if !read_open && !write_open {
            output
                .send(ProxyMessage::Close(close_reason))
                .await
                .map_err(|_| ())?;
            return Ok(());
        }
    }
}

fn target(request: &[u8]) -> Result<String, ()> {
    if request.get(..3) != Some(&[5, 1, 0]) {
        return Err(());
    }
    let length = match request.get(3) {
        Some(1) if request.len() == 10 => 4,
        Some(4) if request.len() == 22 => 16,
        Some(3) if request.len() >= 7 && request.len() == request[4] as usize + 7 => {
            request[4] as usize
        }
        _ => return Err(()),
    };
    let port = u16::from_be_bytes(request[request.len() - 2..].try_into().unwrap());
    match request[3] {
        1 => Ok(format!(
            "{}:{port}",
            Ipv4Addr::from(<[u8; 4]>::try_from(&request[4..8]).unwrap())
        )),
        4 => Ok(format!(
            "[{}]:{port}",
            Ipv6Addr::from(<[u8; 16]>::try_from(&request[4..20]).unwrap())
        )),
        3 => Ok(format!(
            "{}:{port}",
            std::str::from_utf8(&request[5..5 + length]).map_err(|_| ())?
        )),
        _ => unreachable!(),
    }
}

fn reply(status: u8, addr: Option<SocketAddr>) -> Vec<u8> {
    let addr = addr.unwrap_or_else(|| SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0));
    let mut bytes = vec![5, status, 0];
    match addr.ip() {
        IpAddr::V4(ip) => {
            bytes.push(1);
            bytes.extend(ip.octets());
        }
        IpAddr::V6(ip) => {
            bytes.push(4);
            bytes.extend(ip.octets());
        }
    }
    bytes.extend(addr.port().to_be_bytes());
    bytes
}
