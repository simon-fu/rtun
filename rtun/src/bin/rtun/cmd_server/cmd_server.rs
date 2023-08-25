use clap::Parser;
use anyhow::Result;


pub async fn run(_args: CmdArgs) -> Result<()> { 
    Ok(())
}

#[derive(Parser, Debug)]
#[clap(name = "server", author, about, version)]
pub struct CmdArgs {

    url: String,
}

