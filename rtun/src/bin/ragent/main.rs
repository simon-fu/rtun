

use anyhow::Result;

// use clap::Parser;
// use rtun::async_rt;

// pub mod ragent;
// pub mod ragent_connect;
// pub mod ragent_listen;

pub mod ws_server_agent;

// pub mod ws_server_session;

// pub mod local_bridge;

pub mod agent_invoker;

// pub mod agent_ch_ctrl;

// pub mod agent_ch_pty;

fn main() -> Result<()> {

    // let args = CmdArgs::parse();

    // tracing_subscriber::fmt()
    // .with_max_level(tracing::metadata::LevelFilter::DEBUG)
    // .with_env_filter("rtun=debug,ragent=debug")
    // .init();

    // tracing::debug!("running ragent");

    // let _r = async_rt::run_multi_thread(async move {
    //     match args.cmd {
    //         SubCmd::Conn(args) => ragent_connect::run(args).await,
    //         SubCmd::Listen(args) => ragent_listen::run(args).await,
    //     }
    // })??;
    Ok(())
}

