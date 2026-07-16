use aegis::aegis128l::Key;

use crate::packet::{TransportPacketError, decode_transport_packet};
use crate::sequence::{SequenceDrop, SequenceFilter, TS_SHIFT, time_diff};
use crate::wire::{Envelope, EnvelopeStamp, TIME_MASK};

pub type ChannelId = u16;
pub type ConvId = u64;

#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
#[derive(Clone, Copy)]
#[repr(u8)]
pub enum CloseReason {
    LocalClosed = 0,
    RemoteClosed = 1,
    Reset = 2,
    Error = 3,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct CoreMetrics {
    pub packets_received: u64,
    pub packets_sent: u64,
    pub packets_dropped: u64,
    pub authentication_failures: u64,
    pub freshness_drops: u64,
    pub replay_drops: u64,
    pub malformed_packets: u64,
    pub send_failures: u64,
    pub active_sessions: usize,
    pub active_sequences: usize,
    pub latest_seq_time: u32,
}

pub enum CoreEvent {
    TransportPacketReceived {
        channel_id: ChannelId,
        bytes: Vec<u8>,
    },
    TransportPacketSendFailed {
        channel_id: ChannelId,
        bytes: Vec<u8>,
    },
    SessionOpen,
    SessionWrite {
        conv_id: ConvId,
        bytes: Vec<u8>,
    },
    SessionClose {
        conv_id: ConvId,
        reason: CloseReason,
    },
}

pub enum CoreAction {
    SendTransportPacket {
        channel_id: ChannelId,
        bytes: Vec<u8>,
    },
    OpenSession {
        conv_id: ConvId,
    },
    WriteSession {
        conv_id: ConvId,
        bytes: Vec<u8>,
    },
    CloseSession {
        conv_id: ConvId,
        reason: CloseReason,
    },
}

#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub enum CoreEventResult {
    None,
    SessionCreated { conv_id: ConvId },
    SessionWriteBlocked,
}

pub(crate) fn receive_transport_packet(
    envelope_key: &Key,
    metrics: &mut CoreMetrics,
    bytes: Vec<u8>,
) -> Option<Envelope> {
    match decode_transport_packet(envelope_key, bytes) {
        Ok(envelope) => Some(envelope),
        Err(TransportPacketError::InvalidLength) => {
            metrics.packets_dropped += 1;
            None
        }
        Err(TransportPacketError::Authentication) => {
            metrics.authentication_failures += 1;
            None
        }
        Err(TransportPacketError::MalformedEnvelope) => {
            metrics.malformed_packets += 1;
            None
        }
    }
}

pub(crate) fn check_sequence(
    sequence_filter: &mut SequenceFilter,
    metrics: &mut CoreMetrics,
    current_time: u32,
    stamp: EnvelopeStamp,
) -> bool {
    match sequence_filter.check_and_insert(current_time, stamp.time, stamp.seq_id, stamp.seq_no) {
        Ok(()) => {}
        Err(SequenceDrop::Freshness) => {
            metrics.freshness_drops += 1;
            return false;
        }
        Err(SequenceDrop::Replay) => {
            metrics.replay_drops += 1;
            return false;
        }
    }
    true
}

pub(crate) fn note_latest_sequence(
    metrics: &mut CoreMetrics,
    boot_time_ms: u64,
    elapsed_ms: u64,
    packet_time: u32,
    first: bool,
) {
    let current_tick = ((boot_time_ms + elapsed_ms) >> TS_SHIFT) as u32;
    let packet_tick =
        current_tick.wrapping_sub(time_diff(current_tick & TIME_MASK, packet_time) as u32);
    if first || packet_tick.wrapping_sub(metrics.latest_seq_time) < 1 << 31 {
        metrics.latest_seq_time = packet_tick;
    }
}
