use clap::{Arg, Command};
use env_logger::Env;
use futures::future::join_all;
use kcp::Kcp;
use log::debug;
use rand::distributions::{Distribution, WeightedIndex};
use rand::prelude::IteratorRandom;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{mpsc, Mutex, RwLock};

#[derive(Debug)]
enum RawPacket {
    UDP(Vec<u8>, SocketAddr),
    TCP(Vec<u8>, SocketAddr),
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
        .expect("Unable to resolve listen address").next().unwrap();
    let send_addr: Vec<SocketAddr> = matches
        .get_many::<String>("send")
        .unwrap()
        .map(|str| str.to_socket_addrs().expect("Unable to resolve send address").next().unwrap())
        .collect::<Vec<SocketAddr>>();

    let (tx, mut rx) = mpsc::channel::<RawPacket>(65536);
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
                        if let Err(e) = tx.send(RawPacket::UDP(buf[..size].to_vec(), src)).await {
                            eprintln!("Failed to send to channel: {}", e);
                        }
                    }
                    Err(e) => eprintln!("Failed to receive from UDP: {}", e),
                }
            }
        });

        // Output thread
        tokio::spawn(async move {
            while let Some(packet) = rx.recv().await {
                match packet {
                    RawPacket::UDP(data, _) => {
                        let remote = send_addr.iter().choose(&mut rand::thread_rng()).unwrap();
                        let udp_socket_out = udp_socket_outs
                            .iter()
                            .choose(&mut rand::thread_rng())
                            .unwrap();
                        match udp_socket_out.send_to(&data, remote).await {
                            Ok(sent_size) => debug!("Sent {} bytes to {}", sent_size, remote),
                            Err(e) => eprintln!("Failed to send to UDP: {}", e),
                        }
                    }
                    _ => {}
                }
            }
        });
    } else {
        let connections = Arc::new(RwLock::new(HashMap::<
            _,
            Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
        >::new()));

        // UDP input thread
        let tx_udp = tx.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                match udp_socket.recv_from(&mut buf).await {
                    Ok((size, src)) => {
                        debug!("Received {} bytes from UDP {}", size, src);
                        if let Err(e) = tx_udp.send(RawPacket::UDP(buf[..size].to_vec(), src)).await
                        {
                            eprintln!("Failed to send to channel: {}", e);
                        }
                    }
                    Err(e) => eprintln!("Failed to receive from UDP: {}", e),
                }
            }
        });

        // TCP listener thread
        let tcp_listener = TcpListener::bind(listen_addr).await?;
        let tx_tcp = tx.clone();
        let connections_tcp = connections.clone();
        tokio::spawn(async move {
            loop {
                match tcp_listener.accept().await {
                    Ok((tcp_stream, _cid)) => {
                        let (tcp_read, tcp_write) = tcp_stream.into_split();
                        debug!("New TCP connection: {}", _cid);
                        {
                            connections_tcp
                                .write()
                                .await
                                .insert(_cid, Arc::new(Mutex::new(tcp_write)));
                        }

                        let tx_tcp_clone = tx_tcp.clone();
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 65535];
                            loop {
                                match tcp_read.try_read(&mut buf) {
                                    Ok(0) => {
                                        debug!("TCP connection closed: {}", _cid);
                                        break;
                                    }
                                    Ok(len) => {
                                        debug!("Received {} bytes from TCP {}", len, _cid);
                                        if let Err(e) = tx_tcp_clone
                                            .send(RawPacket::TCP(buf[..len].to_vec(), _cid))
                                            .await
                                        {
                                            eprintln!("Failed to send to channel: {}", e);
                                        }
                                    }
                                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                        tokio::time::sleep(tokio::time::Duration::from_millis(1))
                                            .await;
                                    }
                                    Err(e) => {
                                        eprintln!("Failed to read from TCP stream: {}", e);
                                        break;
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => eprintln!("Failed to accept TCP connection: {}", e),
                }
            }
        });

        // Output thread
        let connections_out = connections.clone();
        tokio::spawn(async move {
            while let Some(packet) = rx.recv().await {
                match packet {
                    RawPacket::UDP(data, _) => {
                        let chosen_connection = {
                            connections_out
                                .read()
                                .await
                                .iter()
                                .choose(&mut rand::thread_rng())
                                .map(|(cid, tcp_write)| (cid.clone(), Arc::clone(tcp_write)))
                        };
                        if let Some((_cid, tcp_write)) = chosen_connection {
                            let tcp_write = tcp_write.lock().await;
                            match tcp_write.try_write(&data) {
                                Ok(n) => debug!("Forwarded {} bytes to TCP stream {}", n, _cid),
                                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                    tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                                }
                                Err(e) => {
                                    eprintln!("Failed to write to TCP stream {}: {}", _cid, e);
                                    connections_out.write().await.remove(&_cid);
                                }
                            }
                        }
                    }
                    RawPacket::TCP(data, _) => {
                        let mut cur = 0;
                        while data.len() > cur {
                            let remote = send_addr.iter().choose(&mut rand::thread_rng()).unwrap();
                            let udp_socket_out = udp_socket_outs
                                .iter()
                                .choose(&mut rand::thread_rng())
                                .unwrap();
                            let size = udp_socket_out
                                .send_to(&data[cur..std::cmp::min(data.len(), cur + 1200)], remote)
                                .await
                                .expect("Failed to send to UDP: {}");
                            debug!("Sent {} bytes to UDP {}", size, remote);
                            cur += size;
                        }
                    }
                }
            }
        });
    }

    tokio::signal::ctrl_c().await?;
    println!("Ctrl-C received, shutting down");
    Ok(())
}
