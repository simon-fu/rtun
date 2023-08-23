// /*
//     websocket refer: https://github.com/tokio-rs/axum/blob/axum-v0.6.20/examples/websockets/src/main.rs
// */

// use anyhow::Result;
// use axum::Extension;
// use parking_lot::Mutex;
// use rtun::huid::{gen_huid::gen_huid, HUId};
// use std::sync::Arc;
// use std::net::SocketAddr;
// use tower_http::trace::{DefaultMakeSpan, TraceLayer};
// use axum::{
//     extract::{
//         ws::{WebSocket, WebSocketUpgrade},
//         TypedHeader,
//         connect_info::ConnectInfo,
//     },
//     response::IntoResponse,
//     routing::get,
//     Router, headers,
// };

// use crate::bridge_ws::make_ws_bridge;


// pub async fn run() -> Result<()> {

//     let shared = Arc::new(Shared {
//         _data: Default::default(),
//     });

//     // build our application with some routes
//     let app = Router::new()
//         // .fallback_service(ServeDir::new(assets_dir).append_index_html_on_directories(true))
//         .route("/ws", get(ws_handler))
//         .layer(Extension(shared))
//         .layer(
//             TraceLayer::new_for_http()
//                 .make_span_with(DefaultMakeSpan::default().include_headers(true)),
//         );

//     // run it with hyper
//     let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
//     tracing::debug!("listening on {}", addr);
//     axum::Server::bind(&addr)
//         .serve(app.into_make_service_with_connect_info::<SocketAddr>())
//         .await?;

//     Ok(())
// }


// async fn ws_handler(
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

//     ws.on_upgrade(move |socket| handle_socket(shared, socket))
// }

// struct Shared {
//     _data: Mutex<SharedData>,
// }

// #[derive(Default)]
// struct SharedData {

// }

// async fn handle_socket(shared: Arc<Shared>, socket: WebSocket) {
//     let uid = gen_huid();
//     let r = handle_conn(shared, uid, socket).await;
//     tracing::debug!("conn finished with [{:?}]", r);
// }

// async fn handle_conn(_shared: Arc<Shared>, uid: HUId, socket: WebSocket) -> Result<()>{
    
//     // handle_handshake(shared, &mut socket).await?;

//     let mut session = make_ws_bridge(uid, socket).await?;

//     session.wait_for_completed().await?;

//     Ok(())
// }


