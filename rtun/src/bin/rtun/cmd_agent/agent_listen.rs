
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
use rtun::{huid::{gen_huid::gen_huid, HUId}, channel::{ChId, ChPair}, switch::{ctrl_service::spawn_ctrl_service, agent::ctrl::{make_agent_ctrl, AgentCtrlInvoker}, ctrl_client, invoker_ctrl::{CtrlInvoker, CtrlHandler}, session_stream::make_stream_session, switch_pair::{SwitchPairEntity, make_switch_pair}}, ws::server::{WsStreamAxum, WsSource}, async_rt::spawn_with_name};
use tokio::net::TcpListener;

use std::{sync::Arc, collections::HashMap};
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

use crate::{rest_proto::{PUB_WS, SUB_WS, PUB_SESSIONS, PubParams, SubParams, AgentInfo}, secret::token_verify};


pub async fn run(args: CmdArgs) -> Result<()> {
    let addr: SocketAddr = args.addr.parse()
    .with_context(||"invalid address")?;

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
    });

    let app = Router::new()
    .route(PUB_WS, get(handle_ws_pub))
    .route(SUB_WS, get(handle_ws_sub))
    .route(PUB_SESSIONS, get(get_pub_sessions))
    .route("/echo/ws", get(handle_ws_echo));

    let router = app
    .layer(Extension(shared))
    .layer(
        TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::default().include_headers(true)),
    );

    let tls_cfg = try_load_https_cert(
        args.https_key.as_deref(), 
        args.https_cert.as_deref(),
    ).await
    .with_context(|| "open https key/cert file failed")?;

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

    let mut session = make_stream_session(WsStreamAxum::new(socket.split()).split()).await?;

    // let mut session = make_stream_switch(uid, WsStreamAxum::new(socket)).await?;
    // let switch = session.clone_invoker();

    // let ctrl_ch_id = ChId(0);
    // let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    // let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;
    // let ctrl_pair = ChPair { tx: ctrl_tx, rx: ctrl_rx };

    // let mut ctrl_client = make_ctrl_client(uid, ctrl_pair, switch).await?;
    
    let key = params.agent.unwrap_or_else(||uid.to_string());
    {
        let ctrl_invoker = session.ctrl_client().clone_invoker();
        shared.data.lock().agent_clients.insert(key.clone(), AgentSession { 
            addr, 
            ctrl_invoker,
            expire_at: params.expire_in.map(
                |x|Local::now().timestamp_millis() as u64  + x 
            ) .unwrap_or(u64::MAX/2),
        });
    }
    tracing::info!("add agent session [{key}]-[{}], addr [{}]", uid, addr);

    let _r = session.wait_for_completed().await; 
    
    let _r = {
        shared.data.lock().agent_clients.remove(&key)
    };
    tracing::info!("remove agent session [{}], addr [{}]", uid, addr);

    // let _r = ctrl_client.wait_for_completed().await?;

    Ok(())
}


async fn get_pub_sessions (
    Extension(shared): Extension<Arc<Shared>>,
) -> impl IntoResponse {

    // if let Err(_e) = token_verify(shared.secret.as_deref(), &params.token) {
    //     return StatusCode::UNAUTHORIZED.into_response()
    // }

    let sessions: Vec<AgentInfo> = {
        shared.data.lock().agent_clients.iter().map(|x|{
            AgentInfo {
                name: x.0.clone(),
                addr: x.1.addr.to_string(),
                expire_at: x.1.expire_at,
            }
        }).collect()
    };
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
            let r = run_sub_agent(ctrl, uid, socket).await;
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
    // let mut session = make_ws_server_switch(uid, socket).await?;

    // let mut session = make_stream_switch(uid, WsStreamAxum::new(socket.split())).await?;
    let mut session = make_switch_pair(uid, WsStreamAxum::new(socket.split()).split()).await?;
    let switch = session.clone_invoker();

    // let mut session = make_stream_session( WsStreamAxum::new(socket.split()).split() ).await?;
    // let switch = session.switch().clone_invoker();


    let ctrl_ch_id = ChId(0);
    let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;

    spawn_ctrl_service(uid, ctrl, switch, ChPair { tx: ctrl_tx, rx: ctrl_rx });

    let _r = session.wait_for_completed().await;


    Ok(())
}




struct Shared {
    secret: Option<String>,
    local: Option<AgentCtrlInvoker>,
    data: Mutex<SharedData>,
}

#[derive(Default)]
struct SharedData {
    agent_clients: HashMap<String,  AgentSession>, 
}

struct AgentSession {
    addr: SocketAddr,
    ctrl_invoker: CtrlClientAgent,
    expire_at: u64,
}


// type CtrlClientAgent =  CtrlInvoker<ctrl_client::Entity<StreamSwitchEntity<WsStreamAxum<WebSocket>>>>;
type CtrlClientAgent =  CtrlInvoker<ctrl_client::Entity<SwitchPairEntity< WsSource<SplitStream<WebSocket>> >>>;





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
}

