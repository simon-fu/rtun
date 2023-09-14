use std::{net::SocketAddr, sync::Arc};

use anyhow::{Result, Context};

use clap::Parser;

use rtun::{switch::agent::ch_socks::run_socks_conn, async_rt::spawn_with_name};
use tokio::net::{TcpListener, TcpStream};

use crate::init_log_and_run;

pub fn run(args: CmdArgs) -> Result<()> { 
    init_log_and_run(do_run(args))?
}

async fn do_run(args: CmdArgs) -> Result<()> { 

    let shared = Arc::new(Shared {
        server: rtun::switch::agent::ch_socks::Server::try_new("127.0.0.1:1080").await?,
    });

    {
        let listener = TcpListener::bind(&args.listen).await
        .with_context(||format!("fail to bind address [{}]", args.listen))?;
        tracing::info!("socks5 listen on [{}]", args.listen);

        let shared = shared.clone();

        let task = spawn_with_name("local_sock", async move {
            let r = run_socks_server(shared, listener).await;
            r
        });

        task.await??;
        Ok(())
    }

}


struct Shared {
    server: rtun::switch::agent::ch_socks::Server,
}



async fn run_socks_server( shared: Arc<Shared>, listener: TcpListener ) -> Result<()> {

    loop {
        let (stream, peer_addr)  = listener.accept().await.with_context(||"accept tcp failed")?;
        
        tracing::debug!("[{peer_addr}] client connected");

        let server = shared.server.clone();

        tokio::spawn(async move {
                    
            let r = handle_client_directly(
                stream, 
                peer_addr,
                server,
            ).await;
            tracing::debug!("[{peer_addr}] client finished with {r:?}");
            r
        });
    }
}

async fn handle_client_directly( 
    stream: TcpStream, 
    peer_addr: SocketAddr,
    server: rtun::switch::agent::ch_socks::Server,
) -> Result<()> {

    // let mut version_buffer = [0u8; 1];

    // let n = stream.peek(&mut version_buffer).await?;
    // if n == 0 {
    //     return Err(io::Error::from(ErrorKind::UnexpectedEof).into());
    // }
    
    // run_socks_conn(&version_buffer[..], stream, peer_addr, server).await?;
    run_socks_conn(stream, peer_addr, server).await?;
    Ok(())
}




#[derive(Parser, Debug)]
#[clap(name = "local", author, about, version)]
pub struct CmdArgs {
    #[clap(
        short = 'l',
        long = "listen",
        long_help = "listen address",
        default_value = "0.0.0.0:12080",
    )]
    listen: String,
}

