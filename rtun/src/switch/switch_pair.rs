use anyhow::Result;
use crate::huid::HUId;

use super::{switch_source::{PacketSource, make_switch_source, SwitchSource, SwitchSourceInvoker, SwitchSourceCtrlClient, SwitchSourceEntity}, switch_sink::{PacketSink, SwitchSink, make_switch_sink}};


pub async fn make_switch_pair<S1, S2>(uid: HUId, (sink, source): (S1,S2)) -> Result<SwitchPair<S1, S2>> 
where
    S1: PacketSink,
    S2: PacketSource,
{
    let sink =  make_switch_sink(uid, sink).await?;
    let outgoing_tx = sink.get_mux_tx().await?;

    Ok(SwitchPair {
        sink,
        source: make_switch_source(uid, source, outgoing_tx).await?,
    })
}

pub struct SwitchPair<S1, S2> 
where
    S1: PacketSink,
    S2: PacketSource,
{
    sink: SwitchSink<S1>,
    source: SwitchSource<S2>,
}


impl<S1, S2>  SwitchPair<S1, S2>
where
    S1: PacketSink,
    S2: PacketSource,
{
    pub fn clone_invoker(&self) -> SwitchPairInvoker<S2> {
        self.source.clone_invoker()
    }

    pub async fn shutdown_and_waitfor(&mut self) -> Result<()> {
        let r1 = self.source.shutdown_and_waitfor().await;
        let r2 = self.sink.shutdown_and_waitfor().await;
        r1?;
        r2?;
        Ok(())
    }

    pub async fn wait_for_completed(&mut self) -> Result<Option<()>> {
        self.source.wait_for_completed().await?;
        self.sink.wait_for_completed().await?;
        Ok(None)
    }
}


pub type SwitchPairInvoker<S> = SwitchSourceInvoker<S>;
pub type SwitchPairCtrlClient<S> = SwitchSourceCtrlClient<S>;
pub type SwitchPairEntity<S> = SwitchSourceEntity<S>;