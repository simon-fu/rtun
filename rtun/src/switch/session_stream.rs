
use anyhow::Result;
use crate::{huid::gen_huid::gen_huid, channel::{ChId, ChPair}};

use super::{switch_stream::{PacketStream, make_stream_switch, StreamSwitch, StreamSwitchCtrlClient}, ctrl_client::make_ctrl_client};


pub async fn make_stream_session<S: PacketStream>(stream: S) -> Result<StreamSession<S>> {
    let uid = gen_huid();
            
    let switch_session = make_stream_switch(uid, stream).await?;
    let switch = switch_session.clone_invoker();

    let ctrl_ch_id = ChId(0);

    let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;
    
    let pair = ChPair { tx: ctrl_tx, rx: ctrl_rx };

    let ctrl_session = make_ctrl_client(uid, pair, switch).await?;

    Ok(StreamSession{switch_session, ctrl_session})
}


pub struct StreamSession<S: PacketStream> {
    switch_session: StreamSwitch<S>,
    ctrl_session: StreamSwitchCtrlClient<S>,
}

impl<S: PacketStream> StreamSession<S> {
    pub fn switch(&self) -> &StreamSwitch<S> {
        &self.switch_session
    }

    pub fn ctrl_client(&self) -> &StreamSwitchCtrlClient<S> {
        &self.ctrl_session
    }

    pub async fn wait_for_completed(&mut self) -> Result<()> {
        
        let switch_r = self.switch_session.wait_for_completed().await;
        let ctrl_r = self.ctrl_session.wait_for_completed().await;

        switch_r?;
        ctrl_r?;

        Ok(())
    }
}

