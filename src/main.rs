use clap::{Arg, Command};
use env_logger::Env;
use futures::future::join_all;
use log::{debug, info};
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{mpsc, RwLock};

mod connections;
mod local;
mod packets;
mod remote;
mod rkcp;
mod routing;
mod utils;

use connections::Connections;
use local::{endpoint_from, endpoint_to};
use packets::{TCPPacket, UDPPacket};
use remote::{endpoint_in, endpoint_out, forward_in, forward_out};

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
            forward_in(udp_socket, udp_tx).await;
        });

        // Output thread
        tokio::spawn(async move {
            forward_out(udp_socket_outs, &mut udp_rx, send_addr).await;
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
                endpoint_in(udp_socket, udp_tx, connections, tcp_tx).await;
            });
        }

        // TCP listener thread
        {
            let tcp_listener = TcpListener::bind(listen_addr).await?;
            let connections = connections.clone();
            tokio::spawn(async move {
                endpoint_from(tcp_listener, udp_tx, connections, tcp_tx).await;
            });
        }
        // Output thread
        let connections = connections.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                tokio::spawn(async move {
                    endpoint_to(connections, &mut tcp_rx).await;
                });
                tokio::spawn(async move {
                    endpoint_out(udp_socket_outs, &mut udp_rx, send_addr).await;
                });
                let _ = tokio::signal::ctrl_c().await;
            });
        });
        info!("Started RayNet Endpoint");
    }

    tokio::signal::ctrl_c().await?;
    info!("Ctrl-C received, shutting down");
    debug!("{:?}", connections.read().await);
    Ok(())
}
