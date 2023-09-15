use std::{sync::Arc, time::Duration, ops::Deref};

use anyhow::{Result, Context, bail};
use async_trait::async_trait;
use bytes::{BufMut, BytesMut, Buf};
use chrono::Local;
use parking_lot::Mutex;
use quinn::{ServerConfig, default_runtime, Endpoint, ClientConfig, SendStream, RecvStream, Connection, VarInt};
use tokio::{sync::mpsc, io::{AsyncReadExt, AsyncRead, AsyncWrite}};
use tracing::debug;

use crate::async_rt::spawn_with_name;

use super::ice_peer::IceConn;

#[async_trait]
pub trait UpgradeToQuic {
    async fn upgrade_to_quic(self) -> Result<QuicConn>;
}

#[async_trait]
impl UpgradeToQuic for IceConn {
    async fn upgrade_to_quic(self) -> Result<QuicConn> {
        let uid = self.uid();
        let is_client = self.is_client();
        let local_cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
        let remote_fingerprint = None;

        let remote_addr = self.remote_addr();
        let socket = self.into_async_udp();

        let server_config = {
            // let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
            let cert = &local_cert;
            let cert_der = cert.serialize_der()?;
            let priv_key = cert.serialize_private_key_der();
            let priv_key = rustls::PrivateKey(priv_key);
            let cert_chain = vec![rustls::Certificate(cert_der.clone())];
        
            let mut server_config = ServerConfig::with_single_cert(cert_chain, priv_key)?;
            Arc::get_mut(&mut server_config.transport).with_context(||"get transport config failed")?
            .max_concurrent_uni_streams(0_u8.into())
            .max_idle_timeout(Some(VarInt::from_u32(90_000).into()));
        
            server_config
        };


        let runtime = default_runtime().with_context(||"no quic async runtime")?;

        let mut endpoint = Endpoint::new_with_abstract_socket(
            Default::default(), 
            Some(server_config), 
            socket, 
            runtime,
        )?;

        
        let (conn, keepalive) = if is_client {
            let crypto = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(ServerFingerprintVerification::new(remote_fingerprint))
            .with_no_client_auth();
            
            endpoint.set_default_client_config(ClientConfig::new(Arc::new(crypto)));

            let conn = endpoint.connect(remote_addr, "localhost")?.await?;
            let keepalive = conn.open_bi().await?;
            (conn, keepalive)
        } else {
            let conn = endpoint.accept()
            .await.with_context(||"accept but enpoint none")?
            .await?;
            let keepalive = conn.accept_bi().await?;
            (conn, keepalive)
        };

        let shared = Arc::new(Shared {
            stats: Default::default(),
        });

        let (_guard_tx, guard_rx) = mpsc::channel(1);

        let ctx = AliveContext {
            sending_ping: Default::default(),
            recv_buf: Default::default(),
            send_buf: Default::default(),
            shared: shared.clone(),
            wr: keepalive.0,
            rd: keepalive.1,
            _is_client: is_client,
            guard_rx
        };

        
        spawn_with_name(format!("quic-alive-{uid}"), async move {
            let r = keepalive_task(ctx).await;
            debug!("finished {r:?}");
        });

        debug!("upgrade to quic");

        let conn = QuicConn { conn, shared, _guard_tx };
        Ok(conn)
    }
}

pub struct QuicConn {
    conn: Connection,
    shared: Arc<Shared>,
    _guard_tx: mpsc::Sender<()>,
}

impl Deref for QuicConn {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        // let r = self.conn.peer_identity();
        // let stats = self.conn.stats();
        &self.conn
    }
}

impl QuicConn {
    pub fn get_stats(&self) -> Result<Stats> {
        
        let mut stats = {
            self.shared.stats.lock().clone()
        };
        
        let inner = self.conn.stats();
        stats.tx_bytes = inner.udp_tx.bytes;
        stats.rx_bytes = inner.udp_rx.bytes;

        Ok(stats)
    }

