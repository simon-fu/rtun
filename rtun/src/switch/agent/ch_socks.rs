use std::{io::{ErrorKind, self}, net::SocketAddr, sync::Arc};

use crate::{channel::{ChSender, ChReceiver, ch_stream::ChStream}, proto::OpenSocksArgs};

use anyhow::{anyhow, Result};
use crate::socks::server::{socks5::Socks5TcpHandler, socks4::Socks4TcpHandler};
use shadowsocks::{config::Mode, ServerAddr};
use shadowsocks_service::local::{loadbalancing::{PingBalancerBuilder, PingBalancer}, context::ServiceContext, socks::config::Socks5AuthConfig};
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

    pub async fn run(self, server: Server, tx: ChSender, rx: ChReceiver ) -> Result<()> {
        let stream = ChStream::new2(tx, rx);
        run_socks(stream, self.peer_addr, server).await?;
        Ok(())
    }

}

async fn run_socks(mut stream: ChStream, peer_addr: SocketAddr, server: Server) -> io::Result<()> {
    // tracing::info!("[{peer_addr}] run_socks");

    let mut version_buffer = [0u8; 1];

    let n = stream.peek(&mut version_buffer).await?;
    if n == 0 {
        return Err(ErrorKind::UnexpectedEof.into());
    }

    match version_buffer[0] {
        0x04 => {
            // tracing::info!("[{peer_addr}] handle socks4");
            let handler = Socks4TcpHandler::new(
                server.context, 
                server.balancer, 
                server.mode
            );
            handler.handle_socks4_client(stream, peer_addr).await
        }

        0x05 => {
            // tracing::info!("[{peer_addr}] handle socks5");
            let handler = Socks5TcpHandler::new(
                server.context, 
                server.udp_bind_addr, 
                server.balancer, 
                server.mode, 
                server.socks5_auth
            );
            handler.handle_socks5_client(stream, peer_addr).await
        }

        version => {
            error!("unsupported socks version {:x}", version);
            let err = io::Error::new(ErrorKind::Other, "unsupported socks version");
            Err(err)
        }
    }
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
            udp_bind_addr: Arc::new(
                bind_addr.parse()
                .map_err(|e|anyhow!("{:?}", e))?
            ),
            balancer: PingBalancerBuilder::new(context.clone(), mode).build().await?,
            socks5_auth: Arc::new(Socks5AuthConfig::new()),
            mode,
            context,
        })
    }
}

