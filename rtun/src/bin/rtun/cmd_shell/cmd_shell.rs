use std::{cmp::Ordering, collections::HashMap};

use anyhow::{bail, Context, Result};
use chrono::Local;
use clap::Parser;
use rtun::{
    channel::{ChId, ChPair},
    huid::gen_huid::gen_huid,
    proto::{OpenShellArgs, ProgramArgs},
    switch::{
        ctrl_client::make_ctrl_client, switch_pair::make_switch_pair, switch_sink::PacketSink,
        switch_source::PacketSource,
    },
    term::async_input::get_term_size,
    ws::client::ws_connect_to,
};

use crate::{
    client_utils::client_select_url, init_log_and_run, rest_proto::get_agent_from_url,
    secret::token_gen, terminal::run_term,
};

pub fn run(args: CmdArgs) -> Result<()> {
    init_log_and_run(do_run(args))?
}

async fn do_run(args: CmdArgs) -> Result<()> {
    let signal_url = url::Url::parse(&args.url).with_context(|| "invalid url")?;
    if signal_url.scheme().eq_ignore_ascii_case("quic") {
        let sub_url = select_quic_sub_url(
            &signal_url,
            args.agent.as_deref(),
            args.secret.as_deref(),
            args.quic_insecure,
        )
        .await?;
        let stream = crate::quic_signal::connect_sub_with_opts(&sub_url, args.quic_insecure)
            .await
            .with_context(|| format!("fail to connect to [{}]", sub_url))?;
        tracing::info!("select agent {:?}", get_agent_from_url(&sub_url));
        tracing::debug!("connected to [{}]", sub_url);
        run_shell_with_stream(stream.split()).await
    } else {
        let url =
            client_select_url(&args.url, args.agent.as_deref(), args.secret.as_deref()).await?;
        let url_str = url.as_str();
        let (stream, _r) = ws_connect_to(url_str)
            .await
            .with_context(|| format!("fail to connect to [{}]", url_str))?;
        tracing::info!("select agent {:?}", get_agent_from_url(&url));
        tracing::debug!("connected to [{}]", url_str);
        run_shell_with_stream(stream.split()).await
    }
}

async fn run_shell_with_stream<S1, S2>(stream: (S1, S2)) -> Result<()>
where
    S1: PacketSink,
    S2: PacketSource,
{
    let uid = gen_huid();
    // let mut switch_session = make_stream_switch(uid, stream).await?;
    let mut switch_session = make_switch_pair(uid, stream).await?;
    let switch = switch_session.clone_invoker();

    let ctrl_ch_id = ChId(0);

    let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;

    let pair = ChPair {
        tx: ctrl_tx,
        rx: ctrl_rx,
    };

    // let mut ctrl = ClientChannelCtrl::new(pair, invoker);
    let mut ctrl_session = make_ctrl_client(uid, pair, switch, false).await?;
    let ctrl = ctrl_session.clone_invoker();

    // let ch_id = ChId(1);
    // let size = get_terminal_size().await?;
    let size = get_term_size().with_context(|| "get terminal size failed")?;

    const TERM: &str = "TERM";
    let mut env_vars: HashMap<protobuf::Chars, protobuf::Chars> = HashMap::new();
    match std::env::var(TERM).ok() {
        Some(v) => {
            env_vars.insert(TERM.into(), v.into());
        }
        None => {}
    }

    let shell_args = OpenShellArgs {
        // ch_id: ch_id.0,
        // agent: "".into(),
        // term: "xterm-256color".into(),
        cols: size.cols as u32,
        rows: size.rows as u32,
        program_args: Some(ProgramArgs {
            cols: size.cols as u32,
            rows: size.rows as u32,
            env_vars,
            ..Default::default()
        })
        .into(),
        ..Default::default()
    };

    // let shell_pair = ctrl.open_shell().await.with_context(||"open shell failed")?;
    // tracing::debug!("opened shell ");

    let (shell_tx, shell_rx) = ChPair::new(ChId(1)).split();

    // let shell_tx = ctrl.open_channel(shell_tx).await?;
    // tracing::debug!("opened channel {:?}", shell_tx.ch_id());

    let shell_tx = ctrl.open_shell(shell_tx, shell_args).await?;
    tracing::debug!("opened shell {:?}", shell_tx.ch_id());

    let shell_pair = ChPair {
        tx: shell_tx,
        rx: shell_rx,
    };

    // let r = c2a_open_shell(&mut shell_pair, shell_args).await?;
    // tracing::debug!("opened shell {:?}", r);

    run_term(shell_pair).await?;

    // super::term_crossterm::run(tx, rx).await?;
    // super::term_termwiz::run(tx, rx).await?;

    println!("\r");
    switch_session.shutdown_and_waitfor().await?;
    ctrl_session.shutdown_and_waitfor().await?;

    Ok(())
}

