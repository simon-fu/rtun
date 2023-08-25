/*
    websocket refer: https://github.com/tokio-rs/axum/blob/axum-v0.6.20/examples/websockets/src/main.rs
*/

use anyhow::Result;
use axum::{Extension, extract::Path, http::StatusCode};
use parking_lot::Mutex;
use rtun::{huid::{gen_huid::gen_huid, HUId}, channel::{ChId, ChPair}, switch::{switch_ws_server::make_ws_server_switch, ctrl_service::spawn_ctrl_service, agent::ctrl::make_agent_ctrl, switch_stream::{make_stream_switch, Entity as StreamSwitchEntity}, ctrl_client::{make_ctrl_client, self}, invoker_ctrl::{CtrlInvoker, CtrlHandler}}, ws::server::WsStreamAxum};
use std::{sync::Arc, collections::HashMap};
use std::net::SocketAddr;
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use axum::{
    extract::{
        ws::{WebSocket, WebSocketUpgrade},
        TypedHeader,
        connect_info::ConnectInfo,
    },
    response::IntoResponse,
    routing::get,
    Router, headers,
};

// use crate::local_bridge::{make_local_bridge, LocalBridge};


pub async fn run() -> Result<()> {

    // let local_bridge = make_local_bridge(gen_huid()).await?;

    let shared = Arc::new(Shared {
        data: Default::default(),
        // local_bridge,
    });

    // build our application with some routes
    let app = Router::new()
        // .fallback_service(ServeDir::new(assets_dir).append_index_html_on_directories(true))
        .route("/agents/local/sub", get(local_sub_ws_handler))
        .route("/agents/pub", get(agent_pub_ws_handler))
        .route("/agents/sessions/:session", get(agent_sub_ws_handler))
        .layer(Extension(shared))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );

    // run it with hyper
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::debug!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await?;

    Ok(())
}

async fn agent_pub_ws_handler(
    Extension(shared): Extension<Arc<Shared>>,
    ws: WebSocketUpgrade,
    user_agent: Option<TypedHeader<headers::UserAgent>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let user_agent = if let Some(TypedHeader(user_agent)) = user_agent {
        user_agent.to_string()
    } else {
        String::from("Unknown browser")
    };
    tracing::debug!("connected from {addr}, with agent [{user_agent}].");

    ws.on_upgrade(move |socket| handle_agent_pub(shared, socket, addr))
}

async fn handle_agent_pub(shared: Arc<Shared>, socket: WebSocket, addr: SocketAddr) {
    let uid = gen_huid();
    let r = handle_agent_pub_conn(shared, uid, socket, addr).await;
    tracing::debug!("conn finished with [{:?}]", r);
}

async fn handle_agent_pub_conn(shared: Arc<Shared>, uid: HUId, socket: WebSocket, addr: SocketAddr) -> Result<()>{
    
    let mut session = make_stream_switch(uid, WsStreamAxum::new(socket)).await?;
    let switch = session.clone_invoker();

    let ctrl_ch_id = ChId(0);
    let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;
    let ctrl_pair = ChPair { tx: ctrl_tx, rx: ctrl_rx };

    let mut ctrl_client = make_ctrl_client(uid, ctrl_pair, switch)?;
    
    let key = uid.to_string();
    {
        let ctrl_invoker = ctrl_client.clone_invoker();
        shared.data.lock().agent_clients.insert(key.clone(), ctrl_invoker);
    }
    tracing::debug!("add agent session [{}], addr [{}]", uid, addr);

    let _r = session.wait_for_completed().await; 
    
    let _r = {
        shared.data.lock().agent_clients.remove(&key)
    };
    tracing::debug!("remove agent session [{}], addr [{}]", uid, addr);

    let _r = ctrl_client.wait_for_completed().await?;

    Ok(())
}



async fn agent_sub_ws_handler(
    Extension(shared): Extension<Arc<Shared>>,
    ws: WebSocketUpgrade,
    user_agent: Option<TypedHeader<headers::UserAgent>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(agent_name): Path<String>,
) -> impl IntoResponse {
    let user_agent = if let Some(TypedHeader(user_agent)) = user_agent {
        user_agent.to_string()
    } else {
        String::from("Unknown browser")
    };
    tracing::debug!("connected from {addr}, with agent [{user_agent}], sub to [{}]", agent_name);
    
    let r = {
        shared.data.lock().agent_clients.get(&agent_name).map(|x|x.clone()) 
    };

    match r {
        Some(ctrl) => {
            tracing::debug!("sub agent [{}]", agent_name);
            ws.on_upgrade(move |socket| async move {
                let uid = gen_huid();
                let r = run_sub_agent(ctrl, uid, socket).await;
                tracing::debug!("conn finished with [{:?}]", r);
            })
        },
        None => {
            (
                StatusCode::NOT_FOUND,
                format!("Not found agent [{}]", agent_name),
            ).into_response()
        },
    }
}



