use anyhow::{Result, anyhow, Context};

use bytes::Bytes;
use protobuf::Message;
use crate::{async_rt::spawn_with_name, huid::HUId, proto::{C2ARequest, c2arequest::C2a_req_args, make_open_shell_response_ok, make_open_shell_response_error, make_response_status_ok}, channel::{ChPair, ChId, ChSender}};

use super::{invoker_ctrl::{CtrlHandler, CtrlInvoker}, invoker_switch::{SwitchInvoker, SwitchHanlder}, next_ch_id::NextChId};


pub fn spawn_ctrl_service<H1: CtrlHandler, H2: SwitchHanlder>(
    uid: HUId, 
    agent: CtrlInvoker<H1>, 
    switch: SwitchInvoker<H2>, 
    chpair: ChPair) 
{
    spawn_with_name(format!("ctrl-service-{}", uid), async move {
        let r = ctrl_loop_full(&agent, &switch, chpair).await;
        tracing::debug!("finished with [{:?}]", r);
    });
}


async fn ctrl_loop_full<H1: CtrlHandler, H2: SwitchHanlder>(
    agent: &CtrlInvoker<H1>, 
    switch: &SwitchInvoker<H2>, 
    ctrl_pair: ChPair,
) -> Result<()> {
    
    let ctrl_tx = ctrl_pair.tx;
    let mut ctrl_rx = ctrl_pair.rx;
    let mux_tx = switch.get_mux_tx().await?;
    let mut next_ch_id = NextChId::default();

    loop {
        let packet = ctrl_rx.recv_packet().await?;

        let cmd = C2ARequest::parse_from_bytes(&packet.payload)?
        .c2a_req_args
        .with_context(||"no c2a_req_args")?;
        match cmd {
            C2a_req_args::OpenSell(args) => {
                let ch_id = args.ch_id
                .map(|x|ChId(x))
                .unwrap_or_else(||next_ch_id.next_ch_id());
  
                let r = {
                    let mux_tx = ChSender::new(ch_id, mux_tx.clone());
                    agent.open_shell(mux_tx, args).await
                };

                let (rsp, ch_tx) = match r {
                    Ok(v) => (make_open_shell_response_ok(ch_id.0), Some(v)),
                    Err(e) => (make_open_shell_response_error(e), None),
                };

                ctrl_tx.send_data(rsp.write_to_bytes()?.into()).await
                .map_err(|_x|anyhow!("send data fail"))?;

                if let Some(ch_tx) = ch_tx {
                    switch.add_channel(ch_id, ch_tx).await?;
                }
            },

            C2a_req_args::OpenSocks(args) => {
                let ch_id = args.ch_id
                .map(|x|ChId(x))
                .unwrap_or_else(||next_ch_id.next_ch_id());
  
                let r = {
                    let mux_tx = ChSender::new(ch_id, mux_tx.clone());
                    agent.open_socks(mux_tx, args).await
                };

                let (rsp, ch_tx) = match r {
                    Ok(v) => (make_open_shell_response_ok(ch_id.0), Some(v)),
                    Err(e) => (make_open_shell_response_error(e), None),
                };

                ctrl_tx.send_data(rsp.write_to_bytes()?.into()).await
                .map_err(|_x|anyhow!("send data fail"))?;

                if let Some(ch_tx) = ch_tx {
                    switch.add_channel(ch_id, ch_tx).await?;
                }
            }, 

            C2a_req_args::CloseChannel(args) => {
                // tracing::debug!("recv closing ch req {}", args);
                let _r = switch.remove_channel(ChId(args.ch_id)).await?;

                let rsp = make_response_status_ok();

                let data: Bytes = rsp.write_to_bytes()?.into();
                // tracing::debug!("send closing ch rsp len {}", data.len());

                ctrl_tx.send_data(data).await
                .map_err(|_x|anyhow!("send data fail"))?;
                // tracing::debug!("send closing ch ok");
            }
        }
    }
}


// async fn ctrl_loop_pure_ch<H1: CtrlHandler, H2: SwitchHanlder>(
//     agent: &CtrlInvoker<H1>, 
//     switch: &SwitchInvoker<H2>, 
//     ctrl_pair: ChPair,
// ) -> Result<()> {
    
//     let tx = ctrl_pair.tx;
//     let mut rx = ctrl_pair.rx;
//     let mut next_ch_id = NextChId::default();

//     let mux_tx = switch.get_mux_tx().await?;

//     loop {
//         let r = rx.recv_data().await;
//         let packet = match r {
//             Some(v) => v,
//             None => break,
//         };

//         let req = OpenChannelRequest::parse_from_bytes(&packet.payload)?;
//         let ch_id = match req.open_ch_req {
//             Some(v) => {
//                 match v {
//                     Open_ch_req::ChId(v) => ChId(v),
//                 }
//             },
//             None => {
//                 next_ch_id.next_ch_id()
//             },
//         };

//         let r = {
//             let mux_tx = ChSender::new(ch_id, mux_tx.clone());
//             agent.open_channel(mux_tx).await
//         };

//         let (rsp, ch_tx) = match r {
//             Ok(v) => (make_open_channel_response_ok(ch_id.0), Some(v)),
//             Err(e) => (make_open_channel_response_error(e), None),
//         };

//         tx.send_data(rsp.write_to_bytes()?.into()).await
//         .map_err(|_x|anyhow!("send data fail"))?;

//         if let Some(ch_tx) = ch_tx {
//             switch.add_channel(ch_id, ch_tx).await?;
//         }
        
//     }
//     Ok(())
// }



