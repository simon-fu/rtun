use std::collections::HashMap;

use anyhow::{Result, bail};
use tokio_tungstenite::tungstenite::{Message as WsMessage, self};
use bytes::Bytes;
use futures::{StreamExt, SinkExt};
use protobuf::Message;
use rtun::{actor_service::{ActorEntity, start_actor, handle_first_none, Action, AsyncHandler, ActorHandle}, proto::RawPacket, huid::HUId, channel::{ChId, ChSender, ChReceiver, ChData}};
use tokio::sync::mpsc;


use crate::{client_invoker::{OpAddChannel, ClientEntity, ClientInvoker}, rclient::ws_client_recv_packet};


pub struct WsClientSession<S: 'static + Send> {
    handle: ActorHandle<Entity<S>>,
}

impl<S>  WsClientSession<S> 
where
    S: 'static + Send
{
    pub async fn wait_for_completed(&mut self) -> Result<()> {
        self.handle.wait_for_completed().await?;
        Ok(())
    }
}

impl<S>  WsClientSession<S> 
where
    S: 'static 
        + Send
        + StreamExt<Item = Result<WsMessage, tungstenite::Error>> 
        + SinkExt<WsMessage, Error = tungstenite::Error> 
        + Unpin,
{
    pub fn invoker(&self) -> ClientInvoker<Entity<S>> {
            ClientInvoker::new(self.handle.invoker().clone())
    }
}


pub async fn make_ws_client_session<S>(uid: HUId, socket: S) -> Result<WsClientSession<S>> 
where
    S: 'static 
        + Send
        + StreamExt<Item = Result<WsMessage, tungstenite::Error>> 
        + SinkExt<WsMessage, Error = tungstenite::Error> 
        + Unpin,
{
    
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

    // let session = AgentInvoker::new(handle.invoker().clone());
    // spawn_agent_ctrl(uid, session);


    Ok(WsClientSession {
        handle,
    })
}





#[async_trait::async_trait]
impl<S> AsyncHandler<OpAddChannel> for Entity<S> 
where
    S: 'static 
        + Send
        + StreamExt<Item = Result<WsMessage, tungstenite::Error>> 
        + Unpin,

{
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

impl<S> ClientEntity for Entity<S> 
where
    S: 'static 
        + Send
        + StreamExt<Item = Result<WsMessage, tungstenite::Error>> 
        + Unpin,
{}


type Next = Result<NextPacket>;

pub enum NextPacket {
    ChData(ChData),
    RawPacket(RawPacket),
}

#[inline]
async fn wait_next<S>(entity: &mut Entity<S>) -> Next 
where
    S: 'static 
        + Send
        + StreamExt<Item = Result<WsMessage, tungstenite::Error>> 
        + Unpin,
{
    tokio::select! {
        r = ws_client_recv_packet::<_, RawPacket>(&mut entity.socket) => {
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

async fn handle_next<S>(entity: &mut Entity<S>, next: Next) -> Result<Action> 
where
    S: 'static 
        + Send
        + SinkExt<WsMessage, Error = tungstenite::Error> 
        + Unpin,
{
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

async fn handle_msg<S>(_entity: &mut Entity<S>, _msg: Msg) -> Result<Action> {
    Ok(Action::None)
}



pub struct Entity<S> {
    socket: S,
    // invoker: Option<WeakInvoker<Self>>,
    channels: HashMap<ChId, ChannelItem>,
    // gen_ch_id: ChId,
    outgoing_tx: mpsc::Sender<ChData>,
    outgoing_rx: mpsc::Receiver<ChData>,
}

impl<S> Entity<S> {
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

impl<S> ActorEntity for Entity<S> 
where
    S: 'static + Send,
{
    type Next = Next;

    type Msg = Msg;

    type Result = Self;

    fn into_result(self) -> Self::Result {
        self
    }
}


