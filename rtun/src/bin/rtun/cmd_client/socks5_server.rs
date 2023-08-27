use std::{sync::Arc, io::{self, ErrorKind}, net::SocketAddr};

use anyhow::{Result, anyhow, Context};
use rtun::socks::server::{socks5::Socks5TcpHandler, socks4::Socks4TcpHandler};
use shadowsocks::{config::Mode, ServerAddr};
use shadowsocks_service::local::{loadbalancing::{PingBalancerBuilder, PingBalancer}, context::ServiceContext, socks::config::Socks5AuthConfig};
use tokio::net::{TcpListener, TcpStream};
use tracing::error;

pub async fn run() -> Result<()> {
    let bind_addr = "127.0.0.1:2080";

    // let context = Arc::new(ServiceContext::default());
    // let mode = Mode::TcpOnly;

    let server = Server::try_new(bind_addr).await?;

    let listener = TcpListener::bind(bind_addr).await
    .with_context(||format!("fail to bind address [{}]", bind_addr))?;
    tracing::debug!("socks5 listen on [{}]", bind_addr);

    loop {
        let (stream, peer_addr)  = listener.accept().await.with_context(||"accept tcp failed")?;
        
        tracing::debug!("[{peer_addr}] client connected");

        let server = server.clone();
        tokio::spawn(async move {
            let r = handle_client(
                stream, 
                peer_addr,
                server,
            ).await;
            tracing::debug!("[{peer_addr}] client finished with {r:?}");
            r
        });
    }

    // Ok(())
}

async fn handle_client(
    stream: TcpStream,
    peer_addr: SocketAddr,
    server: Server,
) -> io::Result<()> {
    
    let mut version_buffer = [0u8; 1];
    let n = stream.peek(&mut version_buffer).await?;
    if n == 0 {
        return Err(ErrorKind::UnexpectedEof.into());
    }

    match version_buffer[0] {
        0x04 => {
            tracing::info!("[{peer_addr}] handle socks4");
            let handler = Socks4TcpHandler::new(
                server.context, 
                server.balancer, 
                server.mode
            );
            handler.handle_socks4_client(stream, peer_addr).await
        }

        0x05 => {
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
struct Server {
    context: Arc<ServiceContext>,
    udp_bind_addr: Arc<ServerAddr>,
    balancer: PingBalancer,
    mode: Mode,
    socks5_auth: Arc<Socks5AuthConfig>,
}

impl Server {
    async fn try_new(bind_addr: &str) -> Result<Self> {
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
