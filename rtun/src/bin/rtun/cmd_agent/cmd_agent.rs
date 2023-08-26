use anyhow::Result;
use clap::Parser;
use super::{agent_pub, agent_listen};

// refer https://github.com/clap-rs/clap/tree/master/clap_derive/examples
#[derive(Parser, Debug)]
#[clap(name = "ragent", author, about, version)]
pub struct CmdArgs {
    #[clap(subcommand)]
    cmd: SubCmd,
}

#[derive(Parser, Debug)]
pub enum SubCmd {
    Pub(agent_pub::CmdArgs),
    Listen(agent_listen::CmdArgs),
}

pub async fn run(args: CmdArgs) -> Result<()> {
    match args.cmd {
        SubCmd::Pub(args) => agent_pub::run(args).await,
        SubCmd::Listen(args) => agent_listen::run(args).await,
    }
}


// use anyhow::{Result, Context, bail};
// use clap::Parser;
// use rtun::{ws::client::ws_connect_to, switch::{switch_stream::make_stream_switch, ctrl_service::spawn_ctrl_service, agent::ctrl::make_agent_ctrl}, huid::gen_huid::gen_huid, channel::{ChId, ChPair}};


// pub async fn run(args: CmdArgs) -> Result<()> { 

//     check_args(&args)?;

//     let stream = ws_connect_to(&args.url).await
//     .with_context(||format!("fail to connect to [{}]", args.url))?;

//     tracing::debug!("connected to [{}]", args.url);

//     let uid = gen_huid();
//     let mut switch_session = make_stream_switch(uid, stream).await?;
//     let switch = switch_session.clone_invoker();

//     let mut agent = make_agent_ctrl(uid)?;
//     let ctrl = agent.clone_ctrl();
    
//     let ctrl_ch_id = ChId(0);
//     let (ctrl_tx, ctrl_rx) = ChPair::new(ctrl_ch_id).split();
//     let ctrl_tx = switch.add_channel(ctrl_ch_id, ctrl_tx).await?;
//     let pair = ChPair { tx: ctrl_tx, rx: ctrl_rx };
//     spawn_ctrl_service(uid, ctrl, switch, pair);


//     let r = switch_session.wait_for_completed().await;
//     tracing::debug!("switch session finished with {:?}", r);

//     agent.shutdown().await;
//     let r = agent.wait_for_completed().await;
//     tracing::debug!("agent ctrl finished with {:?}", r);

//     Ok(())
// }

// fn check_args(args: &CmdArgs) -> Result<()> {
//     let url = url::Url::parse(&args.url)
//     .with_context(||"invalid url")?;

//     if url.scheme().eq_ignore_ascii_case("ws") 
//     || url.scheme().eq_ignore_ascii_case("wss") {
//         Ok(())
//     } else {
//         bail!("unsupport protocol [{}]", url.scheme())
//     }
// }





// #[derive(Parser, Debug)]
// #[clap(name = "agent", author, about, version)]
// pub struct CmdArgs {

//     url: String,

//     #[clap(
//         short = 'r',
//         long = "reverse",
//         long_help = "connect to url as reverse agent",
//         // multiple_occurrences = true
//     )]
//     reverse: Option<bool>,
// }