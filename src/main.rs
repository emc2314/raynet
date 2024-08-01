use clap::{Arg, Command};
use env_logger::Env;
use futures::future::join_all;
use log::{debug, error, info};
use rand::distributions::{Distribution, WeightedIndex};
use rand::prelude::IteratorRandom;
use rand::Rng;
use rkcp::session::KcpSession;
use rkcp::socket::KcpSocket;
use std::collections::HashMap;
use std::fmt::{self, Debug};
use std::io;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{mpsc, RwLock};

mod packets;
mod rkcp;
mod utils;

use packets::{KcpOutput, KcpRecv, TCPPacket, UDPPacket};

fn _softmax(vec: &[f64], rng: &mut rand::rngs::ThreadRng) -> usize {
    let exps: Vec<f64> = vec.iter().map(|&x| x.exp()).collect();
    let sum: f64 = exps.iter().sum();

    let dist = WeightedIndex::new(exps.iter().map(|&x| x / sum).collect::<Vec<_>>()).unwrap();
    dist.sample(rng)
}

#[derive(Debug)]
struct _NodeInfo {
    nid: u8,
    addr: SocketAddr,
    clc: u64,
    llc: u64,
}

struct Connections {
    cons: HashMap<SocketAddr, tokio::net::tcp::OwnedWriteHalf>,
    convs: HashMap<SocketAddr, u32>,
    kcps: HashMap<u32, Arc<KcpSession>>,
}

