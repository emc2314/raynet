//! KCP

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::num::NonZeroU32;

use bytes::{Buf, BufMut, Bytes, BytesMut};

use super::KcpResult;
use super::error::Error;

const KCP_CMD_PUSH: u8 = 81; // cmd: push data
const KCP_CMD_ACK: u8 = 82; // cmd: ack
const KCP_CMD_WASK: u8 = 83; // cmd: window probe (ask)
const KCP_CMD_WINS: u8 = 84; // cmd: window size (tell)

const KCP_ASK_SEND: u32 = 1; // need to send IKCP_CMD_WASK
const KCP_ASK_TELL: u32 = 2; // need to send IKCP_CMD_WINS

/// KCP Header size
pub const KCP_OVERHEAD: usize = 28;
const KCP_THRESH_INIT: u16 = 2;
const KCP_THRESH_MIN: u16 = 2;
const KCP_RTO_MAX_SCALE: u32 = 6_000;

const KCP_FASTACK_LIMIT: u32 = 5; // max times to trigger fastack

#[derive(Clone, Copy)]
pub struct KcpParams {
    pub(crate) mtu: usize,
    pub(crate) send_window: u16,
    pub(crate) receive_window: u16,
    pub(crate) nodelay: bool,
    pub(crate) fast_resend: Option<NonZeroU32>,
    pub(crate) congestion_control: bool,
    pub(crate) time_scale: u8,
}

#[derive(Clone, Copy)]
struct KcpTiming {
    rto_nodelay: u32,
    rto_normal: u32,
    rto_initial: u32,
    rto_max: u32,
    interval: u32,
    probe_initial: u32,
    probe_max: u32,
}

impl KcpTiming {
    fn new(time_scale: u8) -> Self {
        let scale = u32::from(time_scale);
        Self {
            rto_nodelay: 3 * scale,
            rto_normal: 10 * scale,
            rto_initial: 20 * scale,
            rto_max: KCP_RTO_MAX_SCALE * scale,
            interval: 2 * scale,
            probe_initial: 700 * scale,
            probe_max: 12_000 * scale,
        }
    }
}

/// Read `conv` from raw buffer
pub fn get_conv(mut buf: &[u8]) -> u64 {
    debug_assert!(buf.len() >= KCP_OVERHEAD);
    buf.get_u64_le()
}

#[inline]
fn timediff(later: u32, earlier: u32) -> i32 {
    later.wrapping_sub(earlier) as i32
}

#[derive(Default)]
struct KcpSegment {
    conv: u64,
    cmd: u8,
    frg: u8,
    wnd: u16,
    ts: u32,
    sn: u32,
    una: u32,
    resendts: u32,
    rto: u32,
    fastack: u32,
    xmit: u32,
    data: Bytes,
}

impl KcpSegment {
    fn new_with_data(data: Bytes) -> Self {
        Self {
            data,
            ..Default::default()
        }
    }

    fn encode(&self, buf: &mut BytesMut) {
        debug_assert!(buf.remaining_mut() >= KCP_OVERHEAD + self.data.len());
        buf.put_u64_le(self.conv);
        buf.put_u8(self.cmd);
        buf.put_u8(self.frg);
        buf.put_u16_le(self.wnd);
        buf.put_u32_le(self.ts);
        buf.put_u32_le(self.sn);
        buf.put_u32_le(self.una);
        buf.put_u32_le(self.data.len() as u32);
        buf.put_slice(&self.data);
    }
}

/// KCP control
pub struct Kcp {
    /// Conversation ID
    conv: u64,
    /// Maximum Transmission Unit
    mtu: usize,
    /// Maximum Segment Size
    mss: usize,
    /// First unacknowledged packet
    snd_una: u32,
    /// Next packet
    snd_nxt: u32,
    /// Next packet to be received
    rcv_nxt: u32,

    /// Congestion window threshold
    ssthresh: u16,

    /// ACK receive variable RTT
    rx_rttval: u32,
    /// ACK receive static RTT
    rx_srtt: u32,
    /// Resend time (calculated by ACK delay time)
    rx_rto: u32,
    /// Minimal resend timeout
    rx_minrto: u32,
    timing: KcpTiming,

    /// Send window
    snd_wnd: u16,
    /// Receive window
    rcv_wnd: u16,
    /// Remote receive window
    rmt_wnd: u16,
    /// Congestion window
    cwnd: u16,
    /// Check window
    /// - IKCP_ASK_TELL, telling window size to remote
    /// - IKCP_ASK_SEND, ask remote for window size
    probe: u32,

