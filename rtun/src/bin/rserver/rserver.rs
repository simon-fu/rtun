
// /*
//     websocket refer: https://github.com/tokio-rs/axum/blob/axum-v0.6.20/examples/websockets/src/main.rs
// */

// use anyhow::{Result, Context, bail};
// use axum::Extension;
// use parking_lot::Mutex;
// use rtun::proto::{HanshakeRequestPacket, HanshakeResponsePacket, DeviceType};
// use std::borrow::Cow;
// use std::ops::ControlFlow;
// use std::sync::Arc;
// use std::{net::SocketAddr, path::PathBuf};
// use tower_http::{
//     services::ServeDir,
//     trace::{DefaultMakeSpan, TraceLayer},
// };
// use axum::{
//     extract::{
//         ws::{Message, WebSocket, WebSocketUpgrade, CloseFrame},
//         TypedHeader,
//         connect_info::ConnectInfo,
//     },
//     response::IntoResponse,
//     routing::get,
//     Router, headers,
// };
// use futures::{sink::SinkExt, stream::StreamExt};
// use protobuf::Message as PBMessage;
// use crate::agent::make_agent;


// pub async fn run() -> Result<()> {
//     let assets_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");

//     let shared = Arc::new(Shared {
//         data: Default::default(),
//     });

//     // build our application with some routes
//     let app = Router::new()
//         .fallback_service(ServeDir::new(assets_dir).append_index_html_on_directories(true))
//         .route("/ws", get(ws_handler))
//         .layer(Extension(shared))
//         // logging so we can see whats going on
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






// /// The handler for the HTTP request (this gets called when the HTTP GET lands at the start
// /// of websocket negotiation). After this completes, the actual switching from HTTP to
// /// websocket protocol will occur.
// /// This is the last point where we can extract TCP/IP metadata such as IP address of the client
// /// as well as things from HTTP headers such as user-agent of the browser etc.
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
//     // finalize the upgrade process by returning upgrade callback.
//     // we can customize the callback by sending additional info such as address.
//     ws.on_upgrade(move |socket| handle_socket(shared, socket))
// }

// struct Shared {
//     data: Mutex<SharedData>,
// }

// #[derive(Default)]
// struct SharedData {

// }

// async fn handle_socket(shared: Arc<Shared>, socket: WebSocket) {
//     let r = handle_session(shared, socket).await;
// }

// async fn handle_session(shared: Arc<Shared>, mut socket: WebSocket) -> Result<()> {
    
//     let dev_type = recv_handshake(&mut socket).await?;
//     tracing::debug!("device type [{dev_type:?}]");

//     match dev_type {
//         DeviceType::DEV_RESERVED => bail!("invalid device type"),
//         DeviceType::DEV_AGENT => handle_agent(shared, socket).await,
//     }
// }

// async fn handle_agent(shared: Arc<Shared>, socket: WebSocket) -> Result<()> {
//     make_agent(socket).await?;
//     Ok(())
// }

// async fn recv_handshake(socket: &mut WebSocket) -> Result<DeviceType> {
//     let msg = socket.recv().await
//     .with_context(|| "recving handshake but closed")?
//     .with_context(|| "recving handshake but failed")?;

//     let req = match msg {
//         Message::Text(s) => bail!("got msg::text {s:?}"),
//         Message::Ping(_ping) => bail!("got msg::ping"),
//         Message::Pong(_pong) => bail!("got msg::pong"),
//         Message::Close(c) => bail!("got msg::close {c:?}"),
//         Message::Binary(d) => {
//             HanshakeRequestPacket::parse_from_bytes(&d[..])
//                 .with_context(||"parse handshake packet failed")?
//         },
//     };

//     let rsp = HanshakeResponsePacket {
//         nonce: 0,
//         ..Default::default()
//     }
//     .write_to_bytes()
//     .with_context(||"encode handshake response failed")?;
    
//     socket.send(Message::Binary(rsp)).await
//         .with_context(||"send handshake response failed")?;

//     Ok(req.device_type())
// }

// /// Actual websocket statemachine (one will be spawned per connection)
// async fn handle_socket0(mut socket: WebSocket, who: SocketAddr) {

//     //send a ping (unsupported by some browsers) just to kick things off and get a response
//     if socket.send(Message::Ping(vec![1, 2, 3])).await.is_ok() {
//         tracing::debug!("Pinged {}...", who);
//     } else {
//         tracing::debug!("Could not send ping {}!", who);
//         // no Error here since the only thing we can do is to close the connection.
//         // If we can not send messages, there is no way to salvage the statemachine anyway.
//         return;
//     }

