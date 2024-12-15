
/*
    1. Scenario: client 
    RUST_LOG=debug cargo run --bin rtun --release -- punch --ice-server stun:stun.miwifi.com:3478 --relay-listen 0.0.0.0:13777 --echo

    2. Scenario: server
    RUST_LOG=debug cargo run --bin rtun --release -- punch -s --ice-server stun:stun.miwifi.com:3478 --relay-to 127.0.0.1:12777 --echo
    
*/



use std::{collections::HashMap, io, net::SocketAddr, sync::Arc, time::Duration};

use bytes::{Buf, BufMut, BytesMut};
use clap::Parser;
use anyhow::{bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, Input};
use tokio::{net::UdpSocket, sync::mpsc};
use tracing::{debug, info, span, Instrument, Level};
use crate::{cmd_punch::{echo::{kick_echo_client, kick_echo_server}, udp_acceptor::{new_udp_reuseport_str, new_udp_reuseport_v4}}, init_log_and_run};
use rtun::ice::ice_peer::{IceArgs, IceConfig, IcePeer};

use super::udp_acceptor::{UdpAcceptor, UdpBuf, UdpReceiver, UdpSender};


pub fn run(args: CmdArgs) -> Result<()> { 
    init_log_and_run(do_run(args))?
}

async fn do_run(args: CmdArgs) -> Result<()> {

    let mut peer = IcePeer::with_config(IceConfig {
        servers: args.ice_servers,
        ..Default::default()
    });

    let as_server = args.as_server;

    let conn = if as_server {
        let remote_str: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("server: input remote args")
        .interact_text().with_context(||"input remote args failed")?;

        let remote_args: IceArgs = serde_json::from_str(&remote_str).with_context(||"parse remote args failed")?;
        
        info!("server gathering candidate...");
        let local_args = peer.server_gather(remote_args).await.with_context(||"server_gather failed")?;
        info!("server gathering candidate done");

        info!("local args \n\n{}\n\n", serde_json::to_string(&local_args)?);
        peer.accept_timeout(Duration::from_secs(999999)).await.with_context(||"accept failed")?

    } else {
        info!("client gathering candidate...");
        let local_args = peer.client_gather().await?;
        info!("client gathering candidate done");
        
        info!("local args \n\n{}\n\n", serde_json::to_string(&local_args)?);

        let remote_str: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("client: input remote args")
        .interact_text().with_context(||"input remote args failed")?;

        let remote_args: IceArgs = serde_json::from_str(&remote_str).with_context(||"parse remote args failed")?;

        peer.dial(remote_args).await.with_context(||"dial failed")?
    };

    
    // let remote_addr = conn.remote_addr();
    let (socket, _cfg, remote_addr) = conn.into_parts();
    // socket.connect(remote_addr).await.with_context(||"udp connect failed")?;
    let tun_socket: Arc<UdpSocketConn> = Arc::new(UdpSocketConn::try_new(
        socket,
        remote_addr,
    ).await?);
    info!("punch connected [{}] -> [{}]", tun_socket.local_addr()?, remote_addr);

    // let tun_socket = Arc::new(socket);

    const HELLO: &str = "hello punch";
    if !as_server {
        let r = tun_socket.send(HELLO.as_bytes()).await;
        debug!("sent hello [{r:?}]");
    } else {
        let mut buf = vec![0_u8; 1700];
        loop {
            let len = tun_socket.recv(&mut buf[..]).await.with_context(||"recv hello failed")?;
            let packet = &buf[..len];
            if packet == HELLO.as_bytes() {
                debug!("recv hello");
                break;
            }
            tracing::warn!("expect hello but len [{}]", packet.len());
        }
    }

    
    if let Some(listen_addr) = &args.relay_listen {
        let src = new_udp_reuseport_str(listen_addr)?;
        let addr = src.local_addr()?;
        info!("listen at [{addr}]");

        if args.echo {
            kick_echo_client(addr).await.with_context(||"kick_echo_client failed")?;
        }

        let mut tun = TunRecver::new(tun_socket.clone());
        client_side_loop(src, &mut tun).await
        
    } else if let Some(target_addr) = &args.relay_to {
        let target_addr: SocketAddr = target_addr.parse().with_context(||format!("invalid target addr [{target_addr}]"))?;

        if args.echo {
            kick_echo_server(target_addr).await.with_context(||"kick_echo_server failed")?;
        }

        let mut tun = TunRecver::new(tun_socket.clone());
        server_side_loop( &mut tun, target_addr).await

    } else {

        {
            let span = span!(parent: None, Level::DEBUG, "recv-verify");
            let socket = tun_socket.clone();
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 1700];
                let mut next_seq = 0_u64;
                loop {
                    let r = socket.recv(&mut buf).await;

                    let len = match r {
                        Ok(len) => len,
                        Err(e) => {
                            tracing::warn!("recv error [{e:?}]");
                            continue;
                        },
                    };

                    if len != 8 {
                        tracing::warn!("invalid len [{len}], seq [{next_seq}]");
                        continue;
                    }

                    let seq = (&buf[..8]).get_u64();
                    if seq != next_seq {
                        tracing::warn!("expect seq [{next_seq}], but [{seq}]");
                        if seq > next_seq {
                            next_seq = seq + 1;
                        }
                        continue;
                    }
                    info!("recv seq [{seq}] ok");
                    next_seq += 1;
                    
                }
            }.instrument(span));
        }

        let mut buf = BytesMut::new();
        for n in 0..300_u64 {
            // let content = format!("msg {n}");
            // let r = tun_socket.send(content.as_bytes()).await.with_context(||"udp 
            buf.put_u64(n);
            let packet = buf.split_to(8);
            let r = tun_socket.send(&packet[..]).await.with_context(||"udp sen d failed");

            match r {
                Ok(_len) => {
                    // info!("No.{n}:  sent len [{_len}]");
                },
                Err(e) => {
                    tracing::warn!("No.{n}: send error [{e:?}]");
                },
            }
    
            // let len = socket.send_to(content.as_bytes(), remote_addr).await.with_context(||"udp sen dto failed")?;
            // info!("sent len [{len}]");
    
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    
        Ok(())
    }

}

