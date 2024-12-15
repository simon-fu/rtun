use std::{collections::{HashMap, HashSet}, net::{IpAddr, SocketAddr}, sync::Arc, time::{Duration, Instant}, u64};

use clap::Parser;
use anyhow::{Context as _, Result};
use futures::StreamExt;
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
    let target_ip: IpAddr = args.target.parse().with_context(||"invalid target ip")?;
    let interval = Duration::from_millis(args.interval);
    let batch_interval = Duration::from_millis(args.batch_interval);

    let ttl = args.ttl;

    let socket = UdpSocket::bind(&args.listen).await
        .with_context(||format!("failed to bind socket addr [{}]", args.listen))?;

    if let Some(ttl) = ttl {
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


    
    

    let mut has_recv = false;
    let mut num = 0_usize;
    let max_ports = 50000;
    let mut try_ports = HashSet::with_capacity(max_ports);

    while !has_recv {
        let start_time = Instant::now();
        let mut targets = Vec::with_capacity(args.count);

        for _ in 0..args.count {
            
            loop {
                let port = rand::thread_rng().gen_range(1024..=u16::MAX);
                
                if try_ports.len() >= max_ports {
                    try_ports.clear();
                }

                if !try_ports.contains(&port) {
                    try_ports.insert(port);
                    let target = SocketAddr::new(target_ip, port);
                    targets.push(target);
                    break;
                }
            }
            num += 1;
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

    let ttl = match args.ttl {
        Some(ttl) => Some(ttl),
        None => {
            let hops = ping_and_half_hops(target.ip()).await.with_context(||"ping_and_get_hops failed")?;
            hops
        }
    };

    // let mut send_tasks = Vec::with_capacity(args.count);
    let mut senders = Vec::with_capacity(args.count);

    for _ in 0..args.count {
        let listen = "0.0.0.0:0";
        let socket = UdpSocket::bind(listen).await
            .with_context(||format!("failed to bind socket addr [{}]", listen))?;

        let socket = Arc::new(socket);
        let local = socket.local_addr()?;

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
            let sender = UdpSender {
                socket: socket.clone(),
                // shared: shared.clone(),
                text: text.clone(),
                target,
                // interval,
                local,
                // ttl,
            };

            if let Some(ttl) = ttl {
                sender.prepare_ttl(ttl).await.with_context(||"prepare_ttl failed")?;
            }

            senders.push(sender);

            // let task = tokio::spawn(async move {
            //     // let r = send_loop(socket, target, content.as_bytes(), interval, &shared).await;
            //     let r = sender.send_loop().await;
            //     info!("send finished [{r:?}]");
            // });

            // send_tasks.push(task);
        }
    }

    // for task in send_tasks {
    //     task.await?;
    // }

    let mut has_recv = false;

    while !has_recv {
        for sender in senders.iter_mut() {
            if !shared.connecteds.lock().is_empty() {
                has_recv = true;
                break;
            }

            sender.send_one().await?;
        }

        info!("send target [{}], num [{}], ttl [{:?}]", target, senders.len(), ttl);

        tokio::time::sleep(interval).await;
    }

    shared.send_conn(&text, interval).await

}

async fn ping_and_half_hops(host: IpAddr) -> Result<Option<u32>> {
    let Some(ttl) = ping_host(host).await? else {
        return Ok(None)
    };

    info!("ping return ttl [{ttl}]");

    let hops = if ttl <= 64 {
        64 - ttl
    } else if ttl <= 128{
        128 - ttl
    } else {
        return Ok(None)
    };

    let half = hops / 2;
    if half == 0 {
        return Ok(None)
    }
    
    info!("ping return half hops [{half}]");
    Ok(Some(half))
}

async fn ping_host(host: IpAddr) -> Result<Option<u32>> {
    info!("try ping host [{host}]...");

    let payload = [0; 8];

    let config = match host {
        IpAddr::V4(_) => surge_ping::Config::default(),
        IpAddr::V6(_) => surge_ping::Config::builder().kind(surge_ping::ICMP::V6).build(),
    };
    let client = surge_ping::Client::new(&config)?;
    
    let mut futs = futures::stream::FuturesUnordered::new();
    for seq in 0..3 {
        let client = client.clone();
        futs.push(async move {
            let mut pinger = client.pinger(host, surge_ping::PingIdentifier(rand::random())).await;
            pinger.ping(surge_ping::PingSequence(seq), &payload).await
        });

        // let r = pinger.ping(surge_ping::PingSequence(seq), &payload).await;
    }
    
    while let Some(r) = futs.next().await {
        info!("ping result: {r:?}");
        match r {
            Ok((packet, _d)) => {
                let ttl = match packet {
                    surge_ping::IcmpPacket::V4(p) => p.get_ttl().map(|x|x as u32),
                    surge_ping::IcmpPacket::V6(p) => Some(p.get_max_hop_limit() as u32),
                };
                if let Some(ttl) = ttl {
                    return Ok(Some(ttl))
                }
            },
            Err(_) => {},
        }
    }

    Ok(None)
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

struct UdpSender {
    socket: Arc<UdpSocket>, 
    target: SocketAddr, 
    text: Arc<String>, 
    // interval: Duration, 
    // shared: Arc<Shared>,
    local: SocketAddr,
    // ttl: Option<u32>,
}

impl UdpSender {
    // async fn send_loop(&self) -> Result<()> {
    //     let local = self.socket.local_addr().with_context(||"get local address failed")?;
    //     loop {
    //         {
    //             if !self.shared.connecteds.lock().is_empty() {
    //                 return Ok(())
    //             }
    //         }
    //         let len = self.socket.send_to(self.text.as_bytes(), self.target).await.with_context(||"send_to failed")?;
    //         info!("sent to [{local}] => [{}]: bytes [{len}]", self.target);
    //         tokio::time::sleep(self.interval).await;
    //     }
    // }

    async fn send_one(&self) -> Result<()> {
        let len = self.socket.send_to(self.text.as_bytes(), self.target).await.with_context(||"send_to failed")?;
        debug!("sent to [{}] => [{}]: bytes [{len}]", self.local, self.target);
        Ok(())
    }

    async fn prepare_ttl(&self, max_ttl: u32) -> Result<()> {
        // let local = self.socket.local_addr().with_context(||"get local address failed")?;
        for ttl in 1..=max_ttl {
            self.socket.set_ttl(ttl).with_context(||format!("failed to set_ttl [{ttl}]"))?;
            self.send_one().await?;
            // let len = self.socket.send_to(self.text.as_bytes(), self.target).await.with_context(||"send_to failed")?;
            // info!("prepare ttl [{ttl}]: [{local}] => [{}]: bytes [{len}]", self.target);
        }
        Ok(())
    }
}

// async fn send_loop(socket: Arc<UdpSocket>, target: SocketAddr, content: &[u8], interval: Duration, shared: &Arc<Shared>) -> Result<()> {
//     let local = socket.local_addr().with_context(||"get local address failed")?;
//     loop {
//         {
//             if !shared.connecteds.lock().is_empty() {
//                 return Ok(())
//             }
//         }
//         let len = socket.send_to(content, target).await.with_context(||"send_to failed")?;
//         info!("sent to [{local}] => [{target}]: bytes [{len}]");
//         tokio::time::sleep(interval).await;
//     }
// }

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
        socket.set_ttl(64).with_context(||"set_ttl 64 failed")?;
        
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