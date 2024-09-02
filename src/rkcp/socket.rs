use std::{
    fmt::{self, Debug},
    io::{self, ErrorKind, Write},
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

use futures_util::future;
use kcp::{Error as KcpError, Kcp, KcpResult};
use log::{debug, error};

use crate::packets::KcpOutput;
use crate::utils::now_millis;

/// Kcp Delay Config
#[derive(Debug, Clone, Copy)]
pub struct KcpNoDelayConfig {
    /// Enable nodelay
    pub nodelay: bool,
    /// Internal update interval (ms)
    pub interval: i32,
    /// ACK number to enable fast resend
    pub resend: i32,
    /// Disable congetion control
    pub nc: bool,
}

impl Default for KcpNoDelayConfig {
    fn default() -> KcpNoDelayConfig {
        KcpNoDelayConfig {
            nodelay: false,
            interval: 100,
            resend: 0,
            nc: false,
        }
    }
}

impl KcpNoDelayConfig {
    /// Get a fastest configuration
    ///
    /// 1. Enable NoDelay
    /// 2. Set ticking interval to be 10ms
    /// 3. Set fast resend to be 2
    /// 4. Disable congestion control
    #[allow(dead_code)]
    pub const fn fastest() -> KcpNoDelayConfig {
        KcpNoDelayConfig {
            nodelay: true,
            interval: 10,
            resend: 2,
            nc: true,
        }
    }

    /// Get a normal configuration
    ///
    /// 1. Disable NoDelay
    /// 2. Set ticking interval to be 40ms
    /// 3. Disable fast resend
    /// 4. Enable congestion control
    #[allow(dead_code)]
    pub const fn normal() -> KcpNoDelayConfig {
        KcpNoDelayConfig {
            nodelay: false,
            interval: 40,
            resend: 0,
            nc: false,
        }
    }
}

/// Kcp Config
#[derive(Debug, Clone, Copy)]
pub struct KcpConfig {
    /// Max Transmission Unit
    pub mtu: usize,
    /// nodelay
    pub nodelay: KcpNoDelayConfig,
    /// Send window size
    pub wnd_size: (u16, u16),
    /// Session expire duration, default is 90 seconds
    pub session_expire: Option<Duration>,
    /// Flush KCP state immediately after write
    pub flush_write: bool,
    /// Flush ACKs immediately after input
    pub flush_acks_input: bool,
    /// Allow recv 0 byte packet. KCP Segments with 0 byte data are skipped by default.
    pub allow_recv_empty_packet: bool,
}

impl Default for KcpConfig {
    fn default() -> KcpConfig {
        KcpConfig {
            mtu: 1165,
            nodelay: KcpNoDelayConfig {
                nodelay: true,
                interval: 10,
                resend: 2,
                nc: true,
            },
            wnd_size: (2048, 2048),
            session_expire: Some(Duration::from_secs(30)),
            flush_write: true,
            flush_acks_input: true,
            allow_recv_empty_packet: false,
        }
    }
}

impl KcpConfig {
    /// Applies config onto `Kcp`
    #[doc(hidden)]
    pub fn apply_config<W: Write>(&self, k: &mut Kcp<W>) {
        k.set_mtu(self.mtu).expect("invalid MTU");

        k.set_nodelay(
            self.nodelay.nodelay,
            self.nodelay.interval,
            self.nodelay.resend,
            self.nodelay.nc,
        );

        k.set_wndsize(self.wnd_size.0, self.wnd_size.1);
    }
}

pub struct KcpSocket {
    kcp: Kcp<KcpOutput>,
    last_update: Instant,
    flush_write: bool,
    flush_ack_input: bool,
    sent_first: bool,
    pending_sender: Option<Waker>,
    pending_receiver: Option<Waker>,
    closed: bool,
    allow_recv_empty_packet: bool,
}

impl Debug for KcpSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KcpSocket")
            .field("kcp.conv", &self.kcp.conv())
            .field("kcp.dead", &self.kcp.is_dead_link())
            .field("last_update", &self.last_update)
            .field("flush_write", &self.flush_write)
            .field("flush_ack_input", &self.flush_ack_input)
            .field("sent_first", &self.sent_first)
            .field("pending_sender", &self.pending_sender.is_some())
            .field("pending_receiver", &self.pending_receiver.is_some())
            .field("closed", &self.closed)
            .field("allow_recv_empty_packet", &self.allow_recv_empty_packet)
            .finish()
    }
}

