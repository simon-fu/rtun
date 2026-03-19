use anyhow::Result;
use clap::Parser;
use tracing::info;

use super::udp_perf::UdpPerfMode;

pub async fn run(args: CmdArgs) -> Result<()> {
    info!(
        "udp bench client skeleton: target [{}], mode [{}], time [{}], len [{}]",
        args.target, args.mode, args.time, args.len
    );
    Ok(())
}

#[derive(Parser, Debug)]
#[clap(name = "udp-client", author, about, version)]
pub struct CmdArgs {
    #[clap(long = "target")]
    pub target: String,

    #[clap(long = "mode", value_enum, default_value_t = UdpPerfMode::Forward)]
    pub mode: UdpPerfMode,

    #[clap(long = "time", default_value_t = 10)]
    pub time: u64,

    #[clap(long = "len", default_value_t = 1200)]
    pub len: usize,
}
