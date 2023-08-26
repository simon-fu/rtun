
/*
    websocket refer: https://github.com/tokio-rs/axum/blob/axum-v0.6.20/examples/websockets/src/main.rs
*/

use anyhow::{Result, Context};
use clap::Parser;
use axum::{Extension, extract::Query, http::StatusCode, Json};
use parking_lot::Mutex;
use rtun::{huid::{gen_huid::gen_huid, HUId}, channel::{ChId, ChPair}, switch::{ctrl_service::spawn_ctrl_service, agent::ctrl::{make_agent_ctrl, AgentCtrlInvoker}, switch_stream::{make_stream_switch, Entity as StreamSwitchEntity}, ctrl_client::{make_ctrl_client, self}, invoker_ctrl::{CtrlInvoker, CtrlHandler}}, ws::server::WsStreamAxum};

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

use crate::rest_proto::{PUB_WS, SUB_WS, PUB_SESSIONS, PubParams, SubParams, AgentInfo};


pub async fn run(args: CmdArgs) -> Result<()> {
    let addr: SocketAddr = args.addr.parse()
    .with_context(||"invalid address")?;

    let mut local_agent = if !args.bridge {
        let uid = gen_huid();
        let agent = make_agent_ctrl(uid)?;
        Some(agent)
    } else {
        None
    };
    

    let shared = Arc::new(Shared {
        local: local_agent.as_ref().map(|x|x.clone_ctrl()),
        data: Default::default(),
    });

    let app = Router::new()
    .route(PUB_WS, get(handle_ws_pub))
    .route(SUB_WS, get(handle_ws_sub))
    .route(PUB_SESSIONS, get(get_pub_sessions));

    // if !args.bridge {
    //     app = app.route("/agents/local/sub", get(local_sub_ws_handler));
    // }

    let app = app
        .layer(Extension(shared))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );

    tracing::debug!("agent listening on http://{}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await?;

    if let Some(agent) = local_agent.as_mut() {
        agent.shutdown().await;
        let _r = agent.wait_for_completed().await?;
    }
    
    Ok(())
}

async fn handle_ws_pub(
    Extension(shared): Extension<Arc<Shared>>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(mut params): Query<PubParams>,
) -> impl IntoResponse {

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
        tracing::debug!("pub conn finished with [{:?}]", r);
    });
    
    let value = match agent_name.parse() {
        Ok(v) => v,
        Err(_e) => return StatusCode::BAD_REQUEST.into_response(),
    };
    rsp.headers_mut().append("agent_name", value);
    rsp
}




async fn handle_pub_conn(shared: Arc<Shared>, uid: HUId, socket: WebSocket, addr: SocketAddr, params: PubParams) -> Result<()>{
    
    let mut session = make_stream_switch(uid, WsStreamAxum::new(socket)).await?;
    let switch = session.clone_invoker();

    let ctrl_ch_id = ChId(0);
    let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;
    let ctrl_pair = ChPair { tx: ctrl_tx, rx: ctrl_rx };

    let mut ctrl_client = make_ctrl_client(uid, ctrl_pair, switch)?;
    
    let key = params.agent.unwrap_or_else(||uid.to_string());
    {
        let ctrl_invoker = ctrl_client.clone_invoker();
        shared.data.lock().agent_clients.insert(key.clone(), AgentSession { 
            addr, 
            ctrl_invoker,
        });
    }
    tracing::debug!("add agent session [{key}]-[{}], addr [{}]", uid, addr);

    let _r = session.wait_for_completed().await; 
    
    let _r = {
        shared.data.lock().agent_clients.remove(&key)
    };
    tracing::debug!("remove agent session [{}], addr [{}]", uid, addr);

    let _r = ctrl_client.wait_for_completed().await?;

    Ok(())
}


async fn get_pub_sessions (
    Extension(shared): Extension<Arc<Shared>>,
) -> impl IntoResponse {
    let sessions: Vec<AgentInfo> = {
        shared.data.lock().agent_clients.iter().map(|x|{
            AgentInfo {
                name: x.0.clone(),
                addr: x.1.addr.to_string(),
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

    tracing::debug!("connected from {addr}, {params:?}", );

    let agent_name = params.agent.as_deref().unwrap_or("local");
    if agent_name == "local" {
        if let Some(local) = shared.local.as_ref() {
            let uid = gen_huid();
            let ctrl = local.clone();
            tracing::debug!("sub local [{uid}]");
    
            return ws.on_upgrade(move |socket| async move {
                // let r = handle_local_sub_conn(shared, uid, socket).await;
                let r = run_sub_agent(ctrl, uid, socket).await;
                tracing::debug!("conn finished with [{:?}]", r);
            })
        }
    }

    let r = {
        shared.data.lock().agent_clients.get(agent_name).map(|x|x.ctrl_invoker.clone()) 
    };
    if let Some(ctrl) = r {
        let uid = gen_huid();
        tracing::debug!("sub agent [{}]", agent_name);

        return ws.on_upgrade(move |socket| async move {
            let r = run_sub_agent(ctrl, uid, socket).await;
            tracing::debug!("conn finished with [{:?}]", r);
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
    let mut session = make_stream_switch(uid, WsStreamAxum::new(socket)).await?;

    let switch = session.clone_invoker();

    let ctrl_ch_id = ChId(0);
    let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;

    spawn_ctrl_service(uid, ctrl, switch, ChPair { tx: ctrl_tx, rx: ctrl_rx });

    let _r = session.wait_for_completed().await;


    Ok(())
}




struct Shared {
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
}


type CtrlClientAgent =  CtrlInvoker<ctrl_client::Entity<StreamSwitchEntity<WsStreamAxum<WebSocket>>>>;






#[derive(Parser, Debug)]
#[clap(name = "agent_listen", author, about, version)]
pub struct CmdArgs {
    #[clap(
        long = "addr",
        long_help = "listen address",
        default_value = "0.0.0.0:9888",
    )]
    addr: String,

    #[clap(
        short = 'b',
        long = "bridge",
        long_help = "run as bridge only",
    )]
    bridge: bool,
}

