
use anyhow::Result;
// use rtun::async_rt;

// pub mod test_tokio;

// pub mod test_async_process;

// pub mod test_portable_pty;

// pub mod test_pty_process;

// pub mod test_tokio_pty_process;

// pub mod rclient;

// pub mod client_invoker;

// pub mod client_ch_pty;

// pub mod ws_client_session;

// pub mod term_crossterm;

// pub mod term_std;

// pub mod term_termwiz;

// Avoid musl's default allocator due to lackluster performance
// https://nickb.dev/blog/default-musl-allocator-considered-harmful-to-performance
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<()> {
    // tracing_subscriber::fmt()
    // .with_max_level(tracing::metadata::LevelFilter::DEBUG)
    // .with_env_filter("rtun=debug,rclient=debug")
    // .init();

    // let _r = async_rt::run_multi_thread(async move {
    //     // let r = test_tokio::test_tokio_main().await;
    //     // let r = test_async_process::test_main().await;
    //     // let r = test_pty_process::test_main().await;
    //     // let r = test_tokio_pty_process::test_main().await;
    //     let r = rclient::run().await;
    //     tracing::debug!("main finshed with {:?}", r);
    //     r
    // })??;
    Ok(())
}

