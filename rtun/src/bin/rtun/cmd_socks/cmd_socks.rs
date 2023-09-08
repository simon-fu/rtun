use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Result, Context};

use clap::Parser;
use parking_lot::Mutex;
use rtun::{ws::client::ws_connect_to, switch::{invoker_ctrl::{CtrlHandler, CtrlInvoker}, next_ch_id::NextChId, session_stream::{make_stream_session, StreamSession}, switch_sink::PacketSink, switch_source::PacketSource}, channel::{ChId, ChPair, ch_stream::ChStream}, proto::{OpenSocksArgs, OpenP2PArgs, open_p2presponse::Open_p2p_rsp}, async_rt::spawn_with_name, stun::punch::{IcePeer, IceConfig}};
use tokio::net::{TcpListener, TcpStream};

use crate::{client_utils::client_select_url, rest_proto::get_agent_from_url};

pub async fn run(args: CmdArgs) -> Result<()> { 

    let shared = Arc::new(Shared {
        data: Mutex::new(SharedData {
            ctrl: None,
        }),
    });

    {
        let listener = TcpListener::bind(&args.listen).await
        .with_context(||format!("fail to bind address [{}]", args.listen))?;
        tracing::info!("socks5 listen on [{}]", args.listen);

        let shared = shared.clone();

        spawn_with_name("local_sock", async move {
            let r = run_socks_server(shared, listener).await;
            r
        });
    }

    let mut last_success = true;

    loop {

        let r = try_connect(&args).await;
        match r {
            Ok(mut session) => {
                tracing::info!("session connected");
                last_success = true;

                // let mut session = make_stream_session(stream).await?;

                {
                    // let ice_agent = Arc::new(
                    //     IceAgent::new(AgentConfig {
                    //         urls: vec![
                    //             webrtc_ice::url::Url::parse_url("stun:stun1.l.google.com:19302")?,
                    //             webrtc_ice::url::Url::parse_url("stun:stun2.l.google.com:19302")?,
                    //             webrtc_ice::url::Url::parse_url("stun:stun.qq.com:3478")?,
                    //         ],
                    //         network_types: vec![NetworkType::Udp4],
                    //         udp_network: UDPNetwork::Ephemeral(Default::default()),
                    //         ..Default::default()
                    //     })
                    //     .await?,
                    // );

                    // ice_agent.on_connection_state_change(Box::new(move |c: ConnectionState| {
                    //     tracing::debug!("ICE Connection State has changed: {c}");
                    //     if c == ConnectionState::Failed {
                            
                    //     }
                    //     Box::pin(async move {})
                    // }));
                    

                    // let (tx, rx) = oneshot::channel();
                    // let mut tx = Some(tx);
                    // ice_agent.on_candidate(Box::new(move |c: Option<Arc<dyn Candidate + Send + Sync>>| {
                    //     if c.is_none() {
                    //         if let Some(tx) = tx.take() {
                    //             let _r = tx.send(());
                    //         }
                    //     }
                    //     Box::pin(async move {})
                    // }));

                    // ice_agent.gather_candidates()?;
                    // let _r = rx.await;

                    // let local_candidates: Vec<String> = ice_agent
                    // .get_local_candidates().await?
                    // .iter()
                    // .map(|c|c.marshal())
                    // .collect();
                    // tracing::debug!("local_candidates: {local_candidates:?}");
                    
                    // let (local_ufrag, local_pwd) = ice_agent.get_local_user_credentials().await;
                    // tracing::debug!("credentials: {local_ufrag:?}, {local_pwd:?}");
                    // tokio::time::sleep(Duration::MAX).await;

                    let mut peer = IcePeer::with_config(IceConfig {
                        servers: vec![
                            "stun:stun1.l.google.com:19302".into(),
                            "stun:stun2.l.google.com:19302".into(),
                            "stun:stun.qq.com:3478".into(),
                        ],
                    });

                    let local_args = peer.gather_until_done().await?;
                    tracing::debug!("local args {local_args:?}");
                    
                    // let (peer, nat) = PunchPeer::bind_and_detect("0.0.0.0:0").await?;

                    // let local_ufrag = peer.local_ufrag().to_string();
                
                    // let nat_type = nat.nat_type();
                    // if nat_type != Some(NatType::Cone) {
                    //     tracing::warn!("nat type {nat_type:?}");
                    // } else {
                    //     tracing::debug!("nat type {nat_type:?}");
                    // }
                    // let mapped = nat.into_mapped().with_context(||"empty mapped address")?;

                    let invoker = session.ctrl_client().clone_invoker();
                    let rsp = invoker.open_p2p(OpenP2PArgs {
                        args: Some(local_args.into()).into(),
                        ..Default::default()
                    }).await?;

                    let rsp = rsp.open_p2p_rsp.with_context(||"no open_p2p_rsp")?;
                    match rsp {
                        Open_p2p_rsp::Args(remote_args) => {
                            tracing::debug!("remote args {remote_args:?}");
                            // let mut tun = launch_tun_peer(peer, args.ufrag.into(), args.addr.parse()?, false);
                            spawn_with_name("peer-client", async move {
                                let conn = peer.dial(remote_args.into()).await?;
                                conn.send_data("I'am client".as_bytes()).await?;
                                let mut buf = vec![0; 1700];
                                let n = conn.recv_data(&mut buf).await?;
                                let msg = std::str::from_utf8(&buf[..n])?;
                                tracing::debug!("recv {msg:?}");
                                Result::<()>::Ok(())
                            });
                        },
                        Open_p2p_rsp::Status(s) => {
                            tracing::warn!("open p2p but {s:?}");
                        },
                        _ => {
                            tracing::warn!("unknown Open_p2p_rsp {rsp:?}");
                        }
                    }                    


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
                tokio::time::sleep(Duration::from_millis(1000)).await;
            },
        }
    }

}


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
    .with_context(||format!("fail to connect to [{}]", url_str))?;

    tracing::info!("select agent {:?}", get_agent_from_url(&url));

    // let uid = gen_huid();
    // let mut switch = make_switch_pair(uid, stream.split()).await?;
    let session = make_stream_session(stream.split()).await?;
    
    Ok(session)
}



async fn run_socks_server<H: CtrlHandler>( shared: Arc<Shared<H>>, listener: TcpListener ) -> Result<()> {
    let mut next_ch_id = NextChId::default();
    loop {
        let (stream, peer_addr)  = listener.accept().await.with_context(||"accept tcp failed")?;
        
        tracing::debug!("[{peer_addr}] client connected");

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
                    tracing::debug!("[{peer_addr}] client finished with {r:?}");
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

    let ch_tx = ctrl.open_socks(ch_tx, open_args).await?;
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
        default_value = "0.0.0.0:12080",
    )]
    listen: String,

    #[clap(
        long = "secret",
        long_help = "authentication secret",
    )]
    secret: Option<String>,
}

