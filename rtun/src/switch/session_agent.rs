
use anyhow::Result;
use crate::{huid::gen_huid::gen_huid, channel::{ChId, ChPair}};

use super::{switch_stream::{PacketStream, make_stream_switch, StreamSwitch}, agent::ctrl::{make_agent_ctrl, AgentCtrl}, ctrl_service::spawn_ctrl_service};


pub async fn make_agent_session<S: PacketStream>(stream: S) -> Result<AgentSession<S>> {
    let uid = gen_huid();
    let switch_session = make_stream_switch(uid, stream).await?;
    let switch = switch_session.clone_invoker();

    let ctrl_session = make_agent_ctrl(uid).await?;
    let ctrl = ctrl_session.clone_ctrl();
    
    let ctrl_ch_id = ChId(0);
    let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;
    let pair = ChPair { tx: ctrl_tx, rx: ctrl_rx };

    spawn_ctrl_service(uid, ctrl, switch, pair);
    
    Ok(AgentSession{switch_session, ctrl_session})

    // let uid = gen_huid();
    
    // let switch_session = make_stream_switch(uid, stream).await?;
    // let switch = switch_session.clone_invoker();

    // let ctrl_ch_id = ChId(0);

    // let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    // let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;
    
    // // let pair = ChPair { tx: ctrl_tx, rx: ctrl_rx };

    // let ctrl_session = make_agent_ctrl(uid).await?;

    // Ok(AgentSession{switch_session, ctrl_session})
}


pub struct AgentSession<S: PacketStream> {
    switch_session: StreamSwitch<S>,
    ctrl_session: AgentCtrl,
}

impl<S: PacketStream> AgentSession<S> {
    pub fn switch(&self) -> &StreamSwitch<S> {
        &self.switch_session
    }

    pub fn ctrl_client(&self) -> &AgentCtrl {
        &self.ctrl_session
    }

    pub async fn wait_for_completed(&mut self) -> Result<()> {
        
        let switch_r = self.switch_session.wait_for_completed().await;
        tracing::debug!("switch session finished {switch_r:?}");

        self.ctrl_session.shutdown().await;
        
        let ctrl_r = self.ctrl_session.wait_for_completed().await;
        tracing::debug!("ctrl session finished {ctrl_r:?}");
        
        switch_r?;
        ctrl_r?;

        Ok(())
    }
}

