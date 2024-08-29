use clap::{Arg, Command};
use env_logger::Env;
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
use packets::{KCPPacket, RayPacket, TCPPacket};
use remote::{endpoint_in, endpoint_out, forward_in, forward_out, stat_request, stat_update};
use routing::{Nodes, WeightInfo};

#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

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
        .arg(
            Arg::new("key")
                .short('k')
                .long("key")
                .help("Pre shared key")
                .num_args(1)
                .default_value("19260817"),
        )
        .get_matches();

    let listen_addr: SocketAddr = matches
        .get_one::<String>("listen")
        .unwrap()
        .to_socket_addrs()
        .expect("Unable to resolve listen address")
        .next()
        .unwrap();
    let nodes = Arc::new(Nodes::new(
        matches
            .get_many::<String>("send")
            .unwrap()
            .map(|str| {
                str.to_socket_addrs()
                    .expect("Unable to resolve send address")
                    .next()
                    .unwrap()
            })
            .collect::<Vec<SocketAddr>>(),
    ));
    let key = blake3::derive_key(
        "RayNet PSK v1",
        matches.get_one::<String>("key").unwrap().as_bytes(),
    )[..16]
        .try_into()
        .unwrap();

    let connections = Arc::new(RwLock::new(Connections::new()));
    let udp_socket = Arc::new(UdpSocket::bind(listen_addr).await?);

    let (stat_tx, stat_rx) = mpsc::channel::<WeightInfo>(32);
    {
        let nodes = nodes.clone();
        let udp_socket = udp_socket.clone();
        tokio::spawn(async move {
            stat_request(nodes, &key, udp_socket).await;
        });
    }
    {
        let nodes = nodes.clone();
        tokio::spawn(async move {
            stat_update(nodes, stat_rx).await;
        });
    }

    if !matches.get_flag("endpoint") {
        let (ray_tx, ray_rx) = mpsc::channel::<RayPacket>(65536);
        // Input thread
        tokio::spawn(async move {
            forward_in(udp_socket, ray_tx, &key, stat_tx).await;
        });

        // Output thread
        tokio::spawn(async move {
            forward_out(ray_rx, nodes, &key).await;
        });
        info!("Started RayNet Forwarder");
    } else {
        let (kcp_tx, kcp_rx) = mpsc::channel::<KCPPacket>(65536);
        let (tcp_tx, tcp_rx) = mpsc::channel::<TCPPacket>(65536);

        // UDP input thread
        {
            let connections = connections.clone();
            let kcp_tx = kcp_tx.clone();
            let tcp_tx = tcp_tx.clone();
            tokio::spawn(async move {
                endpoint_in(udp_socket, kcp_tx, connections, tcp_tx, &key, stat_tx).await;
            });
        }

        // TCP listener thread
        {
            let tcp_listener = TcpListener::bind(listen_addr).await?;
            let connections = connections.clone();
            tokio::spawn(async move {
                endpoint_from(tcp_listener, kcp_tx, connections, tcp_tx).await;
            });
        }
        // Output thread
        let connections = connections.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                tokio::spawn(async move {
                    endpoint_to(connections, tcp_rx).await;
                });
                tokio::spawn(async move {
                    endpoint_out(kcp_rx, nodes, &key).await;
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
