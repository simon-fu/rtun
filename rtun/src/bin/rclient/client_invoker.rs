

// use anyhow::Result;
// use rtun::{actor_service::{ActorEntity, AsyncHandler, Invoker}, channel::{ChId, ChPair}};



// pub trait ClientEntity: ActorEntity 
// + AsyncHandler<OpAddChannel, Response = Result<ChPair>>
// {

// }


// pub struct ClientInvoker<E: ClientEntity> {
//     invoker: Invoker<E>,
// }

// impl<E> ClientInvoker<E> 
// where
//     E: ClientEntity,
// {
//     pub fn new(invoker: Invoker<E>) -> Self {
//         Self {
//             invoker,
//         }
//     }

//     pub async fn add_channel(&self, ch_id: ChId) -> Result<ChPair> {
//         self.invoker.invoke(OpAddChannel(ch_id)).await?
//     }
// }

// pub struct OpAddChannel(pub ChId);


