pub(crate) const TRANSPORT_PACKET_OVERHEAD: usize = 32;
/// Endpoint AEAD tag plus flags byte.
pub(crate) const ENDPOINT_MESSAGE_OVERHEAD: usize = 17;
pub(crate) const MIN_PADDING: usize = 1;
pub(crate) const MAX_TRANSPORT_PACKET_SIZE: usize = 64 * 1024;
pub const MAX_SESSION_DATA_SIZE: usize = 65535;

/// Local first-edge cooldown after send failure: 50 -> 100 -> 200 ms.
pub(crate) const LOCAL_COOLDOWN_INITIAL_MS: u64 = 50;
pub(crate) const LOCAL_COOLDOWN_MAX_MS: u64 = 200;

/// One tick beyond the inclusive maximum past freshness window.
pub(crate) const SESSION_TOMBSTONE_MS: u64 = 646 << 10;

pub(crate) fn effective_transport_mtu(configured_mtu: u32) -> usize {
    (configured_mtu as usize).min(MAX_TRANSPORT_PACKET_SIZE)
}
