use std::collections::HashMap;

use anyhow::{Result, bail, Context};
use futures::{StreamExt, SinkExt};
use protobuf::Message;
use crate::{actor_service::{ActorEntity, start_actor, handle_first_none, Action, AsyncHandler, ActorHandle}, huid::HUId, channel::{ChId, ChSender, ChPacket, CHANNEL_SIZE}, proto::RawPacket};
use tokio::sync::mpsc;

use super::{invoker_switch::{SwitchHanlder, SwitchInvoker, ReqAddChannel, AddChannelResult, ReqRemoveChannel, RemoveChannelResult, ReqGetMuxTx, ReqGetMuxTxResult}, entity_watch::{OpWatch, WatchResult, CtrlGuard}};


pub async fn make_stream_switch<S>(uid: HUId, socket: S) -> Result<StreamSwitch<S>> 
where
    S: 'static 
        + Send
        + StreamExt<Item = Result<StreamPacket, StreamError>> 
        + SinkExt<SinkPacket, Error = SinkError> 
        + Unpin,
{
    
    let (outgoing_tx, outgoing_rx) = mpsc::channel(CHANNEL_SIZE);

    let entity = Entity {
        socket,
        // invoker: None,
        channels: Default::default(),
        // gen_ch_id: ChId(0),
        outgoing_tx,
        outgoing_rx,
        guard: CtrlGuard::new(),
    };

    let handle = start_actor(
        format!("switch-{}", uid),
        entity, 
        handle_first_none,
        wait_next, 
        handle_next, 
        handle_msg,
    );

    Ok(StreamSwitch {
        handle,
    })
}


pub type StreamPacket = Vec<u8>;
pub type StreamError = anyhow::Error;
pub type SinkPacket = ChPacket;
pub type SinkError = anyhow::Error;


pub struct StreamSwitch<S: 'static + Send> {
    handle: ActorHandle<Entity<S>>,
}

impl<S>  StreamSwitch<S> 
where
    S: 'static + Send
{
    pub async fn shutdown_and_waitfor(&mut self) -> Result<()> {
        self.handle.invoker().shutdown().await;
        self.handle.wait_for_completed().await?;
        Ok(())
    }

    pub async fn wait_for_completed(&mut self) -> Result<Option<EntityResult>> {
        self.handle.wait_for_completed().await
    }
}

impl<S>  StreamSwitch<S> 
where
    S: 'static 
        + Send
        + StreamExt<Item = Result<StreamPacket, StreamError>> 
        + SinkExt<SinkPacket, Error = SinkError> 
        + Unpin,
{
    pub fn clone_invoker(&self) -> StreamSwitchInvoker<S> {
        SwitchInvoker::new(self.handle.invoker().clone())
    }
}

pub type StreamSwitchInvoker<S> = SwitchInvoker<Entity<S>>;




#[async_trait::async_trait]
impl<S> AsyncHandler<ReqAddChannel> for Entity<S> 
where
    S: 'static 
        + Send
        + StreamExt<Item = Result<StreamPacket, StreamError>> 
        + Unpin,

{
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
impl<S> AsyncHandler<ReqRemoveChannel> for Entity<S> 
where
    S: 'static 
        + Send
        + StreamExt<Item = Result<StreamPacket, StreamError>> 
        + Unpin,

{
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
impl<S> AsyncHandler<ReqGetMuxTx> for Entity<S> 
where
    S: 'static 
        + Send
        + StreamExt<Item = Result<StreamPacket, StreamError>> 
        + Unpin,

{
    type Response = ReqGetMuxTxResult; 

    async fn handle(&mut self, _req: ReqGetMuxTx) -> Self::Response {
        Ok(self.outgoing_tx.clone())
    }
}

#[async_trait::async_trait]
impl<S> AsyncHandler<OpWatch> for Entity<S> 
where
    S: 'static 
        + Send
        + StreamExt<Item = Result<StreamPacket, StreamError>> 
        + Unpin,
{
    type Response = WatchResult; 

    async fn handle(&mut self, _req: OpWatch) -> Self::Response {
        Ok(self.guard.watch())
    }
}

impl<S> SwitchHanlder for Entity<S> 
where
    S: 'static 
        + Send
        + StreamExt<Item = Result<StreamPacket, StreamError>> 
        + Unpin,
{}


type Next = Result<NextPacket>;

pub enum NextPacket {
    Stream(Vec<u8>),
    Send(ChPacket),
}

async fn recv_next<S>(stream: &mut S) -> Next 
where
    S: 'static 
        + Send
        + StreamExt<Item = Result<StreamPacket, StreamError>> 
        + Unpin,
{
    stream.next().await
    .with_context(||"reach eof")?
    .map(|x|NextPacket::Stream(x))
}

#[inline]
async fn wait_next<S>(entity: &mut Entity<S>) -> Next 
where
    S: 'static 
        + Send
        + StreamExt<Item = Result<StreamPacket, StreamError>> 
        + Unpin,
{
    tokio::select! {
        r = recv_next(&mut entity.socket) => {
            r
            // r.map(|x| x.map(|x|NextPacket::RawPacket(x)))
        },
        
        r = entity.outgoing_rx.recv() => {
            match r {
                Some(d) => Ok(NextPacket::Send(d)),
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
        + SinkExt<SinkPacket, Error = SinkError> 
        + Unpin,
{
    let next = next?;
    match next {
        NextPacket::Send(packet) => {
            entity.socket.send(packet).await?;
        },
        NextPacket::Stream(data) => {
            let raw = RawPacket::parse_from_bytes(&data)
            .with_context(||"invalid raw packet")?;
            let packet = ChPacket {
                ch_id: ChId(raw.ch_id),
                payload: raw.payload,
            };
            
            if let Some(item) = entity.channels.get(&packet.ch_id) {
                let _r = item.tx.send_data(packet.payload).await; 
                // TODO: remove channel if fail
            }
        },
    }

    Ok(Action::None)
}




struct ChannelItem {
    tx: ChSender,
}

impl Drop for ChannelItem {
    fn drop(&mut self) {
        let _r = self.tx.try_send_zero();
    }
}

async fn handle_msg<S>(_entity: &mut Entity<S>, _msg: Msg) -> Result<Action> {
    Ok(Action::None)
}



pub struct Entity<S> {
    socket: S,
    // invoker: Option<WeakInvoker<Self>>,
    channels: HashMap<ChId, ChannelItem>,
    // gen_ch_id: ChId,
    outgoing_tx: mpsc::Sender<ChPacket>,
    outgoing_rx: mpsc::Receiver<ChPacket>,
    guard: CtrlGuard,
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

type EntityResult = ();

impl<S> ActorEntity for Entity<S> 
where
    S: 'static + Send,
{
    type Next = Next;

    type Msg = Msg;

    type Result = EntityResult;

    fn into_result(self, _r: Result<()>) -> Self::Result {
        ()
    }
}


