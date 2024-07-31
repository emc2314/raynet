use clap::{Arg, Command};
use env_logger::Env;
use futures::future::join_all;
use kcp::Kcp;
use log::{debug, error, info};
use rand::distributions::{Distribution, WeightedIndex};
use rand::prelude::IteratorRandom;
use rand::Rng;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{mpsc, RwLock};
use tokio::sync::{mpsc, Mutex, RwLock};


#[derive(Debug)]
struct UDPPacket {
    data: Vec<u8>,
}
#[derive(Debug)]
struct TCPPacket {
    data: Vec<u8>,
    conv: u32,
}

#[derive(Debug)]
struct Connections {
    spare: HashMap<SocketAddr, tokio::net::tcp::OwnedWriteHalf>,
    active: HashMap<u32, Arc<tokio::net::tcp::OwnedWriteHalf>>,
}

impl Connections {
    fn new() -> Connections {
        let spare = HashMap::<SocketAddr, tokio::net::tcp::OwnedWriteHalf>::new();
        let active = HashMap::<u32, Arc<tokio::net::tcp::OwnedWriteHalf>>::new();
        Connections { spare, active }
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
        let connections = Arc::new(RwLock::new(Connections::new()));
        let (tcp_tx, mut tcp_rx) = mpsc::channel::<TCPPacket>(65536);

        // UDP input thread
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                match udp_socket.recv_from(&mut buf).await {
                    Ok((size, src)) => {
                        debug!("Received {} bytes from UDP {}", size, src);
                        let conv = Some(0);
                        if let Some(conv) = conv {
                            if let Err(e) = tcp_tx
                                .send(TCPPacket {
                                    data: buf[..size].to_vec(),
                                    conv: conv,
                                })
                                .await
                            {
                                error!("Failed to send to channel: {}", e);
                            }
                        } else {
                            error!("No spare connection");
                        }
                    }
                    Err(e) => error!("Failed to receive from UDP: {}", e),
                }
            }
        });

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
                                connections.write().await.spare.insert(src, tcp_write);
                            }

                            let tx_clone = udp_tx.clone();
                            let connections = connections.clone();
                            tokio::spawn(async move {
                                let mut buf = vec![0u8; 65535];
                                loop {
                                    match tcp_read.try_read(&mut buf) {
                                        Ok(0) => {
                                            info!("TCP connection closed: {}", src);
                                            // TODO: Inform the opposite end
                                            break;
                                        }
                                        Ok(len) => {
                                            debug!("Received {} bytes from TCP {}", len, src);
                                            let con = connections.read().await;
                                            if con.spare.contains_key(&src) {
                                                drop(con);
                                                let mut con = connections.write().await;
                                                if let Some(tcp_write) = con.spare.remove(&src) {
                                                    let conv = 0;
                                                    con.active.insert(conv, tcp_write.into());
                                                    debug!("Assign TCP {} as {}", src, conv);
                                                }
                                            }
                                            if let Err(e) = tx_clone
                                                .send(UDPPacket {
                                                    data: buf[..len].to_vec(),
                                                })
                                                .await
                                            {
                                                error!("Failed to send to channel: {}", e);
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
                                            // TODO: Inform the opposite end
                                            break;
                                        }
                                    }
                                }
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
                    let conv = packet.conv;
                    let tcp_write = {
                        let con = connections.read().await;
                        if con.active.contains_key(&conv) {
                            Some(con.active[&conv].clone())
                        } else {
                            drop(con);
                            let mut con = connections.write().await;
                            match con.spare.iter().next() {
                                Some((&src, _)) => {
                                    let tcp_write: Arc<tokio::net::tcp::OwnedWriteHalf> =
                                        con.spare.remove(&src).unwrap().into();
                                    con.active.insert(conv, tcp_write.clone());
                                    debug!("Assign TCP {} as {}", src, conv);
                                    Some(tcp_write)
                                }
                                None => None,
                            }
                        }
                    };
                    if let Some(tcp_write) = tcp_write {
                        loop {
                            match tcp_write.try_write(&packet.data) {
                                Ok(n) => {
                                    debug!("Forwarded {} bytes to TCP stream {}", n, conv);
                                    break;
                                }
                                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                    tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                                }
                                Err(e) => {
                                    error!("Failed to write to TCP stream {}: {}", conv, e);
                                    connections.write().await.active.remove(&conv);
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
    Ok(())
}
