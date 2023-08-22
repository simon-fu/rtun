// use anyhow::{Result, Context, bail};
// use axum::extract::ws::{WebSocket, Message as WsMessage};
// use futures::StreamExt;
// use protobuf::Message;
// use rtun::{actor_service::{ActorEntity, start_actor, handle_first_none, wait_next_none, handle_next_none, handle_msg_none, Action}, proto::{AgentClientOutPacket, agent_client_out_packet::Agent_client_out}, util::extract_ws_recv};


// pub struct AgentSession {

// }

// pub async fn make_agent(socket: WebSocket) -> Result<()> {
//     // let (sender, recv) = socket.split();

//     let entity = Entity {
//         socket,
//     };

//     let uid = 111_u64;
//     let handle = start_actor(
//         format!("agent-{}", uid),
//         entity, 
//         handle_first_none,
//         wait_next, 
//         handle_next, 
//         handle_msg,
//     );

//     Ok(())
// }


// #[inline]
// async fn wait_next(entity: &mut Entity) -> Next {
//     entity.socket.recv().await
//         .map(|x|x.with_context(||"recv failed"))
// }

// async fn handle_next(entity: &mut Entity, next: Next) -> Result<Action> {
//     if let Some(pkt_data) = extract_ws_recv(next)? {
//         let packet = AgentClientOutPacket::parse_from_bytes(&pkt_data[..])
//             .with_context(||"parse packet failed")?
//             .agent_client_out
//             .with_context(||"invalid packet: empty")?;

//         match packet {
//             Agent_client_out::ChData(ch) => {

//             },
//             _ => todo!(),
//         }
//     }
//     Ok(Action::None)
// }

// async fn handle_msg(_entity: &mut Entity, _msg: Msg) -> Result<Action> {
//     Ok(Action::None)
// }




// type Next = Option<Result<WsMessage>>;
// struct Entity {
//     socket: WebSocket
// }

// enum Msg {

// }

// impl ActorEntity for Entity {
//     type Next = Next;

//     type Msg = Msg;

//     type Result = Self;

//     fn into_result(self) -> Self::Result {
//         self
//     }
// }


