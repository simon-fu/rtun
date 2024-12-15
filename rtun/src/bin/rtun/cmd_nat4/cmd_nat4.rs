use std::{collections::HashMap, net::{IpAddr, SocketAddr}, sync::Arc, time::{Duration, Instant}, u64};

use clap::Parser;
use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use rand::Rng as _;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

pub fn run(args: CmdArgs) -> Result<()> { 
    crate::init_log_and_run(do_run(args))?
}

async fn do_run(args: CmdArgs) -> Result<()> {
    match args.cmd {
        SubCmd::Nat3(args) => run_nat3(args).await,
        SubCmd::Nat4(args) => run_nat4(args).await,
    }
}

async fn run_nat3(args: Nat3SendCmdArgs) -> Result<()> { 
    let socket = UdpSocket::bind(&args.listen).await
        .with_context(||format!("failed to bind socket addr [{}]", args.listen))?;

    if let Some(ttl) = args.ttl {
        socket.set_ttl(ttl).with_context(||"set ttl failed")?;
        info!("set ttl [{ttl}]")
    }

    let socket = Arc::new(socket);
    let local = socket.local_addr().with_context(||"get local address failed")?;

    let text = Arc::new(
        args.content.as_deref()
            .map(|x|x)
            .unwrap_or(TEXT)
            .to_string()
    );

    let shared = Arc::new(Shared {
        connecteds: Default::default(),
    });

    {
        let socket = socket.clone();
        let text = text.clone();
        let shared = shared.clone();

        tokio::spawn(async move {
            let r = recv_loop(socket, text.as_str(), &shared).await;
            info!("recv finished [{r:?}]");
        });
    }


    let target_ip: IpAddr = args.target.parse().with_context(||"invalid target ip")?;
    let interval = Duration::from_millis(args.interval);
    let batch_interval = Duration::from_millis(args.batch_interval);
    

    let mut has_recv = false;
    let mut num = 0_usize;

    while !has_recv {
        let start_time = Instant::now();
        let mut targets = Vec::with_capacity(args.count);
        for _ in 0..args.count {
            num += 1;
            let port = rand::thread_rng().gen_range(1024..=u16::MAX);
            let target = SocketAddr::new(target_ip, port);
            targets.push(target);
        }

        while start_time.elapsed() < batch_interval {
            for target in targets.iter() {
                
                {
                    has_recv = !shared.connecteds.lock().is_empty();
                    if has_recv {
                        break;
                    }
                }

                let sent_bytes = socket.send_to(text.as_bytes(), target).await
                .with_context(||"send failed")?;
                // info!("sent bytes {sent_bytes}");
                if sent_bytes == text.as_bytes().len() {
                    debug!("=> [{target}, {sent_bytes}]: [{text}]");
                } else {
                    warn!("No.{num}: sent partial {sent_bytes} < {}", text.as_bytes().len());
                }
            }
            
            if has_recv {
                break;
            }

            info!("sent num [{num}]: [{local}] => [{target_ip}]");
            tokio::time::sleep(interval).await;
        }
    }

    shared.send_conn(&text, interval).await
    
}

async fn run_nat4(args: Nat4SendCmdArgs) -> Result<()> { 
    let target: SocketAddr = args.target.parse().with_context(||"invalid target address")?;
    let interval = Duration::from_millis(args.interval);
    
    let text = Arc::new(
        args.content.as_deref()
            .map(|x|x)
            .unwrap_or(TEXT)
            .to_string()
    );

    let shared = Arc::new(Shared {
        connecteds: Default::default(),
    });

    let mut send_tasks = Vec::with_capacity(args.count);

    for _ in 0..args.count {
        let listen = "0.0.0.0:0";
        let socket = UdpSocket::bind(listen).await
            .with_context(||format!("failed to bind socket addr [{}]", listen))?;

        if let Some(ttl) = args.ttl {
            socket.set_ttl(ttl).with_context(||"set ttl failed")?;
            info!("set ttl [{ttl}]")
        }

        let socket = Arc::new(socket);

        {
            let socket = socket.clone();
            let text = text.clone();
            let shared = shared.clone();

            tokio::spawn(async move {
                let r = recv_loop(socket, text.as_str(), &shared).await;
                info!("recv finished [{r:?}]");
            });
        }

        {
            let socket = socket.clone();
            let content = text.clone();
            let shared = shared.clone();

            let task = tokio::spawn(async move {
                let r = send_loop(socket, target, content.as_bytes(), interval, &shared).await;
                info!("send finished [{r:?}]");
            });

            send_tasks.push(task);
        }
    }

    for task in send_tasks {
        task.await?;
    }

    shared.send_conn(&text, interval).await

}

