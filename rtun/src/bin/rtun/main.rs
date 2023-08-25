use anyhow::Result;
use rtun::async_rt;
use clap::Parser;

pub mod cmd_client;

pub mod cmd_agent;

pub mod cmd_server;

pub mod terminal;

pub mod client_ch_pty;

// refer https://github.com/clap-rs/clap/tree/master/clap_derive/examples
#[derive(Parser, Debug)]
#[clap(name = "rtun", author, about, version)]
struct CmdArgs {
    #[clap(subcommand)]
    cmd: SubCmd,
}

#[derive(Parser, Debug)]
enum SubCmd {
    Client(cmd_client::CmdArgs),
    Agent(cmd_agent::CmdArgs),
}

fn init_log() {
    tracing_subscriber::fmt()
    .with_max_level(tracing::metadata::LevelFilter::DEBUG)
    .with_env_filter("rtun=debug,rserver=debug")
    .init();
}

fn main() -> Result<()> {

    init_log();
    let args = CmdArgs::parse();

    let r = async_rt::run_multi_thread(async move { 
        return match args.cmd {
            SubCmd::Client(args) => cmd_client::run(args).await,
            SubCmd::Agent(args) => cmd_agent::run(args).await,
        }
        
    });
    tracing::debug!("main finished with {:?}", r);
    Ok(())
}


