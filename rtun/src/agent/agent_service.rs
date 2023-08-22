// use std::sync::Arc;

// use anyhow::{Result, Context, bail};
// use axum::extract::ws::{WebSocket, Message as WsMessage};
// use protobuf::Message;
// use crate::{actor_service::{ActorEntity, start_actor, handle_first_none, wait_next_none, handle_next_none, handle_msg_none, Action, ActorHandle, AsyncHandler}, proto::{AgentClientOutPacket, agent_client_out_packet::Agent_client_out}, util::extract_ws_recv, huid::gen_huid::gen_huid, channel::ChannelId};

// pub struct AgentServiceConfig{
//     shell_cmd: Vec<String>,
// }

// pub struct AgentService {
//     handle: ActorHandle<Entity>,
// }

// pub async fn make_agent_service(config: Arc<AgentServiceConfig>) -> Result<AgentService> {

//     let entity = Entity {
//         config,
//     };

//     let uid = gen_huid();
//     let handle = start_actor(
//         format!("agent-service-{}", uid),
//         entity, 
//         handle_first_none,
//         wait_next_none, 
//         handle_next_none, 
//         handle_msg,
//     );

//     Ok(AgentService {
//         handle,
//     })
// }

// pub struct OpOpenShell {
//     pub ch_id: ChannelId,
// }


// #[async_trait::async_trait]
// impl AsyncHandler<OpOpenShell> for Entity {
//     type Response = Result<()>; //: MessageResponse<Self, M>;

//     async fn handle(&mut self, _req: OpOpenShell) -> Self::Response {
//         Ok(())
//     }
// }



// async fn handle_msg(_entity: &mut Entity, _msg: Msg) -> Result<Action> {
//     Ok(Action::None)
// }


// type Next = ();
// struct Entity {
//     config: Arc<AgentServiceConfig>,
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


