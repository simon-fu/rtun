
/*
    cargo run --bin rtun --release -- echo
*/

use clap::Parser;
use anyhow::{Context, Result};
use tokio::{io::{AsyncReadExt as _, AsyncWriteExt as _}, net::{TcpListener, TcpStream}};
use tracing::{info, span, warn, Instrument, Level};

use crate::init_log_and_run;



pub fn run(args: CmdArgs) -> Result<()> { 
    init_log_and_run(do_run(args))?
}

async fn do_run(args: CmdArgs) -> Result<()> {
    let buf_size = args.buffer.unwrap_or(32*1024);
    info!("buf_size [{buf_size}]");

    let listener = TcpListener::bind(&args.listen).await.with_context(||format!("failed to listen at [{}]", args.listen))?;

    info!("listening at [{}]", args.listen);

    loop {
        let (stream, from) = listener.accept().await.with_context(||"accept failed")?;
        info!("connected from [{from}]");

        let span = span!(parent: None, Level::DEBUG, "session", s=&from.to_string());
        tokio::spawn(async move {
            let r = echo_loop(stream, buf_size).await;
            if let Err(e) = r {
                warn!("finished error [{e:?}]");
            }
        }.instrument(span));
    }
}

async fn echo_loop(mut stream: TcpStream, buf_size: usize) -> Result<()> {
    let mut buf = vec![0_u8; buf_size];
    loop {
        let len = stream.read(&mut buf[..]).await.with_context(||"read failed")?;
        if len == 0 {
            tracing::debug!("read zero");
            break;
        }

        stream.write_all(&buf[..len]).await.with_context(||"write failed")?;
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

    #[clap(
        short = 'b',
        long = "buffer",
        long_help = "buffer size in unit of bytes",
    )]
    buffer: Option<usize>,
}


