

use std::collections::HashMap;

use anyhow::Result;

use crate::{actor_service::{ActorEntity, start_actor, handle_first_none, AsyncHandler, ActorHandle, wait_next_none, handle_next_none, handle_msg_none}, huid::HUId, channel::{ChSender, CHANNEL_SIZE, ChReceiver, ChId, ChSenderWeak}, async_rt::spawn_with_name, switch::{invoker_ctrl::{OpOpenShell, OpOpenShellResult, OpOpenSocks, OpOpenSocksResult, CtrlWeak}, next_ch_id::NextChId, agent::ch_socks::ChSocks, entity_watch::{CtrlGuard, OpWatch, WatchResult}}};
use tokio::sync::mpsc;

use super::super::invoker_ctrl::{CloseChannelResult, OpCloseChannel, CtrlHandler, CtrlInvoker};

use super::ch_shell::open_shell;

pub type AgentCtrlInvoker = CtrlInvoker<Entity>;

pub struct AgentCtrl {
    handle: ActorHandle<Entity>,
}

impl  AgentCtrl { 

    pub fn clone_ctrl(&self) -> AgentCtrlInvoker {
        CtrlInvoker::new(self.handle.invoker().clone())
    }

    pub async fn shutdown(&self) {
        self.handle.invoker().shutdown().await;
    }

    pub async fn wait_for_completed(&mut self) -> Result<()> {
        self.handle.wait_for_completed().await?;
        Ok(())
    }
}


pub async fn make_agent_ctrl(uid: HUId) -> Result<AgentCtrl> {

    let entity = Entity {
        next_ch_id: Default::default(),
        uid,
        socks_server: super::ch_socks::Server::try_new("127.0.0.1:1080").await?,
        channels: Default::default(),
        weak: None,
        guard: CtrlGuard::new(),
    };

    let handle = start_actor(
        format!("agent-ctrl-{}", uid),
        entity, 
        handle_first_none,
        wait_next_none, 
        handle_next_none, 
        handle_msg_none,
    );
    
    let session = AgentCtrl {
        handle,
    };
    
    let weak = session.clone_ctrl().downgrade();

    session.handle.invoker().invoke(SetWeak(weak)).await?;

    Ok(session)
}


// #[async_trait::async_trait]
// impl AsyncHandler<OpOpenChannel> for Entity {
//     type Response = OpenChannelResult; 

//     async fn handle(&mut self, req: OpOpenChannel) -> Self::Response {
//         let local_ch_id = self.next_ch_id.next_ch_id();
//         let peer_tx = req.0;

//         // let (mux_tx, mux_rx) = mpsc::channel(CHANNEL_SIZE);
//         let (local_tx, local_rx) = self.add_channel(local_ch_id, &peer_tx);
        
//         tracing::debug!("open channel {local_ch_id:?} -> {:?}", peer_tx.ch_id());

//         let name = format!("{}-ch-{}->{}", self.uid, local_ch_id, peer_tx.ch_id());

//         {
//             let weak = self.weak.clone();
//             spawn_with_name(name,  async move {
//                 let r = channel_service(peer_tx, local_rx ).await;
//                 tracing::debug!("finished with {:?}", r);

//                 if let Some(weak) = weak {
//                     if let Some(ctrl) = weak.upgrade() {
//                         let _r = ctrl.close_channel(local_ch_id).await;
//                     }
//                 }
//             });
//         }
        
//         Ok(local_tx)
//     }
// }

#[async_trait::async_trait]
impl AsyncHandler<OpCloseChannel> for Entity {
    type Response = CloseChannelResult; 

    async fn handle(&mut self, req: OpCloseChannel) -> Self::Response {
        Ok(self.remove_channel(req.0))
    }
}

#[async_trait::async_trait]
impl AsyncHandler<OpOpenShell> for Entity {
    type Response = OpOpenShellResult; 

