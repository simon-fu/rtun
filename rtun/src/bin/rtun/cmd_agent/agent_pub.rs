
use std::time::{Duration, Instant};

use anyhow::{Result, Context};
use clap::Parser;
use rtun::{ws::client::ws_connect_to, switch::{session_agent::{make_agent_session, AgentSession}, switch_sink::PacketSink, switch_source::PacketSource, switch_pair::make_switch_pair, ctrl_service::ExitReason}, huid::gen_huid::gen_huid};


use crate::rest_proto::{make_pub_url, make_ws_scheme};


pub async fn run(args0: CmdArgs) -> Result<()> { 
    let mut url = url::Url::parse(&args0.url)
    .with_context(||"invalid url")?;

    make_pub_url(&mut url, args0.agent.as_deref(), args0.secret.as_deref())?; 
    make_ws_scheme(&mut url)?;

    let expire_in = if let Some(minutes) = args0.expire_in {
        tracing::info!("will expire in {minutes} minutes");
        let expire_in = minutes * 60;
        Duration::from_secs(expire_in as u64)
    } else {
        Duration::from_secs(9999999999 * 60)
    };
    
    // let url = url.as_str();
    tokio::select! {
        _r = tokio::time::sleep(expire_in) => {
            tracing::info!("running expired")
        }
        _r = run_loop(&url, expire_in, args0.disable_shell) => {

        }
    }

    Ok(())

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

async fn run_loop(url: &url::Url, expire_in: Duration, disable_shell: bool) {
    let expire_at = Instant::now() + expire_in;

    let mut last_success = true;
    loop {
        
        let url = {
            let now = Instant::now();
            if now >= expire_at {
                return
            }
            let expire_in = expire_at - now;
            let mut url = url.clone();
            url.query_pairs_mut().append_pair("expire_in", expire_in.as_millis().to_string().as_str());
            url
        };

        let r = try_connect(url.as_str()).await;
        match r {
            Ok((name, mut session)) => {
                tracing::info!("session connected, agent [{name}]");
                last_success = true;

                let r = session.ctrl_client().disable_shell(disable_shell).await;
                if let Err(e) = r {
                    tracing::error!("set no socks failed {e:?}");
                }
                
                // let r = stream.next().await;
                // let mut session = make_agent_session(stream).await?;
                let r = session.wait_for_completed().await;
                tracing::info!("session finished {r:?}");
                if let Ok(Some(reason)) = r {
                    match reason {
                        ExitReason::KickDown(_v) => {
                            return;
                        },
                    }
                }
            },
            Err(_e) => {
                if last_success {
                    last_success = false;

                    if cfg!(debug_assertions) {
                        tracing::warn!("connect failed [{_e:?}]");
                    } else {
                        tracing::warn!("connect failed");
                    }
                    
                    tracing::info!("try reconnecting...");
                }
                tokio::time::sleep(Duration::from_millis(1000)).await;
            },
        }
        
    }
}

async fn try_connect(url: &str) -> Result<(String, AgentSession<impl PacketSink, impl PacketSource>)> {

    let (stream, rsp) = ws_connect_to(url).await
    .with_context(||format!("fail to connect to [{}]", url))?;

    let agent_name = rsp.headers().get("agent_name")
    .with_context(||"No agent_name field in response headers")?
    .to_str()
    .with_context(||"invalid response header")?;

    let uid = gen_huid();
    // let (sink, source) = stream.split();
    let switch_session = make_switch_pair(uid, stream.split()).await?;


    let session = make_agent_session(switch_session).await?;

    Ok((agent_name.into(), session))


    // let (stream, rsp) = ws_connect_to(url).await
    // .with_context(||format!("fail to connect to [{}]", url))?;

    // let agent_name = rsp.headers().get("agent_name")
    // .with_context(||"No agent_name field in response headers")?
    // .to_str()
    // .with_context(||"invalid response header")?;

    // let session = make_agent_session(stream).await?;

    // Ok((agent_name.into(), session))
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
        short = 's',
        long = "secret",
        long_help = "authentication secret",
    )]
    secret: Option<String>,

    #[clap(
        short = 'd',
        long = "expire_in",
        long_help = "expire duration in unit of minutes",
    )]
    expire_in: Option<i64>,

    #[clap(
        long = "disable-shell",
        long_help = "disable shell service",
    )]
    disable_shell: bool,
}