/// 用于控制 udp 是否要 connect
struct UdpSocketConn {
    socket: UdpSocket,
}

impl UdpSocketConn {
    async fn try_new(socket: UdpSocket, target: SocketAddr) -> Result<Self> {
        socket.connect(target).await.with_context(||format!("fail to connect udp to [{target}]"))?;
        Ok(Self {
            socket,
        })
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        let addr = self.socket.local_addr()?;
        Ok(addr)
    }

    async fn send(&self, packet: &[u8]) -> Result<()> {
        self.socket.send(packet).await?;
        Ok(())
    }

    async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        let len = self.socket.recv(buf).await?;
        Ok(len)
    }

    async fn recv_buf<B: BufMut>(&self, buf: &mut B) -> Result<usize> {
        let len = self.socket.recv_buf(buf).await?;
        Ok(len)
    }
}

// /// 用于控制 udp 是否要 connect
// struct UdpSocketConn {
//     socket: UdpSocket,
//     target: SocketAddr,
// }

// impl UdpSocketConn {
//     async fn try_new(socket: UdpSocket, target: SocketAddr) -> Result<Self> {
//         Ok(Self {
//             socket,
//             target,
//         })
//     }

//     fn local_addr(&self) -> Result<SocketAddr> {
//         let addr = self.socket.local_addr()?;
//         Ok(addr)
//     }

//     async fn send(&self, packet: &[u8]) -> Result<()> {
//         self.socket.send_to(packet, self.target).await?;
//         Ok(())
//     }

//     async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
//         loop {
//             let (len, from) = self.socket.recv_from(buf).await?;
//             if from == self.target {
//                 return Ok(len)
//             }

//             tracing::warn!("recv expect from [{}] but [{from}]", self.target);
//         }
        
//     }

//     async fn recv_buf<B: BufMut>(&self, buf: &mut B) -> Result<usize> {
//         loop {
//             let (len, from) = self.socket.recv_buf_from(buf).await?;
//             if from == self.target {
//                 return Ok(len)
//             }

//             tracing::warn!("recv expect from [{}] but [{from}]", self.target);
//         }
        
//     }
// }


