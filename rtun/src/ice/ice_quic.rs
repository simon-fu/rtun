use std::{sync::Arc, time::Duration, ops::Deref};

use anyhow::{Result, Context, bail};
use async_trait::async_trait;
use bytes::{BufMut, BytesMut, Buf, Bytes};
use chrono::Local;
use parking_lot::Mutex;
use protobuf::Message;
use quinn::{ServerConfig, default_runtime, Endpoint, ClientConfig, SendStream, RecvStream, Connection, VarInt};
use quinn_proto::ConnectionStats;
use rcgen::Certificate;
use tokio::{sync::{mpsc, broadcast::{self, error::TryRecvError}}, io::{AsyncReadExt, AsyncRead, AsyncWrite}};
use tracing::debug;

use crate::{async_rt::spawn_with_name, proto::{QuicStats, QuicPathStats}};

use super::ice_peer::{IceConn, IceArgs};

use sha2::{Sha256, Digest};

#[derive(Debug, Clone)]
pub struct QuicIceArgs {
    pub ice: IceArgs,
    pub cert_der: Bytes,
}

impl QuicIceArgs {
    pub fn try_new(ice: IceArgs) -> Result<(Self, Certificate)> {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
        let cert_der = cert.serialize_der()?.into();
        Ok((
            Self { ice, cert_der, }, 
            cert,
        ))
    }
}

pub struct QuicIceCert {
    pub cert: Certificate,
    pub cert_der: Vec<u8>,
}

impl QuicIceCert {
    pub fn try_new() -> Result<Self> {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
        let cert_der = cert.serialize_der()?.into();
        Ok(Self { cert, cert_der })
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(self.cert_der.clone())
    }
}


#[async_trait]
pub trait UpgradeToQuic {
    // async fn upgrade_to_quic(self) -> Result<QuicConn>;
    async fn upgrade_to_quic(self, local_cert: &QuicIceCert, remote_cert: Bytes) -> Result<QuicConn>;
}



#[async_trait]
impl UpgradeToQuic for IceConn {
    // async fn upgrade_to_quic(self) -> Result<QuicConn> {
    //     let local_cert = QuicIceCert::try_new()?;
    //     self.upgrade_to_quic2(&local_cert, None).await
    // }

    async fn upgrade_to_quic(self, local_cert: &QuicIceCert, remote_cert: Bytes) -> Result<QuicConn> {

        let uid = self.uid();
        let is_client = self.is_client();
        // let remote_fingerprint = None;

        let remote_addr = self.remote_addr();
        let socket = self.into_async_udp();

        let server_config = {
            // let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
            let cert = &local_cert.cert;
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
            // let mut crypto = rustls::ClientConfig::builder()
            // .with_safe_defaults()
            // .with_custom_certificate_verifier(ServerFingerprintVerification::new(remote_fingerprint))
            // .with_no_client_auth();

            debug!("use remote cert len {}", remote_cert.len());
            let mut certs = rustls::RootCertStore::empty();
            certs.add(&rustls::Certificate(remote_cert.to_vec()))?;
            let client_config = ClientConfig::with_root_certificates(certs);

            // let client_config = match remote_cert {
            //     Some(remote_cert) => {
            //         debug!("use remote cert len {}", remote_cert.len());
            //         let mut certs = rustls::RootCertStore::empty();
            //         certs.add(&rustls::Certificate(remote_cert.to_vec()))?;
            //         ClientConfig::with_root_certificates(certs)
            //     },
            //     None => {
            //         let crypto = rustls::ClientConfig::builder()
            //         .with_safe_defaults()
            //         .with_custom_certificate_verifier(ServerFingerprintVerification::new(remote_fingerprint))
            //         .with_no_client_auth();
            //         ClientConfig::new(Arc::new(crypto))
            //     }
            // };
            
            endpoint.set_default_client_config(client_config);

            let conn = endpoint.connect(remote_addr, "localhost")?.await?;
            let mut keepalive = conn.open_bi().await?;

            {
                let cert = local_cert.to_bytes()?;
                send_token(&mut keepalive.0, &cert[..]).await?;
            }
            
            (conn, keepalive)
        } else {
            let conn = endpoint.accept()
            .await.with_context(||"accept but enpoint none")?
            .await?;
            let mut keepalive = conn.accept_bi().await?;

            recv_token(&mut keepalive.1, &remote_cert[..]).await?;

            (conn, keepalive)
        };

        let shared = Arc::new(Shared {
            stats: Default::default(),
        });

        let (_guard_tx, guard_rx) = mpsc::channel(1);
        let (event_tx, event_rx) = broadcast::channel(1);

        let ctx = AliveContext {
            conn: conn.clone(),
            sending_ping: Default::default(),
            recv_buf: Default::default(),
            send_buf: Default::default(),
            shared: shared.clone(),
            wr: keepalive.0,
            rd: keepalive.1,
            is_client,
            guard_rx,
            event_tx: event_tx.clone(),
        };

        
        spawn_with_name(format!("quic-alive-{uid}"), async move {
            let r = keepalive_task(ctx).await;
            debug!("finished {r:?}");
        });

        debug!("upgrade to quic");

        let conn = QuicConn { conn, shared, _guard_tx, event_rx, };
        Ok(conn)
    }
}

