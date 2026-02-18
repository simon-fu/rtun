use std::{
    io::{self, ErrorKind},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};

use crate::{
    channel::{ch_stream::ChStream, ChReceiver, ChSender},
    proto::OpenSocksArgs,
};

use crate::socks::server::{socks4::Socks4TcpHandler, socks5::Socks5TcpHandler};
use anyhow::{anyhow, bail, Context, Result};
use bytes::Buf;
use shadowsocks::{config::Mode, relay::socks5, ServerAddr};
use shadowsocks_service::local::{
    context::ServiceContext,
    loadbalancing::{PingBalancer, PingBalancerBuilder},
    socks::config::Socks5AuthConfig,
    socks::socks4,
};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tracing::error;

pub struct ChSocks {
    peer_addr: SocketAddr,
}

impl ChSocks {
    pub fn try_new(args: OpenSocksArgs) -> Result<Self> {
        Ok(Self {
            peer_addr: args.peer_addr.parse()?,
        })
    }

    pub async fn run(self, server: Server, tx: ChSender, rx: ChReceiver) -> Result<()> {
        let stream = ChStream::new2(tx, rx);

        // let mut version_buffer = [0u8; 1];

        // let n = stream.peek(&mut version_buffer).await?;
        // if n == 0 {
        //     return Err(io::Error::from(ErrorKind::UnexpectedEof).into());
        // }

        // run_socks_conn(&version_buffer[..], stream, self.peer_addr, server).await?;
        run_socks_conn(stream, self.peer_addr, server).await?;
        Result::<()>::Ok(())
    }
}

pub async fn run_socks_conn<S>(mut stream: S, peer_addr: SocketAddr, server: Server) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut version_buffer = [0u8; 1];

    let n = stream.read(&mut version_buffer).await?;
    if n == 0 {
        return Err(io::Error::from(ErrorKind::UnexpectedEof).into());
    }

    match version_buffer[0] {
        0x04 => {
            let mut stream = BufReader::new(stream);
            let req = read_socks4_handshake(&mut stream).await?;
            // tracing::info!("[{peer_addr}] handle socks4");
            let handler = Socks4TcpHandler::new(server.context, server.balancer, server.mode);
            handler.handle_socks4_req(req, stream, peer_addr).await?;
            Ok(())
        }

        0x05 => {
            let req = read_socks5_handshake(&mut stream).await?;
            // tracing::info!("[{peer_addr}] handle socks5");
            let handler = Socks5TcpHandler::new(
                server.context,
                server.udp_bind_addr,
                server.balancer,
                server.mode,
                server.socks5_auth,
            );
            handler.handle_socks5_req(req, stream, peer_addr).await?;
            Ok(())
        }

        0x09 => {
            // tracing::debug!("handle_custom ...");
            let r = handle_custom(&mut stream)
                .await
                .with_context(|| "handle_custom");
            // tracing::debug!("handle_custom finished [{r:?}]");
            r
        }

        version => {
            error!("unsupported socks version {:x}", version);
            let err = io::Error::new(ErrorKind::Other, "unsupported socks version");
            Err(err.into())
        }
    }
}

async fn read_socks5_handshake<R>(r: &mut R) -> Result<socks5::HandshakeRequest, io::Error>
where
    R: AsyncRead + Unpin,
{
    let mut buf = [0u8; 2];
    let _ = r.read_exact(&mut buf[1..]).await?;

    // let ver = buf[0];
    let nmet = buf[1];

    // if ver != consts::SOCKS5_VERSION {
    //     return Err(Error::UnsupportedSocksVersion(ver));
    // }

    let mut methods = vec![0u8; nmet as usize];
    let _ = r.read_exact(&mut methods).await?;

    Ok(socks5::HandshakeRequest { methods })
}