    async fn handle(&mut self, req: OpOpenShell) -> Self::Response {

        let exec = open_shell(req.1).await?;

        let peer_tx = req.0;
        let local_ch_id = self.next_ch_id.next_ch_id();

        let (local_tx, local_rx) = self.add_channel(local_ch_id, &peer_tx);
        
        tracing::debug!("open shell {local_ch_id:?} -> {:?}", peer_tx.ch_id());

        {
            let name = format!("local-{}-{}->{}", self.uid, local_ch_id, peer_tx.ch_id());
            let weak = self.weak.clone();

            spawn_with_name(name, async move {
    
                let r = exec.run(peer_tx, local_rx).await;
                tracing::debug!("finished with [{:?}]", r);
    
                if let Some(weak) = weak {
                    if let Some(ctrl) = weak.upgrade() {
                        let _r = ctrl.close_channel(local_ch_id).await;
                    }
                }
    
            });
        }

        // shell.spawn(Some(name), peer_tx, local_rx, self.weak.clone(), local_ch_id);

        Ok(local_tx)

    }
}


#[async_trait::async_trait]
impl AsyncHandler<OpOpenSocks> for Entity {
    type Response = OpOpenSocksResult; 

    async fn handle(&mut self, req: OpOpenSocks) -> Self::Response {
        
        let exec = ChSocks::try_new(req.1)?;

        let peer_tx = req.0;
        let local_ch_id = self.next_ch_id.next_ch_id();

        let (local_tx, local_rx) = self.add_channel(local_ch_id, &peer_tx);

        // let (mux_tx, mux_rx) = mpsc::channel(CHANNEL_SIZE);
        
        tracing::debug!("open socks {local_ch_id:?} -> {:?}", peer_tx.ch_id());

        let name = format!("socks-{}-{}->{}", self.uid, local_ch_id, peer_tx.ch_id());
        let weak = self.weak.clone();
        let server = self.socks_server.clone();

        spawn_with_name(name, async move {

            let r = exec.run(server, peer_tx, local_rx).await;
            tracing::debug!("finished with [{:?}]", r);

            if let Some(weak) = weak {
                if let Some(ctrl) = weak.upgrade() {
                    let _r = ctrl.close_channel(local_ch_id).await;
                }
            }

        });

        Ok(local_tx)

    }
}

#[async_trait::async_trait]
impl AsyncHandler<OpWatch> for Entity {
    type Response = WatchResult; 

    async fn handle(&mut self, _req: OpWatch) -> Self::Response {
        Ok(self.guard.watch())
    }
}

impl CtrlHandler for Entity {}

#[async_trait::async_trait]
impl AsyncHandler<SetWeak> for Entity {
    type Response = (); 

    async fn handle(&mut self, req: SetWeak) -> Self::Response {
        self.weak = Some(req.0);
    }
}

struct SetWeak(CtrlWeak<Entity>);


pub struct Entity {
    uid: HUId,
    next_ch_id: NextChId,
    socks_server: super::ch_socks::Server,
    channels: HashMap<ChId, ChItem>,
    weak: Option<CtrlWeak<Self>>,
    guard: CtrlGuard,
}

impl Entity {
    fn add_channel(&mut self, ch_id: ChId, peer_tx: &ChSender) -> (ChSender, ChReceiver) {

        let (local_tx, local_rx) = mpsc::channel(CHANNEL_SIZE);
        let local_tx = ChSender::new(ch_id, local_tx);
        let local_rx = ChReceiver::new(local_rx);
        
        self.channels.insert(ch_id, ChItem {
            peer_tx: peer_tx.downgrade(),
            local_tx: local_tx.downgrade(),
        });

        ( local_tx, local_rx, )
    }

    fn remove_channel(&mut self, ch_id: ChId) -> bool {
        self.channels.remove(&ch_id).is_some()
    }
}


impl ActorEntity for Entity {
    type Next = ();

    type Msg = ();

    type Result = ();

    fn into_result(self, _r: Result<()>) -> Self::Result {
        ()
    }
}

struct ChItem {
    peer_tx: ChSenderWeak,
    local_tx: ChSenderWeak,
}

impl Drop for ChItem {
    fn drop(&mut self) {
        if let Some(tx) = self.local_tx.upgrade() {
            let _r = tx.try_send_zero();
        }

        if let Some(tx) = self.peer_tx.upgrade() {
            let _r = tx.try_send_zero();
        }
    }
}

