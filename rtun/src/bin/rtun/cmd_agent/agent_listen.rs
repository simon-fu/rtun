
/*
    websocket refer: https://github.com/tokio-rs/axum/blob/axum-v0.6.20/examples/websockets/src/main.rs
*/

use anyhow::{Result, Context, bail};
use axum_server::{Handle, tls_rustls::RustlsConfig};
use chrono::Local;
use clap::Parser;
use axum::{Extension, extract::Query, http::StatusCode, Json};
use futures::{StreamExt, stream::SplitStream};
use parking_lot::Mutex;
use rtun::{huid::{gen_huid::gen_huid, HUId}, channel::{ChId, ChPair}, switch::{ctrl_service::spawn_ctrl_service, agent::ctrl::{make_agent_ctrl, AgentCtrlInvoker}, ctrl_client, invoker_ctrl::{CtrlInvoker, CtrlHandler}, session_stream::make_stream_session, switch_pair::{SwitchPairEntity, make_switch_pair}, switch_sink::PacketSink, switch_source::PacketSource}, ws::server::{WsStreamAxum, WsSource}, async_rt::spawn_with_name, proto::KickDownArgs};
use tokio::net::TcpListener;

use std::{sync::Arc, collections::HashMap, path::Path, time::Duration};
use std::net::SocketAddr;
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use axum::{
    extract::{
        ws::{WebSocket, WebSocketUpgrade},
        connect_info::ConnectInfo,
    },
    response::IntoResponse,
    routing::get,
    Router,
};

use crate::{quic_signal::{self, PubResponse, SessionsResponse, SignalRequest, SignalResponse, StatusResponse}, rest_proto::{PUB_WS, SUB_WS, PUB_SESSIONS, PubParams, SubParams, AgentInfo}, secret::token_verify};


pub async fn run(args: CmdArgs) -> Result<()> {
    let addr_r: Result<SocketAddr> = args.addr.parse()
    .with_context(||"invalid address");

    if let Ok(addr) = &addr_r {
        return run_http(args, *addr).await;
    }

    bail!("invalid address [{}]", args.addr)
}


async fn run_http(args: CmdArgs, addr: SocketAddr) -> Result<()> {
    let mut local_agent = if !args.bridge {
        let uid = gen_huid();
        let agent = make_agent_ctrl(uid).await?;
        Some(agent)
    } else {
        None
    };

    let shared = Arc::new(Shared {
        local: local_agent.as_ref().map(|x|x.clone_ctrl()),
        secret: args.secret.clone(),
        data: Default::default(),
        disable_bridge_ch: args.disable_bridge_ch,
    });

    let app = Router::new()
    .route(PUB_WS, get(handle_ws_pub))
    .route(SUB_WS, get(handle_ws_sub))
    .route(PUB_SESSIONS, get(get_pub_sessions))
    .route("/echo/ws", get(handle_ws_echo));

    let router = app
    .layer(Extension(shared.clone()))
    .layer(
        TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::default().include_headers(true)),
    );

    let tls_cfg = try_load_https_cert(
        args.https_key.as_deref(), 
        args.https_cert.as_deref(),
    ).await
    .with_context(|| "open https key/cert file failed")?;

    {
        let shared = shared.clone();
        let key_file = args.https_key.clone();
        let cert_file = args.https_cert.clone();
        spawn_with_name("quic-signal-server", async move {
            let r = run_quic_signal(shared, addr, key_file.as_deref(), cert_file.as_deref()).await;
            tracing::debug!("quic signal server finished [{r:?}]");
            r
        });
    }

    let is_https = tls_cfg.is_some();

    let listener = TcpListener::bind(addr).await
    .with_context(||format!("fail to bind [{}]", addr))?
    .into_std()
    .with_context(||"tcp listener into std failed")?;

    let server_handle = Handle::new();
    
    let task_name = format!("server");
    let task = spawn_with_name(task_name, async move {
        match tls_cfg {
            Some(tls_cfg) => {
                axum_server::from_tcp_rustls(listener, tls_cfg)
                // axum_server::bind_rustls(addr, tls_cfg)
                .handle(server_handle)
                .serve(router.into_make_service_with_connect_info::<SocketAddr>())
                .await
            },
            None => {
                axum_server::from_tcp(listener)
                // axum_server::bind(addr)
                .handle(server_handle)
                .serve(router.into_make_service_with_connect_info::<SocketAddr>())
                .await
            },
        }
        
    });
    
    if is_https {
        tracing::info!("agent listening on https://{}", addr);
    } else {
        tracing::info!("agent listening on http://{}", addr);
    }
    

    let _r = task.await;

    // axum::Server::bind(&addr)
    // .serve(router.into_make_service_with_connect_info::<SocketAddr>())
    // .await?;

    if let Some(agent) = local_agent.as_mut() {
        agent.shutdown().await;
        let _r = agent.wait_for_completed().await?;
    }
    
    Ok(())
}