    /// Last update time
    current: u32,
    /// Next flush interval
    ts_flush: u32,
    /// Enable nodelay
    nodelay: bool,
    /// Updated has been called or not
    updated: bool,

    /// Next check window timestamp
    ts_probe: u32,
    /// Check window wait time
    probe_wait: u32,

    /// Maximum payload size
    incr: usize,

    snd_queue: VecDeque<KcpSegment>,
    rcv_queue: VecDeque<KcpSegment>,
    snd_buf: VecDeque<KcpSegment>,
    rcv_buf: VecDeque<KcpSegment>,

    /// Pending ACK
    acklist: Vec<(u32, u32)>,
    buf: BytesMut,

    /// ACK number to trigger fast resend
    fastresend: Option<NonZeroU32>,
    fastlimit: u32,
    congestion_control: bool,
    output: Vec<Bytes>,
}

impl Kcp {
    /// Creates a KCP control object, `conv` must be equal in both endpoints in one connection.
    ///
    /// `conv` represents conversation.
    pub fn new(conv: u64, params: KcpParams) -> Self {
        let timing = KcpTiming::new(params.time_scale);
        Kcp {
            conv,
            snd_una: 0,
            snd_nxt: 0,
            rcv_nxt: 0,
            ts_probe: 0,
            probe_wait: 0,
            snd_wnd: params.send_window,
            rcv_wnd: params.receive_window,
            rmt_wnd: params.receive_window,
            cwnd: 0,
            incr: 0,
            probe: 0,
            mtu: params.mtu,
            mss: params.mtu - KCP_OVERHEAD,
            buf: BytesMut::with_capacity((params.mtu + KCP_OVERHEAD) * 3),

            snd_queue: VecDeque::new(),
            rcv_queue: VecDeque::new(),
            snd_buf: VecDeque::new(),
            rcv_buf: VecDeque::new(),

            acklist: Vec::new(),

            rx_srtt: 0,
            rx_rttval: 0,
            rx_rto: timing.rto_initial,
            rx_minrto: if params.nodelay {
                timing.rto_nodelay
            } else {
                timing.rto_normal
            },
            timing,

            current: 0,
            ts_flush: timing.interval,
            nodelay: params.nodelay,
            updated: false,
            ssthresh: KCP_THRESH_INIT,
            fastresend: params.fast_resend,
            fastlimit: KCP_FASTACK_LIMIT,
            congestion_control: params.congestion_control,
            output: Vec::new(),
        }
    }

    // move available data from rcv_buf -> rcv_queue
    fn move_buf(&mut self) {
        while !self.rcv_buf.is_empty() {
            let nrcv_que = self.rcv_queue.len();
            {
                let seg = self.rcv_buf.front().unwrap();
                if seg.sn == self.rcv_nxt && nrcv_que < self.rcv_wnd as usize {
                    self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
                } else {
                    break;
                }
            }

            let seg = self.rcv_buf.pop_front().unwrap();
            self.rcv_queue.push_back(seg);
        }
    }

    /// Receive data from buffer
    pub fn recv(&mut self, buf: &mut [u8]) -> KcpResult<usize> {
        if self.rcv_queue.is_empty() {
            return Err(Error::RecvQueueEmpty);
        }

        let peeksize = self.peeksize()?;

        if peeksize > buf.len() {
            return Err(Error::UserBufTooSmall);
        }

        let recover = self.rcv_queue.len() >= self.rcv_wnd as usize;

        // Merge fragment
        let mut written = 0;
        while let Some(seg) = self.rcv_queue.pop_front() {
            let end = written + seg.data.len();
            buf[written..end].copy_from_slice(&seg.data);
            written = end;

            if seg.frg == 0 {
                break;
            }
        }
        debug_assert_eq!(written, peeksize);

        self.move_buf();

        // fast recover
        if self.rcv_queue.len() < self.rcv_wnd as usize && recover {
            // ready to send back IKCP_CMD_WINS in ikcp_flush
            // tell remote my window size
            self.probe |= KCP_ASK_TELL;
        }

        Ok(written)
    }

    /// Check buffer size without actually consuming it
    pub fn peeksize(&self) -> KcpResult<usize> {
        match self.rcv_queue.front() {
            Some(segment) => {
                if segment.frg == 0 {
                    return Ok(segment.data.len());
                }

                if self.rcv_queue.len() < (segment.frg + 1) as usize {
                    return Err(Error::ExpectingFragment);
                }

                let mut len = 0;

                for segment in &self.rcv_queue {
                    len += segment.data.len();
                    if segment.frg == 0 {
                        break;
                    }
                }

                Ok(len)
            }
            None => Err(Error::RecvQueueEmpty),
        }
    }

