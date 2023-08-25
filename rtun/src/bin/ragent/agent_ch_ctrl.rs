use anyhow::{Result, Context, anyhow};
use crate::agent_ch_pty::open_agent_shell;
use protobuf::Message;
use rtun::{async_rt::spawn_with_name, huid::HUId, proto::{C2ARequest, c2arequest::C2a_req_args, OpenChannelResponse, open_channel_response::Open_ch_rsp, ResponseStatus}, channel::ChPair, switch::agent_invoker::{AgentEntity, AgentInvoker}};


pub fn spawn_agent_ctrl<E: AgentEntity>(uid: HUId, agent: AgentInvoker<E>, chpair: ChPair) {
    spawn_with_name(format!("ctrl-{}", uid), async move {
        let r = ctrl_loop(&agent, chpair).await;
        tracing::debug!("finished with [{:?}]", r);
    });
}

async fn ctrl_loop<E: AgentEntity>(agent: &AgentInvoker<E>, chpair: ChPair) -> Result<()> {
    
    let tx = chpair.tx;
    let mut rx = chpair.rx;

    loop {
        let r = rx.recv_data().await;
        let packet = match r {
            Some(v) => v,
            None => break,
        };

        let cmd = C2ARequest::parse_from_bytes(&packet.payload)?
        .c2a_req_args
        .with_context(||"no c2a_req_args")?;
        match cmd {
            C2a_req_args::OpenSell(cmd) => {
                let r = open_agent_shell(agent, cmd).await;
                
                // let rsp = match r {
                //     Ok(_v) => C2AResponse {
                //         status: 0,
                //         reason: "".into(),
                //         ..Default::default()
                //     },
                //     Err(e) => C2AResponse {
                //         status: 0,
                //         reason: format!("{e:?}").into(),
                //         ..Default::default()
                //     },
                // };

                let rsp = OpenChannelResponse {
                    open_ch_rsp: Some(match r {
                        Ok(v) => Open_ch_rsp::ChId(v.0),
                        Err(e) => Open_ch_rsp::Status(ResponseStatus{
                            code: -1,
                            reason: format!("{e:?}").into(),
                            ..Default::default()
                        })
                    }),
                    ..Default::default()
                };

                tx.send_data(rsp.write_to_bytes()?.into()).await
                .map_err(|_x|anyhow!("send data fail"))?;
            },
            _ => {},
        }
    }
    Ok(())
}



