use std::collections::HashMap;

use anyhow::{Result, bail};
use axum::extract::ws::{WebSocket, Message as WsMessage};
use bytes::Bytes;
use protobuf::Message;
use rtun::{actor_service::{ActorEntity, start_actor, handle_first_none, Action, AsyncHandler, ActorHandle}, proto::RawPacket, util::recv_ws_packet, huid::HUId, channel::{ChId, ChSender, ChReceiver, ChData, ChPair, CHANNEL_SIZE}};
use tokio::sync::mpsc;

use rtun::swtich::{SwitchInvoker, AgentEntity, OpRemoveChannel, OpAddChannel};


pub struct WsServerSession {
    handle: ActorHandle<Entity>,
}

impl  WsServerSession { 
    pub fn clone_agent(&self) -> SwitchInvoker<Entity> {
        SwitchInvoker::new(self.handle.invoker().clone())
    }

    pub async fn wait_for_completed(&mut self) -> Result<()> {
        self.handle.wait_for_completed().await?;
        Ok(())
    }
}

// pub type WsServerSession = AgentSession<Entity>;

pub async fn make_ws_server_session(uid: HUId, socket: WebSocket) -> Result<WsServerSession> {
    
    let (outgoing_tx, outgoing_rx) = mpsc::channel(CHANNEL_SIZE);

    let entity = Entity {
        socket,
        // invoker: None,
        channels: Default::default(),
        gen_ch_id: ChId(0),
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

    // let agent = AgentInvoker::new(handle.invoker().clone());
    // let chpair = agent.alloc_channel().await?;
    // spawn_agent_ctrl(uid, agent, chpair);

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
    type Response = Result<ChPair>; 

    async fn handle(&mut self, req: OpAddChannel) -> Self::Response {
        assert!(req.0.is_none(), "{req:?}");
        let ch_id = self.next_ch_id();
        let (tx, rx) = mpsc::channel(CHANNEL_SIZE);
        self.channels.insert(ch_id, ChannelItem { tx });
        tracing::debug!("add channel {ch_id:?}");
        Ok(ChPair {
            tx: ChSender::new(ch_id, self.outgoing_tx.clone()),
            rx: ChReceiver::new(rx),
        })
    }
}

// #[async_trait::async_trait]
// impl AsyncHandler<OpAddChannel> for Entity {
//     type Response = Result<ChPair>; 

//     async fn handle(&mut self, req: OpAddChannel) -> Self::Response {
//         // let ch_id = self.next_ch_id();
//         let ch_id = req.0;
//         let (tx, rx) = mpsc::channel(CHANNEL_SIZE);
//         self.channels.insert(ch_id, ChannelItem { tx });
//         tracing::debug!("add channel {ch_id:?}");
//         Ok(ChPair {
//             tx: ChSender::new(ch_id, self.outgoing_tx.clone()),
//             rx: ChReceiver::new(rx),
//         })
//     }
// }

#[async_trait::async_trait]
impl AsyncHandler<OpRemoveChannel> for Entity {
    type Response = Result<bool>; 

    async fn handle(&mut self, req: OpRemoveChannel) -> Self::Response {
        let ch_id = req.0;
        let exist = self.channels.remove(&ch_id).is_some();
        tracing::debug!("remove channel {ch_id:?} {exist}");
        Ok(exist)
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




struct ChannelItem {
    tx: mpsc::Sender<Bytes>,
}

async fn handle_msg(_entity: &mut Entity, _msg: Msg) -> Result<Action> {
    Ok(Action::None)
}



pub struct Entity {
    socket: WebSocket,
    channels: HashMap<ChId, ChannelItem>,
    gen_ch_id: ChId,
    outgoing_tx: mpsc::Sender<ChData>,
    outgoing_rx: mpsc::Receiver<ChData>,
}

impl Entity {
    fn next_ch_id(&mut self) -> ChId {
        let ch_id = self.gen_ch_id;
        self.gen_ch_id.0 += 1;
        ch_id
    }
}

pub enum Msg {

}

impl ActorEntity for Entity {
    type Next = Next;

    type Msg = Msg;

    type Result = ();

    fn into_result(self, _r: Result<()>) -> Self::Result {
        ()
    }
}