async fn handle_ws_echo(
    Extension(_shared): Extension<Arc<Shared>>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {

    let uid = gen_huid();
    tracing::debug!("[{}] echo connected from {addr}", uid);
    

    ws.on_upgrade(move |socket| async move {
        
        let r = ws_echo_loop(socket).await;
        tracing::debug!("[{}] echo conn finished with [{:?}]", uid, r);
    })
}

async fn ws_echo_loop(mut socket: WebSocket) -> Result<()> {
    use axum::extract::ws::Message;
    while let Some(r) = socket.recv().await {
        let msg = r.with_context(||"recv fail")?;
        match msg {
            Message::Text(s) => {
                socket.send(Message::Text(s)).await
                .with_context(||"send fail")?;
            },
            _ => {}
        }
    }
    Ok(())
}



async fn try_load_https_cert(key_file: Option<&str>, cert_file: Option<&str>) -> Result<Option<RustlsConfig>> {
    if let (Some(key_file), Some(cert_file)) = (key_file, cert_file) {
        let cfg = RustlsConfig::from_pem_file(
            cert_file,
            key_file,
        )
        .await?;
        return Ok(Some(cfg))
    }

    if key_file.is_some() || cert_file.is_some() {
        if key_file.is_none() {
            bail!("no key file")
        }
        
        if cert_file.is_none() {
            bail!("no cert file")
        }
    }

    Ok(None)
}

async fn run_quic_signal(shared: Arc<Shared>, addr: SocketAddr, key_file: Option<&str>, cert_file: Option<&str>) -> Result<()> {
    let server_crypto = match try_load_quic_cert(key_file, cert_file).await? {
        Some(v) => v,
        None => {
            tracing::info!("quic signaling disabled (need --https-key and --https-cert)");
            return Ok(());
        }
    };

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
    Arc::get_mut(&mut server_config.transport)
    .with_context(|| "get quic transport config failed")?
    .max_concurrent_uni_streams(0_u8.into())
    .keep_alive_interval(Some(Duration::from_secs(3)))
    .max_idle_timeout(Some(quinn::VarInt::from_u32(30_000).into()));

    let endpoint = quinn::Endpoint::server(server_config, addr)
    .with_context(|| format!("failed to bind quic://{}", addr))?;

    tracing::info!("agent listening on quic://{}", addr);

    while let Some(conn) = endpoint.accept().await {
        let shared = shared.clone();
        spawn_with_name("quic-signal-conn", async move {
            let r = handle_quic_signal_conn(shared, conn).await;
            tracing::debug!("quic signal conn finished [{r:?}]");
            r
        });
    }

    Ok(())
}

async fn handle_quic_signal_conn(shared: Arc<Shared>, conn: quinn::Connecting) -> Result<()> {
    let conn = conn.await.with_context(|| "setup quic connection failed")?;
    let addr = conn.remote_address();
    let uid = gen_huid();

    let (mut tx, mut rx) = conn.accept_bi().await.with_context(|| "accept first stream failed")?;
    let req = quic_signal::read_request(&mut rx).await.with_context(|| "read quic request failed")?;

    match req {
        SignalRequest::Pub(mut params) => {
            if let Err(e) = token_verify(shared.secret.as_deref(), &params.token) {
                let rsp = SignalResponse::Pub(PubResponse::err(format!("unauthorized: {e}")));
                write_response_and_finish(&mut tx, &rsp).await?;
                return Ok(());
            }

            if params.agent.is_none() {
                params.agent = Some(uid.to_string());
            }
            let agent_name = params.agent.clone();

            let rsp = SignalResponse::Pub(PubResponse::ok(agent_name));
            quic_signal::write_response(&mut tx, &rsp).await?;

            let stream = quic_signal::QuicSignalStream::new(tx, rx, None);
            handle_pub_conn_quic(shared, uid, stream, addr, params).await
        }
        SignalRequest::Sub(params) => {
            if let Err(e) = token_verify(shared.secret.as_deref(), &params.token) {
                let rsp = SignalResponse::Sub(StatusResponse::err(format!("unauthorized: {e}")));
                write_response_and_finish(&mut tx, &rsp).await?;
                return Ok(());
            }

            let agent_name = params.agent.as_deref().unwrap_or("local");

            if agent_name == "local" {
                if let Some(local) = shared.local.as_ref() {
                    let rsp = SignalResponse::Sub(StatusResponse::ok());
                    quic_signal::write_response(&mut tx, &rsp).await?;
                    let stream = quic_signal::QuicSignalStream::new(tx, rx, None);
                    return run_sub_agent_stream(local.clone(), uid, stream.split()).await;
                }
            }

            let ctrl = {
                shared
                    .data
                    .lock()
                    .agent_clients
                    .get(agent_name)
                    .map(|x| x.ctrl_invoker.clone())
            };
            if let Some(ctrl) = ctrl {
                let rsp = SignalResponse::Sub(StatusResponse::ok());
                quic_signal::write_response(&mut tx, &rsp).await?;
                let stream = quic_signal::QuicSignalStream::new(tx, rx, None);
                let stream = stream.split();

                return match ctrl {
                    CtrlClientAgent::Ws(ctrl) => run_sub_agent_stream(ctrl, uid, stream).await,
                    CtrlClientAgent::Quic(ctrl) => run_sub_agent_stream(ctrl, uid, stream).await,
                };
            }

            let rsp = SignalResponse::Sub(StatusResponse::err(format!("not found agent [{}]", agent_name)));
            write_response_and_finish(&mut tx, &rsp).await?;
            Ok(())
        }
        SignalRequest::Sessions => {
            let rsp = SignalResponse::Sessions(SessionsResponse::ok(collect_pub_sessions(&shared)));
            write_response_and_finish(&mut tx, &rsp).await?;
            Ok(())
        }
    }
}

async fn write_response_and_finish(tx: &mut quinn::SendStream, rsp: &SignalResponse) -> Result<()> {
    quic_signal::write_response(tx, rsp).await?;
    tx.finish().await.with_context(|| "finish quic response failed")?;
    Ok(())
}

async fn try_load_quic_cert(key_file: Option<&str>, cert_file: Option<&str>) -> Result<Option<rustls::ServerConfig>> {
    if let (Some(key_path), Some(cert_path)) = (key_file, cert_file) {
        let key_path: &Path = key_path.as_ref();
        let cert_path: &Path = cert_path.as_ref();

        let key = tokio::fs::read(key_path).await.context("failed to read private key")?;
        let key = if key_path.extension().map_or(false, |x| x == "der") {
            rustls::PrivateKey(key)
        } else {
            let pkcs8 = rustls_pemfile::pkcs8_private_keys(&mut &*key)
                .context("malformed PKCS #8 private key")?;
            match pkcs8.into_iter().next() {
                Some(x) => rustls::PrivateKey(x),
                None => {
                    let rsa = rustls_pemfile::rsa_private_keys(&mut &*key)
                        .context("malformed PKCS #1 private key")?;
                    match rsa.into_iter().next() {
                        Some(x) => rustls::PrivateKey(x),
                        None => {
                            anyhow::bail!("no private keys found");
                        }
                    }
                }
            }
        };

        let cert_chain = tokio::fs::read(cert_path).await.context("failed to read certificate chain")?;
        let cert_chain = if cert_path.extension().map_or(false, |x| x == "der") {
            vec![rustls::Certificate(cert_chain)]
        } else {
            rustls_pemfile::certs(&mut &*cert_chain)
                .context("invalid PEM-encoded certificate")?
                .into_iter()
                .map(rustls::Certificate)
                .collect()
        };

        let server_crypto = rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)?;

        return Ok(Some(server_crypto));
    }

    if key_file.is_some() || cert_file.is_some() {
        if key_file.is_none() {
            bail!("no key file");
        }
        if cert_file.is_none() {
            bail!("no cert file");
        }
    }

    Ok(None)
}

