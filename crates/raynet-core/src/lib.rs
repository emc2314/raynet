//! Runtime-independent RayNet protocol building blocks.

pub mod core;
pub mod kcp;
pub mod routing;
pub mod utils;

pub use core::{
    ChannelId, ChannelState, CloseReason, ConfigError, ConvId, CoreAction, CoreError, CoreEvent,
    CoreStructuredEvent, EndpointConfig, EndpointCore, Envelope, HopPacketError, Metadata, Metric,
    NodeId, NonceFilter, OpenFailureReason, RandomStream, RelayConfig, RelayCore, RoutePlan,
    SessionFrame, Target, TransportMetrics, WireError, open_hop_payload, seal_hop_payload,
};
pub use routing::{
    LocalChannelState, LocalChannelTable, RouteChannel, RouteEdgeState, RouteNode, RoutePlanner,
    RouteTopology,
};
