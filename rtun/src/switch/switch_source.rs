use std::collections::HashMap;

use crate::{
    actor_service::{
        handle_first_none, start_actor, Action, ActorEntity, ActorHandle, AsyncHandler,
    },
    channel::{ChId, ChPacket, ChSender},
    huid::HUId,
    proto::RawPacket,
};
use anyhow::{Context, Result};
use futures::StreamExt;
use protobuf::Message;
use tokio::sync::mpsc;

use super::{
    ctrl_client::CtrlClient,
    entity_watch::{CtrlGuard, OpWatch, WatchResult},
    invoker_switch::{
        AddChannelResult, RemoveChannelResult, ReqAddChannel, ReqGetMuxTx, ReqGetMuxTxResult,
        ReqRemoveChannel, SwitchHanlder, SwitchInvoker,
    },
};

pub async fn make_switch_source<S>(
    uid: HUId,
    socket: S,
    outgoing_tx: mpsc::Sender<ChPacket>,
) -> Result<SwitchSource<S>>
where
    S: PacketSource,
{
    let entity = Entity {
        socket,
        // invoker: None,
        channels: Default::default(),
        // gen_ch_id: ChId(0),
        outgoing_tx,
        // outgoing_rx,
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

    Ok(SwitchSource { handle })
}

pub type StreamPacket = Vec<u8>;
pub type StreamError = anyhow::Error;
// pub type SinkPacket = ChPacket;
// pub type SinkError = anyhow::Error;

pub trait PacketSource:
    'static + Send + Unpin + StreamExt<Item = Result<StreamPacket, StreamError>>
{
}

// pub trait SwitchOps {

// }

// pub struct SwitchSession<E: ActorEntity> {
//     handle: ActorHandle<E>,
// }

pub struct SwitchSource<S: 'static + Send> {
    handle: ActorHandle<Entity<S>>,
}

impl<S> SwitchSource<S>
where
    S: 'static + Send,
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

impl<S> SwitchSource<S>
where
    S: PacketSource,
{
    pub fn clone_invoker(&self) -> SwitchSourceInvoker<S> {
        SwitchInvoker::new(self.handle.invoker().clone())
    }
}

pub type SwitchSourceInvoker<S> = SwitchInvoker<Entity<S>>;
pub type SwitchSourceCtrlClient<S> = CtrlClient<Entity<S>>;
pub type SwitchSourceEntity<S> = Entity<S>;

#[async_trait::async_trait]
impl<S> AsyncHandler<ReqAddChannel> for Entity<S>
where
    S: 'static + Send + StreamExt<Item = Result<StreamPacket, StreamError>> + Unpin,
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
    S: 'static + Send + StreamExt<Item = Result<StreamPacket, StreamError>> + Unpin,
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
    S: 'static + Send + StreamExt<Item = Result<StreamPacket, StreamError>> + Unpin,
{
    type Response = ReqGetMuxTxResult;

    async fn handle(&mut self, _req: ReqGetMuxTx) -> Self::Response {
        Ok(self.outgoing_tx.clone())
    }
}

#[async_trait::async_trait]
impl<S> AsyncHandler<OpWatch> for Entity<S>
where
    S: 'static + Send + StreamExt<Item = Result<StreamPacket, StreamError>> + Unpin,
{
    type Response = WatchResult;

    async fn handle(&mut self, _req: OpWatch) -> Self::Response {
        Ok(self.guard.watch())
    }
}

impl<S> SwitchHanlder for Entity<S> where
    S: 'static + Send + StreamExt<Item = Result<StreamPacket, StreamError>> + Unpin
{
}

type Next = Result<Vec<u8>>;

// async fn recv_next<S>(stream: &mut S) -> Next
// where
//     S: 'static
//         + Send
//         + StreamExt<Item = Result<StreamPacket, StreamError>>
//         + Unpin,
// {
//     stream.next().await
//     .with_context(||"reach eof")?
//     .map(|x|NextPacket::Stream(x))
// }

#[inline]
async fn wait_next<S>(entity: &mut Entity<S>) -> Next
where
    S: PacketSource,
{
    entity.socket.next().await.with_context(|| "reach eof")?
}

async fn handle_next<S>(entity: &mut Entity<S>, next: Next) -> Result<Action>
where
    S: PacketSource,
{
    let data = next?;

    let raw = RawPacket::parse_from_bytes(&data).with_context(|| "invalid raw packet")?;
    let packet = ChPacket {
        ch_id: ChId(raw.ch_id),
        payload: raw.payload,
    };

    if let Some(item) = entity.channels.get(&packet.ch_id) {
        let _r = item.tx.send_data(packet.payload).await;
        // TODO: remove channel if fail
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
    // outgoing_rx: mpsc::Receiver<ChPacket>,
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

pub enum Msg {}

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