async fn client_side_loop(listen_socket: UdpSocket, tun: &mut TunRecver) -> Result<()> {

    let (mux_tx, mut mux_rx) = mpsc::channel::<BytesMut>(256);
    let mut acceptor = UdpAcceptor::try_new(listen_socket)?;
    
    let mut next_id = 0_u64;

    loop {
        // let buf = buf.get_mut();
        let (conn, from) = tokio::select! {
            r = acceptor.accept() => {
                r.with_context(||"accept faield")?
            }
            r = tun.recv() => {
                r?;
                tun.demux_to_session().await;
                continue;
            }
            r = mux_rx.recv() => {
                let Some(packet) = r else {
                    return Ok(());
                };

                assert!(packet.len() > META_LEN, "invalid packet len {}", packet.len());

                // let meta = parse_meta(&packet).with_context(||"client-send")?;
                // debug!("send packet len [{}], [{meta:?}]", packet.len());

                let _r = tun.socket().send(&packet[..]).await;
                
                continue;
            }
        };

        next_id += 1;
        let id = next_id;

        debug!("new udp client from [{from}], id [{id}]");
        

        let (tx, rx) = mpsc::channel(256);

        tun.add_sender(id, SessionSender {
            tx,
        });

        let (sender, recver) = conn.split();
        let (holder_tx, holder_rx) = mpsc::channel::<()>(1);

        {
            let span = span!(parent: None, Level::DEBUG, "client<-tun", id=&id);
            tokio::spawn(async move {
                let r = tun_to_client_loop(rx, sender, holder_rx).await;
                debug!("finished {r:?}");
            }.instrument(span));    
        }

        {
            let tx = mux_tx.clone();
            let span = span!(parent: None, Level::DEBUG, "client->tun", id=&id);
            tokio::spawn(async move {
                let r = client_to_tun_loop(id, recver, tx).await;
                debug!("finished {r:?}");
                drop(holder_tx);
            }.instrument(span));  
        }
        
    }
}

async fn server_side_loop(tun: &mut TunRecver, target: SocketAddr) -> Result<()> {

    let (mux_tx, mut mux_rx) = mpsc::channel::<BytesMut>(256);

    loop {

        tokio::select! {
            r = mux_rx.recv() => {
                let Some(packet) = r else {
                    break;
                };
                let _r = tun.socket().send(&packet[..]).await;
                continue;
            }
            r = tun.recv() => {
                r?;
            }
        }

        for (id, packet) in tun.demux() {
            // debug!("recv packet len [{}], id [{:?}]", packet.len(), id);

            let r = tun.send_packet_to_session((id, packet)).await;
            let Err((id, packet)) = r else {
                continue;
            };
    
            debug!("new session, id [{id}]");

            make_session(tun, target, &mux_tx, id, packet).await?;
        }
        
    }
    Ok(())
}

async fn make_session(tun: &mut TunRecver, target: SocketAddr, mux_tx: &mpsc::Sender<BytesMut>, id: u64, packet: BytesMut ) -> Result<()> {
    let (tx, rx) = mpsc::channel(256);

    let sender = SessionSender {
        tx,
    };

    let _r = sender.tx.send(packet).await;

    tun.add_sender(id, sender);

    // let socket = UdpSocket::bind("0.0.0.0:0").await.with_context(||"bind session socket failed")?;
    let socket = new_udp_reuseport_v4().with_context(||"bind session socket failed")?;
    socket.connect(target).await.with_context(||"connect to target failed")?;
    let socket = Arc::new(socket);

    let (holder_tx, holder_rx) = mpsc::channel::<()>(1);

    {
        let socket = socket.clone();
        let span = span!(parent: None, Level::DEBUG, "tun->target", id=&id);
        tokio::spawn(async move {
            let r = tun_to_target_loop(rx, &socket, holder_rx).await;
            debug!("finished {r:?}");
        }.instrument(span));    
    }

    {
        let tx = mux_tx.clone();
        let span = span!(parent: None, Level::DEBUG, "tun<-target", id=&id);
        tokio::spawn(async move {
            let r = target_to_tun_loop(id, &socket, tx).await;
            debug!("finished {r:?}");
            drop(holder_tx);
        }.instrument(span));  
    }

    Ok(())
}



