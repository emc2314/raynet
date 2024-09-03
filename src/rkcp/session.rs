use std::{
    fmt::{self, Debug},
    io::ErrorKind,
    ops::Deref,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};

use byte_string::ByteStr;
use futures_util::{future, ready};
use kcp::KcpResult;
use log::{debug, error, trace, warn};
use spin::Mutex as SpinMutex;
use tokio::{
    sync::{
        mpsc::{self, error::TrySendError},
        Notify,
    },
    time::{self, Instant},
};

use crate::packets::KcpRecv;
use crate::rkcp::socket::KcpSocket;

pub struct KcpSession {
    socket: SpinMutex<KcpSocket>,
    recv: KcpRecv,
    closed: AtomicBool,
    session_expire: Option<Duration>,
    //session_close_notifier: Option<(mpsc::Sender<SocketAddr>, SocketAddr)>,
    input_tx: mpsc::Sender<Vec<u8>>,
    notifier: Notify,
}

impl Drop for KcpSession {
    fn drop(&mut self) {
        warn!(
            "[SESSION] KcpSession conv {} is dropping, closed? {}",
            self.socket.lock().conv(),
            self.closed.load(Ordering::Acquire),
        );
    }
}

impl Debug for KcpSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KcpSession")
            .field("socket", self.socket.lock().deref())
            .field("recv", &self.recv)
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .field("session_expire", &self.session_expire)
            //.field("session_close_notifier", &self.session_close_notifier)
            .field("input_tx", &self.input_tx)
            .field("notifier", &self.notifier)
            .finish()
    }
}

impl KcpSession {
    fn new(
        socket: KcpSocket,
        recv: KcpRecv,
        session_expire: Option<Duration>,
        //session_close_notifier: Option<(mpsc::Sender<SocketAddr>, SocketAddr)>,
        input_tx: mpsc::Sender<Vec<u8>>,
    ) -> KcpSession {
        KcpSession {
            socket: SpinMutex::new(socket),
            closed: AtomicBool::new(false),
            recv,
            session_expire,
            //session_close_notifier,
            input_tx,
            notifier: Notify::new(),
        }
    }

    pub fn new_shared(
        socket: KcpSocket,
        recv: KcpRecv,
        session_expire: Option<Duration>,
        //session_close_notifier: Option<(mpsc::Sender<SocketAddr>, SocketAddr)>,
        is_client: bool,
    ) -> Arc<KcpSession> {
        let (input_tx, mut input_rx) = mpsc::channel(65536);

        let session = Arc::new(KcpSession::new(
            socket,
            recv,
            session_expire,
            //session_close_notifier,
            input_tx,
        ));

        let io_task_handle = {
            let session = session.clone();
            tokio::spawn(async move {
                while !session.closed.load(Ordering::Relaxed) {
                    let input_opt = input_rx.recv().await;
                    if let Some(input_buffer) = input_opt {
                        if input_buffer.len() < kcp::KCP_OVERHEAD {
                            error!(
                                "packet too short, received {} bytes, but at least {} bytes",
                                input_buffer.len(),
                                kcp::KCP_OVERHEAD
                            );
                            continue;
                        }

                        let input_conv = kcp::get_conv(&input_buffer);
                        debug!(
                            "[SESSION] Input recv {} bytes, conv: {}",
                            input_buffer.len(),
                            input_conv
                        );
                        trace!("[SESSION] Input data {:?}", ByteStr::new(&input_buffer));

                        let mut socket = session.socket.lock();

                        // Server may allocate another conv for this client.
                        if !socket.waiting_conv() && socket.conv() != input_conv {
                            warn!(
                                "[SESSION] Input input conv: {} replaces session conv: {}",
                                input_conv,
                                socket.conv()
                            );
                            socket.set_conv(input_conv);
                        }

                        match socket.input(&input_buffer).await {
                            Ok(waked) => {
                                debug!("[SESSION] KCP input {} bytes from channel, waked? {} sender/receiver",
                                                       input_buffer.len(), waked);
                            }
                            Err(err) => {
                                error!("[SESSION] KCP input {} bytes from channel failed, error: {}, input buffer {:?}",
                                                       input_buffer.len(), err, ByteStr::new(&input_buffer));
                            }
                        }
                        session.notify();
                    } else {
                        break;
                    }
                }
                if !session.closed.load(Ordering::Relaxed) {
                    error!("[SESSION] KCP input thread closed");
                    session.closed.store(true, Ordering::Release);
                    session.notify();
                }
            })
        };

        // Per-session updater
        {
            let session = session.clone();
            tokio::spawn(async move {
                let mut recv_buffer = Vec::new();
                let mut recv_buffer_size = 0;
                while !session.closed.load(Ordering::Relaxed) {
                    let next = {
                        let mut socket = session.socket.lock();

                        let is_closed = session.closed.load(Ordering::Acquire);
                        if is_closed && socket.can_close() {
                            warn!("[SESSION] KCP session closing");
                            break;
                        }

                        // server socket expires
                        if !is_client {
                            // If this is a server stream, close it automatically after a period of time
                            let last_update_time = socket.last_update_time();
                            let elapsed = last_update_time.elapsed();

                            if let Some(session_expire) = session.session_expire {
                                if elapsed > session_expire {
                                    if elapsed > session_expire * 2 {
                                        // Force close. Client may have already gone.
                                        debug!(
                                            "[SESSION] force close inactive session, conv: {}, last_update: {}s ago",
                                            socket.conv(),
                                            elapsed.as_secs()
                                        );
                                        break;
                                    }

                                    if !is_closed {
                                        warn!(
                                            "[SESSION] closing inactive session, conv: {}, last_update: {}s ago",
                                            socket.conv(),
                                            elapsed.as_secs()
                                        );
                                        session.closed.store(true, Ordering::Release);
                                    }
                                }
                            }
                        }

                        // If window is full, flush it immediately
                        if socket.need_flush() {
                            if let Err(e) = socket.flush().await {
                                error!("[SESSION] Flush failed, error: {}", e);
                            }
                        }

                        match socket.update().await {
                            Ok(next_next) => Instant::from_std(next_next),
                            Err(err) => {
                                error!("[SESSION] KCP update failed, error: {}", err);
                                Instant::now() + Duration::from_millis(10)
                            }
                        }
                    };

                    if session.recv(&mut recv_buffer, &mut recv_buffer_size).await.is_err() {
                        error!(
                            "[SESSION] Recv thread send data failed for session {}",
                            session.conv()
                        );
                        break;
                    }

                    tokio::select! {
                        _ = time::sleep_until(next) => {},
                        _ = session.notifier.notified() => {},
                    }
                }

                {
                    // Close the socket.
                    // Wake all pending tasks and let all send/recv return EOF

                    let mut socket = session.socket.lock();
                    socket.close();
                }

                //if let Some((ref notifier, peer_addr)) = session.session_close_notifier {
                //    let _ = notifier.send(peer_addr).await;
                //}

                session.closed.store(true, Ordering::Release);
                io_task_handle.abort();

                warn!("[SESSION] KCP session closed");
            });
        }

        session
    }