    /// Send bytes into buffer
    pub fn send(&mut self, mut buf: &[u8]) -> KcpResult<usize> {
        let sent_size = buf.len();

        debug_assert!(self.mss > 0);

        let count = buf.len().div_ceil(self.mss).max(1);

        if count >= self.rcv_wnd as usize || count >= 256 {
            return Err(Error::UserBufTooBig);
        }

        for i in 0..count {
            let size = self.mss.min(buf.len());

            let (lf, rt) = buf.split_at(size);

            let mut new_segment = KcpSegment::new_with_data(Bytes::copy_from_slice(lf));
            buf = rt;

            new_segment.frg = (count - i - 1) as u8;

            self.snd_queue.push_back(new_segment);
        }

        Ok(sent_size)
    }

    fn update_ack(&mut self, rtt: u32) {
        if self.rx_srtt == 0 {
            self.rx_srtt = rtt;
            self.rx_rttval = rtt / 2;
        } else {
            let delta = rtt.abs_diff(self.rx_srtt);
            self.rx_rttval = (3 * self.rx_rttval + delta) / 4;
            self.rx_srtt = ((7 * self.rx_srtt + rtt) / 8).max(1);
        }
        let rto = self.rx_srtt + self.timing.interval.max(4 * self.rx_rttval);
        self.rx_rto = rto.clamp(self.rx_minrto, self.timing.rto_max);
    }

    #[inline]
    fn shrink_buf(&mut self) {
        self.snd_una = match self.snd_buf.front() {
            Some(seg) => seg.sn,
            None => self.snd_nxt,
        };
    }

    fn parse_ack(&mut self, sn: u32) {
        if timediff(sn, self.snd_una) < 0 || timediff(sn, self.snd_nxt) >= 0 {
            return;
        }

        let mut i = 0_usize;
        while i < self.snd_buf.len() {
            match sn.cmp(&self.snd_buf[i].sn) {
                Ordering::Equal => {
                    self.snd_buf.remove(i);
                    break;
                }
                Ordering::Less => break,
                _ => i += 1,
            }
        }
    }

    fn parse_una(&mut self, una: u32) {
        while let Some(seg) = self.snd_buf.front() {
            if timediff(una, seg.sn) > 0 {
                self.snd_buf.pop_front();
            } else {
                break;
            }
        }
    }

    fn parse_fastack(&mut self, sn: u32, _ts: u32) {
        if timediff(sn, self.snd_una) < 0 || timediff(sn, self.snd_nxt) >= 0 {
            return;
        }

        for seg in &mut self.snd_buf {
            if timediff(sn, seg.sn) < 0 {
                break;
            } else if sn != seg.sn {
                #[cfg(feature = "fastack-conserve")]
                {
                    seg.fastack += 1;
                }
                #[cfg(not(feature = "fastack-conserve"))]
                if timediff(_ts, seg.ts) >= 0 {
                    seg.fastack += 1;
                }
            }
        }
    }

    #[inline]
    fn ack_push(&mut self, sn: u32, ts: u32) {
        self.acklist.push((sn, ts));
    }

    fn parse_data(&mut self, new_segment: KcpSegment) {
        let sn = new_segment.sn;

        if timediff(sn, self.rcv_nxt.wrapping_add(u32::from(self.rcv_wnd))) >= 0
            || timediff(sn, self.rcv_nxt) < 0
        {
            return;
        }

        let mut repeat = false;
        let mut new_index = self.rcv_buf.len();

        for segment in self.rcv_buf.iter().rev() {
            if segment.sn == sn {
                repeat = true;
                break;
            }
            if timediff(sn, segment.sn) > 0 {
                break;
            }
            new_index -= 1;
        }

        if !repeat {
            self.rcv_buf.insert(new_index, new_segment);
        }

        // move available data from rcv_buf -> rcv_queue
        self.move_buf();
    }