const TOKEN_FIRST: u8 = 0x58;

async fn send_token(stream: &mut SendStream, cert: &[u8]) -> Result<()> {

    let token = make_fingerprint(cert)?;
    let payload = token.as_bytes();

    let mut buf = BytesMut::with_capacity(3 + payload.len());

    buf.put_u8(TOKEN_FIRST);
    buf.put_u16(payload.len() as u16);
    buf.put_slice(payload);

    stream.write_all(&buf[..]).await?;

    Ok(())
}

async fn recv_token(stream: &mut RecvStream, cert: &[u8]) -> Result<()> {

    let mut header = [0_u8; 3];
    stream.read_exact(&mut header).await?;

    let mut header = &header[..];
    let first = header.get_u8();
    let payload_len = header.get_u16() as usize;

    if first != TOKEN_FIRST {
        bail!("expect first [{TOKEN_FIRST}] but [{first}]")
    }

    let mut buf = vec![0; payload_len];
    stream.read_exact(&mut buf).await?;

    let token = String::from_utf8(buf)?;

    let expect = make_fingerprint(cert)?;

    if token != expect {
        bail!("expect token [{expect}] but [{token}]")
    }

    Ok(())
}

pub struct QuicConn {
    conn: Connection,
    shared: Arc<Shared>,
    _guard_tx: mpsc::Sender<()>,
    event_rx: broadcast::Receiver<QEvent>,
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

    pub fn try_remote_stats(&mut self) -> Option<QuicStats> {
        loop {
            let r = self.event_rx.try_recv();
            match r {
                Ok(ev) => {
                    match ev {
                        QEvent::RemoteStats(stats) => return Some(stats),
                    }
                },
                Err(e) => {
                    match e {
                        TryRecvError::Empty => return None,
                        TryRecvError::Closed => return None,
                        TryRecvError::Lagged(_e) => {}, // try again
                    }
                },
            }
        }
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

#[derive(Clone)]
pub enum QEvent {
    RemoteStats(QuicStats),
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
                if ctx.is_client {
                    ctx.send_buf.clear();
                    let ping = Ping::new();
                    let len = ping.write_to_buf(&mut ctx.send_buf)?;
    
                    ctx.wr.write_all(&ctx.send_buf[..len]).await?;
                    ctx.sending_ping = true;
                    // debug!("sent ping {ping:?}, buf.len {}", ctx.send_buf.len());
                }
            }
            _r = ctx.guard_rx.recv() => {
                debug!("read guard {_r:?}");
                return Ok(())
            }
        }
    }
}