impl Connections {
    fn new() -> Connections {
        let cons = HashMap::new();
        let convs = HashMap::new();
        let kcps = HashMap::new();
        Connections { cons, convs, kcps }
    }
}
impl Debug for Connections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connections")
            .field("cons", &self.cons)
            .field("convs", &self.convs)
            .field("kcps", &self.kcps)
            .finish()
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let env = Env::default()
        .filter_or("RAYNET_LOG_LEVEL", "trace")
        .write_style_or("RAYNET_LOG_STYLE", "always");
    env_logger::init_from_env(env);

    let matches = Command::new("RayNet")
        .version("0.1.0")
        .about("Cross the fire")
        .arg(
            Arg::new("listen")
                .short('l')
                .long("listen")
                .value_name("ADDRESS")
                .help("Sets the address to listen on")
                .num_args(1)
                .default_value("[::0]:8443"),
        )
        .arg(
            Arg::new("send")
                .short('s')
                .long("send")
                .value_name("ADDRESS")
                .help("Sets the address to send to")
                .num_args(1..)
                .default_value("[::1]:8443")
                .use_value_delimiter(true)
                .value_delimiter(','),
        )
        .arg(
            Arg::new("endpoint")
                .short('e')
                .long("endpoint")
                .help("Indicate endpoint")
                .num_args(0),
        )
        .get_matches();

    let listen_addr: SocketAddr = matches
        .get_one::<String>("listen")
        .unwrap()
        .to_socket_addrs()
        .expect("Unable to resolve listen address")
        .next()
        .unwrap();
    let send_addr: Vec<SocketAddr> = matches
        .get_many::<String>("send")
        .unwrap()
        .map(|str| {
            str.to_socket_addrs()
                .expect("Unable to resolve send address")
                .next()
                .unwrap()
        })
        .collect::<Vec<SocketAddr>>();

    let connections = Arc::new(RwLock::new(Connections::new()));
    let (udp_tx, mut udp_rx) = mpsc::channel::<UDPPacket>(65536);
    let udp_socket = UdpSocket::bind(listen_addr).await?;
    let udp_socket_outs: Vec<UdpSocket> = join_all(
        (0..32)
            .map(|_| UdpSocket::bind("[::0]:0"))
            .collect::<Vec<_>>(),
    )
    .await
    .into_iter()
    .collect::<Result<_, _>>()?;
    if !matches.get_flag("endpoint") {
        // Input thread
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                match udp_socket.recv_from(&mut buf).await {
                    Ok((size, src)) => {
                        debug!("Received {} bytes from {}", size, src);
                        if let Err(e) = udp_tx
                            .send(UDPPacket {
                                data: buf[..size].to_vec(),
                            })
                            .await
                        {
                            error!("Failed to send to channel: {}", e);
                        }
                    }
                    Err(e) => error!("Failed to receive from UDP: {}", e),
                }
            }
        });

        // Output thread
        tokio::spawn(async move {
            while let Some(packet) = udp_rx.recv().await {
                let remote = send_addr.iter().choose(&mut rand::thread_rng()).unwrap();
                let udp_socket_out = udp_socket_outs
                    .iter()
                    .choose(&mut rand::thread_rng())
                    .unwrap();
                match udp_socket_out.send_to(&packet.data, remote).await {
                    Ok(sent_size) => debug!("Sent {} bytes to {}", sent_size, remote),
                    Err(e) => error!("Failed to send to UDP: {}", e),
                }
            }
        });
        info!("Started RayNet Forwarder");
    } else {
        let (tcp_tx, mut tcp_rx) = mpsc::channel::<TCPPacket>(65536);

        // UDP input thread
        {
            let connections = connections.clone();
            let udp_tx = udp_tx.clone();
            let tcp_tx = tcp_tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65535];
                loop {
                    match udp_socket.recv_from(&mut buf).await {
                        Ok((size, src)) => {
                            debug!("Received {} bytes from UDP {}", size, src);
                            let conv = kcp::get_conv(&buf);
                            let session = {
                                let con = connections.read().await;
                                if con.kcps.contains_key(&conv) {
                                    Some(con.kcps[&conv].clone())
                                } else {
                                    drop(con);
                                    let mut con = connections.write().await;
                                    con.cons
                                        .keys()
                                        .cloned()
                                        .collect::<Vec<_>>()
                                        .iter()
                                        .find_map(|addr| {
                                            if !con.convs.contains_key(addr) {
                                                let config = Default::default();
                                                let kcp = KcpSocket::new(
                                                    &config,
                                                    conv,
                                                    KcpOutput::new(udp_tx.clone()),
                                                    false,
                                                )
                                                .unwrap();
                                                let session = KcpSession::new_shared(
                                                    kcp,
                                                    KcpRecv::new(tcp_tx.clone(), addr.clone()),
                                                    config.session_expire,
                                                    false,
                                                );
                                                con.convs.insert(addr.clone(), conv);
                                                con.kcps.insert(conv, session.clone());
                                                info!("Assign TCP {} as {}", addr, conv);
                                                Some(session)
                                            } else {
                                                None
                                            }
                                        })
                                }
                            };
                            if let Some(session) = session {
                                if let Err(_) = session.input(&buf[..size]).await {
                                    error!("KCP session {} closed when input", conv);
                                    let mut con = connections.write().await;
                                    if let Some(conv) = con.convs.remove(&session.kcp_recv().addr) {
                                        con.kcps.remove(&conv);
                                    }
                                }
                            } else {
                                error!("No spare connection");
                            }
                        }
                        Err(e) => error!("Failed to receive from UDP: {}", e),
                    }
                }
            });
        }

        // TCP listener thread
        {
            let tcp_listener = TcpListener::bind(listen_addr).await?;
            let connections = connections.clone();
            tokio::spawn(async move {
                loop {
                    match tcp_listener.accept().await {
                        Ok((tcp_stream, src)) => {
                            let (tcp_read, tcp_write) = tcp_stream.into_split();
                            info!("New TCP connection: {}", src);
                            {
                                connections.write().await.cons.insert(src, tcp_write);
                            }

                            let connections = connections.clone();
                            let udp_tx = udp_tx.clone();
                            let tcp_tx = tcp_tx.clone();
                            tokio::spawn(async move {
                                let mut buf = vec![0u8; 65535];
                                loop {
                                    match tcp_read.try_read(&mut buf) {
                                        Ok(0) => {
                                            info!("TCP connection closed: {}", src);
                                            let mut con = connections.write().await;
                                            if let Some(conv) = con.convs.remove(&src) {
                                                con.kcps.remove(&conv);
                                            }
                                            // TODO: Inform the opposite end
                                            break;
                                        }
                                        Ok(len) => {
                                            debug!("Received {} bytes from TCP {}", len, src);
                                            let session = {
                                                let con = connections.read().await;
                                                if let Some(conv) = con.convs.get(&src) {
                                                    Some(con.kcps[conv].clone())
                                                } else {
                                                    drop(con);
                                                    let mut con = connections.write().await;
                                                    let conv = rand::thread_rng().gen::<u32>();
                                                    con.cons
                                                        .keys()
                                                        .cloned()
                                                        .collect::<Vec<_>>()
                                                        .iter()
                                                        .find_map(|addr| {
                                                            if !con.convs.contains_key(addr) {
                                                                let config = Default::default();
                                                                let kcp = KcpSocket::new(
                                                                    &config,
                                                                    conv,
                                                                    KcpOutput::new(udp_tx.clone()),
                                                                    false,
                                                                )
                                                                .unwrap();
                                                                let session =
                                                                    KcpSession::new_shared(
                                                                        kcp,
                                                                        KcpRecv::new(
                                                                            tcp_tx.clone(),
                                                                            addr.clone(),
                                                                        ),
                                                                        config.session_expire,
                                                                        true,
                                                                    );
                                                                con.convs
                                                                    .insert(addr.clone(), conv);
                                                                con.kcps
                                                                    .insert(conv, session.clone());
                                                                info!(
                                                                    "Assign TCP {} as {}",
                                                                    addr, conv
                                                                );
                                                                Some(session)
                                                            } else {
                                                                None
                                                            }
                                                        })
                                                }
                                            };
                                            if let Some(session) = session {
                                                if let Err(_) = session.send(&buf[..len]).await {
                                                    error!(
                                                        "KCP session {} closed when send",
                                                        session.conv()
                                                    );
                                                    let mut con = connections.write().await;
                                                    if let Some(conv) = con.convs.remove(&src) {
                                                        con.kcps.remove(&conv);
                                                    }
                                                }
                                            } else {
                                                error!("No spare connection");
                                            }
                                        }
                                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                            tokio::time::sleep(tokio::time::Duration::from_millis(
                                                1,
                                            ))
                                            .await;
                                        }
                                        Err(e) => {
                                            error!("Failed to read from TCP stream: {}", e);
                                            let mut con = connections.write().await;
                                            if let Some(conv) = con.convs.remove(&src) {
                                                con.kcps.remove(&conv);
                                            }
                                            // TODO: Inform the opposite end
                                            break;
                                        }
                                    }
                                }
                                connections.write().await.cons.remove(&src);
                            });
                        }
                        Err(e) => error!("Failed to accept TCP connection: {}", e),
                    }
                }
            });
        }
        // Output thread
        {
            let connections = connections.clone();
            tokio::spawn(async move {
                while let Some(packet) = tcp_rx.recv().await {
                    let addr = packet.addr;
                    if let Some(tcp_write) = connections.read().await.cons.get(&addr) {
                        loop {
                            match tcp_write.try_write(&packet.data) {
                                Ok(n) => {
                                    debug!("Forwarded {} bytes to TCP stream {}", n, addr);
                                    break;
                                }
                                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                    tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                                }
                                Err(e) => {
                                    error!("Failed to write to TCP stream {}: {}", addr, e);
                                    let mut con = connections.write().await;
                                    if let Some(conv) = con.convs.remove(&addr) {
                                        con.kcps.remove(&conv);
                                    }
                                    con.cons.remove(&addr);
                                    break;
                                }
                            }
                        }
                    } else {
                        error!("No spare connection");
                    }
                }
            });
        }
        tokio::spawn(async move {
            while let Some(packet) = udp_rx.recv().await {
                let mut cur = 0;
                while packet.data.len() > cur {
                    let remote = send_addr.iter().choose(&mut rand::thread_rng()).unwrap();
                    let udp_socket_out = udp_socket_outs
                        .iter()
                        .choose(&mut rand::thread_rng())
                        .unwrap();
                    let size = udp_socket_out
                        .send_to(
                            &packet.data[cur..std::cmp::min(packet.data.len(), cur + 1200)],
                            remote,
                        )
                        .await
                        .expect("Failed to send to UDP: {}");
                    debug!("Sent {} bytes to UDP {}", size, remote);
                    cur += size;
                }
            }
        });
        info!("Started RayNet Endpoint");
    }

    tokio::signal::ctrl_c().await?;
    info!("Ctrl-C received, shutting down");
    debug!("{:?}", connections.read().await);
    Ok(())
}
