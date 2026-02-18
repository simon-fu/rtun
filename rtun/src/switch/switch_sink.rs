use crate::{
    actor_service::{
        handle_first_none, handle_msg_none, start_actor, Action, ActorEntity, ActorHandle,
        AsyncHandler,
    },
    channel::{ChPacket, CHANNEL_SIZE},
    huid::HUId,
};
use anyhow::{bail, Result};
use futures::SinkExt;
use tokio::sync::mpsc;

use super::invoker_switch::{ReqGetMuxTx, ReqGetMuxTxResult};

pub async fn make_switch_sink<S>(uid: HUId, socket: S) -> Result<SwitchSink<S>>
where
    S: PacketSink,
{
    let (outgoing_tx, outgoing_rx) = mpsc::channel(CHANNEL_SIZE);

    let entity = Entity {
        socket,
        outgoing_tx,
        outgoing_rx,
        // guard: CtrlGuard::new(),
    };

    let handle = start_actor(
        format!("switch-{}", uid),
        entity,
        handle_first_none,
        wait_next,
        handle_next,
        handle_msg_none,
    );

    Ok(SwitchSink { handle })
}

pub type SinkPacket = ChPacket;
pub type SinkError = anyhow::Error;

pub trait PacketSink: 'static + Send + Unpin + SinkExt<SinkPacket, Error = SinkError> {}

pub struct SwitchSink<S: 'static + Send> {
    handle: ActorHandle<Entity<S>>,
}

impl<S> SwitchSink<S>
where
    S: PacketSink,
{
    // pub fn clone_invoker(&self) -> Invoker<Entity<S>> {
    //     self.handle.invoker().clone()
    // }

    pub async fn get_mux_tx(&self) -> ReqGetMuxTxResult {
        self.handle.invoker().invoke(ReqGetMuxTx).await?
    }

    pub async fn shutdown_and_waitfor(&mut self) -> Result<()> {
        self.handle.invoker().shutdown().await;
        self.handle.wait_for_completed().await?;
        Ok(())
    }

    pub async fn wait_for_completed(&mut self) -> Result<Option<SwitchSinkResult>> {
        self.handle.wait_for_completed().await
    }
}

pub type SwitchSinkResult = EntityResult;

#[async_trait::async_trait]
impl<S> AsyncHandler<ReqGetMuxTx> for Entity<S>
where
    S: PacketSink,
{
    type Response = ReqGetMuxTxResult;

    async fn handle(&mut self, _req: ReqGetMuxTx) -> Self::Response {
        Ok(self.outgoing_tx.clone())
    }
}

// #[async_trait::async_trait]
// impl<S> AsyncHandler<OpWatch> for Entity<S>
// where
//     S: PacketSink,
// {
//     type Response = WatchResult;

//     async fn handle(&mut self, _req: OpWatch) -> Self::Response {
//         Ok(self.guard.watch())
//     }
// }

type Next = Result<ChPacket>;

#[inline]
async fn wait_next<S>(entity: &mut Entity<S>) -> Next
where
    S: 'static + Send + Unpin,
{
    let r = entity.outgoing_rx.recv().await;
    match r {
        Some(d) => Ok(d),
        None => bail!("no one care when wait next"),
    }
}

async fn handle_next<S>(entity: &mut Entity<S>, next: Next) -> Result<Action>
where
    S: 'static + Send + SinkExt<SinkPacket, Error = SinkError> + Unpin,
{
    let packet = next?;
    entity.socket.send(packet).await?;

    loop {
        let r = entity.outgoing_rx.try_recv();
        match r {
            Ok(packet) => {
                entity.socket.send(packet).await?;
            }
            Err(e) => match e {
                mpsc::error::TryRecvError::Empty => break,
                mpsc::error::TryRecvError::Disconnected => return Err(e.into()),
            },
        }
    }

    Ok(Action::None)
}

pub struct Entity<S> {
    socket: S,
    outgoing_tx: mpsc::Sender<ChPacket>,
    outgoing_rx: mpsc::Receiver<ChPacket>,
    // guard: CtrlGuard,
}

type Msg = ();

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
