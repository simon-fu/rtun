use anyhow::{Result, Context};
use clap::Parser;
use rtun::{ws::client::ws_connect_to, switch::{switch_stream::make_stream_switch, ctrl_client::make_ctrl_client}, huid::gen_huid::gen_huid, channel::{ChId, ChPair}, proto::OpenShellArgs, term::async_input::get_term_size};

use crate::{terminal::run_term, client_utils::client_select_url};

pub async fn run(args: CmdArgs) -> Result<()> { 

    // let mut url = url::Url::parse(&args.url)
    // .with_context(||"invalid url")?;

    // let url = if url.scheme().eq_ignore_ascii_case("ws") 
    //     || url.scheme().eq_ignore_ascii_case("wss") {
    //     url.as_str()
    // } else if url.scheme().eq_ignore_ascii_case("http") 
    //     || url.scheme().eq_ignore_ascii_case("https") {
    //         match args.agent.as_deref() {
    //             Some(agent) => {
    //                 make_sub_url(&mut url, Some(agent))?;
    //             },
    //             None => {
    //                 let agent = query_and_select_agent(&url).await?;
    //                 make_sub_url(&mut url, Some(agent.name.as_str()))?
    //             },
    //         }
            
    //         make_ws_scheme(&mut url)?;
    //         url.as_str()
    // }
    // else {
    //     bail!("unsupport protocol [{}]", url.scheme())
    // };

    let url = client_select_url(&args.url, args.agent.as_deref()).await?;
    let url = url.as_str();

    let (stream, _r) = ws_connect_to(url).await
    .with_context(||format!("fail to connect to [{}]", url))?;

    tracing::debug!("connected to [{}]", url);

    let uid = gen_huid();
    let mut switch_session = make_stream_switch(uid, stream).await?;
    let switch = switch_session.clone_invoker();

    let ctrl_ch_id = ChId(0);

    let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
    let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;
    
    let pair = ChPair { tx: ctrl_tx, rx: ctrl_rx };

    // let mut ctrl = ClientChannelCtrl::new(pair, invoker);
    let mut ctrl_session = make_ctrl_client(uid, pair, switch)?;
    let ctrl = ctrl_session.clone_invoker();

    // let ch_id = ChId(1);
    // let size = get_terminal_size().await?;
    let size = get_term_size()
    .with_context(||"get terminal size failed")?;
    let shell_args = OpenShellArgs {
        // ch_id: ch_id.0,
        // agent: "".into(),
        // term: "xterm-256color".into(),
        cols: size.cols as u32,
        rows: size.rows as u32,
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
#[clap(name = "client", author, about, version)]
pub struct CmdArgs {

    #[clap(help="eg: http://127.0.0.1:8080")]
    url: String,

    #[clap(
        short = 'a',
        long = "agent",
        long_help = "agent name",
    )]
    agent: Option<String>,
}





