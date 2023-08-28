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

