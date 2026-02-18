//! [KCP](https://github.com/skywind3000/kcp) implementation in Rust.
//!
//! A Fast and Reliable ARQ Protocol

extern crate bytes;
// #[macro_use]
// extern crate log;

mod error;
mod kcp;

/// The `KCP` prelude
pub mod prelude {
    pub use super::{get_conv, Kcp, KCP_OVERHEAD};
}

pub use self::kcp::{get_conv, get_sn, set_conv, Kcp, KCP_OVERHEAD};
pub use error::Error;

/// KCP result
pub type KcpResult<T> = Result<T, Error>;

pub mod kcp2;
// pub use self::kcp2::{get_conv, get_sn, set_conv, Kcp, KCP_OVERHEAD, KcpWrite, KcpWriteBuf, KcpBufOps};

#[cfg(test)]
mod test;
