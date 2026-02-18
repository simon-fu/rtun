use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Local;
use clap::Parser;
use rtun::{
    huid::gen_huid::gen_huid,
    switch::{
        ctrl_service::ExitReason,
        session_agent::{make_agent_session, AgentSession},
        switch_pair::make_switch_pair,
        switch_sink::PacketSink,
        switch_source::PacketSource,
    },
    ws::client::ws_connect_to,
};

use crate::rest_proto::{make_pub_url, make_ws_scheme};
use crate::secret::token_gen;

pub async fn run(args0: CmdArgs) -> Result<()> {
    let mut url = url::Url::parse(&args0.url).with_context(|| "invalid url")?;

    let is_quic = url.scheme().eq_ignore_ascii_case("quic");

    if is_quic {
        make_pub_quic_url(&mut url, args0.agent.as_deref(), args0.secret.as_deref())?;
    } else {
        make_pub_url(&mut url, args0.agent.as_deref(), args0.secret.as_deref())?;
        make_ws_scheme(&mut url)?;
    }

    let expire_in = if let Some(minutes) = args0.expire_in {
        tracing::info!("will expire in {minutes} minutes");
        let expire_in = minutes * 60;
        Duration::from_secs(expire_in as u64)
    } else {
        Duration::from_secs(9999999999 * 60)
    };

    if is_quic {
        tokio::select! {
            _r = tokio::time::sleep(expire_in) => {
                tracing::info!("running expired")
            }
            _r = run_loop_quic(&url, expire_in, args0.disable_shell, args0.quic_insecure) => {}
        }
    } else {
        tokio::select! {
            _r = tokio::time::sleep(expire_in) => {
                tracing::info!("running expired")
            }
            _r = run_loop_ws(&url, expire_in, args0.disable_shell) => {}
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

async fn run_loop_ws(url: &url::Url, expire_in: Duration, disable_shell: bool) {
    let expire_at = Instant::now() + expire_in;

    let mut last_success = true;
    loop {
        let url = {
            let now = Instant::now();
            if now >= expire_at {
                return;
            }
            let expire_in = expire_at - now;
            let mut url = url.clone();
            url.query_pairs_mut()
                .append_pair("expire_in", expire_in.as_millis().to_string().as_str());
            url
        };

        let r = try_connect_ws(url.as_str()).await;
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
                        }
                    }
                }
            }
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
            }
        }
    }
}

async fn try_connect_ws(
    url: &str,
) -> Result<(String, AgentSession<impl PacketSink, impl PacketSource>)> {
    let (stream, rsp) = ws_connect_to(url)
        .await
        .with_context(|| format!("fail to connect to [{}]", url))?;

    let agent_name = rsp
        .headers()
        .get("agent_name")
        .with_context(|| "No agent_name field in response headers")?
        .to_str()
        .with_context(|| "invalid response header")?;

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

async fn run_loop_quic(
    url: &url::Url,
    expire_in: Duration,
    disable_shell: bool,
    quic_insecure: bool,
) {
    let expire_at = Instant::now() + expire_in;

    let mut last_success = true;
    loop {
        let url = {
            let now = Instant::now();
            if now >= expire_at {
                return;
            }
            let expire_in = expire_at - now;
            let mut url = url.clone();
            url.query_pairs_mut()
                .append_pair("expire_in", expire_in.as_millis().to_string().as_str());
            url
        };

        let r = try_connect_quic(url.as_str(), quic_insecure).await;
        match r {
            Ok((name, mut session)) => {
                tracing::info!("session connected, agent [{name}]");
                last_success = true;

                let r = session.ctrl_client().disable_shell(disable_shell).await;
                if let Err(e) = r {
                    tracing::error!("set no socks failed {e:?}");
                }

                let r = session.wait_for_completed().await;
                tracing::info!("session finished {r:?}");
                if let Ok(Some(reason)) = r {
                    match reason {
                        ExitReason::KickDown(_v) => {
                            return;
                        }
                    }
                }
            }
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
            }
        }
    }
}

async fn try_connect_quic(
    url: &str,
    quic_insecure: bool,
) -> Result<(String, AgentSession<impl PacketSink, impl PacketSource>)> {
    let parsed = url::Url::parse(url).with_context(|| format!("invalid url [{}]", url))?;
    let (agent_name, stream) = crate::quic_signal::connect_pub_with_opts(&parsed, quic_insecure)
        .await
        .with_context(|| format!("fail to connect to [{}]", parsed))?;

    let uid = gen_huid();
    let switch_session = make_switch_pair(uid, stream.split()).await?;
    let session = make_agent_session(switch_session).await?;
    Ok((agent_name, session))
}

fn make_pub_quic_url(
    url: &mut url::Url,
    agent_name: Option<&str>,
    secret: Option<&str>,
) -> Result<()> {
    if let Some(agent_name) = agent_name {
        url.query_pairs_mut().append_pair("agent", agent_name);
    }

    let token = token_gen(secret, Local::now().timestamp_millis() as u64)?;
    url.query_pairs_mut().append_pair("token", token.as_str());
    url.query_pairs_mut()
        .append_pair("ver", rtun::version::ver_brief());

    Ok(())
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

    #[clap(short = 's', long = "secret", long_help = "authentication secret")]
    secret: Option<String>,

    #[clap(
        short = 'd',
        long = "expire_in",
        long_help = "expire duration in unit of minutes"
    )]
    expire_in: Option<i64>,

    #[clap(long = "disable-shell", long_help = "disable shell service")]
    disable_shell: bool,

    #[clap(
        long = "quic-insecure",
        long_help = "skip quic tls certificate verification (quic:// only)"
    )]
    quic_insecure: bool,
}