async fn read_socks4_handshake<R>(r: &mut R) -> Result<socks4::HandshakeRequest>
where
    R: AsyncBufRead + Unpin,
{
    use socks4::Address;
    use socks4::Command;

    mod consts {
        pub const SOCKS4_VERSION: u8 = 4;

        pub const SOCKS4_COMMAND_CONNECT: u8 = 1;
        pub const SOCKS4_COMMAND_BIND: u8 = 2;

        // pub const SOCKS4_RESULT_REQUEST_GRANTED:                   u8 = 90;
        // pub const SOCKS4_RESULT_REQUEST_REJECTED_OR_FAILED:        u8 = 91;
        // pub const SOCKS4_RESULT_REQUEST_REJECTED_CANNOT_CONNECT:   u8 = 92;
        // pub const SOCKS4_RESULT_REQUEST_REJECTED_DIFFERENT_USER_ID: u8 = 93;
    }

    let mut buf = [consts::SOCKS4_VERSION; 8];
    let _ = r.read_exact(&mut buf[1..]).await?;

    // let vn = buf[0];
    // if vn != consts::SOCKS4_VERSION {
    //     return Err(Error::UnsupportedSocksVersion(vn));
    // }

    let cd = buf[1];

    let command = match cd {
        consts::SOCKS4_COMMAND_CONNECT => Command::Connect,
        consts::SOCKS4_COMMAND_BIND => Command::Bind,
        _ => bail!("UnsupportedSocksVersion({cd})"),
    };

    // let command = match command {
    //     Some(c) => c,
    //     None => {
    //         return Err(Error::UnsupportedSocksVersion(cd));
    //     }
    // };

    // let port = BigEndian::read_u16(&buf[2..4]);
    let port = (&buf[2..4]).get_u16();

    let mut user_id = Vec::new();
    let _ = r.read_until(b'\0', &mut user_id).await?;
    if user_id.is_empty() || user_id.last() != Some(&b'\0') {
        return Err(io::Error::from(ErrorKind::UnexpectedEof).into());
    }
    user_id.pop(); // Pops the last b'\0'

    let dst = if buf[4] == 0x00 && buf[5] == 0x00 && buf[6] == 0x00 && buf[7] != 0x00 {
        // SOCKS4a, indicates that it is a HOST address
        let mut host = Vec::new();
        let _ = r.read_until(b'\0', &mut host).await?;
        if host.is_empty() || host.last() != Some(&b'\0') {
            return Err(io::Error::from(ErrorKind::UnexpectedEof).into());
        }
        host.pop(); // Pops the last b'\0'

        match String::from_utf8(host) {
            Ok(host) => Address::DomainNameAddress(host, port),
            Err(..) => {
                // return Err(Error::AddressHostInvalidEncoding);
                bail!("AddressHostInvalidEncoding")
            }
        }
    } else {
        let ip = Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]);
        Address::SocketAddress(SocketAddrV4::new(ip, port))
    };

    Ok(socks4::HandshakeRequest {
        cd: command,
        dst,
        user_id,
    })
}

// pub async fn run_socks_conn<S>(version_buffer: &[u8], stream: S, peer_addr: SocketAddr, server: Server) -> io::Result<()>
// where
//     S: AsyncRead + AsyncWrite + Unpin,
// {
//     // tracing::info!("[{peer_addr}] run_socks");

//     // let mut version_buffer = [0u8; 1];

//     // let n = stream.peek(&mut version_buffer).await?;
//     // if n == 0 {
//     //     return Err(ErrorKind::UnexpectedEof.into());
//     // }

//     match version_buffer[0] {
//         0x04 => {
//             // tracing::info!("[{peer_addr}] handle socks4");
//             let handler = Socks4TcpHandler::new(
//                 server.context,
//                 server.balancer,
//                 server.mode
//             );
//             handler.handle_socks4_client(stream, peer_addr).await
//         }

