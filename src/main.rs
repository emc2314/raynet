use clap::Parser;
use env_logger::Env;
use log::{debug, info};
use serde::Deserialize;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::{fs, io};
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

#[derive(Deserialize, Default)]
struct FileConfig {
    endpoint: Option<bool>,
    listen: Option<String>,
    send: Option<Vec<String>>,
    key: Option<String>,
}

#[derive(Parser)]
#[command(name = "RayNet")]
#[command(version = "0.1.0")]
#[command(about = "Cross the fire")]
struct CliConfig {
    #[arg(short, long)]
    endpoint: bool,
    #[arg(short, long, value_name = "ADDRESS")]
    listen: Option<String>,
    #[arg(short, long, value_name = "ADDRESS", num_args = 1.., value_delimiter = ',')]
    send: Option<Vec<String>>,
    #[arg(short, long)]
    key: Option<String>,
    #[arg(short, long, value_name = "CONFIG_FILE")]
    config: Option<String>,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let env = Env::default()
        .filter_or("RAYNET_LOG_LEVEL", "trace")
        .write_style_or("RAYNET_LOG_STYLE", "always");
    env_logger::init_from_env(env);

    let cli = CliConfig::parse();
    let file_config = match cli.config.as_ref() {
        Some(path) => {
            let config_str =
                fs::read_to_string(Path::new(path)).expect("Failed to read config file");
            toml::from_str(&config_str).expect("Failed to parse config file")
        }
        None => FileConfig::default(),
    };

    let endpoint = cli.endpoint || file_config.endpoint.unwrap_or(false);
    let listen_addr: SocketAddr = cli
        .listen
        .or(file_config.listen)
        .unwrap_or_else(|| "[::0]:8443".to_string())
        .to_socket_addrs()
        .expect("Unable to resolve listen address")
        .next()
        .unwrap();
    let nodes = Arc::new(Nodes::new(
        cli.send
            .or(file_config.send)
            .unwrap_or_else(|| vec!["[::1]:8443".to_string()]),
    ));
    let key = blake3::derive_key(
        "RayNet PSK v1",
        cli.key
            .or(file_config.key)
            .unwrap_or_else(|| "19260817".to_string())
            .as_bytes(),
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

    if !endpoint {
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
