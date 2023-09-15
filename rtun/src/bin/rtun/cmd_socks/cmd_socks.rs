use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Result, Context};

use clap::Parser;
use parking_lot::Mutex;
use rtun::{ws::client::ws_connect_to, switch::{invoker_ctrl::{CtrlHandler, CtrlInvoker}, next_ch_id::NextChId, session_stream::{make_stream_session, StreamSession}, switch_sink::PacketSink, switch_source::PacketSource}, channel::{ChId, ChPair, ch_stream::ChStream}, proto::OpenSocksArgs, async_rt::spawn_with_name};
use tokio::net::{TcpListener, TcpStream};


use crate::{client_utils::client_select_url, rest_proto::get_agent_from_url, init_log_and_run};
use super::{p2p_throughput::kick_p2p, quic_pool::AgentPool};

pub fn run(args: CmdArgs) -> Result<()> { 
    init_log_and_run(do_run(args))?
}

async fn do_run(args: CmdArgs) -> Result<()> { 

    let shared = Arc::new(Shared {
        data: Mutex::new(SharedData {
            ctrl: None,
        }),
    });

    let agent_pool = AgentPool::new();

    {
        let listen_addr = &args.listen;
        let listen_addr: SocketAddr = listen_addr.parse().with_context(|| format!("invalid addr {listen_addr:?}"))?;

        for n in 0..3 {
            let listen_addr = SocketAddr::new(listen_addr.ip(), listen_addr.port()+n);
            let listener = TcpListener::bind(listen_addr).await
            .with_context(||format!("fail to bind address [{listen_addr}]"))?;
            tracing::info!("socks5(quic) listen on [{listen_addr}]");
    
            let pool = agent_pool.clone();
    
            spawn_with_name("local_sock", async move {
                let r = run_socks_via_quic(pool, listener).await;
                r
            });
        }

    }

    if let Some(socks_ws) = &args.socks_ws {
        let listen_addr = if socks_ws == "0" || socks_ws == "1" || socks_ws == "true" {
            "0.0.0.0:13080"
        } else {
            socks_ws
        };
        let listener = TcpListener::bind(listen_addr).await
        .with_context(||format!("fail to bind address [{listen_addr}]"))?;
        tracing::info!("socks5(ws) listen on [{listen_addr}]");

        let shared = shared.clone();
        spawn_with_name("local_sock", async move {
            let r = run_socks_via_ctrl(shared, listener).await;
            r
        });
    }

    let mut last_success = true;

    loop {

        let r = try_connect(&args).await;
        match r {
            Ok(mut session) => {
                last_success = true;

                // let mut session = make_stream_session(stream).await?;

                agent_pool.set_agent("default".into(), session.ctrl_client().clone_invoker()).await?;

                if let Some(ptype) = args.mode {
                    let invoker = session.ctrl_client().clone_invoker();
                    let _r = kick_p2p(invoker, ptype).await?;
                }

                {
                    shared.data.lock().ctrl = Some(session.ctrl_client().clone_invoker());
                }
                
                let r = session.wait_for_completed().await;
                tracing::info!("session finished {r:?}");
            },
            Err(e) => {
                if last_success {
                    last_success = false;
                    tracing::warn!("connect failed [{e:?}]");
                    tracing::info!("try reconnecting...");
                }
            },
        }

        tokio::time::sleep(Duration::from_millis(1000)).await;
    }

}

// type CtrlSession = self::ctrl::Ctrl;
// type SessionInvoker = self::ctrl::SessionInvoker;
// mod ctrl {
//     use futures::stream::{SplitSink, SplitStream};
//     use rtun::{switch::{session_stream::StreamSession, switch_pair::SwitchPairInvoker}, ws::client::{WsSink, WsSource}};
//     use tokio::net::TcpStream;
//     use tokio_tungstenite::{WebSocketStream, MaybeTlsStream, tungstenite::Message};

//     type Source = WsSource<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>;
//     type Sink = WsSink<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>;
//     pub type Ctrl = StreamSession<Sink, Source>;

//     // pub type Invoker = CtrlInvoker<Entity<Entity<impl PacketSource>>>;
//     pub type SessionInvoker = SwitchPairInvoker<Source>;
    
// }


struct Shared<H: CtrlHandler> {
    data: Mutex<SharedData<H>>,
}

struct SharedData<H: CtrlHandler> {
    ctrl: Option<CtrlInvoker<H>>,
}

async fn try_connect(args: &CmdArgs) -> Result<StreamSession<impl PacketSink, impl PacketSource>> {
    let url = client_select_url(&args.url, args.agent.as_deref(), args.secret.as_deref()).await?;
    let url_str = url.as_str();

    let (stream, _r) = ws_connect_to(url_str).await
    .with_context(||format!("connect to agent failed"))?;

    let agent_name = get_agent_from_url(&url);
    tracing::info!("connected to agent {agent_name:?}");

    // let uid = gen_huid();
    // let mut switch = make_switch_pair(uid, stream.split()).await?;
    let session = make_stream_session(stream.split(), false).await?;
    
    Ok(session)
}

