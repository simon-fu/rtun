
/*
    1. Scenario: client -> agent 
    agent --secret xyz --listen 0.0.0.0:8889 
    socks --secret xyz --agent 127.0.0.1:8889 --listen 0.0.0.0:1080
    relay --secret xyz --agent 127.0.0.1:8889 127.0.0.1:1022->192.168.1.100:22
    relay --secret xyz --agent 127.0.0.1:8889 127.0.0.1:1022<-192.168.1.100:22
    relay --secret xyz --agent 127.0.0.1:8889 udp://127.0.0.1:1022<-192.168.1.100:22 

    2. Scenario: client -> bridge -> agent
    bridge --secret abc --listen 0.0.0.0:9888 
    agent --secret xyz --bridge :abc@127.0.0.1:9888/myroom/myagent
    socks --secret xyz --bridge :abc@127.0.0.1:9888/myroom/myagent --listen 0.0.0.0:1080 --p2p
    socks --secret xyz --bridge :abc@127.0.0.1:9888/myroom --listen 0.0.0.0:1080
    relay --secret xyz --bridge :abc@127.0.0.1:9888/myroom/myagent 127.0.0.1:1022->192.168.1.100:22
*/

#[test]
fn test() {
    println!("{:?}", ::url::Url::parse("quic://:abc@127.0.0.1:9888/myroom/myagent"));
    println!("{:?}", ::url::Url::parse("127.0.0.1:9888/myroom/myagent"));
    // println!("{:?}", ::url::Url::parse(":abc@127.0.0.1:9888/myroom/myagent"));
    println!("{:?}", ::url::Url::parse("quic://").unwrap().join(":abc@127.0.0.1:9888/myroom/myagent"));

    // let url = ::url::Url::parse("quic://:abc@127.0.0.1:9888/myroom/myagent").unwrap();
}

use clap::Parser;
use anyhow::Result;
use crate::init_log_and_run;
use super::{sub_bridge, sub_agent};

pub fn run(args: CmdArgs) -> Result<()> { 
    init_log_and_run(do_run(args))?
}

async fn do_run(args: CmdArgs) -> Result<()> {
    match args.cmd {
        SubCmd::Agent(args) => sub_agent::run(args).await,
        SubCmd::Bridge(args) => sub_bridge::run(args).await,
    }
}



// refer https://github.com/clap-rs/clap/tree/master/clap_derive/examples
#[derive(Parser, Debug)]
#[clap(name = "quic", author, about, version)]
pub struct CmdArgs {
    #[clap(subcommand)]
    cmd: SubCmd,
}

#[derive(Parser, Debug)]
pub enum SubCmd {
    Agent(sub_agent::CmdArgs),
    Bridge(sub_bridge::CmdArgs),
}
