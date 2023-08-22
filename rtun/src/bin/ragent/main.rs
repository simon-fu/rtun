
use anyhow::Result;

use rtun::async_rt;

pub mod ragent;

pub mod ws_server_agent;

pub mod ws_server_session;

pub mod agent_invoker;

pub mod agent_ch_ctrl;

pub mod agent_ch_pty;

fn main() -> Result<()> {

    tracing_subscriber::fmt()
    .with_max_level(tracing::metadata::LevelFilter::DEBUG)
    .with_env_filter("rtun=debug,ragent=debug")
    .init();

    tracing::debug!("running ragent");

    let _r = async_rt::run_multi_thread(async move {
        let r = ws_server_agent::run().await;
        r
    })??;
    Ok(())
}

