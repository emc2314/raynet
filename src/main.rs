use clap::{Arg, Command};
use rand::prelude::IteratorRandom;
use rand::Rng;
use rand::SeedableRng;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{Mutex, RwLock};

#[tokio::main]
async fn main() -> io::Result<()> {
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

    let listen_addr: &String = matches.get_one("listen").unwrap();
    let send_addr: Vec<String> = matches
        .get_many("send")
        .unwrap()
        .map(|str: &String| String::from(str))
        .collect::<Vec<String>>();

    if !matches.get_flag("endpoint") {
        let udp_socket = UdpSocket::bind(listen_addr).await?;
        let mut buf = vec![0u8; 65535];
        loop {
            let (size, src) = udp_socket.recv_from(&mut buf).await?;
            println!("Received {} bytes from {}", size, src);

            let remote = &send_addr[rand::thread_rng().gen_range(0..send_addr.len())];
            let sent_size = udp_socket.send_to(&buf[..size], remote).await?;
            println!("Sent {} bytes to {}", sent_size, remote);
        }
    } else {
        let udp_socket = UdpSocket::bind(listen_addr).await?;
        let udp_socket = Arc::new(udp_socket);

        let udp_read = udp_socket.clone();
        let udp_write = Arc::new(Mutex::new(udp_socket.clone()));
        let connections = Arc::new(RwLock::new(HashMap::<
            _,
            Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
        >::new()));
        tokio::spawn({
            let mut rng = rand::rngs::StdRng::from_seed(rand::rngs::OsRng.gen());
            let udp_read = udp_read.clone();
            let connections = connections.clone();
            async move {
                let mut buf = vec![0u8; 65535];
                loop {
                    let (size, _src) = { udp_read.recv_from(&mut buf).await? };
                    if let Some((_cid, tcp_write)) =
                        { connections.read().await.iter().choose(&mut rng) }
                    {
                        loop {
                            let tcp_write = tcp_write.lock().await;
                            tcp_write.writable().await?;
                            match tcp_write.try_write(&buf[..size]) {
                                Ok(n) => {
                                    println!(
                                        "Forwarded {} bytes to TCP stream {} from {}",
                                        n, _src, _cid
                                    );
                                    break;
                                }
                                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                    continue;
                                }
                                Err(e) => {
                                    println!("Writing to TCP connection err: {}", e);
                                    break;
                                }
                            }
                        }
                    }
                }
                #[allow(unreachable_code)]
                Result::<(), std::io::Error>::Ok(())
            }
        });

        let tcp_listener = TcpListener::bind(listen_addr).await?;
        loop {
            let (tcp_stream, addr) = tcp_listener.accept().await?;
            let (tcp_read, tcp_write) = tcp_stream.into_split();
            println!("New TCP connection: {}", addr);

            let client_id = addr.to_string();
            connections
                .write()
                .await
                .insert(client_id.clone(), Arc::new(Mutex::new(tcp_write)));

            tokio::spawn({
                let udp_write = udp_write.clone();
                let connections = connections.clone();
                let send_addr = send_addr.clone();
                async move {
                    let mut buf = vec![0u8; 65535];
                    loop {
                        tcp_read.readable().await?;
                        match tcp_read.try_read(&mut buf) {
                            Ok(0) => {
                                println!("TCP connection closed: {}", addr);
                                break;
                            }
                            Ok(len) => {
                                println!("Received {} bytes from {}", len, addr);
                                let remote =
                                    &send_addr[rand::thread_rng().gen_range(0..send_addr.len())];
                                let mut cur = 0;
                                while len > cur {
                                    let size = udp_write
                                        .lock()
                                        .await
                                        .send_to(&buf[cur..std::cmp::min(cur + 1200, len)], remote)
                                        .await?;
                                    println!("Sent {} bytes to {}", size, remote);
                                    cur += size;
                                }
                            }
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                continue;
                            }
                            Err(e) => {
                                eprintln!("Failed to read from TCP stream: {}", e);
                                break;
                            }
                        }
                    }
                    connections.write().await.remove(&client_id);
                    Result::<(), std::io::Error>::Ok(())
                }
            });
        }
    }
}
