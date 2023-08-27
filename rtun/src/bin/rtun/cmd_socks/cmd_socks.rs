use std::net::SocketAddr;

use anyhow::{Result, Context, bail, anyhow};
use bytes::BytesMut;
use clap::Parser;
use rtun::{ws::client::ws_connect_to, switch::{switch_stream::make_stream_switch, ctrl_client::make_ctrl_client, invoker_ctrl::{CtrlHandler, CtrlInvoker}, next_ch_id::NextChId}, huid::gen_huid::gen_huid, channel::{ChId, ChPair, ChSender, ChReceiver}, proto::OpenSocksArgs};
use tokio::{net::{TcpListener, TcpStream}, io::{AsyncReadExt, AsyncWriteExt}};

use crate::client_utils::client_select_url;

pub async fn run(args: CmdArgs) -> Result<()> { 

    let url = client_select_url(&args.url, args.agent.as_deref()).await?;
    let url = url.as_str();

    let (stream, _r) = ws_connect_to(url).await
    .with_context(||format!("fail to connect to [{}]", url))?;

    tracing::debug!("connected to [{}]", url);

    let uid = gen_huid();
    let mut switch_session = make_stream_switch(uid, stream).await?;
    let switch = switch_session.clone_invoker();

    let ctrl_ch_id = ChId(0);

    let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;
    
    let pair = ChPair { tx: ctrl_tx, rx: ctrl_rx };

    // let mut ctrl = ClientChannelCtrl::new(pair, invoker);
    let mut ctrl_session = make_ctrl_client(uid, pair, switch).await?;
    let ctrl = ctrl_session.clone_invoker();

    let listener = TcpListener::bind(&args.listen).await
    .with_context(||format!("fail to bind address [{}]", args.listen))?;
    tracing::debug!("socks5 listen on [{}]", args.listen);
    
    let mut watch = ctrl.watch().await?;

    tokio::select! {
        r = run_socks_server(ctrl, listener) => {
            tracing::debug!("socks server completed {:?}", r);
        }
        r = watch.watch() => {
            tracing::debug!("connection completed {:?}", r);
        }
    }
    

    println!("\r");
    switch_session.shutdown_and_waitfor().await?;
    ctrl_session.shutdown_and_waitfor().await?;

    Ok(())
}

async fn run_socks_server<H: CtrlHandler>( ctrl: CtrlInvoker<H>, listener: TcpListener ) -> Result<()> {
    let mut next_ch_id = NextChId::default();
    loop {
        let (stream, peer_addr)  = listener.accept().await.with_context(||"accept tcp failed")?;
        
        tracing::debug!("[{peer_addr}] client connected");
        let ctrl = ctrl.clone();
        let ch_id = next_ch_id.next_ch_id();

        tokio::spawn(async move {
            
            let r = handle_client(
                ctrl,
                ch_id,
                stream, 
                peer_addr,
            ).await;
            tracing::debug!("[{peer_addr}] client finished with {r:?}");
            r
        });
    }
}

async fn handle_client<H: CtrlHandler>( 
    ctrl: CtrlInvoker<H>, 
    ch_id: ChId, 
    mut stream: TcpStream, 
    peer_addr: SocketAddr 
) -> Result<()> {

    let (ch_tx, mut ch_rx) = ChPair::new(ch_id).split();

    let open_args = OpenSocksArgs {
        ch_id: Some(ch_id.0),
        peer_addr: peer_addr.to_string().into(),
        ..Default::default()
    };

    let ch_tx = ctrl.open_socks(ch_tx, open_args).await?;
    tracing::debug!("opened socks {} -> {:?}", peer_addr, ch_tx.ch_id());

    let r = copy_loop(&mut stream, &ch_tx, &mut ch_rx).await;
    let _r = ctrl.close_channel(ch_id).await;
    r
}

async fn copy_loop(stream: &mut TcpStream, ch_tx: &ChSender, ch_rx: &mut ChReceiver) -> Result<()> {
    let mut buf = BytesMut::new();
    loop {
        tokio::select! {
            r = ch_rx.recv_packet() => {
                let packet = r.with_context(||"recv but channel closed")?;

                stream.write_all(&packet.payload[..]).await?;
            },
            r = stream.read_buf(&mut buf) => {
                let n = r?;
                if n == 0 {
                    bail!("socket closed")
                }
                let payload = buf.split().freeze();
                ch_tx.send_data(payload).await
                .map_err(|_e|anyhow!("send but channel closed"))?;
            }
        }
    }
}


#[derive(Parser, Debug)]
#[clap(name = "client", author, about, version)]
pub struct CmdArgs {

    #[clap(help="eg: http://127.0.0.1:8080")]
    url: String,

    #[clap(
        short = 'a',
        long = "agent",
        long_help = "agent name",
    )]
    agent: Option<String>,

    #[clap(
        short = 'l',
        long = "listen",
        long_help = "listen address",
        default_value = "0.0.0.0:2080",
    )]
    listen: String,
}

