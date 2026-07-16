use std::net::SocketAddr;

use raynet_shell_plugins::proxy_socks5::Socks5Proxy;
use raynet_shell_plugins::{ProxyMessage, ProxyPlugin, ProxySession};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn socks5_entry_to_exit() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = target.accept().await.unwrap();
        let mut bytes = [0; 4];
        stream.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"ping");
        stream.write_all(b"pong").await.unwrap();
    });

    let (mut client, status) = connect(target_addr).await;
    assert_eq!(status, 0);

    client.write_all(b"ping").await.unwrap();
    let mut bytes = [0; 4];
    client.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"pong");
}

#[tokio::test]
async fn socks5_reports_connect_failure() {
    let (_, status) = connect("127.0.0.1:0".parse().unwrap()).await;
    assert_eq!(status, 5);
}

#[tokio::test]
async fn socks5_preserves_half_close() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = target.accept().await.unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"request");
        stream.write_all(b"response").await.unwrap();
    });

    let (mut client, status) = connect(target_addr).await;
    assert_eq!(status, 0);
    client.write_all(b"request").await.unwrap();
    client.shutdown().await.unwrap();
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(1), client.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&bytes, b"response");
}

async fn connect(target: SocketAddr) -> (TcpStream, u8) {
    let (mut listener, proxy) = Socks5Proxy::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let mut client = TcpStream::connect(listener.local_addr()).await.unwrap();
    client.write_all(&[5, 1, 0]).await.unwrap();
    let mut method = [0; 2];
    client.read_exact(&mut method).await.unwrap();
    assert_eq!(method, [5, 0]);
    client.write_all(&connect_request(target)).await.unwrap();
    bridge(listener.accept().await.unwrap(), proxy.open());
    let mut reply = [0; 10];
    client.read_exact(&mut reply).await.unwrap();
    (client, reply[1])
}

fn bridge(left: ProxySession, right: ProxySession) {
    let (left_input, left_output) = left.split();
    let (right_input, right_output) = right.split();
    tokio::spawn(pipe(left_output, right_input));
    tokio::spawn(pipe(right_output, left_input));
}

async fn pipe(mut output: mpsc::Receiver<ProxyMessage>, input: mpsc::Sender<ProxyMessage>) {
    while let Some(message) = output.recv().await {
        let closed = matches!(message, ProxyMessage::Close(_));
        if input.send(message).await.is_err() || closed {
            return;
        }
    }
}

fn connect_request(target: SocketAddr) -> Vec<u8> {
    let SocketAddr::V4(target) = target else {
        unreachable!()
    };
    let mut request = vec![5, 1, 0, 1];
    request.extend(target.ip().octets());
    request.extend(target.port().to_be_bytes());
    request
}