async fn run_socks_via_quic<H: CtrlHandler>( pool: AgentPool<H>, listener: TcpListener ) -> Result<()> {

    loop {
        let (mut stream, peer_addr)  = listener.accept().await.with_context(||"accept tcp failed")?;
        
        tracing::trace!("[{peer_addr}] client connected");

        let pool = pool.clone();
        tokio::spawn(async move {
            let r = async move {
                let (mut wr1, mut rd1) = pool.get_ch().await?;
                let (mut rd2, mut wr2) = stream.split();
                tokio::select! {
                    r = tokio::io::copy(&mut rd2, &mut wr1) => {r?;},
                    r = tokio::io::copy(&mut rd1, &mut wr2) => {r?;},
                }
                Result::<()>::Ok(())
            }.await;
            tracing::trace!("[{peer_addr}] client finished with {r:?}");
            r
        });


        
    }
}


async fn run_socks_via_ctrl<H: CtrlHandler>( shared: Arc<Shared<H>>, listener: TcpListener ) -> Result<()> {
    let mut next_ch_id = NextChId::default();
    loop {
        let (stream, peer_addr)  = listener.accept().await.with_context(||"accept tcp failed")?;
        
        tracing::trace!("[{peer_addr}] client connected");

        let r = {
            shared.data.lock().ctrl.clone()
        };

        match r {
            Some(ctrl) => {
                let ch_id = next_ch_id.next_ch_id();
        
                tokio::spawn(async move {
                    
                    let r = handle_client(
                        ctrl,
                        ch_id,
                        stream, 
                        peer_addr,
                    ).await;
                    tracing::trace!("[{peer_addr}] client finished with {r:?}");
                    r
                });
            },
            None => {},
        }
    }
}

async fn handle_client<H: CtrlHandler>( 
    ctrl: CtrlInvoker<H>, 
    ch_id: ChId, 
    mut stream: TcpStream, 
    peer_addr: SocketAddr 
) -> Result<()> {

    let (ch_tx, ch_rx) = ChPair::new(ch_id).split();

    let open_args = OpenSocksArgs {
        ch_id: Some(ch_id.0),
        peer_addr: peer_addr.to_string().into(),
        ..Default::default()
    };

    let ch_tx = {
        let r = ctrl.open_socks(ch_tx, open_args).await
        .with_context(||"open socks ch failed");
        match r {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("{e:?}");
                return Err(e)
            }
        }
    };
    tracing::debug!("opened socks {} -> {:?}", peer_addr, ch_tx.ch_id());

    // let mut ch_rx = ch_rx;
    // let r = copy::copy_loop(&mut stream, &ch_tx, &mut ch_rx).await;

    let mut ch_stream = ChStream::new2(ch_tx, ch_rx);
    // let r = copy_stream_bidir(&mut stream, &mut ch_stream).await;
    let r = tokio::io::copy_bidirectional(&mut stream, &mut ch_stream).await;
    
    let _r = ctrl.close_channel(ch_id).await;
    r?;
    Ok(())
}

// mod copy {
//     use anyhow::{Result, bail, Context, anyhow};
//     use bytes::BytesMut;
//     use rtun::channel::{ChSender, ChReceiver};
//     use tokio::{net::TcpStream, io::{AsyncWriteExt, AsyncReadExt}};

//     pub async fn copy_loop(stream: &mut TcpStream, ch_tx: &ChSender, ch_rx: &mut ChReceiver) -> Result<()> {
//         let mut buf = BytesMut::new();
//         loop {
//             tokio::select! {
//                 r = ch_rx.recv_packet() => {
//                     let packet = r.with_context(||"recv but channel closed")?;
    
//                     stream.write_all(&packet.payload[..]).await
//                     .with_context(||"write but stream closed")?;
//                 },
//                 r = stream.read_buf(&mut buf) => {
//                     let n = r.with_context(||"recv but stream closed")?;
//                     if n == 0 {
//                         bail!("socket recv-zero closed")
//                     }
//                     let payload = buf.split().freeze();
//                     ch_tx.send_data(payload).await
//                     .map_err(|_e|anyhow!("send but channel closed"))?;
//                 }
//             }
//         }
//     }
// }



#[derive(Parser, Debug)]
#[clap(name = "socks", author, about, version)]
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
        default_value = "0.0.0.0:12080",
    )]
    listen: String,

    #[clap(
        long = "secret",
        long_help = "authentication secret",
    )]
    secret: Option<String>,

    #[clap(
        long = "mode",
        long_help = "tunnel mode",
    )]
    mode: Option<u32>,

    #[clap(
        long = "socks-ws",
        long_help = "listen addr for socks via ws",
    )]
    socks_ws: Option<String>,
}

