

use anyhow::Result;
use rtun::{actor_service::{ActorEntity, AsyncHandler, Invoker}, channel::{ChId, ChSender, ChReceiver}};



pub trait AgentEntity: ActorEntity 
+ AsyncHandler<OpAddChannel, Response = Result<(ChSender, ChReceiver)>>
{

}


pub struct AgentInvoker<E: AgentEntity> {
    invoker: Invoker<E>,
}

impl<E> AgentInvoker<E> 
where
    E: AgentEntity,
{
    pub fn new(invoker: Invoker<E>) -> Self {
        Self {
            invoker,
        }
    }

    pub async fn add_channel(&self, ch_id: ChId) -> Result<(ChSender, ChReceiver)> {
        self.invoker.invoke(OpAddChannel(ch_id)).await?
    }
}

pub struct OpAddChannel(pub ChId);


