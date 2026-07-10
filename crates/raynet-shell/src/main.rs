use clap::Parser;
use env_logger::Env;
use log::{debug, info};
use serde::Deserialize;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::{fs, io};
use tokio::net::UdpSocket;

mod channels;
mod connections;
mod endpoint;
mod local;
mod relay;
mod remote;
mod transport;
mod utils;

use channels::UdpChannels;

use mimalloc::MiMalloc;
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

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
    let channels = Arc::new(UdpChannels::from_names(
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

    let udp_socket = Arc::new(UdpSocket::bind(listen_addr).await?);

    let connections = if !endpoint {
        relay::run(udp_socket, channels, key).await?;
        None
    } else {
        Some(endpoint::run(listen_addr, udp_socket, channels, key).await?)
    };

    tokio::signal::ctrl_c().await?;
    info!("Ctrl-C received, shutting down");
    if let Some(connections) = connections {
        debug!("{:?}", connections.read().await);
    }
    Ok(())
}
