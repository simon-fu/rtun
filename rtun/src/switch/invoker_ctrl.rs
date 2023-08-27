


use anyhow::Result;
use crate::{channel::{ChId, ChSender}, actor_service::{ActorEntity, AsyncHandler, Invoker, WeakInvoker}, proto::{OpenShellArgs, OpenSocksArgs}};

use super::entity_watch::{OpWatch, WatchResult};

pub trait CtrlHandler: ActorEntity 
+ AsyncHandler<OpWatch, Response = WatchResult>
+ AsyncHandler<OpOpenChannel, Response = OpenChannelResult>
+ AsyncHandler<OpCloseChannel, Response = CloseChannelResult>
+ AsyncHandler<OpOpenShell, Response = OpOpenShellResult>
+ AsyncHandler<OpOpenSocks, Response = OpOpenSocksResult>
{

}


pub struct CtrlInvoker<H: CtrlHandler> {
    invoker: Invoker<H>,
}

impl<H: CtrlHandler> Clone for CtrlInvoker<H> {
    fn clone(&self) -> Self {
        Self { invoker: self.invoker.clone() }
    }
}

impl<H> CtrlInvoker<H> 
where
    H: CtrlHandler,
{
    pub fn new(invoker: Invoker<H>) -> Self {
        Self {
            invoker,
        }
    }

    pub fn downgrade(&self) -> CtrlWeak<H> {
        CtrlWeak {
            weak: self.invoker.downgrade(),
        }
    }

    pub async fn watch(&self) -> WatchResult {
        self.invoker.invoke(OpWatch).await?
    }

    // pub async fn open_channel_easy(&self, ch_id: ChId) -> Result<ChPair> {
    //     let pair = ChPair::new(ch_id);
    //     Ok(ChPair {
    //         tx: self.open_channel(pair.tx).await?,
    //         rx: pair.rx,
    //     })
    // }

    pub async fn open_channel(&self, ch_tx: ChSender) -> OpenChannelResult {
        self.invoker.invoke(OpOpenChannel(ch_tx)).await?
    }

    pub async fn close_channel(&self, ch_id: ChId) -> CloseChannelResult {
        self.invoker.invoke(OpCloseChannel(ch_id)).await?
    }

    pub async fn open_shell(&self, ch_tx: ChSender, args: OpenShellArgs) -> OpOpenShellResult {
        self.invoker.invoke(OpOpenShell(ch_tx, args)).await?
    }

    pub async fn open_socks(&self, ch_tx: ChSender, args: OpenSocksArgs) -> OpOpenShellResult {
        self.invoker.invoke(OpOpenSocks(ch_tx, args)).await?
    }
}

pub struct CtrlWeak<H: CtrlHandler> {
    weak: WeakInvoker<H>,
}

impl<H: CtrlHandler> Clone for CtrlWeak<H> {
    fn clone(&self) -> Self {
        Self { weak: self.weak.clone() }
    }
}

impl <H: CtrlHandler> CtrlWeak<H> {
    pub fn upgrade(&self) -> Option<CtrlInvoker<H>> {
        self.weak.upgrade().map(|invoker| CtrlInvoker {
            invoker,
        })
    }
}


#[derive(Debug)]
pub struct OpOpenChannel(pub ChSender);

pub type OpenChannelResult = Result<ChSender>;

#[derive(Debug)]
pub struct OpCloseChannel(pub ChId);

pub type CloseChannelResult = Result<bool>;

#[derive(Debug)]
pub struct OpOpenShell(pub ChSender, pub OpenShellArgs);

pub type OpOpenShellResult = Result<ChSender>;

#[derive(Debug)]
pub struct OpOpenSocks(pub ChSender, pub OpenSocksArgs);

pub type OpOpenSocksResult = Result<ChSender>;