async fn handle_ws_pub(
    Extension(shared): Extension<Arc<Shared>>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(mut params): Query<PubParams>,
) -> impl IntoResponse {

    if let Err(_e) = token_verify(shared.secret.as_deref(), &params.token) {
        return StatusCode::UNAUTHORIZED.into_response()
    }

    let uid = gen_huid();
    tracing::debug!("[{}] pub connected from {addr}, {params:?}", uid);
    
    let agent_name = match params.agent.as_deref() {
        Some(v) => v.to_string(),
        None => {
            let agent_name = uid.to_string();
            params.agent = Some(agent_name.clone());
            agent_name
        },
    };

    let mut rsp = ws.on_upgrade(move |socket| async move {
        
        let r = handle_pub_conn(shared, uid, socket, addr, params).await;
        tracing::debug!("[{}] pub conn finished with [{:?}]", uid, r);
    });
    
    let value = match agent_name.parse() {
        Ok(v) => v,
        Err(_e) => return StatusCode::BAD_REQUEST.into_response(),
    };
    rsp.headers_mut().append("agent_name", value);
    rsp
}




async fn handle_pub_conn(shared: Arc<Shared>, uid: HUId, socket: WebSocket, addr: SocketAddr, params: PubParams) -> Result<()> {

    let mut session = make_stream_session(WsStreamAxum::new(socket.split()).split(), shared.disable_bridge_ch).await?;

    // let mut session = make_stream_switch(uid, WsStreamAxum::new(socket)).await?;
    // let switch = session.clone_invoker();

    // let ctrl_ch_id = ChId(0);
    // let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    // let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;
    // let ctrl_pair = ChPair { tx: ctrl_tx, rx: ctrl_rx };

    // let mut ctrl_client = make_ctrl_client(uid, ctrl_pair, switch).await?;
    
    let key = params.agent.unwrap_or_else(||uid.to_string());
    let old = {
        let ctrl_invoker = session.ctrl_client().clone_invoker();
        shared.data.lock().agent_clients.insert(key.clone(), AgentSession { 
            uid,
            addr, 
            ctrl_invoker: CtrlClientAgent::Ws(ctrl_invoker),
            expire_at: params.expire_in.map(
                |x|Local::now().timestamp_millis() as u64  + x 
            ) .unwrap_or(u64::MAX/2),
            ver: params.ver.clone(),
        })
    };
    
    if let Some(old) = old {
        tracing::info!("kick agent session [{key}]-[{}], addr [{}]", old.uid, old.addr);
        kick_agent_session(
            &old.ctrl_invoker,
            KickDownArgs {
                code: -1,
                reason: "replace by other session".into(),
                ..Default::default()
            },
        )
        .await;
    }
    
    tracing::info!("add agent session [{key}]-[{}], addr [{}]", uid, addr);

    let _r = session.wait_for_completed().await; 
    
    let _r = {
        let mut data = shared.data.lock();
        let r = data.agent_clients.remove_entry(&key);
        match r {
            Some((key, value)) => {
                if value.uid == uid {
                    Some(value)
                } else {
                    // put back
                    data.agent_clients.insert(key, value);
                    None
                }
            },
            None => None,
        }
    };

    if _r.is_some() {
        tracing::info!("remove agent session [{key}]-[{uid}], addr [{addr}]");
    }
    

    // let _r = ctrl_client.wait_for_completed().await?;

    Ok(())
}

