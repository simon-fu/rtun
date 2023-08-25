

use anyhow::{Result, anyhow, bail, Context};
use protobuf::Message;

use crate::{actor_service::{ActorEntity, start_actor, handle_first_none, AsyncHandler, ActorHandle, wait_next_none, handle_next_none, handle_msg_none}, huid::HUId, channel::{ChId, ChPair}, proto::{OpenChannelRequest, OpenChannelResponse, open_channel_response::Open_ch_rsp, OpenShellArgs, C2ARequest, c2arequest::C2a_req_args}};

use super::{invoker_ctrl::{OpOpenChannel, CloseChannelResult, OpenChannelResult, OpCloseChannel, CtrlHandler, CtrlInvoker, OpOpenShell, OpOpenShellResult}, invoker_switch::{SwitchInvoker, SwitchHanlder}, next_ch_id::NextChId};


pub struct CtrlClient<H: SwitchHanlder> {
    handle: ActorHandle<Entity<H>>,
}

impl<H: SwitchHanlder>  CtrlClient<H> { 

    pub fn clone_invoker(&self) -> CtrlInvoker<Entity<H>> {
        CtrlInvoker::new(self.handle.invoker().clone())
    }

    pub async fn shutdown(&self) {
        self.handle.invoker().shutdown().await;
    }

    pub async fn wait_for_completed(&mut self) -> Result<()> {
        self.handle.wait_for_completed().await?;
        Ok(())
    }

    pub async fn shutdown_and_waitfor(&mut self) -> Result<()> {
        self.handle.invoker().shutdown().await;
        self.handle.wait_for_completed().await?;
        Ok(())
    }
}


pub fn make_ctrl_client<H: SwitchHanlder>(uid: HUId, pair: ChPair, switch: SwitchInvoker<H>) -> Result<CtrlClient<H>> {

    // let mux_tx = switch.get_mux_tx().await?;

    let entity = Entity {
        // gen_ch_id: ChId(0),
        // uid,
        pair,
        switch,
        // mux_tx,
        next_ch_id: Default::default(),
    };

    let handle = start_actor(
        format!("ctrl-client-{}", uid),
        entity, 
        handle_first_none,
        wait_next_none, 
        handle_next_none, 
        handle_msg_none,
    );

    Ok(CtrlClient {
        handle,
    })
}


#[async_trait::async_trait]
impl<H: SwitchHanlder> AsyncHandler<OpOpenChannel> for Entity<H> {
    type Response = OpenChannelResult; 

    async fn handle(&mut self, req: OpOpenChannel) -> Self::Response {
        let data = OpenChannelRequest {
            open_ch_req: None,
            ..Default::default()
        }.write_to_bytes()?;

        self.pair.tx.send_data(data.into()).await.map_err(|_e|anyhow!("send failed"))?;

        let packet = self.pair.rx.recv_data().await.with_context(||"recv failed")?;

        let rsp = OpenChannelResponse::parse_from_bytes(&packet.payload)
        .with_context(||"parse response failed")?
        .open_ch_rsp.with_context(||"has no response")?;
        
        let ch_id = match rsp {
            Open_ch_rsp::ChId(v) => ChId(v),
            Open_ch_rsp::Status(status) => bail!("response status {:?}", status),
            // _ => bail!("unknown"),
        };

        let tx = self.switch.add_channel(ch_id, req.0).await?;
        
        Ok(tx)
    }
}

#[async_trait::async_trait]
impl<H: SwitchHanlder> AsyncHandler<OpCloseChannel> for Entity<H> {
    type Response = CloseChannelResult; 

    async fn handle(&mut self, req: OpCloseChannel) -> Self::Response { 
        // TODO: call remote 
        let r = self.switch.remove_channel(req.0).await?;
        Ok(r)
    }
}

#[async_trait::async_trait]
impl<H: SwitchHanlder> AsyncHandler<OpOpenShell> for Entity<H> {
    type Response = OpOpenShellResult; 

    async fn handle(&mut self, mut req: OpOpenShell) -> Self::Response {

        let req_ch_id = self.next_ch_id.next_ch_id();
        req.1.ch_id = Some(req_ch_id.0);

        let tx = self.switch.add_channel(req_ch_id, req.0).await?;

        let r = c2a_open_shell(&mut self.pair, req.1).await;
        match r {
            Ok(v) => {
                assert_eq!(v, req_ch_id);
                Ok(tx)
            }
            Err(e) => {
                let _r = self.switch.remove_channel(req_ch_id).await;
                Err(e)
            }
        }
    }
}


impl<H: SwitchHanlder> CtrlHandler for Entity<H> {}



pub struct Entity<H: SwitchHanlder> {
    // uid: HUId,
    // gen_ch_id: ChId,
    pair: ChPair,
    switch: SwitchInvoker<H>,
    // mux_tx: ChTx,
    next_ch_id: NextChId,
}

// impl<H: SwitchHanlder> Entity<H> {
//     fn next_ch_id(&mut self) -> ChId {
//         let ch_id = self.gen_ch_id;
//         self.gen_ch_id.0 += 1;
//         ch_id
//     }
// }


impl<H: SwitchHanlder> ActorEntity for Entity<H> {
    type Next = ();

    type Msg = ();

    type Result = ();

    fn into_result(self, _r: Result<()>) -> Self::Result {
        ()
    }
}


pub async fn c2a_open_shell(pair: &mut ChPair, args: OpenShellArgs) -> Result<ChId> {
    let data = C2ARequest {
        c2a_req_args: Some(C2a_req_args::OpenSell(args)),
        ..Default::default()
    }.write_to_bytes()?;

    pair.tx.send_data(data.into()).await.map_err(|_e|anyhow!("send open shell failed"))?;

    let packet = pair.rx.recv_data().await.with_context(||"recv open shell response failed")?;

    // let rsp = C2AResponse::parse_from_bytes(&data).with_context(||"parse open shell response failed")?;
    let rsp = OpenChannelResponse::parse_from_bytes(&packet.payload)
    .with_context(||"parse open shell response failed")?
    .open_ch_rsp.with_context(||"has no response")?;

    let shell_ch_id = match rsp {
        Open_ch_rsp::ChId(v) => ChId(v),
        Open_ch_rsp::Status(status) => bail!("open shell response status {:?}", status),
        // _ => bail!("unknown"),
    };

    // let pair = self.invoker.add_channel(shell_ch_id).await?;
    Ok(shell_ch_id)
}
