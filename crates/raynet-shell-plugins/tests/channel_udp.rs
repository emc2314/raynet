use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use raynet_shell_plugins::channel_udp::{ForwardUdp, ReverseUdp};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

fn free_addr() -> SocketAddr {
    free_addrs(1)[0]
}

fn free_addrs(count: usize) -> Vec<SocketAddr> {
    let sockets = (0..count)
        .map(|_| UdpSocket::bind("127.0.0.1:0").unwrap())
        .collect::<Vec<_>>();
    sockets
        .iter()
        .map(|socket| socket.local_addr().unwrap())
        .collect()
}

#[tokio::test]
async fn forward_udp() {
    let destinations = free_addrs(2);
    let (fail_tx, _fail_rx) = mpsc::channel(16);
    let mut receiver = ForwardUdp::receiver(7, destinations.clone()).await.unwrap();
    let sender = ForwardUdp::sender(
        7,
        vec!["127.0.0.1:0".parse().unwrap(); 2],
        destinations,
        fail_tx,
    )
    .await
    .unwrap();

    sender.send(b"forward".to_vec()).await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap(),
        b"forward"
    );
}

#[tokio::test]
async fn reverse_udp() {
    let destinations = free_addrs(2);
    let key = [42; 32];
    let (fail_tx, _fail_rx) = mpsc::channel(16);
    let sender = ReverseUdp::sender(9, destinations.clone(), key, fail_tx)
        .await
        .unwrap();
    let mut receiver = ReverseUdp::receiver(
        9,
        vec!["127.0.0.1:0".parse().unwrap(); 2],
        destinations,
        key,
    )
    .await
    .unwrap();

    for _ in 0..100 {
        if sender.send(b"reverse".to_vec()).await.is_ok() {
            match timeout(Duration::from_millis(50), receiver.recv()).await {
                Ok(Some(bytes)) if bytes == b"reverse" => return,
                _ => {}
            }
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!();
}

#[tokio::test]
async fn reverse_udp_rejects_another_key() {
    let addr = free_addr();
    let (fail_tx, mut fail_rx) = mpsc::channel(16);
    let sender = ReverseUdp::sender(9, vec![addr], [1; 32], fail_tx)
        .await
        .unwrap();
    let _receiver =
        ReverseUdp::receiver(9, vec!["127.0.0.1:0".parse().unwrap()], vec![addr], [2; 32])
            .await
            .unwrap();

    sleep(Duration::from_millis(50)).await;
    sender.send(b"wrong key".to_vec()).await.unwrap();
    let (channel_id, bytes) = timeout(Duration::from_secs(1), fail_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(channel_id, 9);
    assert_eq!(bytes, b"wrong key");
}