struct AliveContext {
    conn: Connection,
    sending_ping: bool,
    recv_buf: BytesMut,
    send_buf: BytesMut,
    shared: Arc<Shared>,
    wr: SendStream, 
    rd: RecvStream,
    is_client: bool,
    guard_rx: mpsc::Receiver<()>,
    event_tx: broadcast::Sender<QEvent>,
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
                    self.send_buf.clear();
                    // debug!("recv {ping:?}, sent {pong:?}, buf.len {}", self.send_buf.len());
    
                    self.send_stats().await?;
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
                Packet::Stats(packet) => {
                    let _r = self.event_tx.send(QEvent::RemoteStats(packet.stats));
                }
            }
        }
        
        Ok(())
    }

    async fn send_stats(&mut self) -> Result<()> {
        let stats = self.conn.stats();
        let stats = to_stats(&stats);
        let stats = PacketStats::new(stats);
        stats.write_to_buf(&mut self.send_buf)?;
        self.wr.write_all(&self.send_buf[..]).await?;
        self.send_buf.clear();

        Ok(())
    }

}



fn to_stats(stats: &ConnectionStats) -> QuicStats {
    QuicStats {
        path: Some(QuicPathStats {
            rtt: Some(stats.path.rtt.as_millis() as u32).into(),
            cwnd: Some(stats.path.cwnd).into(),
            ..Default::default()
        }).into(),
        ..Default::default()
    }
}



#[derive(Debug)]
struct PacketStats {
    stats: QuicStats,
}

impl PacketStats {
    fn new(stats: QuicStats) -> Self {
        Self { 
            stats,
        }
    }

    fn write_to_buf<B: BufMut>(&self, mut buf: B) -> Result<usize> {
        if buf.remaining_mut() < Self::MIN_LEN {
            bail!("buf too small for stats")
        }
        
        let payload_len = self.stats.compute_size() as u16;

        if buf.remaining_mut() < (Self::MIN_LEN + payload_len as usize) {
            bail!("buf too small for stats payload")
        }

        buf.put_u8(Self::START_BYTE);
        buf.put_u16(payload_len);
        self.stats.write_to_writer(&mut (&mut buf).writer())?;

        Ok(Self::MIN_LEN)
    }

    fn parse_from(buf: &mut BytesMut) -> Result<Option<Self>> {
        if buf.remaining() < Self::MIN_LEN {
            return Ok(None)
        }
        
        let mut data = &buf[..];
        let first = data.get_u8();
        if first != Self::START_BYTE {
            bail!("invalid start byte")
        }

        let payload_len = data.get_u16() as usize;
        if buf.remaining() < Self::MIN_LEN + payload_len {
            return Ok(None)
        }

        let stats = QuicStats::parse_from_bytes(&data[..])?;

        buf.advance(Self::MIN_LEN + payload_len);


        Ok(Some(Self { stats }))
    }

    const MIN_LEN: usize = 3;
    const START_BYTE: u8 = 0x81;
}

enum Packet {
    Ping(Ping),
    Pong(Pong),
    Stats(PacketStats),
}