    pub fn is_ping_timeout(&self) -> bool {
        let update_ts = {
            self.shared.stats.lock().update_ts
        };

        let elapsed_ms = Local::now().timestamp_millis() - update_ts;
        Duration::from_millis(elapsed_ms as u64) >= (ping_interval() * 5/2)
    }

    pub fn ping_interval(&self) -> Duration {
        ping_interval()
    }
}

fn ping_interval() -> Duration {
    const ALIVE_INTERVAL_MILLI: u64 = 5000;
    Duration::from_millis(ALIVE_INTERVAL_MILLI)
}

struct Shared {
    stats: Mutex<Stats>,
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub latency: i64,
    pub update_ts: i64,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
}




async fn keepalive_task(mut ctx: AliveContext) -> Result<()> {
    let mut interval = tokio::time::interval(ping_interval());

    loop {
        tokio::select! {
            r = ctx.rd.read_buf(&mut ctx.recv_buf) => {
                let n = r.with_context(||"stream closed")?;
                if n == 0 {
                    debug!("read zero");
                    return Ok(())
                }
                ctx.process_packet().await?;
            }
            _r = interval.tick(), if !ctx.sending_ping => {
                ctx.send_buf.clear();

                let ping = Ping::new();
                let len = ping.write_to_buf(&mut ctx.send_buf)?;

                ctx.wr.write_all(&ctx.send_buf[..len]).await?;
                ctx.sending_ping = true;
                // debug!("sent ping {ping:?}, buf.len {}", ctx.send_buf.len());
            }
            _r = ctx.guard_rx.recv() => {
                debug!("read guard {_r:?}");
                return Ok(())
            }
        }
    }
}


struct AliveContext {
    sending_ping: bool,
    recv_buf: BytesMut,
    send_buf: BytesMut,
    shared: Arc<Shared>,
    wr: SendStream, 
    rd: RecvStream,
    _is_client: bool,
    guard_rx: mpsc::Receiver<()>,
}

impl AliveContext {

    async fn process_packet(&mut self) -> Result<()> {
        // let data = &self.recv_buf[..];
        
        while !self.recv_buf.is_empty() {

            let packet = {
                match Packet::parse_from(&mut self.recv_buf)? {
                    Some(v) => v,
                    None => break,
                }
            };
            
            match packet {
                Packet::Ping(ping) => {
                    self.send_buf.clear();
                    let pong = Pong::new(ping.req_ts);
                    let len = pong.write_to_buf(&mut self.send_buf)?;
                    self.wr.write_all(&self.send_buf[..len]).await?;
                    // debug!("recv {ping:?}, sent {pong:?}, buf.len {}", self.send_buf.len());
    
                },
                Packet::Pong(pong) => {
                    let now = Local::now().timestamp_millis();
                    let latency = now - pong.req_ts;
                    self.sending_ping = false;
                    debug!("latency {latency} ms");
                    // debug!("recv {pong:?}, now {now}, latency {latency} ms");
                    let mut stats = self.shared.stats.lock();
                    stats.latency = latency;
                    stats.update_ts = now;
                },
            }
        }
        
        
    
        Ok(())
    }
}



enum Packet {
    Ping(Ping),
    Pong(Pong),
}

impl Packet {
    // fn parse_from(data: &[u8]) -> Result<Option<Self>> {
    fn parse_from<B: Buf>(buf: B) -> Result<Option<Self>> {
        let first = buf.chunk()[0];
        match first {
            Ping::START_BYTE => {
                Ok(Ping::parse_from(buf)?.map(|x|Self::Ping(x)))
            },
            Pong::START_BYTE => {
                Ok(Pong::parse_from(buf)?.map(|x|Self::Pong(x)))
            },
            _ => {
                bail!("unknown start byte")
            }
        }

    }
}

#[derive(Debug)]
struct Ping {
    req_ts: i64,
}

impl Ping {
    fn new() -> Self {
        Self { 
            req_ts: Local::now().timestamp_millis(),
        }
    }

    fn write_to_buf<B: BufMut>(&self, mut buf: B) -> Result<usize> {
        if buf.remaining_mut() < Self::MIN_LEN {
            bail!("buf too small for ping")
        }

        buf.put_u8(Self::START_BYTE);
        buf.put_i64(self.req_ts);

        Ok(Self::MIN_LEN)
    }