//         0x05 => {
//             // tracing::info!("[{peer_addr}] handle socks5");
//             let handler = Socks5TcpHandler::new(
//                 server.context,
//                 server.udp_bind_addr,
//                 server.balancer,
//                 server.mode,
//                 server.socks5_auth
//             );
//             handler.handle_socks5_client(stream, peer_addr).await
//         }

//         version => {
//             error!("unsupported socks version {:x}", version);
//             let err = io::Error::new(ErrorKind::Other, "unsupported socks version");
//             Err(err)
//         }
//     }
// }

async fn handle_custom<S>(stream: &mut S) -> Result<(), io::Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let cmd = stream.read_u8().await?;
    match cmd {
        0 => handle_echo(stream).await,
        _ => {
            error!("unsupported custom cmd {cmd:?}");
            let err = io::Error::new(ErrorKind::Other, "unsupported custom cmd");
            Err(err.into())
        }
    }
}

async fn handle_echo<S>(stream: &mut S) -> Result<(), io::Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = vec![0_u8; 16 * 1024];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }

        stream.write_all(&buf[..n]).await?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct Server {
    context: Arc<ServiceContext>,
    udp_bind_addr: Arc<ServerAddr>,
    balancer: PingBalancer,
    mode: Mode,
    socks5_auth: Arc<Socks5AuthConfig>,
}

impl Server {
    pub async fn try_new(bind_addr: &str) -> Result<Self> {
        let context = Arc::new(ServiceContext::default());
        let mode = Mode::TcpOnly;

        Ok(Self {
            udp_bind_addr: Arc::new(bind_addr.parse().map_err(|e| anyhow!("{:?}", e))?),
            balancer: PingBalancerBuilder::new(context.clone(), mode)
                .build()
                .await?,
            socks5_auth: Arc::new(Socks5AuthConfig::new()),
            mode,
            context,
        })
    }
}

pub use socks_bridge::run_socks5_conn_bridge;

mod socks_bridge {
    use anyhow::{bail, Result};
    use shadowsocks::relay::socks5::{
        self, HandshakeRequest, HandshakeResponse, PasswdAuthRequest, PasswdAuthResponse,
    };
    use shadowsocks_service::local::socks::config::Socks5AuthConfig;
    use std::{
        io::{self, ErrorKind},
        sync::Arc,
    };
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
    use tracing::{error, trace};

    use super::read_socks5_handshake;

    pub async fn run_socks5_conn_bridge<R1, W1, R2, W2>(
        src_rd: &mut R1,
        src_wr: &mut W1,
        dst_rd: &mut R2,
        dst_wr: &mut W2,
        auth: &Arc<Socks5AuthConfig>,
        // peer_addr: SocketAddr,
    ) -> Result<()>
    where
        R1: AsyncRead + Unpin,
        W1: AsyncWrite + Unpin,
        R2: AsyncRead + Unpin,
        W2: AsyncWrite + Unpin,
    {
        let mut version_buffer = [0u8; 1];

        let n = src_rd.read(&mut version_buffer).await?;
        if n == 0 {
            return Err(io::Error::from(ErrorKind::UnexpectedEof).into());
        }

        match version_buffer[0] {
            0x05 => {}

            version => {
                error!("unsupported socks version {:x}", version);
                let err = io::Error::new(ErrorKind::Other, "unsupported socks version");
                return Err(err.into());
            }
        }

        let req = read_socks5_handshake(src_rd).await?;
        check_auth(src_rd, src_wr, &req, auth).await?;

        let req = HandshakeRequest::new(vec![socks5::SOCKS5_AUTH_METHOD_NONE]);
        req.write_to(dst_wr).await?;

        let rsp = HandshakeResponse::read_from(dst_rd).await?;
        if rsp.chosen_method != socks5::SOCKS5_AUTH_METHOD_NONE {
            let msg = format!("expect response method [None] but [{}]", rsp.chosen_method);
            bail!("{msg}");
        }

        tokio::select! {
            r = tokio::io::copy(dst_rd, src_wr) => {r?;},
            r = tokio::io::copy(src_rd, dst_wr) => {r?;},
        }

        Ok(())
    }

