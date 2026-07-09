//! Synchronous core logic that avoids async/runtime dependencies.
pub mod machine;
pub mod nonce;
pub mod packet;
pub mod wire;

pub use machine::{
    ChannelConfig, ChannelId, ChannelState, CloseReason, ConfigDelta, ConfigError, CoreAction,
    CoreError, CoreEvent, DestinationRoute, EndpointConfig, EndpointCore, EndpointId,
    LocalConnectionId, LogLevel, Metadata, Metric, NodeId, OpenFailureReason, RelayConfig,
    RelayCore, StreamId, Target, TransportMetrics,
};
pub use nonce::NonceFilter;
pub use packet::{HopPacketError, open_hop_payload, seal_hop_payload};
pub use wire::{EndpointFrame, Envelope, PacketType, TraceEntry, WireError};
