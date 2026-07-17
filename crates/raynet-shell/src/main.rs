use clap::Parser;
use env_logger::Env;
use log::info;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::{fs, io};

mod channels;
mod config;
mod endpoint;
mod relay;
mod remote;
mod transport;
mod utils;

use channels::channels;
use config::{ChannelConfig, FileConfig, build_route};
use raynet_shell_plugins::channel_udp::{ForwardUdp, ReverseUdp};
use raynet_shell_plugins::proxy_socks5::Socks5Proxy;
use raynet_shell_plugins::{ChannelReceiver, ChannelSendFailure, ChannelSender};
use tokio::sync::mpsc;

use mimalloc::MiMalloc;
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Parser)]
#[command(name = "RayNet")]
#[command(version = "0.1.0")]
#[command(about = "Cross the fire")]
struct CliConfig {
    #[arg(short, long)]
    endpoint: bool,
    #[arg(short, long, value_name = "ADDRESS")]
    listen: Option<String>,
    #[arg(long)]
    relay_key: Option<String>,
    #[arg(long)]
    endpoint_key: Option<String>,
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
    let relay_psk = cli
        .relay_key
        .or(file_config.relay_key)
        .expect("relay key is required");
    let channel_key = blake3::derive_key("RayNet channel PSK v1", relay_psk.as_bytes());
    let envelope_key = blake3::derive_key("RayNet envelope PSK v1", relay_psk.as_bytes())[..16]
        .try_into()
        .unwrap();
    let (fail_tx, fail_rx) = mpsc::channel::<ChannelSendFailure>(256);
    let (channel_senders, channel_receivers) =
        build_channels(file_config.channels, channel_key, fail_tx).await?;
    let channels = channels(channel_senders, channel_receivers, fail_rx);

    if !endpoint {
        relay::run(channels, envelope_key, file_config.core.relay_tuning());
    } else {
        let route = file_config.route.map(build_route);
        let listen_addr: SocketAddr = cli
            .listen
            .or(file_config.listen)
            .unwrap_or_else(|| "[::0]:8443".to_string())
            .to_socket_addrs()
            .expect("Unable to resolve listen address")
            .next()
            .unwrap();
        let endpoint_psk = cli
            .endpoint_key
            .or(file_config.endpoint_key)
            .expect("endpoint key is required");
        let message_key = blake3::derive_key("RayNet endpoint PSK v1", endpoint_psk.as_bytes())
            [..16]
            .try_into()
            .unwrap();
        let (proxy_listener, proxy) = Socks5Proxy::bind(listen_addr).await?;
        endpoint::run(
            channels,
            envelope_key,
            message_key,
            route,
            file_config.core.endpoint_tuning(),
            proxy_listener,
            std::sync::Arc::new(proxy),
        )
    }

    tokio::signal::ctrl_c().await?;
    info!("Ctrl-C received, shutting down");
    Ok(())
}

async fn build_channels(
    configs: Vec<ChannelConfig>,
    channel_key: [u8; 32],
    failures: mpsc::Sender<ChannelSendFailure>,
) -> io::Result<(Vec<ChannelSender>, Vec<ChannelReceiver>)> {
    let mut senders = Vec::new();
    let mut receivers = Vec::new();
    for config in configs {
        match config {
            ChannelConfig::Tx {
                id,
                bind,
                destination,
            } => senders.push(
                ForwardUdp::sender(
                    id,
                    bind.resolve()?,
                    destination.resolve()?,
                    failures.clone(),
                )
                .await?,
            ),
            ChannelConfig::Rx { id, bind } => {
                receivers.push(ForwardUdp::receiver(id, bind.resolve()?).await?)
            }
            ChannelConfig::RevTx { id, bind } => senders.push(
                ReverseUdp::sender(id, bind.resolve()?, channel_key, failures.clone()).await?,
            ),
            ChannelConfig::RevRx {
                id,
                bind,
                destination,
            } => receivers.push(
                ReverseUdp::receiver(id, bind.resolve()?, destination.resolve()?, channel_key)
                    .await?,
            ),
        }
    }
    Ok((senders, receivers))
}