    async fn check_auth<R, W>(
        rd: &mut R,
        wr: &mut W,
        handshake_req: &HandshakeRequest,
        auth: &Arc<Socks5AuthConfig>,
    ) -> io::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        use std::io::Error;

        let allow_none = !auth.auth_required();

        for method in handshake_req.methods.iter() {
            match *method {
                socks5::SOCKS5_AUTH_METHOD_PASSWORD => {
                    let resp = HandshakeResponse::new(socks5::SOCKS5_AUTH_METHOD_PASSWORD);
                    trace!("reply handshake {:?}", resp);
                    resp.write_to(wr).await?;

                    return check_auth_password(rd, wr, auth).await;
                }
                socks5::SOCKS5_AUTH_METHOD_NONE => {
                    if !allow_none {
                        trace!("none authentication method is not allowed");
                    } else {
                        let resp = HandshakeResponse::new(socks5::SOCKS5_AUTH_METHOD_NONE);
                        trace!("reply handshake {:?}", resp);
                        resp.write_to(wr).await?;

                        return Ok(());
                    }
                }
                _ => {
                    trace!("unsupported authentication method {}", method);
                }
            }
        }

        let resp = HandshakeResponse::new(socks5::SOCKS5_AUTH_METHOD_NOT_ACCEPTABLE);
        resp.write_to(wr).await?;

        trace!("reply handshake {:?}", resp);

        Err(Error::new(
            ErrorKind::Other,
            "currently shadowsocks-rust does not support authentication",
        ))
    }

    async fn check_auth_password<R, W>(
        rd: &mut R,
        wr: &mut W,
        auth: &Arc<Socks5AuthConfig>,
    ) -> io::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        use std::io::Error;

        const PASSWORD_AUTH_STATUS_FAILURE: u8 = 255;

        // Read initiation negociation

        let req = match PasswdAuthRequest::read_from(rd).await {
            Ok(i) => i,
            Err(err) => {
                let rsp = PasswdAuthResponse::new(err.as_reply().as_u8());
                let _ = rsp.write_to(wr).await;

                return Err(Error::new(
                    ErrorKind::Other,
                    format!("Username/Password Authentication Initial request failed: {err}"),
                ));
            }
        };

        let user_name = match std::str::from_utf8(&req.uname) {
            Ok(u) => u,
            Err(..) => {
                let rsp = PasswdAuthResponse::new(PASSWORD_AUTH_STATUS_FAILURE);
                let _ = rsp.write_to(wr).await;

                return Err(Error::new(
                    ErrorKind::Other,
                    "Username/Password Authentication Initial request uname contains invaid characters",
                ));
            }
        };

        let password = match std::str::from_utf8(&req.passwd) {
            Ok(u) => u,
            Err(..) => {
                let rsp = PasswdAuthResponse::new(PASSWORD_AUTH_STATUS_FAILURE);
                let _ = rsp.write_to(wr).await;

                return Err(Error::new(
                    ErrorKind::Other,
                    "Username/Password Authentication Initial request passwd contains invaid characters",
                ));
            }
        };

        if auth.passwd.check_user(user_name, password) {
            trace!(
                "socks5 authenticated with Username/Password method, user: {}, password: {}",
                user_name,
                password
            );

            let rsp = PasswdAuthResponse::new(0);
            rsp.write_to(wr).await?;

            Ok(())
        } else {
            let rsp = PasswdAuthResponse::new(PASSWORD_AUTH_STATUS_FAILURE);
            rsp.write_to(wr).await?;

            error!(
                "socks5 rejected Username/Password user: {}, password: {}",
                user_name, password
            );

            Err(Error::new(
                ErrorKind::Other,
                format!("Username/Password Authentication failed, user: {user_name}, password: {password}"),
            ))
        }
    }
}
