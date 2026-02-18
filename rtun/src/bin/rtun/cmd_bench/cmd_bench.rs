/*
    cargo run --bin rtun --release -- bench -s 127.0.0.1:51080 -a 127.0.0.1 -p 12345
*/

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use fast_socks5::client::{Config, Socks5Stream};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tracing::{info, span, warn, Instrument, Level};

use crate::init_log_and_run;

pub fn run(args: CmdArgs) -> Result<()> {
    init_log_and_run(do_run(args))?
}

async fn do_run(args: CmdArgs) -> Result<()> {
    let buf_size = args.buffer.unwrap_or(32 * 1024);
    let duration = Duration::from_secs(args.seconds.unwrap_or(30));

    info!("buf_size [{buf_size}]");
    info!("duration [{duration:?}]");

    let mut config = Config::default();
    config.set_skip_auth(false);

    // Creating a SOCKS stream to the target address through the socks server
    let stream = Socks5Stream::connect(
        &args.socks,
        args.target_addr.clone(),
        args.target_port,
        config,
    )
    .await
    .with_context(|| format!("failed to connect to socks server [{}]", args.socks))?;

    info!("connected to socks server [{}]", args.socks);

    let (reader, mut writer) = tokio::io::split(stream);

    let read_task = {
        let span = span!(parent: None, Level::DEBUG, "read-half");
        tokio::spawn(
            async move {
                let r = reading_loop(reader, buf_size).await;
                if let Err(e) = r {
                    warn!("finished error [{e:?}]");
                }
            }
            .instrument(span),
        )
    };

    let buf = vec![0_u8; buf_size];
    let start_time = Instant::now();

    while start_time.elapsed() < duration {
        writer
            .write_all(&buf[..])
            .await
            .with_context(|| "write failed")?;
    }

    read_task.await?;

    Ok(())
}

async fn reading_loop<T: AsyncRead + Unpin>(mut stream: T, buf_size: usize) -> Result<()> {
    let mut buf = vec![0_u8; buf_size];
    let mut total_bytes = 0_u64;
    let mut last_bytes = 0_u64;
    let mut last_time = Instant::now();

    loop {
        let len = stream
            .read(&mut buf[..])
            .await
            .with_context(|| "read failed")?;
        if len == 0 {
            info!("read zero");
            break;
        }
        total_bytes += len as u64;

        let now = Instant::now();
        let elapsed = (now - last_time).as_millis() as u64;
        if elapsed >= 1500 {
            let bytes = total_bytes - last_bytes;
            let rate = 1000 * bytes / elapsed;
            info!("recv rate {}/s", indicatif::HumanBytes(rate));

            last_time = now;
            last_bytes = total_bytes;
        }
    }

    Ok(())
}

// refer https://github.com/clap-rs/clap/tree/master/clap_derive/examples
#[derive(Parser, Debug)]
#[clap(name = "echo", author, about, version)]
pub struct CmdArgs {
    #[clap(
        short = 'l',
        long = "listen",
        long_help = "listen address",
        default_value = "0.0.0.0:12345"
    )]
    listen: String,

    #[clap(short = 's', long = "socks", long_help = "socks proxy address")]
    socks: String,

    #[clap(short = 'a', long = "addr", long_help = "target domain/ip")]
    target_addr: String,

    #[clap(short = 'p', long = "port", long_help = "target port")]
    target_port: u16,

    #[clap(
        short = 'b',
        long = "buffer",
        long_help = "buffer size in unit of bytes"
    )]
    buffer: Option<usize>,

    #[clap(long = "seconds", long_help = "bench duration in seconds")]
    seconds: Option<u64>,
}