async fn select_quic_sub_url(
    signal_url: &url::Url,
    agent: Option<&str>,
    secret: Option<&str>,
    quic_insecure: bool,
) -> Result<url::Url> {
    let mut sub_url = signal_url.clone();

    if let Some(agent) = agent {
        sub_url.query_pairs_mut().append_pair("agent", agent);
    } else {
        let selected = select_latest_quic_agent(signal_url, quic_insecure).await?;
        sub_url
            .query_pairs_mut()
            .append_pair("agent", selected.name.as_str());
        if let Some(instance_id) = selected.instance_id.as_deref() {
            sub_url
                .query_pairs_mut()
                .append_pair("instance_id", instance_id);
        }
    }

    let token = token_gen(secret, Local::now().timestamp_millis() as u64)?;
    sub_url
        .query_pairs_mut()
        .append_pair("token", token.as_str());

    Ok(sub_url)
}

async fn select_latest_quic_agent(
    signal_url: &url::Url,
    quic_insecure: bool,
) -> Result<crate::rest_proto::AgentInfo> {
    let mut agents =
        crate::quic_signal::query_sessions_with_opts(signal_url, quic_insecure).await?;
    if agents.is_empty() {
        bail!("agent list empty");
    }
    agents.sort_by(cmp_agent_priority);
    Ok(agents.swap_remove(0))
}

fn cmp_agent_priority(
    a: &crate::rest_proto::AgentInfo,
    b: &crate::rest_proto::AgentInfo,
) -> Ordering {
    b.expire_at
        .cmp(&a.expire_at)
        .then_with(|| cmp_instance_id_desc(a.instance_id.as_deref(), b.instance_id.as_deref()))
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.addr.cmp(&b.addr))
}

fn cmp_instance_id_desc(a: Option<&str>, b: Option<&str>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => b.cmp(a),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

// async fn query_and_select_agent(url: &url::Url) -> Result<AgentInfo> {
//     let mut agents = get_agents(url)
//     .await?;

//     if agents.len() == 0 {
//         bail!("agent list empty")
//     }

//     Ok(agents.swap_remove(0))
// }

// async fn get_agents(url: &url::Url) -> Result<Vec<AgentInfo>> {
//     let mut url = url.clone();
//     make_pub_sessions(&mut url)?;
//     reqwest::get(url)
//     .await?
//     .json()
//     .await
//     .map_err(|e|e.into())
// }

#[derive(Parser, Debug)]
#[clap(name = "shell", author, about, version)]
pub struct CmdArgs {
    #[clap(help = "eg: https://127.0.0.1:8888 or quic://127.0.0.1:8888")]
    url: String,

    #[clap(short = 'a', long = "agent", long_help = "agent name")]
    agent: Option<String>,

    #[clap(long = "secret", long_help = "authentication secret")]
    secret: Option<String>,

    #[clap(
        long = "quic-insecure",
        long_help = "skip quic tls certificate verification (quic:// only)"
    )]
    quic_insecure: bool,
}