    /// Call this when you received a packet from raw connection
    pub fn input(&mut self, mut buf: &[u8]) -> KcpResult<usize> {
        if buf.len() < KCP_OVERHEAD {
            return Err(Error::InvalidSegmentSize);
        }

        let input_size = buf.len();
        let mut flag = false;
        let mut max_ack = 0;
        let old_una = self.snd_una;
        let mut latest_ts = 0;

        while buf.remaining() >= KCP_OVERHEAD {
            let conv = buf.get_u64_le();
            if conv != self.conv {
                return Err(Error::ConvInconsistent);
            }

            let cmd = buf.get_u8();
            let frg = buf.get_u8();
            let wnd = buf.get_u16_le();
            let ts = buf.get_u32_le();
            let sn = buf.get_u32_le();
            let una = buf.get_u32_le();
            let len = buf.get_u32_le() as usize;

            if buf.remaining() < len {
                return Err(Error::InvalidSegmentDataSize);
            }

            match cmd {
                KCP_CMD_PUSH | KCP_CMD_ACK | KCP_CMD_WASK | KCP_CMD_WINS => {}
                _ => {
                    return Err(Error::UnsupportedCmd);
                }
            }

            self.rmt_wnd = wnd;

            self.parse_una(una);
            self.shrink_buf();

            match cmd {
                KCP_CMD_ACK => {
                    let rtt = timediff(self.current, ts);
                    if rtt >= 0 {
                        self.update_ack(rtt as u32);
                    }
                    self.parse_ack(sn);
                    self.shrink_buf();

                    if !flag {
                        flag = true;
                        max_ack = sn;
                        latest_ts = ts;
                    } else if timediff(sn, max_ack) > 0 {
                        #[cfg(feature = "fastack-conserve")]
                        {
                            max_ack = sn;
                            latest_ts = ts;
                        }
                        #[cfg(not(feature = "fastack-conserve"))]
                        if timediff(ts, latest_ts) > 0 {
                            max_ack = sn;
                            latest_ts = ts;
                        }
                    }
                }
                KCP_CMD_PUSH => {
                    if timediff(sn, self.rcv_nxt.wrapping_add(u32::from(self.rcv_wnd))) < 0 {
                        self.ack_push(sn, ts);
                        if timediff(sn, self.rcv_nxt) >= 0 {
                            let mut segment =
                                KcpSegment::new_with_data(Bytes::copy_from_slice(&buf[..len]));

                            segment.conv = conv;
                            segment.cmd = cmd;
                            segment.frg = frg;
                            segment.wnd = wnd;
                            segment.ts = ts;
                            segment.sn = sn;
                            segment.una = una;

                            self.parse_data(segment);
                        }
                    }
                }
                KCP_CMD_WASK => {
                    // ready to send back IKCP_CMD_WINS in ikcp_flush
                    // tell remote my window size
                    self.probe |= KCP_ASK_TELL;
                }
                KCP_CMD_WINS => {}
                _ => unreachable!(),
            }
            buf.advance(len);
        }

        if flag {
            self.parse_fastack(max_ack, latest_ts);
        }

        if timediff(self.snd_una, old_una) > 0 && self.cwnd < self.rmt_wnd {
            let mss = self.mss;
            if self.cwnd < self.ssthresh {
                self.cwnd += 1;
                self.incr += mss;
            } else {
                self.incr = self.incr.max(mss);
                self.incr += (mss * mss) / self.incr + (mss / 16);
                if (self.cwnd as usize + 1) * mss <= self.incr {
                    self.cwnd = self.incr.div_ceil(mss).min(self.rmt_wnd as usize) as u16;
                }
            }
            if self.cwnd > self.rmt_wnd {
                self.cwnd = self.rmt_wnd;
                self.incr = self.rmt_wnd as usize * mss;
            }
        }

        Ok(input_size - buf.len())
    }

    fn wnd_unused(&self) -> u16 {
        if self.rcv_queue.len() < self.rcv_wnd as usize {
            self.rcv_wnd - self.rcv_queue.len() as u16
        } else {
            0
        }
    }

    fn probe_wnd_size(&mut self) {
        // probe window size (if remote window size equals zero)
        if self.rmt_wnd == 0 {
            if self.probe_wait == 0 {
                self.probe_wait = self.timing.probe_initial;
                self.ts_probe = self.current.wrapping_add(self.probe_wait);
            } else if timediff(self.current, self.ts_probe) >= 0 {
                self.probe_wait = self.probe_wait.max(self.timing.probe_initial);

                self.probe_wait += self.probe_wait / 2;
                self.probe_wait = self.probe_wait.min(self.timing.probe_max);

                self.ts_probe = self.current.wrapping_add(self.probe_wait);
                self.probe |= KCP_ASK_SEND;
            }
        } else {
            self.ts_probe = 0;
            self.probe_wait = 0;
        }
    }

