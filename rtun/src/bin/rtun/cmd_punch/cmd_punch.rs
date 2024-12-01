
/*
    1. Scenario: client 
    punch --server stun:stun.miwifi.com:3478

    2. Scenario: server
    
*/



use std::{sync::Arc, time::Duration};

use clap::Parser;
use anyhow::{Context, Result};
use dialoguer::{theme::ColorfulTheme, Input};
use tokio::net::UdpSocket;
use tracing::debug;
use crate::init_log_and_run;
use rtun::ice::ice_peer::{IceArgs, IceConfig, IcePeer};


pub fn run(args: CmdArgs) -> Result<()> { 
    init_log_and_run(do_run(args))?
}

async fn do_run(args: CmdArgs) -> Result<()> {
    debug!("hello {args:?}");
    
    debug!("kick gather candidate, servers {:?}", args.servers);

    let mut peer = IcePeer::with_config(IceConfig {
        servers: args.servers,
        ..Default::default()
    });

    let conn = match args.remote {
        Some(remote) => {
            let remote_args: IceArgs = serde_json::from_str(&remote).with_context(||"parse remote args failed")?;
            let local_args = peer.server_gather(remote_args).await.with_context(||"server_gather failed")?;
            debug!("local args [\n{}\n]", serde_json::to_string(&local_args)?);
            peer.accept_timeout(Duration::from_secs(999999)).await.with_context(||"accept failed")?
        },
        None => {
            let local_args = peer.client_gather().await?;
            debug!("local args [\n{}\n]", serde_json::to_string(&local_args)?);

            let remote_str: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("remote args")
            .interact_text().with_context(||"input remote args failed")?;

            let remote_args: IceArgs = serde_json::from_str(&remote_str).with_context(||"parse remote args failed")?;

            peer.dial(remote_args).await.with_context(||"dial failed")?
        },
    };

    debug!("connected [{}] -> [{}]", conn.local_addr()?, conn.remote_addr());
    // let remote_addr = conn.remote_addr();
    let (socket, _cfg, remote_addr) = conn.into_parts();
    // let conn = conn.into_async_udp();
    let r = socket.connect(remote_addr).await.with_context(||"udp connect failed");
    debug!("udp connect result {r:?}");
    let socket = Arc::new(socket);

    
    if let Some(listen_addr) = &args.relay_listen {
        let src = UdpSocket::bind(listen_addr).await.with_context(||format!("failed to listen at [{listen_addr}]"))?;
        debug!("listen at [{listen_addr}]");
        let mut buf = vec![0_u8; 1700];
        let (len , from) = src.recv_from(&mut buf).await.with_context(||"recv first failed")?;
        debug!("recv first from [{from}], len [{len}]");
        src.connect(from).await.with_context(||"connect to from failed")?;
        socket.send(&buf[..len]).await.with_context(||"send first failed")?;
        debug!("relay path: from[{from}] -> [{}] -> [{}] -> tunnel", src.local_addr()?, socket.local_addr()?);
        relay_udp(&src, &socket).await
    } else if let Some(target_addr) = &args.relay_to {
        
        let listen_addr = "0.0.0.0:0";
        let dst = UdpSocket::bind(listen_addr).await.with_context(||format!("failed to listen at [{listen_addr}]"))?;
        dst.connect(target_addr).await.with_context(||"connect to target failed")?;
        debug!("relay path: tunnel -> [{}] -> [{}] -> target[{target_addr}]", socket.local_addr()?, dst.local_addr()?);
        relay_udp(&socket, &dst).await

    } else {

        {
            let socket = socket.clone();
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 1700];
                loop {
                    let r = socket.recv(&mut buf).await;
                    debug!("recv {r:?}");
                }
            });
        }

        for n in 0..5 {
            let content = format!("msg {n}");
            let r = socket.send(content.as_bytes()).await.with_context(||"udp sen d failed");
            debug!("No.{n}: sent result [{r:?}]");
    
            // let len = socket.send_to(content.as_bytes(), remote_addr).await.with_context(||"udp sen dto failed")?;
            // debug!("sent len [{len}]");
    
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    
        Ok(())
    }


}

async fn relay_udp(src: &UdpSocket, dst: &UdpSocket) -> Result<()> {
    let mut src_buf = vec![0_u8; 1700];
    let mut dst_buf = vec![0_u8; 1700];

    loop {
        tokio::select! {
            r = src.recv(&mut src_buf) => {
                let len = r.with_context(||"src recv failed")?;
                let _r = dst.send(&src_buf[..len]).await;
            }
            r = dst.recv(&mut dst_buf) => {
                let len = r.with_context(||"dst recv failed")?;
                let _r = src.send(&dst_buf[..len]).await;
            }
        }
    }
}

// refer https://github.com/clap-rs/clap/tree/master/clap_derive/examples
#[derive(Parser, Debug)]
#[clap(name = "punch", author, about, version)]
pub struct CmdArgs {
    #[clap(
        short = 's',
        long = "server",
        long_help = "stun/turn server address, eg. stun:stun.miwifi.com:3478",
    )]
    servers: Vec<String>,

    #[clap(
        long = "remote",
        long_help = "remote ice args, run as server",
    )]
    remote: Option<String>,

    #[clap(
        long = "relay-listen",
        long_help = "relay proxy listen",
    )]
    relay_listen: Option<String>,

    #[clap(
        long = "relay-to",
        long_help = "relay to targeet",
    )]
    relay_to: Option<String>,
}

// #[derive(Parser, Debug)]
// pub enum SubCmd {
//     Agent(sub_agent::CmdArgs),
//     Bridge(sub_bridge::CmdArgs),
// }


