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
    let cfg = build_nat3_run_config(args)?;
    hard_nat::run_nat3(cfg).await
}

fn build_nat3_run_config(args: Nat3SendCmdArgs) -> Result<Nat3RunConfig> {
    let discover_public_addr = args.discover_public_addr || !args.stun_servers.is_empty();
    let target_ip: IpAddr = args.target.parse().with_context(|| "invalid target ip")?;
    Ok(Nat3RunConfig {
        content: args.content,
        target_ip,
        count: args.count,
        listen: args.listen,
        ttl: args.ttl,
        interval: Duration::from_millis(args.interval),
        batch_interval: Duration::from_millis(args.batch_interval),
        discover_public_addr,
        stun_servers: args.stun_servers,
    })
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

    #[clap(
        long = "discover-public-addr",
        long_help = "discover public mapped address with STUN before nat3 probing"
    )]
    discover_public_addr: bool,

    #[clap(
        long = "stun-server",
        long_help = "stun server address, eg. stun:stun.miwifi.com:3478"
    )]
    stun_servers: Vec<String>,
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

#[cfg(test)]
mod tests {
    use super::{build_nat3_run_config, CmdArgs};
    use clap::Parser;

    fn parse_cmd_args_for_test(extra: &[&str]) -> CmdArgs {
        let mut argv = vec!["rtun", "nat3", "-t", "203.0.113.10"];
        argv.extend_from_slice(extra);
        CmdArgs::try_parse_from(argv).unwrap()
    }

    #[test]
    fn nat3_cli_defaults_to_discovery_disabled_without_stun_servers() {
        let args = parse_cmd_args_for_test(&[]);
        let dump = format!("{args:?}");
        assert!(dump.contains("discover_public_addr: false"), "{dump}");
        assert!(dump.contains("stun_servers: []"), "{dump}");
    }

    #[test]
    fn nat3_cli_accepts_discovery_flag_and_multiple_stun_servers() {
        let args = parse_cmd_args_for_test(&[
            "--discover-public-addr",
            "--stun-server",
            "stun:stun.miwifi.com:3478",
            "--stun-server",
            "1.1.1.1:3478",
        ]);
        let dump = format!("{args:?}");
        assert!(dump.contains("discover_public_addr: true"), "{dump}");
        assert!(dump.contains("stun:stun.miwifi.com:3478"), "{dump}");
        assert!(dump.contains("1.1.1.1:3478"), "{dump}");
    }

    #[test]
    fn nat3_cli_treats_stun_servers_as_discovery_enabled() {
        let args = parse_cmd_args_for_test(&["--stun-server", "stun:stun.cloudflare.com:3478"]);
        let cfg = build_nat3_run_config(match args.cmd {
            super::SubCmd::Nat3(args) => args,
            super::SubCmd::Nat4(_) => unreachable!("expected nat3 subcommand"),
        })
        .unwrap();
        assert!(cfg.discover_public_addr);
        assert_eq!(cfg.stun_servers, vec!["stun:stun.cloudflare.com:3478"]);
    }
}
