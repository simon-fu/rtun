

// use anyhow::Result;
// use rtun::{actor_service::{ActorEntity, AsyncHandler, Invoker, WeakInvoker}, channel::{ChId, ChPair}};



// pub trait AgentEntity: ActorEntity 
// + AsyncHandler<OpAddChannel, Response = Result<ChPair>>
// // + AsyncHandler<OpAddChannel, Response = Result<ChPair>>
// + AsyncHandler<OpRemoveChannel, Response = Result<bool>>
// {

// }

// #[derive(Clone)]
// pub struct AgentInvoker<E: AgentEntity> {
//     invoker: Invoker<E>,
// }

// impl<E> AgentInvoker<E> 
// where
//     E: AgentEntity,
// {
//     pub fn new(invoker: Invoker<E>) -> Self {
//         Self {
//             invoker,
//         }
//     }

//     pub fn downgrade(&self) -> AgentWeakInvoker<E> {
//         AgentWeakInvoker {
//             weak: self.invoker.downgrade(),
//         }
//     }

//     pub async fn alloc_channel(&self) -> Result<ChPair> {
//         self.invoker.invoke(OpAddChannel(None)).await?
//     }

//     pub async fn add_channel(&self, ch_id: ChId) -> Result<ChPair> {
//         self.invoker.invoke(OpAddChannel(Some(ch_id))).await?
//     }

//     // pub async fn add_channel(&self, ch_id: ChId) -> Result<ChPair> {
//     //     self.invoker.invoke(OpAddChannel(ch_id)).await?
//     // }

//     pub async fn remove_channel(&self, ch_id: ChId) -> Result<bool> {
//         self.invoker.invoke(OpRemoveChannel(ch_id)).await?
//     }
// }

// pub struct AgentWeakInvoker<E: AgentEntity> {
//     weak: WeakInvoker<E>,
// }

// impl <E: AgentEntity> AgentWeakInvoker<E> {
//     pub fn upgrade(&self) -> Option<AgentInvoker<E>> {
//         self.weak.upgrade().map(|invoker| AgentInvoker {
//             invoker,
//         })
//     }
// }


// // pub struct OpAllocChannel;

// #[derive(Debug)]
// pub struct OpAddChannel(pub Option<ChId>);

// #[derive(Debug)]
// pub struct OpRemoveChannel(pub ChId);

