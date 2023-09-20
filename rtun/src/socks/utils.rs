//! Network Utilities
use tracing::trace;

use std::{
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
};

// use tokio::io::{AsyncRead, AsyncReadExt};




use shadowsocks::relay::socks5::Address;
use tokio::io::{copy_bidirectional, AsyncRead, AsyncWrite};


// /// Consumes all data from `reader` and throws away until EOF
// pub async fn ignore_until_end<R>(reader: &mut R) -> io::Result<()>
// where
//     R: AsyncRead + Unpin,
// {
//     let mut buffer = [0u8; 2048];

//     loop {
//         let n = reader.read(&mut buffer).await?;
//         if n == 0 {
//             break;
//         }
//     }

//     Ok(())
// }

/// Helper function for converting IPv4 mapped IPv6 address
///
/// This is the same as `Ipv6Addr::to_ipv4_mapped`, but it is still unstable in the current libstd
#[allow(unused)]
pub(crate) fn to_ipv4_mapped(ipv6: &Ipv6Addr) -> Option<Ipv4Addr> {
    match ipv6.octets() {
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, a, b, c, d] => Some(Ipv4Addr::new(a, b, c, d)),
        _ => None,
    }
}


pub(crate) async fn establish_tcp_tunnel_bypassed<P, S>(
    plain: &mut P,
    shadow: &mut S,
    peer_addr: SocketAddr,
    target_addr: &Address,
) -> io::Result<()>
where
    P: AsyncRead + AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    trace!("established tcp tunnel {} <-> {} bypassed", peer_addr, target_addr);

    match copy_bidirectional(plain, shadow).await {
        Ok((rn, wn)) => {
            trace!(
                "tcp tunnel {} <-> {} (bypassed) closed, L2R {} bytes, R2L {} bytes",
                peer_addr,
                target_addr,
                rn,
                wn
            );
        }
        Err(err) => {
            trace!(
                "tcp tunnel {} <-> {} (bypassed) closed with error: {}",
                peer_addr,
                target_addr,
                err
            );
        }
    }

    Ok(())
}

