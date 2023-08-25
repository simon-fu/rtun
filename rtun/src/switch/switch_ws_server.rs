use std::collections::HashMap;

use anyhow::{Result, bail};
use axum::extract::ws::{WebSocket, Message as WsMessage};
use protobuf::Message;
use crate::{actor_service::{ActorEntity, start_actor, handle_first_none, Action, AsyncHandler, ActorHandle}, proto::RawPacket, util::recv_ws_packet, huid::HUId, channel::{ChId, ChSender, ChPacket, CHANNEL_SIZE}};
use tokio::sync::mpsc;

use super::invoker_switch::{ReqRemoveChannel, SwitchHanlder, SwitchInvoker, ReqAddChannel, AddChannelResult, RemoveChannelResult, ReqGetMuxTx, ReqGetMuxTxResult};


pub struct WsServerSwitch {
    handle: ActorHandle<Entity>,
}

impl  WsServerSwitch { 
    pub fn clone_switch(&self) -> SwitchInvoker<Entity> {
        SwitchInvoker::new(self.handle.invoker().clone())
    }

    // pub fn clone_ctrl(&self) -> CtrlInvoker<Entity> {
    //     CtrlInvoker::new(self.handle.invoker().clone())
    // }

    pub async fn wait_for_completed(&mut self) -> Result<()> {
        self.handle.wait_for_completed().await?;
        Ok(())
    }
}


pub async fn make_ws_server_switch(uid: HUId, socket: WebSocket) -> Result<WsServerSwitch> {
    
    let (outgoing_tx, outgoing_rx) = mpsc::channel(CHANNEL_SIZE);

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

    // let agent = AgentInvoker::new(handle.invoker().clone());
    // let chpair = agent.alloc_channel().await?;
    // spawn_agent_ctrl(uid, agent, chpair);

    Ok(WsServerSwitch {
        handle,
    })
}



// #[async_trait::async_trait]
// impl AsyncHandler<OpOpenChannel> for Entity {
//     type Response = OpenChannelResult; 

//     async fn handle(&mut self, req: OpOpenChannel) -> Self::Response {
//         let ch_id = self.next_ch_id();
//         let tx = req.0;
        
//         tracing::debug!("open channel {ch_id:?} -> {:?}", tx.ch_id());

//         self.channels.insert(ch_id, ChannelItem { tx });
        
//         Ok(ChSender::new(ch_id, self.outgoing_tx.clone()))
//     }
// }

// #[async_trait::async_trait]
// impl AsyncHandler<OpCloseChannel> for Entity {
//     type Response = CloseChannelResult; 

//     async fn handle(&mut self, req: OpCloseChannel) -> Self::Response {
//         let ch_id = req.0;
        
//         let old = self.channels.remove(&ch_id);

//         if let Some(old) = &old {
//             tracing::debug!("close channel {ch_id:?} -> {:?}", old.tx.ch_id());
//             Ok(true)
//         } else {
//             Ok(false)
//         }
//     }
// }

// impl CtrlHandler for Entity {}

#[async_trait::async_trait]
impl AsyncHandler<ReqAddChannel> for Entity {
    type Response = AddChannelResult; 

    async fn handle(&mut self, req: ReqAddChannel) -> Self::Response {
        let ch_id = req.0;
        let tx = req.1;

        tracing::debug!("add channel {ch_id:?} -> {:?}", tx.ch_id());

        self.channels.insert(ch_id, ChannelItem { tx });

        Ok(ChSender::new(ch_id, self.outgoing_tx.clone()))
    }
}

#[async_trait::async_trait]
impl AsyncHandler<ReqRemoveChannel> for Entity {
    type Response = RemoveChannelResult; 

    async fn handle(&mut self, req: ReqRemoveChannel) -> Self::Response {
        let ch_id = req.0;
        
        let old = self.channels.remove(&ch_id);

        if let Some(old) = &old {
            tracing::debug!("remove channel {ch_id:?} -> {:?}", old.tx.ch_id());
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[async_trait::async_trait]
impl AsyncHandler<ReqGetMuxTx> for Entity {
    type Response = ReqGetMuxTxResult; 

    async fn handle(&mut self, _req: ReqGetMuxTx) -> Self::Response {
        Ok(self.outgoing_tx.clone())
    }
}


impl SwitchHanlder for Entity {}



type Next = Result<NextPacket>;

pub enum NextPacket {
    ChPacket(ChPacket),
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
                Some(d) => Ok(NextPacket::ChPacket(d)),
                None => bail!("no one care when wait next"),
            }
        }
    }
    // recv_ws_packet::<_, RawPacket>(&mut entity.socket).await
}

async fn handle_next(entity: &mut Entity, next: Next) -> Result<Action> {
    let next = next?;
    match next {
        NextPacket::ChPacket(data) => {
            let raw = RawPacket {
                ch_id: data.ch_id.0,
                payload: data.payload,
                ..Default::default()
            }
            .write_to_bytes()?;

            entity.socket.send(WsMessage::Binary(raw)).await?;
        },
        NextPacket::RawPacket(packet) => {
            let packet = ChPacket {
                ch_id: ChId(packet.ch_id),
                payload: packet.payload,
            };
            if let Some(item) = entity.channels.get(&packet.ch_id) {
                let _r = item.tx.send_data( packet.payload ).await; 
                // TODO: remove channel if fail
            }
        },
    }

    Ok(Action::None)
}




struct ChannelItem {
    tx: ChSender,
}

async fn handle_msg(_entity: &mut Entity, _msg: Msg) -> Result<Action> {
    Ok(Action::None)
}



pub struct Entity {
    socket: WebSocket,
    channels: HashMap<ChId, ChannelItem>,
    // gen_ch_id: ChId,
    outgoing_tx: mpsc::Sender<ChPacket>,
    outgoing_rx: mpsc::Receiver<ChPacket>,
}

// impl Entity {
//     fn next_ch_id(&mut self) -> ChId {
//         let ch_id = self.gen_ch_id;
//         self.gen_ch_id.0 += 1;
//         ch_id
//     }
// }

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


