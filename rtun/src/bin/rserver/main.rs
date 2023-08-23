
use anyhow::Result;
use rtun::async_rt;

mod rserver;

mod agent;

mod bridge_ws;


fn main() -> Result<()> {
    tracing_subscriber::fmt()
    .with_max_level(tracing::metadata::LevelFilter::DEBUG)
    .with_env_filter("rtun=debug,rserver=debug")
    .init();

    tracing::debug!("running rserver");

    // let _r = async_rt::run_multi_thread(rserver::run())??;
    let _r = async_rt::run_multi_thread(async move {
        Result::<()>::Ok(())
    })??;
    Ok(())
}