struct SessionSender {
    tx: mpsc::Sender<BytesMut>,
}

struct TunRecver {
    tun_socket: Arc<UdpSocketConn>,
    senders: HashMap<u64, SessionSender>,
    buf: UdpBuf,
}

impl TunRecver {
    fn new(tun_socket: Arc<UdpSocketConn>) -> Self {
        Self {
            tun_socket,
            senders: Default::default(),
            buf: Default::default(),
        }
    }

    fn socket(&self) -> &Arc<UdpSocketConn> {
        &self.tun_socket
    }

    fn add_sender(&mut self, id: u64, sender: SessionSender) {
        self.senders.insert(id, sender);
    }

    async fn recv(&mut self) -> Result<()> {

        let buf = self.buf.get_mut();
        let len = self.tun_socket.recv_buf(buf).await.with_context(||"tun socket recv failed")?;

        check_eof(len)?;

        Ok(())
    }

    fn demux(&mut self) -> impl Iterator<Item = (u64, BytesMut)> {
        PacketIter {
            buf: self.buf.get_mut().split(),
        }
    }

    async fn demux_to_session(&mut self) {
        for v in self.demux() {
            let _r = self.send_packet_to_session(v).await;
        }
    }

    async fn send_packet_to_session(&mut self, (id, packet): (u64, BytesMut)) -> Result<(), (u64, BytesMut)> {
        match self.senders.get(&id) {
            Some(sender) => {
                let r = sender.tx.send(packet).await;
                if r.is_err() {
                    self.senders.remove(&id);
                }
                Ok(())
            },
            None => Err((id, packet)),
        }
    }
}

async fn client_to_tun_loop(id: u64, mut socket: UdpReceiver, tx: mpsc::Sender<BytesMut>) -> Result<()> {
    let mut src_buf = UdpBuf::default();

    loop {
        let buf = src_buf.get_mut();
        
        let builder = PacketBuilder::new(buf);

        let r = tokio::select! {
            r = socket.recv_buf(builder.buf) => {
                r
            }
            _r = tokio::time::sleep(TIMEOUT) => {
                return timeout();
            }
        };

        {
            let len = r.with_context(||"recv from target faield")?;
            check_eof(len)?;
        }
 

        let packet = builder.build(id);
        
        // let packet = buf.split_to(len+8);
        assert!(buf.is_empty(), "buf len {}", buf.len());
        // debug!("mux packet len [{}]", packet.len());

        let r = tx.send(packet).await;
        if r.is_err() {
            debug!("tun has closed");
            return Ok(())
        }
    }
}

async fn tun_to_client_loop(mut rx: mpsc::Receiver<BytesMut>, socket: UdpSender, mut holder_rx: mpsc::Receiver<()>) -> Result<()> {
    loop {

        let r = tokio::select! {
            r = rx.recv() => r,
            _r = holder_rx.recv() => return Ok(()),
        };

        let Some(packet) = r else {
            debug!("tun has closed");
            return Ok(())
        };

        let _r = socket.send(&packet[..]).await;
    }
}


const META_LEN: usize = 8;

struct PacketBuilder<'a> {
    buf: &'a mut BytesMut,
}

impl<'a> PacketBuilder<'a> {
    fn new(buf: &'a mut BytesMut) -> Self {
        if buf.is_empty() {
            buf.put_slice(&[0_u8; META_LEN]);
        }

        Self {
            buf,
        }
    }

    // fn buf<'s: 'a>(&'s mut self) -> &'s mut BytesMut {
    //     self.buf
    // }

    fn build(self, id: u64) -> BytesMut {
        build_meta(self.buf, id);
        self.buf.split()
    }
}

struct PacketIter {
    buf: BytesMut,
}


impl Iterator for PacketIter {
    type Item = (u64, BytesMut);

    fn next(&mut self) -> Option<Self::Item> {
        if self.buf.is_empty() {
            return None
        }
        
        let r = parse_meta(&self.buf[..]);

        let meta = match r {
            Err(e) => {
                tracing::warn!("{e:?}");
                return None
            },
            Ok(v) => v,
        };

        self.buf.advance(META_LEN);
        let packet = self.buf.split_to(meta.len);
        Some((meta.id, packet))
    }
}