impl Packet {
    // fn parse_from(data: &[u8]) -> Result<Option<Self>> {
    fn parse_from(buf: &mut BytesMut) -> Result<Option<Self>> {
        let first = buf.chunk()[0];
        match first {
            Ping::START_BYTE => {
                Ok(Ping::parse_from(buf)?.map(|x|Self::Ping(x)))
            },
            Pong::START_BYTE => {
                Ok(Pong::parse_from(buf)?.map(|x|Self::Pong(x)))
            },
            PacketStats::START_BYTE => {
                Ok(PacketStats::parse_from(buf)?.map(|x|Self::Stats(x)))
            },
            _ => {
                bail!("unknown start byte {first:02x}")
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

    fn parse_from(buf: &mut BytesMut) -> Result<Option<Self>> {
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

    fn parse_from(buf: &mut BytesMut) -> Result<Option<Self>> {
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


// struct ServerFingerprintVerification(Option<String>);

// impl ServerFingerprintVerification {
//     fn new(fingerprint: Option<String>) -> Arc<Self> {
//         Arc::new(Self(fingerprint))
//     }
// }

// impl rustls::client::ServerCertVerifier for ServerFingerprintVerification {
//     fn verify_server_cert(
//         &self,
//         _end_entity: &rustls::Certificate,
//         _intermediates: &[rustls::Certificate],
//         _server_name: &rustls::ServerName,
//         _scts: &mut dyn Iterator<Item = &[u8]>,
//         _ocsp_response: &[u8],
//         _now: std::time::SystemTime,
//     ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {

//         // let fingerprint = make_fingerprint(&end_entity.0)
//         //     .map_err(|_e|rustls::Error::General("can't make fingerprint".into()))?;
//         // debug!("got cert len {}, fingerprint {fingerprint:?}", end_entity.0.len());

//         // debug!("intermediates num {}", _intermediates.len());
//         // for (n, c) in _intermediates.iter().enumerate() {
//         //     let v = make_fingerprint(&c.0)
//         //     .map_err(|_e|rustls::Error::General("can't make intermediate fingerprint".into()))?;
//         //     debug!("intermediates[{n}]: len {}, fingerprint {v:?}", c.0.len());
//         // }

//         // if let Some(expect) = self.0.as_ref() {
//         //     if fingerprint != *expect {
//         //         debug!("expect fingerprint {expect:?} but {fingerprint:?}");
//         //         return Err(rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidYet))
//         //     }
//         // } 

//         Ok(rustls::client::ServerCertVerified::assertion())

//     }
// }

const SHA256_ALG: &str = "sha-256";

fn make_fingerprint(cert_data: &[u8]) -> Result<String> {
    make_fingerprint2(SHA256_ALG, cert_data)
}

fn make_fingerprint2(algorithm: &str, cert_data: &[u8]) -> Result<String> {
    if algorithm != SHA256_ALG {
        bail!("unsupported fingerprint algorithm [{algorithm}]")
    }

    let mut h = Sha256::new();
    h.update(cert_data);
    let hashed = h.finalize();
    
    Ok(FingerprintDisplay(&hashed[..]).to_string())
}

struct FingerprintDisplay<'a>(&'a [u8]);
impl<'a> std::fmt::Display for FingerprintDisplay<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.0.len();

        if len > 0 {
            for b in &self.0[..len-1] {
                let x = *b;
                write!(f, "{x:02x}:")?;
            }
            let x = self.0[len-1];
            write!(f, "{x:02x}")?;
        }

        Ok(())
    }
}

#[test]
fn test_make_fingerprint() {
    let cert_bytes = "12345".as_bytes();
    let s1 = make_fingerprint(cert_bytes).unwrap();
    assert_eq!(s1, "59:94:47:1a:bb:01:11:2a:fc:c1:81:59:f6:cc:74:b4:f5:11:b9:98:06:da:59:b3:ca:f5:a9:c1:73:ca:cf:c5")
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
    
    let cert1 = QuicIceCert::try_new()?;
    let cert2 = QuicIceCert::try_new()?;

    let cert_der1 = cert1.to_bytes()?;
    let cert_der2 = cert2.to_bytes()?;

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
        let conn = conn.upgrade_to_quic(&cert1, cert_der2.into()).await?;
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
        let conn = conn.upgrade_to_quic(&cert2, cert_der1.into()).await?;
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

    let cert1 = QuicIceCert::try_new()?;
    let cert2 = QuicIceCert::try_new()?;

    let cert_der1 = cert1.to_bytes()?;
    let cert_der2 = cert2.to_bytes()?;

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
        let conn = conn.upgrade_to_quic(&cert1, cert_der2.into()).await?;
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
        let conn = conn.upgrade_to_quic(&cert2, cert_der1.into()).await?;
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

#[cfg(test)]
mod test_cert {
    use super::QuicIceCert;

    #[test]
    fn test_cert() {
        let cert = QuicIceCert::try_new().unwrap();
        let data1 = cert.to_bytes().unwrap();
        let data2 = cert.to_bytes().unwrap();
        assert_eq!(data1, data2);
    }
}