    /// Determine when you should call `update`.
    /// Return when you should invoke `update` in millisec, if there is no `input`/`send` calling.
    /// You can call `update` at that time without calling it repeatedly.
    pub fn check(&self, current: u32) -> u32 {
        if !self.updated {
            return 0;
        }

        let mut ts_flush = self.ts_flush;
        let mut tm_packet = u32::MAX;

        if timediff(current, ts_flush) >= 10000 || timediff(current, ts_flush) < -10000 {
            ts_flush = current;
        }

        if timediff(current, ts_flush) >= 0 {
            return 0;
        }

        let tm_flush = timediff(ts_flush, current) as u32;
        for seg in &self.snd_buf {
            let diff = timediff(seg.resendts, current);
            if diff <= 0 {
                return 0;
            }
            tm_packet = tm_packet.min(diff as u32);
        }

        tm_packet.min(tm_flush).min(self.timing.interval)
    }

    /// Get `waitsnd`, how many packet is waiting to be sent
    #[inline]
    pub fn wait_snd(&self) -> usize {
        self.snd_buf.len() + self.snd_queue.len()
    }

    fn emit_output(buf: &mut BytesMut, output: &mut Vec<Bytes>) {
        if !buf.is_empty() {
            output.push(buf.split().freeze());
        }
    }

    pub fn take_output(&mut self) -> Vec<Bytes> {
        std::mem::take(&mut self.output)
    }

    fn flush_ack(&mut self, segment: &mut KcpSegment) {
        for &(sn, ts) in &self.acklist {
            if self.buf.len() + KCP_OVERHEAD > self.mtu {
                Self::emit_output(&mut self.buf, &mut self.output);
            }
            segment.sn = sn;
            segment.ts = ts;
            segment.encode(&mut self.buf);
        }
        self.acklist.clear();
    }

    fn flush_probe_command(&mut self, cmd: u8, segment: &mut KcpSegment) {
        segment.cmd = cmd;
        if self.buf.len() + KCP_OVERHEAD > self.mtu {
            Self::emit_output(&mut self.buf, &mut self.output);
        }
        segment.encode(&mut self.buf);
    }

    fn flush_probe_commands(&mut self, segment: &mut KcpSegment) {
        if (self.probe & KCP_ASK_SEND) != 0 {
            self.flush_probe_command(KCP_CMD_WASK, segment);
        }

        if (self.probe & KCP_ASK_TELL) != 0 {
            self.flush_probe_command(KCP_CMD_WINS, segment);
        }
        self.probe = 0;
    }