impl KcpSocket {
    pub fn new(c: &KcpConfig, conv: u32, output: KcpOutput, stream: bool) -> KcpResult<KcpSocket> {
        let mut kcp = if stream {
            Kcp::new_stream(conv, output)
        } else {
            Kcp::new(conv, output)
        };
        c.apply_config(&mut kcp);

        // Ask server to allocate one
        if conv == 0 {
            kcp.input_conv();
        }

        kcp.update(now_millis() as u32)?;

        Ok(KcpSocket {
            kcp,
            last_update: Instant::now(),
            flush_write: c.flush_write,
            flush_ack_input: c.flush_acks_input,
            sent_first: false,
            pending_sender: None,
            pending_receiver: None,
            closed: false,
            allow_recv_empty_packet: c.allow_recv_empty_packet,
        })
    }

    /// Call every time you got data from transmission
    pub async fn input(&mut self, buf: &[u8]) -> KcpResult<bool> {
        match self.kcp.input(buf) {
            Ok(..) => {}
            Err(KcpError::ConvInconsistent(expected, actual)) => {
                error!(
                    "[INPUT] Conv expected={} actual={} ignored",
                    expected, actual
                );
                return Ok(false);
            }
            Err(err) => return Err(err),
        }
        self.last_update = Instant::now();

        if self.flush_ack_input {
            self.kcp.async_flush_ack().await?;
        }

        Ok(self.try_wake_pending_waker())
    }

    /// Call if you want to send some data
    pub fn poll_send(&mut self, cx: &mut Context<'_>, mut buf: &[u8]) -> Poll<KcpResult<usize>> {
        if self.closed {
            return Err(io::Error::from(ErrorKind::BrokenPipe).into()).into();
        }

        // If:
        //     1. Have sent the first packet (asking for conv)
        //     2. Too many pending packets
        if self.sent_first
            && (self.kcp.wait_snd() >= self.kcp.snd_wnd() as usize
                || self.kcp.wait_snd() >= self.kcp.rmt_wnd() as usize
                || self.kcp.waiting_conv())
        {
            debug!(
                "[SEND] waitsnd={} sndwnd={} rmtwnd={} excceeded or waiting conv={}",
                self.kcp.wait_snd(),
                self.kcp.snd_wnd(),
                self.kcp.rmt_wnd(),
                self.kcp.waiting_conv()
            );

            if let Some(waker) = self.pending_sender.replace(cx.waker().clone()) {
                if !cx.waker().will_wake(&waker) {
                    waker.wake();
                }
            }
            return Poll::Pending;
        }

        if !self.sent_first && self.kcp.waiting_conv() && buf.len() > self.kcp.mss() {
            buf = &buf[..self.kcp.mss()];
        }

        let n = self.kcp.send(buf)?;
        self.sent_first = true;

        if self.kcp.wait_snd() >= self.kcp.snd_wnd() as usize
            || self.kcp.wait_snd() >= self.kcp.rmt_wnd() as usize
        {
            self.kcp.flush()?;
        }

        self.last_update = Instant::now();

        if self.flush_write {
            self.kcp.flush()?;
        }

        Ok(n).into()
    }

    /// Call if you want to send some data
    #[allow(dead_code)]
    pub async fn send(&mut self, buf: &[u8]) -> KcpResult<usize> {
        future::poll_fn(|cx| self.poll_send(cx, buf)).await
    }