async fn handle_pub_conn_quic(
    shared: Arc<Shared>,
    uid: HUId,
    stream: quic_signal::QuicSignalStream,
    addr: SocketAddr,
    params: PubParams,
) -> Result<()> {
    let mut session = make_stream_session(stream.split(), shared.disable_bridge_ch).await?;

    let key = params.agent.clone().unwrap_or_else(|| uid.to_string());
    let old = {
        let ctrl_invoker = session.ctrl_client().clone_invoker();
        shared.data.lock().agent_clients.insert(
            key.clone(),
            AgentSession {
                uid,
                addr,
                ctrl_invoker: CtrlClientAgent::Quic(ctrl_invoker),
                expire_at: params
                    .expire_in
                    .map(|x| Local::now().timestamp_millis() as u64 + x)
                    .unwrap_or(u64::MAX / 2),
                ver: params.ver.clone(),
            },
        )
    };

    if let Some(old) = old {
        tracing::info!("kick agent session [{key}]-[{}], addr [{}]", old.uid, old.addr);
        kick_agent_session(
            &old.ctrl_invoker,
            KickDownArgs {
                code: -1,
                reason: "replace by other session".into(),
                ..Default::default()
            },
        )
        .await;
    }

    tracing::info!("add agent session [{key}]-[{}], addr [{}]", uid, addr);

    let _r = session.wait_for_completed().await;

    let _r = {
        let mut data = shared.data.lock();
        let r = data.agent_clients.remove_entry(&key);
        match r {
            Some((key, value)) => {
                if value.uid == uid {
                    Some(value)
                } else {
                    data.agent_clients.insert(key, value);
                    None
                }
            }
            None => None,
        }
    };

    if _r.is_some() {
        tracing::info!("remove agent session [{key}]-[{uid}], addr [{addr}]");
    }

    Ok(())
}

