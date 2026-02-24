/*
    usage:
        ./rtun nat4 nat4 -t 221.221.153.138:12333 --ttl 6 --count 512

        ./rtun nat4 nat3 -l 0.0.0.0:12333 -t 36.112.207.162 --batch-interval 3000 --interval 500 --count 512
*/
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use anyhow::{Context as _, Result};
use clap::Parser;
use rtun::p2p::hard_nat::{self, Nat3RunConfig, Nat4RunConfig};

pub fn run(args: CmdArgs) -> Result<()> {
    crate::init_log_and_run(do_run(args))?
}

async fn do_run(args: CmdArgs) -> Result<()> {
    match args.cmd {
        SubCmd::Nat3(args) => run_nat3(args).await,
        SubCmd::Nat4(args) => run_nat4(args).await,
    }
}

async fn run_nat3(args: Nat3SendCmdArgs) -> Result<()> {
    let target_ip: IpAddr = args.target.parse().with_context(|| "invalid target ip")?;
    let cfg = Nat3RunConfig {
        content: args.content,
        target_ip,
        count: args.count,
        listen: args.listen,
        ttl: args.ttl,
        interval: Duration::from_millis(args.interval),
        batch_interval: Duration::from_millis(args.batch_interval),
    };
    hard_nat::run_nat3(cfg).await
}

async fn run_nat4(args: Nat4SendCmdArgs) -> Result<()> {
    let target: SocketAddr = args
        .target
        .parse()
        .with_context(|| "invalid target address")?;

    let cfg = Nat4RunConfig {
        content: args.content,
        target,
        count: args.count,
        ttl: args.ttl,
        interval: Duration::from_millis(args.interval),
    };
    hard_nat::run_nat4(cfg).await
}

#[derive(Parser, Debug)]
#[clap(name = "nat4")]
pub struct CmdArgs {
    #[clap(subcommand)]
    cmd: SubCmd,
}

#[derive(Parser, Debug)]
enum SubCmd {
    Nat3(Nat3SendCmdArgs),
    Nat4(Nat4SendCmdArgs),
}

#[derive(Parser, Debug)]
#[clap(name = "nat3-send")]
pub struct Nat3SendCmdArgs {
    #[clap(long_help = "content for sending")]
    content: Option<String>,

    #[clap(short = 't', long = "target", long_help = "target ip")]
    target: String,

    #[clap(
        short = 'c',
        long = "count",
        long_help = "target port count",
        default_value = "64"
    )]
    count: usize,

    #[clap(
        short = 'l',
        long = "listen",
        long_help = "listen address",
        default_value = "0.0.0.0:0"
    )]
    listen: String,

    #[clap(long = "ttl", long_help = "set socket ttl")]
    ttl: Option<u32>,

    #[clap(
        long = "interval",
        long_help = "interval in milli seconds",
        default_value = "1000"
    )]
    interval: u64,

    #[clap(
        long = "batch-interval",
        long_help = "interval in milli seconds",
        default_value = "5000"
    )]
    batch_interval: u64,
}

#[derive(Parser, Debug)]
#[clap(name = "nat4-send")]
pub struct Nat4SendCmdArgs {
    #[clap(long_help = "content for sending")]
    content: Option<String>,

    #[clap(short = 't', long = "target", long_help = "target address")]
    target: String,

    #[clap(
        short = 'c',
        long = "count",
        long_help = "socket count",
        default_value = "64"
    )]
    count: usize,

    #[clap(long = "ttl", long_help = "set socket ttl")]
    ttl: Option<u32>,

    #[clap(
        long = "interval",
        long_help = "interval in milli seconds",
        default_value = "1000"
    )]
    interval: u64,
}
