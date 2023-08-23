

use anyhow::Result;
use crate::{channel::{ChId, ChPair}, actor_service::{ActorEntity, AsyncHandler, Invoker, WeakInvoker}};

pub trait AgentEntity: ActorEntity 
+ AsyncHandler<OpAddChannel, Response = Result<ChPair>>
// + AsyncHandler<OpAddChannel, Response = Result<ChPair>>
+ AsyncHandler<OpRemoveChannel, Response = Result<bool>>
{

}

#[derive(Clone)]
pub struct SwitchInvoker<E: AgentEntity> {
    invoker: Invoker<E>,
}

impl<E> SwitchInvoker<E> 
where
    E: AgentEntity,
{
    pub fn new(invoker: Invoker<E>) -> Self {
        Self {
            invoker,
        }
    }

    pub fn downgrade(&self) -> AgentWeakInvoker<E> {
        AgentWeakInvoker {
            weak: self.invoker.downgrade(),
        }
    }

    pub async fn alloc_channel(&self) -> Result<ChPair> {
        self.invoker.invoke(OpAddChannel(None)).await?
    }

    pub async fn add_channel(&self, ch_id: ChId) -> Result<ChPair> {
        self.invoker.invoke(OpAddChannel(Some(ch_id))).await?
    }

    // pub async fn add_channel(&self, ch_id: ChId) -> Result<ChPair> {
    //     self.invoker.invoke(OpAddChannel(ch_id)).await?
    // }

    pub async fn remote_channel(&self, ch_id: ChId) -> Result<bool> {
        self.invoker.invoke(OpRemoveChannel(ch_id)).await?
    }
}

pub struct AgentWeakInvoker<E: AgentEntity> {
    weak: WeakInvoker<E>,
}

impl <E: AgentEntity> AgentWeakInvoker<E> {
    pub fn upgrade(&self) -> Option<SwitchInvoker<E>> {
        self.weak.upgrade().map(|invoker| SwitchInvoker {
            invoker,
        })
    }
}


// pub struct OpAllocChannel;

#[derive(Debug)]
pub struct OpAddChannel(pub Option<ChId>);

#[derive(Debug)]
pub struct OpRemoveChannel(pub ChId);


