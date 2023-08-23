/*
    websocket refer: https://github.com/tokio-rs/axum/blob/axum-v0.6.20/examples/websockets/src/main.rs
*/

use anyhow::Result;
use axum::Extension;
use parking_lot::Mutex;
use protobuf::Message;
use rtun::{huid::{gen_huid::gen_huid, HUId}, proto::ServerHi, channel::ChId};
use std::sync::Arc;
use std::net::SocketAddr;
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use axum::{
    extract::{
        ws::{WebSocket, WebSocketUpgrade, Message as WsMessage},
        TypedHeader,
        connect_info::ConnectInfo,
    },
    response::IntoResponse,
    routing::get,
    Router, headers,
};

use crate::{ws_server_session::make_ws_server_session, agent_ch_ctrl::spawn_agent_ctrl, local_bridge::{make_local_bridge, LocalBridge}};


pub async fn run() -> Result<()> {

    let local_bridge = make_local_bridge(gen_huid()).await?;

    let shared = Arc::new(Shared {
        _data: Default::default(),
        local_bridge,
    });

    // build our application with some routes
    let app = Router::new()
        // .fallback_service(ServeDir::new(assets_dir).append_index_html_on_directories(true))
        .route("/agents/local/sub", get(ws_handler))
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


async fn ws_handler(
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

    ws.on_upgrade(move |socket| handle_socket(shared, socket))
}

struct Shared {
    local_bridge: LocalBridge,
    _data: Mutex<SharedData>,
}

#[derive(Default)]
struct SharedData {

}

async fn handle_socket(shared: Arc<Shared>, socket: WebSocket) {
    let uid = gen_huid();
    let r = handle_conn(shared, uid, socket).await;
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


async fn handle_conn(_shared: Arc<Shared>, uid: HUId, mut socket: WebSocket) -> Result<()>{
    
    // handle_handshake(shared, &mut socket).await?;

    let ch_id = ChId(0);

    let packet = ServerHi {
        ch_id: ch_id.0,
        ..Default::default()
    }.write_to_bytes()?;

    socket.send(WsMessage::Binary(packet.into())).await?;

    let mut session = make_ws_server_session(uid, socket).await?;
    let agent = session.clone_agent();
    
    let chpair = agent.alloc_channel().await?;
    assert_eq!(ch_id, chpair.tx.ch_id());
    spawn_agent_ctrl(uid, agent, chpair);

    session.wait_for_completed().await?;

    Ok(())
}





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








