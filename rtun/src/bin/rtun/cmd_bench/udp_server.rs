use anyhow::Result;
use clap::Parser;
use tracing::info;

pub async fn run(args: CmdArgs) -> Result<()> {
    info!("udp bench server skeleton: listen [{}]", args.listen);
    Ok(())
}

#[derive(Parser, Debug)]
#[clap(name = "udp-server", author, about, version)]
pub struct CmdArgs {
    #[clap(long = "listen", default_value = "0.0.0.0:9001")]
    pub listen: String,
}
