use std::collections::HashMap;

use anyhow::{Result, bail};

use bytes::Bytes;
use rtun::{actor_service::{ActorEntity, start_actor, handle_first_none, Action, AsyncHandler, ActorHandle}, huid::HUId, channel::{ChId, ChSender, ChReceiver, ChData, ChPair, CHANNEL_SIZE}};
use tokio::sync::mpsc;

use rtun::swtich::{SwitchInvoker, AgentEntity, OpRemoveChannel, OpAddChannel};


pub struct LocalBridge {
    handle: ActorHandle<Entity>,
}

impl  LocalBridge { 
    pub fn clone_invoker(&self) -> SwitchInvoker<Entity> {
        SwitchInvoker::new(self.handle.invoker().clone())
    }

    pub async fn wait_for_completed(&mut self) -> Result<()> {
        self.handle.wait_for_completed().await?;
        Ok(())
    }
}


pub async fn make_local_bridge(uid: HUId) -> Result<LocalBridge> {

    let entity = Entity {
        // socket,
        // invoker: None,
        gen_ch_id: ChId(0),
        client_side: Side::new(),
        agent_side: Side::new(),
    };

    let handle = start_actor(
        format!("local-{}", uid),
        entity, 
        handle_first_none,
        wait_next, 
        handle_next, 
        handle_msg,
    );

    Ok(LocalBridge {
        handle,
    })
}


#[async_trait::async_trait]
impl AsyncHandler<OpAddChannel> for Entity {
    type Response = Result<ChPair>; 

    async fn handle(&mut self, req: OpAddChannel) -> Self::Response { 
        match req.0 {
            Some(ch_id) => {
                Ok(self.client_side.add_channel(ch_id))
            },
            None => {
                let ch_id = self.next_ch_id();

                let (tx, rx) = mpsc::channel(CHANNEL_SIZE);
                self.agent_side.channels.insert(ch_id, ChannelItem { tx });
                tracing::debug!("add channel {ch_id:?}");
                Ok(ChPair {
                    tx: ChSender::new(ch_id, self.agent_side.outgoing_tx.clone()),
                    rx: ChReceiver::new(rx),
                })
            }
        }
    }
}


#[async_trait::async_trait]
impl AsyncHandler<OpRemoveChannel> for Entity {
    type Response = Result<bool>; 

    async fn handle(&mut self, req: OpRemoveChannel) -> Self::Response {
        let ch_id = req.0;
        let exist = self.agent_side.channels.remove(&ch_id).is_some();
        tracing::debug!("remove channel {ch_id:?} {exist}");
        Ok(exist)
    }
}

impl AgentEntity for Entity {}


type Next = Result<NextPacket>;

pub enum NextPacket {
    ClientChData(ChData),
    AgentChData(ChData),
}

#[inline]
async fn wait_next(entity: &mut Entity) -> Next {
    tokio::select! {        
        r = entity.client_side.outgoing_rx.recv() => {
            match r {
                Some(d) => Ok(NextPacket::ClientChData(d)),
                None => bail!("no one care when wait next"),
            }
        }

        r = entity.agent_side.outgoing_rx.recv() => {
            match r {
                Some(d) => Ok(NextPacket::AgentChData(d)),
                None => bail!("no one care when wait next"),
            }
        }
    }
}

async fn handle_next(entity: &mut Entity, next: Next) -> Result<Action> {
    let next = next?;
    match next {
        NextPacket::AgentChData(packet) => {
            let ch_id = packet.ch_id;
            if let Some(item) = entity.client_side.channels.get(&ch_id) {
                let _r = item.tx.send(packet.payload).await; 
            }
        },
        NextPacket::ClientChData(packet) => {
            let ch_id = packet.ch_id;
            if let Some(item) = entity.agent_side.channels.get(&ch_id) {
                let _r = item.tx.send(packet.payload).await; 
            }
        },
    }

    Ok(Action::None)
}



struct ChannelItem {
    tx: mpsc::Sender<Bytes>,
}

async fn handle_msg(_entity: &mut Entity, _msg: Msg) -> Result<Action> {
    Ok(Action::None)
}

struct Side {
    channels: HashMap<ChId, ChannelItem>,
    outgoing_tx: mpsc::Sender<ChData>,
    outgoing_rx: mpsc::Receiver<ChData>,
}

impl Side {
    fn new() -> Self {
        let (outgoing_tx, outgoing_rx) = mpsc::channel(CHANNEL_SIZE);
        Self {
            channels: Default::default(),
            outgoing_tx,
            outgoing_rx,
        }
    }

    fn add_channel(&mut self, ch_id: ChId) -> ChPair {
        let (tx, rx) = mpsc::channel(CHANNEL_SIZE);
        self.channels.insert(ch_id, ChannelItem { tx });
        
        ChPair {
            tx: ChSender::new(ch_id, self.outgoing_tx.clone()),
            rx: ChReceiver::new(rx),
        }
    }
}

pub struct Entity {
    // socket: WebSocket,
    // invoker: Option<WeakInvoker<Self>>,

    gen_ch_id: ChId,

    client_side: Side,
    agent_side: Side,
}

impl Entity {
    // fn invoker(&self) -> Result<Invoker<Self>> {
    //     self.invoker.as_ref().
    //     with_context(||"no invoker")?
    //     .upgrade()
    //     .with_context(||"invoker gone")
    // }

    fn next_ch_id(&mut self) -> ChId {
        let ch_id = self.gen_ch_id;
        self.gen_ch_id.0 += 1;
        ch_id
    }
}

pub enum Msg {

}

impl ActorEntity for Entity {
    type Next = Next;

    type Msg = Msg;

    type Result = ();

    fn into_result(self, _r: Result<()>) -> Self::Result {
        ()
    }
}


