

use anyhow::Result;

use crate::{actor_service::{ActorEntity, start_actor, handle_first_none, AsyncHandler, ActorHandle, wait_next_none, handle_next_none, handle_msg_none}, huid::HUId, channel::{ChSender, CHANNEL_SIZE, ChReceiver}, async_rt::spawn_with_name, switch::{invoker_ctrl::{OpOpenShell, OpOpenShellResult, OpOpenSocks, OpOpenSocksResult}, next_ch_id::NextChId, agent::ch_socks::ChSocks}};
use tokio::sync::mpsc;

use super::super::invoker_ctrl::{OpOpenChannel, CloseChannelResult, OpenChannelResult, OpCloseChannel, CtrlHandler, CtrlInvoker};
use super::ch_service::channel_service;
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
    };

    let handle = start_actor(
        format!("agent-ctrl-{}", uid),
        entity, 
        handle_first_none,
        wait_next_none, 
        handle_next_none, 
        handle_msg_none,
    );

    Ok(AgentCtrl {
        handle,
    })
}


#[async_trait::async_trait]
impl AsyncHandler<OpOpenChannel> for Entity {
    type Response = OpenChannelResult; 

    async fn handle(&mut self, req: OpOpenChannel) -> Self::Response {
        let ch_id = self.next_ch_id.next_ch_id();
        let ch_tx = req.0;

        let (mux_tx, mux_rx) = mpsc::channel(CHANNEL_SIZE);
        
        tracing::debug!("open channel {ch_id:?} -> {:?}", ch_tx.ch_id());

        let name = format!("{}-ch-{}->{}", self.uid, ch_id, ch_tx.ch_id());

        spawn_with_name(name,  async move {
            let r = channel_service(ch_tx, ChReceiver::new(mux_rx) ).await;
            tracing::debug!("finished with {:?}", r)
        });
        
        Ok(ChSender::new(ch_id, mux_tx))
    }
}

#[async_trait::async_trait]
impl AsyncHandler<OpCloseChannel> for Entity {
    type Response = CloseChannelResult; 

    async fn handle(&mut self, _req: OpCloseChannel) -> Self::Response {
        Ok(true)
    }
}

#[async_trait::async_trait]
impl AsyncHandler<OpOpenShell> for Entity {
    type Response = OpOpenShellResult; 

    async fn handle(&mut self, req: OpOpenShell) -> Self::Response {

        let shell = open_shell(req.1).await?;

        let ch_id = self.next_ch_id.next_ch_id();
        let ch_tx = req.0;

        let (mux_tx, mux_rx) = mpsc::channel(CHANNEL_SIZE);
        
        tracing::debug!("open shell {ch_id:?} -> {:?}", ch_tx.ch_id());

        let name = format!("local-{}-{}->{}", self.uid, ch_id, ch_tx.ch_id());

        shell.spawn(Some(name), ch_tx, ChReceiver::new(mux_rx));

        Ok(ChSender::new(ch_id, mux_tx))

    }
}

#[async_trait::async_trait]
impl AsyncHandler<OpOpenSocks> for Entity {
    type Response = OpOpenSocksResult; 

    async fn handle(&mut self, req: OpOpenSocks) -> Self::Response {

        let ch_id = self.next_ch_id.next_ch_id();
        let ch_tx = req.0;

        let (mux_tx, mux_rx) = mpsc::channel(CHANNEL_SIZE);
        
        tracing::debug!("open socks {ch_id:?} -> {:?}", ch_tx.ch_id());

        let name = format!("socks-{}-{}->{}", self.uid, ch_id, ch_tx.ch_id());

        ChSocks::new(ch_tx, ChReceiver::new(mux_rx))
        .spawn(self.socks_server.clone(), name, req.1).await?;

        Ok(ChSender::new(ch_id, mux_tx))
    }
}


impl CtrlHandler for Entity {}


pub struct Entity {
    uid: HUId,
    next_ch_id: NextChId,
    socks_server: super::ch_socks::Server,
}


impl ActorEntity for Entity {
    type Next = ();

    type Msg = ();

    type Result = ();

    fn into_result(self, _r: Result<()>) -> Self::Result {
        ()
    }
}