//     // receive single message from a client (we can either receive or send with socket).
//     // this will likely be the Pong for our Ping or a hello message from client.
//     // waiting for message from a client will block this task, but will not block other client's
//     // connections.
//     if let Some(msg) = socket.recv().await {
//         if let Ok(msg) = msg {
//             if process_message(msg, who).is_break() {
//                 return;
//             }
//         } else {
//             tracing::debug!("client {who} abruptly disconnected");
//             return;
//         }
//     }

//     // Since each client gets individual statemachine, we can pause handling
//     // when necessary to wait for some external event (in this case illustrated by sleeping).
//     // Waiting for this client to finish getting its greetings does not prevent other clients from
//     // connecting to server and receiving their greetings.
//     for i in 1..5 {
//         if socket
//             .send(Message::Text(format!("Hi {i} times!")))
//             .await
//             .is_err()
//         {
//             tracing::debug!("client {who} abruptly disconnected");
//             return;
//         }
//         tokio::time::sleep(std::time::Duration::from_millis(100)).await;
//     }

//     // By splitting socket we can send and receive at the same time. In this example we will send
//     // unsolicited messages to client based on some sort of server's internal event (i.e .timer).
//     let (mut sender, mut receiver) = socket.split();

//     // Spawn a task that will push several messages to the client (does not matter what client does)
//     let mut send_task = tokio::spawn(async move {
//         let n_msg = 20;
//         for i in 0..n_msg {
//             // In case of any websocket error, we exit.
//             if sender
//                 .send(Message::Text(format!("Server message {i} ...")))
//                 .await
//                 .is_err()
//             {
//                 return i;
//             }

//             tokio::time::sleep(std::time::Duration::from_millis(300)).await;
//         }

//         tracing::debug!("Sending close to {who}...");
//         if let Err(e) = sender
//             .send(Message::Close(Some(CloseFrame {
//                 code: axum::extract::ws::close_code::NORMAL,
//                 reason: Cow::from("Goodbye"),
//             })))
//             .await
//         {
//             tracing::debug!("Could not send Close due to {}, probably it is ok?", e);
//         }
//         n_msg
//     });

//     // This second task will receive messages from client and print them on server console
//     let mut recv_task = tokio::spawn(async move {
//         let mut cnt = 0;
//         while let Some(Ok(msg)) = receiver.next().await {
//             cnt += 1;
//             // print message and break if instructed to do so
//             if process_message(msg, who).is_break() {
//                 break;
//             }
//         }
//         cnt
//     });

//     // If any one of the tasks exit, abort the other.
//     tokio::select! {
//         rv_a = (&mut send_task) => {
//             match rv_a {
//                 Ok(a) => tracing::debug!("{} messages sent to {}", a, who),
//                 Err(a) => tracing::debug!("Error sending messages {:?}", a)
//             }
//             recv_task.abort();
//         },
//         rv_b = (&mut recv_task) => {
//             match rv_b {
//                 Ok(b) => tracing::debug!("Received {} messages", b),
//                 Err(b) => tracing::debug!("Error receiving messages {:?}", b)
//             }
//             send_task.abort();
//         }
//     }

//     // returning from the handler closes the websocket connection
//     tracing::debug!("Websocket context {} destroyed", who);
// }

// /// helper to print contents of messages to stdout. Has special treatment for Close.
// fn process_message(msg: Message, who: SocketAddr) -> ControlFlow<(), ()> {
//     match msg {
//         Message::Text(t) => {
//             tracing::debug!(">>> {} sent str: {:?}", who, t);
//         }
//         Message::Binary(d) => {
//             tracing::debug!(">>> {} sent {} bytes: {:?}", who, d.len(), d);
            
//         }
//         Message::Close(c) => {
//             if let Some(cf) = c {
//                 tracing::debug!(
//                     ">>> {} sent close with code {} and reason `{}`",
//                     who, cf.code, cf.reason
//                 );
//             } else {
//                 tracing::debug!(">>> {} somehow sent close message without CloseFrame", who);
//             }
//             return ControlFlow::Break(());
//         }

//         Message::Pong(v) => {
//             tracing::debug!(">>> {} sent pong with {:?}", who, v);
//         }
//         // You should never need to manually handle Message::Ping, as axum's websocket library
//         // will do so for you automagically by replying with Pong and copying the v according to
//         // spec. But if you need the contents of the pings you can see them here.
//         Message::Ping(v) => {
//             tracing::debug!(">>> {} sent ping with {:?}", who, v);
//         }
//     }
//     ControlFlow::Continue(())
// }
