use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::{
    async_rt::spawn_with_name,
    channel::{ChId, ChPair, ChSender},
    huid::HUId,
    proto::{
        c2arequest::C2a_req_args, make_open_p2p_response_error, make_open_shell_response_error,
        make_open_shell_response_ok, make_response_status_ok, C2ARequest, KickDownArgs, Pong,
    },
};
use bytes::Bytes;
use protobuf::Message;
use tokio::task::JoinHandle;

use super::{
    invoker_ctrl::{CtrlHandler, CtrlInvoker},
    invoker_switch::{SwitchHanlder, SwitchInvoker},
    next_ch_id::NextChId,
};

pub fn spawn_ctrl_service<H1: CtrlHandler, H2: SwitchHanlder>(
    uid: HUId,
    agent: CtrlInvoker<H1>,
    switch: SwitchInvoker<H2>,
    mut chpair: ChPair,
) -> JoinHandle<Result<ExitReason>> {
    let task = spawn_with_name(format!("ctrl-service-{}", uid), async move {
        let r = ctrl_loop_full(&agent, &switch, &mut chpair).await;
        tracing::debug!("finished with [{:?}]", r);
        switch.shutdown().await;
        r
    });
    task
}

#[derive(Debug)]
pub enum ExitReason {
    KickDown(KickDownArgs),
}

async fn ctrl_loop_full<H1: CtrlHandler, H2: SwitchHanlder>(
    agent: &CtrlInvoker<H1>,
    switch: &SwitchInvoker<H2>,
    ctrl_pair: &mut ChPair,
) -> Result<ExitReason> {
    let ctrl_tx = &mut ctrl_pair.tx;
    let ctrl_rx = &mut ctrl_pair.rx;

    let mux_tx = switch.get_mux_tx().await?;
    let mut agent_watch = agent.watch().await?;

    let mut next_ch_id = NextChId::default();
    let alive_timeout = Duration::from_secs(60);

    loop {
        // let packet = ctrl_rx.recv_packet().await?;

        let packet = tokio::select! {
            _r = agent_watch.watch() => bail!("agent has gone"),
            r = ctrl_rx.recv_packet() => r?,
            _r = tokio::time::sleep(alive_timeout) => {
                bail!("wait for ping timeout")
            }
        };

        let cmd = C2ARequest::parse_from_bytes(&packet.payload)?
            .c2a_req_args
            .with_context(|| "no c2a_req_args")?;
        match cmd {
            C2a_req_args::OpenSell(args) => {
                let ch_id = args
                    .ch_id
                    .map(|x| ChId(x))
                    .unwrap_or_else(|| next_ch_id.next_ch_id());

                let r = {
                    let mux_tx = ChSender::new(ch_id, mux_tx.clone());
                    agent.open_shell(mux_tx, args).await
                };

                let (rsp, ch_tx) = match r {
                    Ok(v) => (make_open_shell_response_ok(ch_id.0), Some(v)),
                    Err(e) => (make_open_shell_response_error(e), None),
                };

                ctrl_tx
                    .send_data(rsp.write_to_bytes()?.into())
                    .await
                    .map_err(|_x| anyhow!("send data fail"))?;

                if let Some(ch_tx) = ch_tx {
                    switch.add_channel(ch_id, ch_tx).await?;
                }
            }

            C2a_req_args::OpenSocks(args) => {
                let ch_id = args
                    .ch_id
                    .map(|x| ChId(x))
                    .unwrap_or_else(|| next_ch_id.next_ch_id());

                let r = {
                    let mux_tx = ChSender::new(ch_id, mux_tx.clone());
                    agent.open_socks(mux_tx, args).await
                };

                let (rsp, ch_tx) = match r {
                    Ok(v) => (make_open_shell_response_ok(ch_id.0), Some(v)),
                    Err(e) => (make_open_shell_response_error(e), None),
                };

                ctrl_tx
                    .send_data(rsp.write_to_bytes()?.into())
                    .await
                    .map_err(|_x| anyhow!("send data fail"))?;

                if let Some(ch_tx) = ch_tx {
                    switch.add_channel(ch_id, ch_tx).await?;
                }
            }

            C2a_req_args::CloseChannel(args) => {
                // tracing::debug!("recv closing ch req {}", args);
                let _r = switch.remove_channel(ChId(args.ch_id)).await?;

                let rsp = make_response_status_ok();

                let data: Bytes = rsp.write_to_bytes()?.into();
                // tracing::debug!("send closing ch rsp len {}", data.len());

                ctrl_tx
                    .send_data(data)
                    .await
                    .map_err(|_x| anyhow!("send data fail"))?;
                // tracing::debug!("send closing ch ok");
            }

            C2a_req_args::Ping(args) => {
                let rsp = Pong {
                    timestamp: args.timestamp,
                    ..Default::default()
                };

                let data: Bytes = rsp.write_to_bytes()?.into();
                ctrl_tx
                    .send_data(data)
                    .await
                    .map_err(|_x| anyhow!("send data fail"))?;
            }

            C2a_req_args::KickDown(args) => {
                let rsp = make_response_status_ok();
                let data: Bytes = rsp.write_to_bytes()?.into();
                ctrl_tx
                    .send_data(data)
                    .await
                    .map_err(|_x| anyhow!("send data fail"))?;

                tracing::warn!("recv kick down {args}");
                return Ok(ExitReason::KickDown(args));
            }

            C2a_req_args::OpenP2p(args) => {
                let r = agent.open_p2p(args).await;

                // let r = handle_open_p2p(args).await;
                let rsp = match r {
                    Ok(rsp) => rsp,
                    Err(e) => make_open_p2p_response_error(e),
                };

                let data: Bytes = rsp.write_to_bytes()?.into();
                ctrl_tx
                    .send_data(data)
                    .await
                    .map_err(|_x| anyhow!("send data fail"))?;
            }
        }
    }
}

// async fn handle_open_p2p(mut args: OpenP2PArgs) -> Result<(String, SocketAddr)> {
//     let (peer, nat) = PunchPeer::bind_and_detect("0.0.0.0:0").await?;

//     let local_ufrag = peer.local_ufrag().to_string();

//     let nat_type = nat.nat_type();
//     if nat_type != Some(NatType::Cone) {
//         tracing::warn!("nat type {nat_type:?}");
//     } else {
//         tracing::debug!("nat type {nat_type:?}");
//     }

//     let mapped = nat.into_mapped().with_context(||"empty mapped address")?;

//     let args = args.args.take().with_context(||"no p2p args")?;

//     let mut tun = launch_tun_peer(peer, args.ufrag.into(), args.addr.parse()?, false);
//     spawn_with_name("", async move {
//         tun.wait_for_completed().await
//     });

//     Ok((local_ufrag, mapped))

// }

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