async fn local_sub_ws_handler(
    Extension(shared): Extension<Arc<Shared>>,
    ws: WebSocketUpgrade,
    user_agent: Option<TypedHeader<headers::UserAgent>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let user_agent = if let Some(TypedHeader(user_agent)) = user_agent {
        user_agent.to_string()
    } else {
        String::from("Unknown browser")
    };
    tracing::debug!("connected from {addr}, with agent [{user_agent}].");

    ws.on_upgrade(move |socket| handle_local_sub(shared, socket))
}



async fn handle_local_sub(shared: Arc<Shared>, socket: WebSocket) {
    let uid = gen_huid();
    let r = handle_local_sub_conn(shared, uid, socket).await;
    tracing::debug!("conn finished with [{:?}]", r);
}

// async fn handle_conn(shared: Arc<Shared>, uid: HUId, mut socket: WebSocket) -> Result<()>{
    
//     // handle_handshake(shared, &mut socket).await?;

//     let invoker = shared.local_bridge.clone_invoker();
//     let chpair = invoker.alloc_channel().await?;

//     let ch_id = chpair.tx.ch_id();

//     let packet = ServerHi {
//         ch_id: ch_id.0,
//         ..Default::default()
//     }.write_to_bytes()?;

//     socket.send(WsMessage::Binary(packet.into())).await?;

//     let mut session = make_ws_server_session(uid, socket).await?;
//     let agent = session.clone_agent();
    
//     let chpair = agent.alloc_channel().await?;
//     assert_eq!(ch_id, chpair.tx.ch_id());
//     spawn_agent_ctrl(uid, agent, chpair);

//     session.wait_for_completed().await?;

//     Ok(())
// }



async fn handle_local_sub_conn(_shared: Arc<Shared>, uid: HUId, socket: WebSocket) -> Result<()>{
    
    // handle_handshake(shared, &mut socket).await?;


    // let packet = ServerHi {
    //     ch_id: ctrl_ch_id.0,
    //     ..Default::default()
    // }.write_to_bytes()?;

    // socket.send(WsMessage::Binary(packet.into())).await?;

    let mut agent = make_agent_ctrl(uid)?;
    let ctrl = agent.clone_ctrl();
    let ctrl = ctrl.clone();

    let run_result = run_sub_agent(ctrl, uid, socket).await;
    agent.shutdown().await;
    let _r = agent.wait_for_completed().await?;

    run_result
}

async fn run_sub_agent<H1: CtrlHandler>(ctrl: CtrlInvoker<H1>, uid: HUId, socket: WebSocket) -> Result<()> {
    let mut session = make_ws_server_switch(uid, socket).await?;
    let switch = session.clone_switch();

    let ctrl_ch_id = ChId(0);
    let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;

    spawn_ctrl_service(uid, ctrl, switch, ChPair { tx: ctrl_tx, rx: ctrl_rx });

    let _r = session.wait_for_completed().await;


    Ok(())
}

// async fn handle_conn(_shared: Arc<Shared>, uid: HUId, mut socket: WebSocket) -> Result<()>{
    
//     // handle_handshake(shared, &mut socket).await?;

//     let ch_id = ChId(0);

//     let packet = ServerHi {
//         ch_id: ch_id.0,
//         ..Default::default()
//     }.write_to_bytes()?;

//     socket.send(WsMessage::Binary(packet.into())).await?;

//     let mut session = make_ws_server_session(uid, socket).await?;
//     let agent = session.clone_agent();
    
//     let chpair = agent.alloc_channel().await?;
//     assert_eq!(ch_id, chpair.tx.ch_id());
//     spawn_agent_ctrl(uid, agent, chpair);

//     session.wait_for_completed().await?;

//     Ok(())
// }





// async fn handle_handshake(_shared: Arc<Shared>, socket: &mut WebSocket) -> Result<()> {
//     let req: HanshakeRequestPacket = recv_ws_packet(socket).await?;
//     match req.device_type() {
//         DeviceType::DEV_RESERVED => bail!("invalid device type"),
//         DeviceType::DEV_AGENT => bail!("unexpect device agent"),
//         DeviceType::DEV_CLIENT => { },
//     }

//     let rsp = HanshakeResponsePacket {
//         nonce: 0,
//         ..Default::default()
//     }
//     .write_to_bytes()
//     .with_context(||"encode handshake response failed")?;

//     socket.send(Message::Binary(rsp)).await
//     .with_context(||"send handshake response failed")?;

//     Ok(())
// }





struct Shared {
    // local_bridge: LocalBridge,
    data: Mutex<SharedData>,
}

#[derive(Default)]
struct SharedData {
    // agent_clients: HashMap<String, CtrlClient<StreamSwitchEntity<WsStreamAxum<WebSocket>>> >, 
    agent_clients: HashMap<String, AgentClient >, 
}


type AgentClient =  CtrlInvoker<ctrl_client::Entity<StreamSwitchEntity<WsStreamAxum<WebSocket>>>>;




