
use anyhow::{Result, Context};
use clap::Parser;
use rtun::{ws::client::ws_connect_to, switch::{switch_stream::make_stream_switch, ctrl_service::spawn_ctrl_service, agent::ctrl::make_agent_ctrl}, huid::gen_huid::gen_huid, channel::{ChId, ChPair}};

use crate::rest_proto::{make_pub_url, make_ws_scheme};


pub async fn run(args0: CmdArgs) -> Result<()> { 
    let mut url = url::Url::parse(&args0.url)
    .with_context(||"invalid url")?;

    make_pub_url(&mut url, args0.agent.as_deref())?; 
    make_ws_scheme(&mut url)?;
    
    let url = url.as_str();

    let (stream, rsp) = ws_connect_to(url).await
    .with_context(||format!("fail to connect to [{}]", url))?;

    let agent_name = rsp.headers().get("agent_name")
    .with_context(||"No agent_name field in response headers")?
    .to_str()
    .with_context(||"invalid response header")?;

    tracing::debug!("connected to [{url}]");
    tracing::debug!("published name [{agent_name}]");

    let uid = gen_huid();
    let mut switch_session = make_stream_switch(uid, stream).await?;
    let switch = switch_session.clone_invoker();

    let mut agent = make_agent_ctrl(uid)?;
    let ctrl = agent.clone_ctrl();
    
    let ctrl_ch_id = ChId(0);
    let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;
    let pair = ChPair { tx: ctrl_tx, rx: ctrl_rx };
    spawn_ctrl_service(uid, ctrl, switch, pair);


    let r = switch_session.wait_for_completed().await;
    tracing::debug!("switch session finished with {:?}", r);

    agent.shutdown().await;
    let r = agent.wait_for_completed().await;
    tracing::debug!("agent ctrl finished with {:?}", r);

    Ok(())
}








#[derive(Parser, Debug)]
#[clap(name = "agent", author, about, version)]
pub struct CmdArgs {

    url: String,

    #[clap(
        short = 'a',
        long = "name",
        long_help = "agent name",
        // multiple_occurrences = true
    )]
    agent: Option<String>,

    // #[clap(
    //     short = 'r',
    //     long = "reverse",
    //     long_help = "connect to url as reverse agent",
    //     // multiple_occurrences = true
    // )]
    // reverse: Option<bool>,
}