    #[allow(dead_code)]
    pub fn kcp_socket(&self) -> &SpinMutex<KcpSocket> {
        &self.socket
    }

    pub fn kcp_recv(&self) -> &KcpRecv {
        &self.recv
    }

    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    #[allow(dead_code)]
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify();
    }

    pub async fn input(&self, buf: &[u8]) -> Result<(), SessionClosedError> {
        self.input_tx
            .send(buf.to_owned())
            .await
            .map_err(|_| SessionClosedError)
    }

    pub fn conv(&self) -> u32 {
        let socket = self.socket.lock();
        socket.conv()
    }

    pub fn notify(&self) {
        self.notifier.notify_one();
    }

    pub fn poll_send(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<KcpResult<usize>> {
        // Mutex doesn't have poll_lock, spinning on it.
        let mut kcp = self.socket.lock();
        let result = ready!(kcp.poll_send(cx, buf));
        self.notify();
        result.into()
    }

    /// `send` data in `buf`
    pub async fn send(&self, buf: &[u8]) -> KcpResult<usize> {
        future::poll_fn(|cx| self.poll_send(cx, buf)).await
    }

    pub async fn recv(
        &self,
        recv_buffer: &mut Vec<u8>,
        recv_buffer_size: &mut usize,
    ) -> Result<(), ErrorKind> {
        if *recv_buffer_size > 0 {
            match self.recv.send(&recv_buffer[..*recv_buffer_size]).await {
                Ok(_) => {
                    debug!("[SESSION] recv {} bytes and send to TCP", recv_buffer_size);
                    *recv_buffer_size = 0;
                }
                Err(_) => {
                    return Err(ErrorKind::BrokenPipe);
                }
            }
        }
        let mut socket = self.socket.lock();
        while let Ok(size) = socket.peek_size() {
            if size > 0 {
                if recv_buffer.len() < size {
                    recv_buffer.resize(size, 0);
                }
                if let Ok(size) = socket.try_recv(recv_buffer) {
                    *recv_buffer_size = size;
                    match self.recv.try_send(&recv_buffer[..*recv_buffer_size]) {
                        Ok(_) => {
                            debug!("[SESSION] recv {} bytes and send to TCP", recv_buffer_size);
                            *recv_buffer_size = 0;
                        }
                        Err(TrySendError::Full(_)) => {
                            return Ok(());
                        }
                        Err(TrySendError::Closed(_)) => {
                            return Err(ErrorKind::BrokenPipe);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

pub struct SessionClosedError;
