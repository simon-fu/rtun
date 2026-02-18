use crate::{
    actor_service::{ActorEntity, AsyncHandler, Invoker, WeakInvoker},
    channel::{ChId, ChSender, ChTx},
};
use anyhow::Result;

use super::entity_watch::{OpWatch, WatchResult};

#[derive(Debug)]
pub struct ReqAddChannel(pub ChId, pub ChSender);

pub type AddChannelResult = Result<ChSender>;

#[derive(Debug)]
pub struct ReqRemoveChannel(pub ChId);

pub type RemoveChannelResult = Result<bool>;

#[derive(Debug)]
pub struct ReqGetMuxTx;

pub type ReqGetMuxTxResult = Result<ChTx>;

pub trait SwitchHanlder:
    ActorEntity
    + AsyncHandler<ReqAddChannel, Response = AddChannelResult>
    + AsyncHandler<ReqRemoveChannel, Response = RemoveChannelResult>
    + AsyncHandler<ReqGetMuxTx, Response = ReqGetMuxTxResult>
    + AsyncHandler<OpWatch, Response = WatchResult>
{
}

#[derive(Clone)]
pub struct SwitchInvoker<H: SwitchHanlder> {
    invoker: Invoker<H>,
}

impl<E> SwitchInvoker<E>
where
    E: SwitchHanlder,
{
    pub fn new(invoker: Invoker<E>) -> Self {
        Self { invoker }
    }

    pub fn downgrade(&self) -> SwitchInvokerWeak<E> {
        SwitchInvokerWeak {
            weak: self.invoker.downgrade(),
        }
    }

    pub async fn shutdown(&self) {
        self.invoker.shutdown().await
    }

    pub async fn watch(&self) -> WatchResult {
        self.invoker.invoke(OpWatch).await?
    }

    pub async fn add_channel(&self, ch_id: ChId, sender: ChSender) -> AddChannelResult {
        self.invoker.invoke(ReqAddChannel(ch_id, sender)).await?
    }

    pub async fn remove_channel(&self, ch_id: ChId) -> RemoveChannelResult {
        self.invoker.invoke(ReqRemoveChannel(ch_id)).await?
    }

    pub async fn get_mux_tx(&self) -> ReqGetMuxTxResult {
        self.invoker.invoke(ReqGetMuxTx).await?
    }
}

// impl<'a, H: SwitchHanlder> AsRef<SwitchRef<'a, H>> for SwitchInvoker<H> {
//     fn as_ref(&self) -> &SwitchRef<'a, H> {

//     }
// }

// #[derive(Clone)]
// pub struct SwitchRef<'a, H: SwitchHanlder> {
//     invoker: &'a Invoker<H>,
// }

// impl<'a, E> SwitchRef<'a, E>
// where
//     E: SwitchHanlder,
// {
//     pub fn new(invoker: &'a Invoker<E>) -> Self {
//         Self {
//             invoker,
//         }
//     }

//     pub fn downgrade(&self) -> CtrlClientWeak<E> {
//         CtrlClientWeak {
//             weak: self.invoker.downgrade(),
//         }
//     }

//     pub async fn add_channel(&self, ch_id: ChId, sender: ChSender) -> AddChannelResult {
//         self.invoker.invoke(ReqAddChannel(ch_id, sender)).await?
//     }

//     pub async fn remove_channel(&self, ch_id: ChId) -> RemoveChannelResult {
//         self.invoker.invoke(ReqRemoveChannel(ch_id)).await?
//     }

//     pub async fn get_mux_tx(&self) -> ReqGetMuxTxResult {
//         self.invoker.invoke(ReqGetMuxTx).await?
//     }
// }

#[derive(Clone)]
pub struct SwitchInvokerWeak<H: SwitchHanlder> {
    weak: WeakInvoker<H>,
}

impl<E: SwitchHanlder> SwitchInvokerWeak<E> {
    pub fn upgrade(&self) -> Option<SwitchInvoker<E>> {
        self.weak.upgrade().map(|invoker| SwitchInvoker { invoker })
    }
}