fn collect_pub_sessions(shared: &Arc<Shared>) -> Vec<AgentInfo> {
    shared
        .data
        .lock()
        .agent_clients
        .iter()
        .map(|x| AgentInfo {
            name: x.0.clone(),
            addr: x.1.addr.to_string(),
            expire_at: x.1.expire_at,
            ver: x.1.ver.clone(),
        })
        .collect()
}


async fn get_pub_sessions (
    Extension(shared): Extension<Arc<Shared>>,
) -> impl IntoResponse {

    // if let Err(_e) = token_verify(shared.secret.as_deref(), &params.token) {
    //     return StatusCode::UNAUTHORIZED.into_response()
    // }

    let sessions = collect_pub_sessions(&shared);
    Json(sessions)
}


async fn handle_ws_sub(
    Extension(shared): Extension<Arc<Shared>>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(params): Query<SubParams>,
) -> impl IntoResponse {

    if let Err(_e) = token_verify(shared.secret.as_deref(), &params.token) {
        return StatusCode::UNAUTHORIZED.into_response()
    }

    tracing::debug!("connected from {addr}, {params:?}", );

    let agent_name = params.agent.as_deref().unwrap_or("local");
    if agent_name == "local" {
        if let Some(local) = shared.local.as_ref() {
            let uid = gen_huid();
            let ctrl = local.clone();
            tracing::info!("[{uid}] [{addr}] sub local ");
    
            return ws.on_upgrade(move |socket| async move {
                // let r = handle_local_sub_conn(shared, uid, socket).await;
                let r = run_sub_agent(ctrl, uid, socket).await;
                tracing::info!("[{}] sub conn finished with [{:?}]", uid, r);
            })
        }
    }

    let r = {
        shared.data.lock().agent_clients.get(agent_name).map(|x|x.ctrl_invoker.clone()) 
    };
    if let Some(ctrl) = r {
        let uid = gen_huid();
        tracing::info!("[{uid}] [{addr}] sub agent [{agent_name}]");

        return ws.on_upgrade(move |socket| async move {
            let r = match ctrl {
                CtrlClientAgent::Ws(ctrl) => run_sub_agent(ctrl, uid, socket).await,
                CtrlClientAgent::Quic(ctrl) => run_sub_agent(ctrl, uid, socket).await,
            };
            tracing::info!("[{}] sub conn finished with [{:?}]", uid, r);
        })
    }

    (
        StatusCode::NOT_FOUND,
        format!("Not found agent {:?}", params.agent),
    ).into_response()
}



// async fn local_sub_ws_handler(
//     Extension(shared): Extension<Arc<Shared>>,
//     ws: WebSocketUpgrade,
//     user_agent: Option<TypedHeader<headers::UserAgent>>,
//     ConnectInfo(addr): ConnectInfo<SocketAddr>,
// ) -> impl IntoResponse {
//     let user_agent = if let Some(TypedHeader(user_agent)) = user_agent {
//         user_agent.to_string()
//     } else {
//         String::from("Unknown browser")
//     };
//     tracing::debug!("connected from {addr}, with agent [{user_agent}].");

