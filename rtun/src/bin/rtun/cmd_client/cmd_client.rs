use anyhow::{Result, Context, bail};
use clap::Parser;
use rtun::{ws::client::ws_connect_to, switch::{switch_stream::make_stream_switch, ctrl_client::make_ctrl_client}, huid::gen_huid::gen_huid, channel::{ChId, ChPair}, proto::OpenShellArgs};

use crate::terminal::{run_term, get_terminal_size};

pub async fn run(args: CmdArgs) -> Result<()> { 

    check_args(&args)?;

    let stream = ws_connect_to(&args.url).await
    .with_context(||format!("fail to connect to [{}]", args.url))?;

    tracing::debug!("connected to [{}]", args.url);

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
    let size = get_terminal_size().await?;
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

fn check_args(args: &CmdArgs) -> Result<()> {
    let url = url::Url::parse(&args.url)
    .with_context(||"invalid url")?;

    if url.scheme().eq_ignore_ascii_case("ws") 
    || url.scheme().eq_ignore_ascii_case("wss") {
        Ok(())
    } else {
        bail!("unsupport protocol [{}]", url.scheme())
    }
}

#[derive(Parser, Debug)]
#[clap(name = "client", author, about, version)]
pub struct CmdArgs {

    url: String,
}




