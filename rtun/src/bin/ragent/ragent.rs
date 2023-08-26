// use anyhow::Result;
// use clap::Parser;

// pub async fn run(args: CmdArgs) -> Result<()> {
//     tracing::debug!("{args:?}");
//     Ok(())
// }

// #[derive(Parser, Debug)]
// #[clap(name = "agent", author, about, version)]
// pub struct CmdArgs {

//     url: String,

//     #[clap(
//         short = 'b',
//         long = "bridge",
//         long_help = "run as bridge only",
//     )]
//     bridge: bool,
// }
