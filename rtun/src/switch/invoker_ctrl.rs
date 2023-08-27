


use anyhow::Result;
use crate::{channel::{ChId, ChSender}, actor_service::{ActorEntity, AsyncHandler, Invoker, WeakInvoker}, proto::{OpenShellArgs, OpenSocksArgs}};

pub trait CtrlHandler: ActorEntity 
+ AsyncHandler<OpOpenChannel, Response = OpenChannelResult>
+ AsyncHandler<OpCloseChannel, Response = CloseChannelResult>
+ AsyncHandler<OpOpenShell, Response = OpOpenShellResult>
+ AsyncHandler<OpOpenSocks, Response = OpOpenSocksResult>
{

}


pub struct CtrlInvoker<E: CtrlHandler> {
    invoker: Invoker<E>,
}

impl<E: CtrlHandler> Clone for CtrlInvoker<E> {
    fn clone(&self) -> Self {
        Self { invoker: self.invoker.clone() }
    }
}

impl<E> CtrlInvoker<E> 
where
    E: CtrlHandler,
{
    pub fn new(invoker: Invoker<E>) -> Self {
        Self {
            invoker,
        }
    }

    pub fn downgrade(&self) -> CtrlWeak<E> {
        CtrlWeak {
            weak: self.invoker.downgrade(),
        }
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

pub struct CtrlWeak<E: CtrlHandler> {
    weak: WeakInvoker<E>,
}

impl <E: CtrlHandler> CtrlWeak<E> {
    pub fn upgrade(&self) -> Option<CtrlInvoker<E>> {
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


