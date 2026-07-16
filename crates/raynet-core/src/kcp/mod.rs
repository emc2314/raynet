mod error;
#[allow(clippy::module_inception)]
mod kcp;

pub use error::Error;
pub type KcpResult<T> = Result<T, error::Error>;
pub use kcp::{KCP_OVERHEAD, Kcp, KcpParams, get_conv};