#[allow(unused)]
#[derive(Debug)]
struct Meta {
    id: u64,
    len: usize,
}

fn build_meta(buf: &mut BytesMut, id: u64) {
    assert!(buf.len() >= META_LEN);
        
    let len = buf.len() - META_LEN;
    let mut meta_buf = &mut buf[..META_LEN];

    let id_bytes = id.to_be_bytes();
    meta_buf.put_slice(&id_bytes[2..]);
    
    meta_buf.put_u16(len as u16);
}

fn parse_meta(buf: &[u8]) -> Result<Meta> {
    // assert!(buf.len() >= META_LEN, "{}, {origin}", buf.len());

    if buf.len() < META_LEN {
        bail!("packet as least [{META_LEN}] but [{}]", buf.len());
    }

    let mut meta_buf = &buf[..META_LEN];
    
    let bytes = [
        0, 0, 
        meta_buf[0], meta_buf[1], meta_buf[2], meta_buf[3], meta_buf[4], meta_buf[5]
    ];

    let id = u64::from_be_bytes(bytes);
    meta_buf.advance(6);

    let len = meta_buf.get_u16() as usize;
    if len > (buf.len() - META_LEN) {
        bail!("meta.len [{len}] exceed [{}]", (buf.len() - META_LEN))
    }

    Ok(Meta {
        id,
        len: len as usize,
    })
}

// const X25: crc::Crc<u16> = crc::Crc::<u16>::new(&crc::CRC_16_IBM_SDLC);



async fn tun_to_target_loop(mut rx: mpsc::Receiver<BytesMut>, socket: &UdpSocket, mut holder_rx: mpsc::Receiver<()>) -> Result<()> {
    loop {
        let r = tokio::select! {
            r = rx.recv() => {
                r    
            }
            _r = holder_rx.recv() => {
                return Ok(())
            }
        };

        // let r = rx.recv().await;

        let Some(packet) = r else {
            debug!("tun has closed");
            return Ok(())
        };

        // debug!("packet len [{}]", packet.len());

        let _r = socket.send(&packet[..]).await;
    }
}

async fn target_to_tun_loop(id: u64, socket: &UdpSocket, tx: mpsc::Sender<BytesMut>) -> Result<()> {
    let mut src_buf = UdpBuf::default();

    loop {
        let buf = src_buf.get_mut();
        let builder = PacketBuilder::new(buf);
        // prepare_meta(buf);
        
        let r = tokio::select! {
            r = socket.recv_buf(builder.buf) => {
                r
            }
            _r = tokio::time::sleep(TIMEOUT) => {
                return timeout();
            }
        };

        let len = r.with_context(||"recv from target faield")?;
        check_eof(len)?;

        // // buf.put_u64(id);
        // put_meta(buf, id);
        
        // let packet = buf.split_to(len+META_LEN);
        let packet = builder.build(id);

        let r = tx.send(packet).await;
        if r.is_err() {
            debug!("tun has closed");
            return Ok(())
        }
    }
}

#[inline]
fn check_eof(len: usize) -> Result<()> {
    if len == 0 {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into())
    } else {
        Ok(())
    }
}

#[inline]
fn timeout() -> Result<()> {
    Err(io::Error::from(io::ErrorKind::TimedOut).into())
}

const TIMEOUT: Duration = Duration::from_secs(5*60);



// refer https://github.com/clap-rs/clap/tree/master/clap_derive/examples
#[derive(Parser, Debug)]
#[clap(name = "punch", author, about, version)]
pub struct CmdArgs {
    #[clap(
        long = "ice-server",
        long_help = "stun/turn server address, eg. stun:stun.miwifi.com:3478",
    )]
    ice_servers: Vec<String>,

    #[clap(
        short = 's',
        long_help = "run as server",
    )]
    as_server: bool,


    #[clap(
        long = "relay-listen",
        long_help = "relay proxy listen",
    )]
    relay_listen: Option<String>,

    #[clap(
        long = "relay-to",
        long_help = "relay to targeet",
    )]
    relay_to: Option<String>,

    #[clap(
        short = 'e',
        long = "echo",
        long_help = "run as server",
    )]
    echo: bool,
}



