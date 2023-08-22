use anyhow::{Result, Context, anyhow};
use crate::{agent_invoker::{AgentInvoker, AgentEntity}, agent_ch_pty::open_agent_shell};
use protobuf::Message;
use rtun::{async_rt::spawn_with_name, huid::HUId, proto::{C2ARequest, c2arequest::C2a_req_args, C2AResponse}, channel::ChId};


pub fn spawn_agent_ctrl<E: AgentEntity>(uid: HUId, agent: AgentInvoker<E>) {
    spawn_with_name(format!("ctrl-{}", uid), async move {
        let r = ctrl_loop(&agent).await;
        tracing::debug!("finished with [{:?}]", r);
    });
}

async fn ctrl_loop<E: AgentEntity>(agent: &AgentInvoker<E>) -> Result<()> {
    
    let (tx, mut rx) = agent.add_channel(ChId(0)).await?;

    loop {
        let r = rx.recv_data().await;
        let data = match r {
            Some(v) => v,
            None => break,
        };

        let cmd = C2ARequest::parse_from_bytes(&data)?
        .c2a_req_args
        .with_context(||"no c2a_req_args")?;
        match cmd {
            C2a_req_args::OpenSell(cmd) => {
                let r = open_agent_shell(agent, cmd).await;
                
                let rsp = match r {
                    Ok(_v) => C2AResponse {
                        status: 0,
                        reason: "".into(),
                        ..Default::default()
                    },
                    Err(e) => C2AResponse {
                        status: 0,
                        reason: format!("{e:?}").into(),
                        ..Default::default()
                    },
                };

                tx.send_data(rsp.write_to_bytes()?.into()).await
                .map_err(|_x|anyhow!("send data fail"))?;
            },
            _ => {},
        }
    }
    Ok(())
}