    fn parse_from<B: Buf>(mut buf: B) -> Result<Option<Self>> {
        if buf.remaining() < Self::MIN_LEN {
            return Ok(None)
        }
        
        let first = buf.get_u8();
        if first != Self::START_BYTE {
            bail!("invalid start byte")
        }

        let req_ts = buf.get_i64();
        Ok(Some(Self { req_ts }))
    }

    const MIN_LEN: usize = 9;
    const START_BYTE: u8 = 0x79;
}

#[derive(Debug)]
struct Pong {
    req_ts: i64,
    rsp_ts: i64,
}

impl Pong {
    fn new(req_ts: i64) -> Self {
        Self { 
            rsp_ts: Local::now().timestamp_millis(),
            req_ts,
        }
    }

    fn write_to_buf<B: BufMut>(&self, mut buf: B) -> Result<usize> {
        if buf.remaining_mut() < Self::MIN_LEN {
            bail!("buf too small for pong")
        }

        buf.put_u8(Self::START_BYTE);
        buf.put_i64(self.req_ts);
        buf.put_i64(self.rsp_ts);

        Ok(Self::MIN_LEN)
    }

    fn parse_from<B: Buf>(mut buf: B) -> Result<Option<Self>> {
        if buf.remaining() < Self::MIN_LEN {
            return Ok(None)
        }
        
        let first = buf.get_u8();
        if first != Self::START_BYTE {
            bail!("invalid start byte")
        }

        let req_ts = buf.get_i64();
        let rsp_ts = buf.get_i64();
        Ok(Some(Self { req_ts, rsp_ts }))
    }

    const MIN_LEN: usize = 17;
    const START_BYTE: u8 = 0x97;
}


struct ServerFingerprintVerification(Option<String>);

impl ServerFingerprintVerification {
    fn new(fingerprint: Option<String>) -> Arc<Self> {
        Arc::new(Self(fingerprint))
    }
}

impl rustls::client::ServerCertVerifier for ServerFingerprintVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        // if let Some(expect) = self.0.as_ref() {
        //     let fingerprint = make_fingerprint(SHA256_ALG, &end_entity.0)
        //     .map_err(|_e|rustls::Error::General("can't get fingerprint".into()))?;
        //     if fingerprint != *expect {
        //         debug!("expect fingerprint {expect:?} but {fingerprint:?}");
        //         return Err(rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidYet))
        //     }
        // } 
        Ok(rustls::client::ServerCertVerified::assertion())

    }
}

pub struct QuicStream{
    pub tx: SendStream,
    pub rx: RecvStream,
}

impl QuicStream {
    pub fn new(pair: (SendStream, RecvStream)) -> Self {
        Self { tx: pair.0, rx: pair.1 }
    }
}

impl AsyncRead for QuicStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.rx)
        .poll_read(cx, buf)
    }
}

impl AsyncWrite for QuicStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::result::Result<usize, std::io::Error>> {
        std::pin::Pin::new(&mut self.tx)
        .poll_write(cx, buf)
    }

    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::result::Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.tx)
        .poll_flush(cx)
    }

    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::result::Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.tx)
        .poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::result::Result<usize, std::io::Error>> {
        std::pin::Pin::new(&mut self.tx)
        .poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.tx.is_write_vectored()
    }
}


