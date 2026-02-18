use std::collections::HashMap;

use anyhow::{Context, Result};
use clap::Parser;
use rtun::{
    channel::{ChId, ChPair},
    huid::gen_huid::gen_huid,
    proto::{OpenShellArgs, ProgramArgs},
    switch::{ctrl_client::make_ctrl_client, switch_pair::make_switch_pair},
    term::async_input::get_term_size,
    ws::client::ws_connect_to,
};

use crate::{
    client_utils::client_select_url, init_log_and_run, rest_proto::get_agent_from_url,
    terminal::run_term,
};

pub fn run(args: CmdArgs) -> Result<()> {
    init_log_and_run(do_run(args))?
}

async fn do_run(args: CmdArgs) -> Result<()> {
    let url = client_select_url(&args.url, args.agent.as_deref(), args.secret.as_deref()).await?;
    let url_str = url.as_str();

    let (stream, _r) = ws_connect_to(url_str)
        .await
        .with_context(|| format!("fail to connect to [{}]", url_str))?;

    tracing::info!("select agent {:?}", get_agent_from_url(&url));
    tracing::debug!("connected to [{}]", url_str);

    let uid = gen_huid();
    // let mut switch_session = make_stream_switch(uid, stream).await?;
    let mut switch_session = make_switch_pair(uid, stream.split()).await?;
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
    #[clap(help = "eg: http://127.0.0.1:8080")]
    url: String,

    #[clap(short = 'a', long = "agent", long_help = "agent name")]
    agent: Option<String>,

    #[clap(long = "secret", long_help = "authentication secret")]
    secret: Option<String>,
}