    #[allow(dead_code)]
    pub fn try_recv(&mut self, buf: &mut [u8]) -> KcpResult<usize> {
        if self.closed {
            return Ok(0);
        }
        self.kcp.recv(buf)
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<KcpResult<usize>> {
        if self.closed {
            return Ok(0).into();
        }

        match self.kcp.recv(buf) {
            e @ (Err(KcpError::RecvQueueEmpty) | Err(KcpError::ExpectingFragment)) => {
                debug!(
                    "[RECV] rcvwnd={} peeksize={} r={:?}",
                    self.kcp.rcv_wnd(),
                    self.kcp.peeksize().unwrap_or(0),
                    e
                );
            }
            Err(err) => return Err(err).into(),
            Ok(n) => {
                if n == 0 && !self.allow_recv_empty_packet {
                    debug!(
                        "[RECV] rcvwnd={} peeksize={} r=Ok(0)",
                        self.kcp.rcv_wnd(),
                        self.kcp.peeksize().unwrap_or(0),
                    );
                } else {
                    self.last_update = Instant::now();
                    return Ok(n).into();
                }
            }
        }

        if let Some(waker) = self.pending_receiver.replace(cx.waker().clone()) {
            if !cx.waker().will_wake(&waker) {
                waker.wake();
            }
        }

        Poll::Pending
    }

    #[allow(dead_code)]
    pub async fn recv(&mut self, buf: &mut [u8]) -> KcpResult<usize> {
        future::poll_fn(|cx| self.poll_recv(cx, buf)).await
    }

    pub async fn flush(&mut self) -> KcpResult<()> {
        self.kcp.async_flush().await?;
        self.last_update = Instant::now();
        Ok(())
    }

    fn try_wake_pending_waker(&mut self) -> bool {
        let mut waked = false;

        if self.pending_sender.is_some()
            && self.kcp.wait_snd() < self.kcp.snd_wnd() as usize
            && self.kcp.wait_snd() < self.kcp.rmt_wnd() as usize
            && !self.kcp.waiting_conv()
        {
            let waker = self.pending_sender.take().unwrap();
            waker.wake();

            waked = true;
        }

        if self.pending_receiver.is_some() {
            if let Ok(peek) = self.kcp.peeksize() {
                if self.allow_recv_empty_packet || peek > 0 {
                    let waker = self.pending_receiver.take().unwrap();
                    waker.wake();

                    waked = true;
                }
            }
        }

        waked
    }

    pub async fn update(&mut self) -> KcpResult<Instant> {
        let now = now_millis() as u32;
        self.kcp.async_update(now).await?;
        let next = self.kcp.check(now);

        self.try_wake_pending_waker();

        Ok(Instant::now() + Duration::from_millis(next as u64))
    }

    pub fn close(&mut self) {
        self.closed = true;
        if let Some(w) = self.pending_sender.take() {
            w.wake();
        }
        if let Some(w) = self.pending_receiver.take() {
            w.wake();
        }
    }

    pub fn can_close(&self) -> bool {
        self.kcp.wait_snd() == 0
    }

    pub fn conv(&self) -> u32 {
        self.kcp.conv()
    }

    pub fn set_conv(&mut self, conv: u32) {
        self.kcp.set_conv(conv);
    }

    pub fn waiting_conv(&self) -> bool {
        self.kcp.waiting_conv()
    }

    pub fn peek_size(&self) -> KcpResult<usize> {
        self.kcp.peeksize()
    }

    pub fn last_update_time(&self) -> Instant {
        self.last_update
    }

    pub fn need_flush(&self) -> bool {
        (self.kcp.wait_snd() >= self.kcp.snd_wnd() as usize
            || self.kcp.wait_snd() >= self.kcp.rmt_wnd() as usize)
            && !self.kcp.waiting_conv()
    }
}