//     ws.on_upgrade(move |socket| async move {
//         let uid = gen_huid();
//         let r = handle_local_sub_conn(shared, uid, socket).await;
//         tracing::debug!("conn finished with [{:?}]", r);
//     })
// }

// async fn handle_local_sub_conn(_shared: Arc<Shared>, uid: HUId, socket: WebSocket) -> Result<()>{
    
//     let mut agent = make_agent_ctrl(uid)?;
//     let ctrl = agent.clone_ctrl();
//     let ctrl = ctrl.clone();

//     let run_result = run_sub_agent(ctrl, uid, socket).await;
//     agent.shutdown().await;
//     let _r = agent.wait_for_completed().await?;

//     run_result
// }



async fn run_sub_agent<H1: CtrlHandler>(ctrl: CtrlInvoker<H1>, uid: HUId, socket: WebSocket) -> Result<()> {
    run_sub_agent_stream(ctrl, uid, WsStreamAxum::new(socket.split()).split()).await
}

async fn run_sub_agent_stream<H1, S1, S2>(ctrl: CtrlInvoker<H1>, uid: HUId, stream: (S1, S2)) -> Result<()>
where
    H1: CtrlHandler,
    S1: PacketSink,
    S2: PacketSource,
{
    let mut session = make_switch_pair(uid, stream).await?;
    let switch = session.clone_invoker();

    let ctrl_ch_id = ChId(0);
    let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;

    spawn_ctrl_service(uid, ctrl, switch, ChPair { tx: ctrl_tx, rx: ctrl_rx });

    let _r = session.wait_for_completed().await;
    Ok(())
}

async fn kick_agent_session(ctrl: &CtrlClientAgent, args: KickDownArgs) {
    match ctrl {
        CtrlClientAgent::Ws(ctrl) => {
            let _r = ctrl.kick_down(args.clone()).await;
        }
        CtrlClientAgent::Quic(ctrl) => {
            let _r = ctrl.kick_down(args).await;
        }
    }
}




struct Shared {
    secret: Option<String>,
    local: Option<AgentCtrlInvoker>,
    data: Mutex<SharedData>,
    disable_bridge_ch: bool,
}

#[derive(Default)]
struct SharedData {
    agent_clients: HashMap<String,  AgentSession>, 
}

struct AgentSession {
    uid: HUId,
    addr: SocketAddr,
    ctrl_invoker: CtrlClientAgent,
    expire_at: u64,
    ver: Option<String>,
}


// type WsCtrlClientAgent =  CtrlInvoker<ctrl_client::Entity<StreamSwitchEntity<WsStreamAxum<WebSocket>>>>;
type WsCtrlClientAgent = CtrlInvoker<ctrl_client::Entity<SwitchPairEntity<WsSource<SplitStream<WebSocket>>>>>;
type QuicCtrlClientAgent = CtrlInvoker<ctrl_client::Entity<SwitchPairEntity<quic_signal::QuicSource>>>;

#[derive(Clone)]
enum CtrlClientAgent {
    Ws(WsCtrlClientAgent),
    Quic(QuicCtrlClientAgent),
}





#[derive(Parser, Debug)]
#[clap(name = "agent_listen", author, about, version)]
pub struct CmdArgs {
    #[clap(
        long = "addr",
        long_help = "listen address",
        default_value = "0.0.0.0:19888",
    )]
    addr: String,

    #[clap(
        short = 'b',
        long = "bridge",
        long_help = "run as bridge only",
    )]
    bridge: bool,

    #[clap(
        long = "https-cert",
        long_help = "https cert file",
    )]
    https_cert: Option<String>,

    #[clap(
        long = "https-key",
        long_help = "http key file",
    )]
    https_key: Option<String>,

    #[clap(
        long = "secret",
        long_help = "authentication secret",
    )]
    secret: Option<String>,

    #[clap(
        long = "disable-bridge-ch",
        long_help = "disable bridge channel",
    )]
    disable_bridge_ch: bool,
}