async fn recv_loop(socket: Arc<UdpSocket>, text: &str, shared: &Arc<Shared>) -> Result<()> {
    let local = socket.local_addr().with_context(||"get local address failed")?;

    let mut buf = vec![0_u8; 1700];
    loop {
        let (len, from) = socket.recv_from(&mut buf).await.with_context(||"recv_from failed")?;
        let packet = &buf[..len];
        if packet == text.as_bytes() {
            let old = shared.connecteds.lock().insert(from, socket.clone());
            info!("recv text [{local}] <= [{from}], text [{text}]");
            if old.is_none() {
                info!("connected from target [{from:?}]");
            }
        } else {
            info!("recv unknown [{local}] <= [{from}], bytes [{len}]");
        }
        
    }
}

async fn send_loop(socket: Arc<UdpSocket>, target: SocketAddr, content: &[u8], interval: Duration, shared: &Arc<Shared>) -> Result<()> {
    let local = socket.local_addr().with_context(||"get local address failed")?;
    loop {
        {
            if !shared.connecteds.lock().is_empty() {
                return Ok(())
            }
        }
        let len = socket.send_to(content, target).await.with_context(||"send_to failed")?;
        info!("sent to [{local}] => [{target}]: bytes [{len}]");
        tokio::time::sleep(interval).await;
    }
}

struct Shared {
    connecteds: Mutex<HashMap<SocketAddr, Arc<UdpSocket>>>,
}

impl Shared {
    async fn send_conn(&self, text: &str, interval: Duration) -> Result<()> {
        let from_addrs = {
            self.connecteds.lock().clone()
        };
    
        info!("connected targets {from_addrs:?}");

        let Some((target, socket)) = from_addrs.iter().next() else {
            return Ok(())
        };

        let local = socket.local_addr().with_context(||"get local address faield")?;
        
        loop {

            let sent_bytes = socket.send_to(text.as_bytes(), target).await
                .with_context(||"send failed")?;
        
            info!("send conn: [{local} => {target}, {sent_bytes}]: [{text}]");
    
            tokio::time::sleep(interval).await;
        }
    }
}


#[derive(Parser, Debug)]
#[clap(name = "nat4")]
pub struct CmdArgs {
    #[clap(subcommand)]
    cmd: SubCmd,
}

#[derive(Parser, Debug)]
enum SubCmd {
    Nat3(Nat3SendCmdArgs),
    Nat4(Nat4SendCmdArgs),
}

#[derive(Parser, Debug)]
#[clap(name = "nat3-send")]
pub struct Nat3SendCmdArgs {
    #[clap(
        long_help = "content for sending",
    )]
    content: Option<String>,

    #[clap(
        short = 't',
        long = "target",
        long_help = "target ip",
    )]
    target: String,

    #[clap(
        short = 'c',
        long = "count",
        long_help = "target port count",
        default_value = "64",
    )]
    count: usize,

    #[clap(
        short = 'l',
        long = "listen",
        long_help = "listen address",
        default_value = "0.0.0.0:0",
    )]
    listen: String,

    #[clap(
        long = "ttl",
        long_help = "set socket ttl",
    )]
    ttl: Option<u32>,

    #[clap(
        long = "interval",
        long_help = "interval in milli seconds",
        default_value = "1000",
    )]
    interval: u64,

    #[clap(
        long = "batch-interval",
        long_help = "interval in milli seconds",
        default_value = "5000",
    )]
    batch_interval: u64,
}

#[derive(Parser, Debug)]
#[clap(name = "nat4-send")]
pub struct Nat4SendCmdArgs {
    #[clap(
        long_help = "content for sending",
    )]
    content: Option<String>,

    #[clap(
        short = 't',
        long = "target",
        long_help = "target address",
    )]
    target: String,

    #[clap(
        short = 'c',
        long = "count",
        long_help = "socket count",
        default_value = "64",
    )]
    count: usize,

    #[clap(
        long = "ttl",
        long_help = "set socket ttl",
    )]
    ttl: Option<u32>,

    #[clap(
        long = "interval",
        long_help = "interval in milli seconds",
        default_value = "1000",
    )]
    interval: u64,
}


const TEXT: &'static str = "nat hello";