
use std::time::Duration;

use anyhow::{Result, Context};
use clap::Parser;
use rtun::{ws::client::ws_connect_to, switch::{switch_stream::PacketStream, session_agent::{make_agent_session, AgentSession}}};


use crate::rest_proto::{make_pub_url, make_ws_scheme};


pub async fn run(args0: CmdArgs) -> Result<()> { 
    let mut url = url::Url::parse(&args0.url)
    .with_context(||"invalid url")?;

    make_pub_url(&mut url, args0.agent.as_deref(), args0.secret.as_deref())?; 
    make_ws_scheme(&mut url)?;
    
    let url = url.as_str();

    let mut last_success = true;
    loop {
        let r = try_connect(url).await;
        match r {
            Ok((name, mut session)) => {
                tracing::info!("session connected, agent [{name}]");
                last_success = true;
                // let r = stream.next().await;
                // let mut session = make_agent_session(stream).await?;
                let r = session.wait_for_completed().await;
                tracing::info!("session finished {r:?}");
            },
            Err(e) => {
                if last_success {
                    last_success = false;
                    tracing::warn!("connect failed [{e:?}]");
                    tracing::info!("try reconnecting...");
                }
                tokio::time::sleep(Duration::from_millis(1000)).await;
            },
        }
        
    }

    // use rtun::huid::gen_huid::gen_huid;
    // use rtun::switch::switch_stream::make_stream_switch;
    // use rtun::switch::agent::ctrl::make_agent_ctrl;
    // use rtun::channel::ChId;
    // use rtun::channel::ChPair;
    // use rtun::switch::ctrl_service::spawn_ctrl_service;
    
    // let (stream, rsp) = ws_connect_to(url).await
    // .with_context(||format!("fail to connect to [{}]", url))?;

    // let agent_name = rsp.headers().get("agent_name")
    // .with_context(||"No agent_name field in response headers")?
    // .to_str()
    // .with_context(||"invalid response header")?;

    // tracing::debug!("connected to [{url}]");
    // tracing::debug!("published name [{agent_name}]");


    // let uid = gen_huid();
    // let mut switch_session = make_stream_switch(uid, stream).await?;
    // let switch = switch_session.clone_invoker();

    // let mut agent = make_agent_ctrl(uid).await?;
    // let ctrl = agent.clone_ctrl();
    
    // let ctrl_ch_id = ChId(0);
    // let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    // let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;
    // let pair = ChPair { tx: ctrl_tx, rx: ctrl_rx };
    // spawn_ctrl_service(uid, ctrl, switch, pair);


    // let r = switch_session.wait_for_completed().await;
    // tracing::debug!("switch session finished with {:?}", r);

    // agent.shutdown().await;
    // let r = agent.wait_for_completed().await;
    // tracing::debug!("agent ctrl finished with {:?}", r);
    // Ok(())

}

async fn try_connect(url: &str) -> Result<(String, AgentSession<impl PacketStream>)> {
    let (stream, rsp) = ws_connect_to(url).await
    .with_context(||format!("fail to connect to [{}]", url))?;

    let agent_name = rsp.headers().get("agent_name")
    .with_context(||"No agent_name field in response headers")?
    .to_str()
    .with_context(||"invalid response header")?;

    let session = make_agent_session(stream).await?;

    Ok((agent_name.into(), session))
}








#[derive(Parser, Debug)]
#[clap(name = "agent", author, about, version)]
pub struct CmdArgs {

    url: String,

    #[clap(
        short = 'a',
        long = "agent",
        long_help = "agent name",
        // multiple_occurrences = true
    )]
    agent: Option<String>,

    #[clap(
        long = "secret",
        long_help = "authentication secret",
    )]
    secret: Option<String>,

    // #[clap(
    //     short = 'r',
    //     long = "reverse",
    //     long_help = "connect to url as reverse agent",
    //     // multiple_occurrences = true
    // )]
    // reverse: Option<bool>,
}