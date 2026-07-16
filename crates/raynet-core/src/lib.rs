//! Runtime-independent RayNet protocol building blocks.

mod endpoint;
pub(crate) mod kcp;
mod limits;
mod machine;
mod packet;
mod random;
mod relay;
mod routing;
mod sequence;
mod wire;

pub use endpoint::{EndpointConfig, EndpointCore, KcpConfig};
pub use limits::MAX_SESSION_DATA_SIZE;
pub use machine::{
    ChannelId, CloseReason, ConvId, CoreAction, CoreEvent, CoreEventResult, CoreMetrics,
};
pub use relay::{RelayConfig, RelayCore};
pub use routing::{RouteConfig, RouteEdge, RouteGraph, RouteNode};
