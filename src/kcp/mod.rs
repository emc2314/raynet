mod error;
mod kcp;

pub use error::Error;
pub type KcpResult<T> = Result<T, error::Error>;
#[allow(unused_imports)]
pub use kcp::{get_conv, get_sn, set_conv, Kcp, KCP_OVERHEAD};