#[tokio::test]
async fn manual_test_ice_quic_drop() -> Result<()> {
    use crate::async_rt::spawn_with_name;
    use crate::ice::ice_peer::{IcePeer, IceConfig};

    tracing_subscriber::fmt()
    .with_max_level(tracing::Level::INFO)
    .with_env_filter(tracing_subscriber::EnvFilter::from("rtun=debug"))
    .with_target(false)
    .init();

    let servers = vec![ ];

    let mut peer1 = IcePeer::with_config(IceConfig {
        servers: servers.clone(),
        ..Default::default()
    });

    let arg1 = peer1.client_gather().await?;
    debug!("arg1 {arg1:?}");

    let mut peer2 = IcePeer::with_config(IceConfig {
        servers: servers.clone(),
        ..Default::default()
    });
    
    let arg2 = peer2.server_gather(arg1).await?;
    debug!("arg2 {arg2:?}");


    let task1 = spawn_with_name(format!("client-{}", peer1.uid()), async move {
        let conn = peer1.dial(arg2).await?;
        let conn = conn.upgrade_to_quic().await?;
        let (mut wr, mut rd) = conn.open_bi().await?;
        wr.write_all("I'am conn1".as_bytes()).await?;
        let mut buf = vec![0; 1700];
        loop {
            let n = rd.read(&mut buf).await
            .with_context(||"read stream failed")?
            .with_context(||"stream closed")?;
            if n == 0 {
                debug!("recv zero");
                break;
            }
            let msg = std::str::from_utf8(&buf[..n])?;
            debug!("recv {msg:?}");
        }

        Result::<()>::Ok(())
    });

    let task2 = spawn_with_name(format!("server-{}", peer2.uid()), async move {
        let conn = peer2.accept().await?;
        let conn = conn.upgrade_to_quic().await?;
        let (mut wr, mut _rd) = conn.accept_bi().await?;
        wr.write_all("I'am conn2".as_bytes()).await?;

        Result::<()>::Ok(())
    });

    let r1 = task1.await?;
    let r2 = task2.await?;

    debug!("task1 finished {r1:?}");
    debug!("task2 finished {r2:?}");

    r1?;
    r2?;

    Ok(())
}


#[tokio::test]
async fn manual_test_ice_quic() -> Result<()> {
    use crate::async_rt::spawn_with_name;
    use crate::ice::ice_peer::{IcePeer, IceConfig};

    tracing_subscriber::fmt()
    .with_max_level(tracing::Level::INFO)
    .with_env_filter(tracing_subscriber::EnvFilter::from("rtun=debug"))
    .with_target(false)
    .init();

    let servers = vec![
        // "stun:stun1.l.google.com:19302".into(),
        // "stun:stun2.l.google.com:19302".into(),
        // "stun:stun.qq.com:3478".into(),
    ];

    let mut peer1 = IcePeer::with_config(IceConfig {
        servers: servers.clone(),
        ..Default::default()
    });

    let arg1 = peer1.client_gather().await?;
    debug!("arg1 {arg1:?}");

    let mut peer2 = IcePeer::with_config(IceConfig {
        servers: servers.clone(),
        ..Default::default()
    });
    
    let arg2 = peer2.server_gather(arg1).await?;
    debug!("arg2 {arg2:?}");


    let task1 = spawn_with_name("client", async move {
        let conn = peer1.dial(arg2).await?;
        let conn = conn.upgrade_to_quic().await?;
        let (mut wr, mut rd) = conn.open_bi().await?;
        wr.write_all("I'am conn1".as_bytes()).await?;
        let mut buf = vec![0; 1700];
        loop {
            let n = rd.read(&mut buf).await
            .with_context(||"read stream failed")?
            .with_context(||"stream closed")?;
            if n == 0 {
                break;
            }
            let msg = std::str::from_utf8(&buf[..n])?;
            debug!("recv {msg:?}");
        }

        Result::<()>::Ok(())
    });

    let task2 = spawn_with_name("server", async move {
        let conn = peer2.accept().await?;
        let conn = conn.upgrade_to_quic().await?;
        let (mut wr, mut rd) = conn.accept_bi().await?;
        wr.write_all("I'am conn2".as_bytes()).await?;
        let mut buf = vec![0; 1700];

        loop {
            let n = rd.read(&mut buf).await
            .with_context(||"read stream failed")?
            .with_context(||"stream closed")?;
            if n == 0 {
                break;
            }
            let msg = std::str::from_utf8(&buf[..n])?;
            debug!("recv {msg:?}");
        }
        Result::<()>::Ok(())
    });

    let r1 = task1.await?;
    let r2 = task2.await?;

    debug!("task1 finished {r1:?}");
    debug!("task2 finished {r2:?}");

    r1?;
    r2?;

    Ok(())
}

