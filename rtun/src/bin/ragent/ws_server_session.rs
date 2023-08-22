use std::collections::HashMap;

use anyhow::{Result, bail};
use axum::extract::ws::{WebSocket, Message as WsMessage};
use bytes::Bytes;
use protobuf::Message;
use rtun::{actor_service::{ActorEntity, start_actor, handle_first_none, Action, AsyncHandler, ActorHandle}, proto::RawPacket, util::recv_ws_packet, huid::HUId, channel::{ChId, ChSender, ChReceiver, ChData}};
use tokio::sync::mpsc;

use crate::{agent_invoker::{AgentInvoker, AgentEntity, OpAddChannel}, agent_ch_ctrl::spawn_agent_ctrl};


pub struct WsServerSession {
    handle: ActorHandle<Entity>,
}

impl  WsServerSession {
    pub async fn wait_for_completed(&mut self) -> Result<()> {
        self.handle.wait_for_completed().await?;
        Ok(())
    }
}

// pub type WsServerSession = AgentSession<Entity>;

pub async fn make_ws_server_session(uid: HUId, socket: WebSocket) -> Result<WsServerSession> {
    
    let (outgoing_tx, outgoing_rx) = mpsc::channel(512);

    let entity = Entity {
        socket,
        // invoker: None,
        channels: Default::default(),
        // gen_ch_id: ChId(0),
        outgoing_tx,
        outgoing_rx
    };

    let handle = start_actor(
        format!("ws-serv-{}", uid),
        entity, 
        handle_first_none,
        wait_next, 
        handle_next, 
        handle_msg,
    );

    // let invoker = handle.invoker().downgrade();
    // handle.invoker().invoke(SetInvoker(invoker)).await??;

    let session = AgentInvoker::new(handle.invoker().clone());
    spawn_agent_ctrl(uid, session);

    // let wait4completed = handle.take_completed()
    // .with_context(||"must have wait4completed")?;

    Ok(WsServerSession {
        handle,
    })
}



// struct SetInvoker(WeakInvoker<Entity>);

// #[async_trait::async_trait]
// impl AsyncHandler<SetInvoker> for Entity {
//     type Response = Result<()>; //: MessageResponse<Self, M>;

//     async fn handle(&mut self, req: SetInvoker) -> Self::Response {
//         self.invoker = Some(req.0);
//         Ok(())
//     }
// }




#[async_trait::async_trait]
impl AsyncHandler<OpAddChannel> for Entity {
    type Response = Result<(ChSender, ChReceiver)>; 

    async fn handle(&mut self, req: OpAddChannel) -> Self::Response {
        // let ch_id = self.next_ch_id();
        let ch_id = req.0;
        let (tx, rx) = mpsc::channel(256);
        self.channels.insert(ch_id, ChannelItem { tx });
        
        Ok((
            ChSender::new(ch_id, self.outgoing_tx.clone()),
            ChReceiver::new(rx),
        ))
    }
}

impl AgentEntity for Entity {}


type Next = Result<NextPacket>;

pub enum NextPacket {
    ChData(ChData),
    RawPacket(RawPacket),
}

#[inline]
async fn wait_next(entity: &mut Entity) -> Next {
    tokio::select! {
        r = recv_ws_packet::<_, RawPacket>(&mut entity.socket) => {
            r.map(|x| NextPacket::RawPacket(x))
        },
        
        r = entity.outgoing_rx.recv() => {
            match r {
                Some(d) => Ok(NextPacket::ChData(d)),
                None => bail!("no one care when wait next"),
            }
        }
    }
    // recv_ws_packet::<_, RawPacket>(&mut entity.socket).await
}

async fn handle_next(entity: &mut Entity, next: Next) -> Result<Action> {
    let next = next?;
    match next {
        NextPacket::ChData(data) => {
            let raw = RawPacket {
                ch_id: data.ch_id.0,
                payload: data.payload,
                ..Default::default()
            }
            .write_to_bytes()?;

            entity.socket.send(WsMessage::Binary(raw)).await?;
        },
        NextPacket::RawPacket(packet) => {
            let ch_id = ChId(packet.ch_id);
            if let Some(item) = entity.channels.get(&ch_id) {
                let _r = item.tx.send(packet.payload).await; 
                // TODO: remove channel if fail
            }
        },
    }

    Ok(Action::None)
}



// type Next = Result<AgentServerInPacket>;

// #[inline]
// async fn wait_next(entity: &mut Entity) -> Next {
//     recv_ws_packet::<_, AgentServerInPacket>(&mut entity.socket).await
//     // entity.socket.recv().await
//     //     .map(|x|x.with_context(||"recv failed"))
// }

// async fn handle_next(entity: &mut Entity, next: Next) -> Result<Action> {
//     let packet = next?
//     .agent_server_in
//     .with_context(||"invalid AgentServerInPacket")?;
//     match packet {
//         Agent_server_in::OpenChannel(op) => {
//             let ch_id = ChannelId(op.ch_id());
//             let arg = op.channel_arg.with_context(||"no channel_arg")? ;
//             match arg {
//                 Channel_arg::OpenShell(shell_arg) => {
//                     // let program = std::env::var("SHELL").unwrap_or("bash".to_string());
//                     let program = "bash".to_string();
//                     tracing::debug!("opening shell... [{}]", program);
//                     let (pty_sender, pty_recver) = make_async_pty_process(&program, &["-i"], shell_arg.row as u16, shell_arg.col as u16).await?;
//                     tracing::debug!("opened shell [{}]", program);

//                     let invoker = entity.invoker()?;
//                     let (tx, rx) = mpsc::channel(256);
//                     entity.channels.insert(ch_id, ChannelItem { tx });
//                     spawn_with_name("", async move {
//                         let r = copy_pty_channel(ch_id, pty_sender, pty_recver, invoker, rx).await;
//                         tracing::debug!("copy_pty_channel finished with [{:?}]", r);
//                     });
//                 },
//                 _ => {},
//             }
            
//         },
//         Agent_server_in::ChData(ch_data) => {
//             let ch_id = ChannelId(ch_data.ch_id);
//             if let Some(item) = entity.channels.get(&ch_id) {
//                 let _r = item.tx.send(ch_data.payload).await; 
//                 // TODO: remove channel if fail
//             }
//         }
//         _ => {},
//     }
//     Ok(Action::None)
// }


struct ChannelItem {
    tx: mpsc::Sender<Bytes>,
}

async fn handle_msg(_entity: &mut Entity, _msg: Msg) -> Result<Action> {
    Ok(Action::None)
}



pub struct Entity {
    socket: WebSocket,
    // invoker: Option<WeakInvoker<Self>>,
    channels: HashMap<ChId, ChannelItem>,
    // gen_ch_id: ChId,
    outgoing_tx: mpsc::Sender<ChData>,
    outgoing_rx: mpsc::Receiver<ChData>,
}

impl Entity {
    // fn invoker(&self) -> Result<Invoker<Self>> {
    //     self.invoker.as_ref().
    //     with_context(||"no invoker")?
    //     .upgrade()
    //     .with_context(||"invoker gone")
    // }

    // fn next_ch_id(&mut self) -> ChId {
    //     self.gen_ch_id.0 += 1;
    //     self.gen_ch_id
    // }
}

pub enum Msg {

}

impl ActorEntity for Entity {
    type Next = Next;

    type Msg = Msg;

    type Result = Self;

    fn into_result(self) -> Self::Result {
        self
    }
}