    /// Flush pending data in buffer.
    pub fn flush(&mut self) -> KcpResult<()> {
        if !self.updated {
            return Err(Error::NeedUpdate);
        }

        let mut segment = KcpSegment {
            conv: self.conv,
            cmd: KCP_CMD_ACK,
            wnd: self.wnd_unused(),
            una: self.rcv_nxt,
            ..Default::default()
        };

        self.flush_ack(&mut segment);
        self.probe_wnd_size();
        self.flush_probe_commands(&mut segment);

        // calculate window size
        let mut cwnd = self.snd_wnd.min(self.rmt_wnd);
        if self.congestion_control {
            cwnd = cwnd.min(self.cwnd);
        }

        // move data from snd_queue to snd_buf
        while timediff(self.snd_nxt, self.snd_una.wrapping_add(u32::from(cwnd))) < 0 {
            match self.snd_queue.pop_front() {
                Some(mut new_segment) => {
                    new_segment.conv = self.conv;
                    new_segment.cmd = KCP_CMD_PUSH;
                    new_segment.wnd = segment.wnd;
                    new_segment.ts = self.current;
                    new_segment.sn = self.snd_nxt;
                    self.snd_nxt = self.snd_nxt.wrapping_add(1);
                    new_segment.una = self.rcv_nxt;
                    new_segment.resendts = self.current;
                    new_segment.rto = self.rx_rto;
                    new_segment.fastack = 0;
                    new_segment.xmit = 0;
                    self.snd_buf.push_back(new_segment);
                }
                None => break,
            }
        }

        // calculate resent
        let resent = self.fastresend.map(NonZeroU32::get);

        let rtomin = if !self.nodelay { self.rx_rto >> 3 } else { 0 };
        let rto_max = self.timing.rto_max;

        let mut lost = false;
        let mut change = 0;

        for snd_segment in &mut self.snd_buf {
            let mut need_send = false;

            if snd_segment.xmit == 0 {
                need_send = true;
                snd_segment.xmit += 1;
                snd_segment.rto = self.rx_rto;
                snd_segment.resendts = self.current.wrapping_add(snd_segment.rto + rtomin);
            } else if timediff(self.current, snd_segment.resendts) >= 0 {
                need_send = true;
                snd_segment.xmit += 1;
                if !self.nodelay {
                    snd_segment.rto =
                        (snd_segment.rto + snd_segment.rto.max(self.rx_rto)).min(rto_max);
                } else {
                    snd_segment.rto = (snd_segment.rto + snd_segment.rto / 2).min(rto_max);
                }
                snd_segment.resendts = self.current.wrapping_add(snd_segment.rto);
                lost = true;
            } else if resent.is_some_and(|threshold| snd_segment.fastack >= threshold)
                && (snd_segment.xmit <= self.fastlimit || self.fastlimit == 0)
            {
                need_send = true;
                snd_segment.xmit += 1;
                snd_segment.fastack = 0;
                snd_segment.resendts = self.current.wrapping_add(snd_segment.rto);
                change += 1;
            }

            if need_send {
                snd_segment.ts = self.current;
                snd_segment.wnd = segment.wnd;
                snd_segment.una = self.rcv_nxt;

                let need = KCP_OVERHEAD + snd_segment.data.len();

                if self.buf.len() + need > self.mtu {
                    Self::emit_output(&mut self.buf, &mut self.output);
                }

                snd_segment.encode(&mut self.buf);
            }
        }

        Self::emit_output(&mut self.buf, &mut self.output);

        // update ssthresh
        if change > 0 {
            let inflight = self.snd_nxt.wrapping_sub(self.snd_una);
            self.ssthresh = (inflight as u16 / 2).max(KCP_THRESH_MIN);
            self.cwnd = u32::from(self.ssthresh)
                .saturating_add(resent.unwrap())
                .min(u32::from(u16::MAX)) as u16;
            self.incr = self.cwnd as usize * self.mss;
        }

        if lost {
            self.ssthresh = (cwnd / 2).max(KCP_THRESH_MIN);
            self.cwnd = 1;
            self.incr = self.mss;
        }

        if self.cwnd < 1 {
            self.cwnd = 1;
            self.incr = self.mss;
        }

        Ok(())
    }

    pub fn update(&mut self, current: u32) -> KcpResult<()> {
        self.current = current;

        if !self.updated {
            self.updated = true;
            self.ts_flush = self.current;
        }

        let mut slap = timediff(self.current, self.ts_flush);

        if !(-10000..10000).contains(&slap) {
            self.ts_flush = self.current;
            slap = 0;
        }

        if slap >= 0 {
            self.ts_flush = self.ts_flush.wrapping_add(self.timing.interval);
            if timediff(self.current, self.ts_flush) >= 0 {
                self.ts_flush = self.current.wrapping_add(self.timing.interval);
            }
            self.flush()?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_header_layout_and_peer_decode() {
        let fast_resend = NonZeroU32::new(2);
        let mut sender = Kcp::new(
            7,
            KcpParams {
                mtu: 1400,
                send_window: 60_000,
                receive_window: 60_000,
                nodelay: true,
                fast_resend,
                congestion_control: true,
                time_scale: 1,
            },
        );
        sender.update(123).unwrap();
        sender.send(b"KCP").unwrap();
        sender.flush().unwrap();
        let packet = sender.take_output().remove(0);

        assert_eq!(KCP_OVERHEAD, 28);
        assert_eq!(u64::from_le_bytes(packet[0..8].try_into().unwrap()), 7);
        assert_eq!(packet[8], KCP_CMD_PUSH);
        assert_eq!(packet[9], 0);
        assert_eq!(
            u16::from_le_bytes(packet[10..12].try_into().unwrap()),
            60_000
        );
        assert_eq!(u32::from_le_bytes(packet[12..16].try_into().unwrap()), 123);
        assert_eq!(u32::from_le_bytes(packet[16..20].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(packet[20..24].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(packet[24..28].try_into().unwrap()), 3);

        let mut receiver = Kcp::new(
            7,
            KcpParams {
                mtu: 1400,
                send_window: 64,
                receive_window: 256,
                nodelay: true,
                fast_resend,
                congestion_control: true,
                time_scale: 5,
            },
        );
        receiver.update(123).unwrap();
        receiver.input(&packet).unwrap();
        let mut output = [0; 3];
        assert_eq!(receiver.recv(&mut output).unwrap(), 3);
        assert_eq!(&output, b"KCP");
    }
}
