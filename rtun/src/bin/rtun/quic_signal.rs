use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{self, Poll},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use futures::{
    channel::mpsc as fmpsc,
    stream::StreamExt,
    Sink, SinkExt, Stream,
};
use protobuf::Message as PbMessage;
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use rtun::{
    channel::ChPacket,
    proto::RawPacket,
    switch::{
        switch_sink::{PacketSink, SinkError},
        switch_source::{PacketSource, StreamError, StreamPacket},
    },
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};

use crate::rest_proto::{AgentInfo, PubParams, SubParams};

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
const SIGNAL_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(3);
const SIGNAL_IDLE_TIMEOUT_MS: u32 = 30_000;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SignalRequest {
    Pub(PubParams),
    Sub(SubParams),
    Sessions,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SignalResponse {
    Pub(PubResponse),
    Sub(StatusResponse),
    Sessions(SessionsResponse),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StatusResponse {
    pub code: i32,
    pub reason: String,
}

impl StatusResponse {
    pub fn ok() -> Self {
        Self {
            code: 0,
            reason: String::new(),
        }
    }

    pub fn err(reason: impl Into<String>) -> Self {
        Self {
            code: -1,
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PubResponse {
    pub code: i32,
    pub reason: String,
    pub agent_name: Option<String>,
}

impl PubResponse {
    pub fn ok(agent_name: Option<String>) -> Self {
        Self {
            code: 0,
            reason: String::new(),
            agent_name,
        }
    }

    pub fn err(reason: impl Into<String>) -> Self {
        Self {
            code: -1,
            reason: reason.into(),
            agent_name: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionsResponse {
    pub code: i32,
    pub reason: String,
    pub sessions: Vec<AgentInfo>,
}

impl SessionsResponse {
    pub fn ok(sessions: Vec<AgentInfo>) -> Self {
        Self {
            code: 0,
            reason: String::new(),
            sessions,
        }
    }

    pub fn err(reason: impl Into<String>) -> Self {
        Self {
            code: -1,
            reason: reason.into(),
            sessions: Vec::new(),
        }
    }
}

pub async fn read_request(rx: &mut RecvStream) -> Result<SignalRequest> {
    read_json(rx).await
}

pub async fn write_response(tx: &mut SendStream, rsp: &SignalResponse) -> Result<()> {
    write_json(tx, rsp).await
}

pub async fn connect_pub(url: &url::Url) -> Result<(String, QuicSignalStream)> {
    connect_pub_with_opts(url, false).await
}

pub async fn connect_pub_with_opts(url: &url::Url, insecure: bool) -> Result<(String, QuicSignalStream)> {
    let params = parse_pub_params(url)?;
    let (endpoint, conn) = connect(url, insecure).await?;
    let (mut tx, mut rx) = conn.open_bi().await.with_context(|| "open bi stream failed")?;

    write_json(&mut tx, &SignalRequest::Pub(params.clone())).await?;

    let rsp: SignalResponse = read_json(&mut rx).await?;
    match rsp {
        SignalResponse::Pub(rsp) => {
            if rsp.code != 0 {
                bail!("pub failed [{}] {}", rsp.code, rsp.reason);
            }
            let agent_name = rsp
                .agent_name
                .or(params.agent)
                .with_context(|| "no agent name in pub response")?;
            Ok((agent_name, QuicSignalStream::new(tx, rx, Some(endpoint))))
        }
        _ => bail!("invalid pub response"),
    }
}

pub async fn connect_sub(url: &url::Url) -> Result<QuicSignalStream> {
    connect_sub_with_opts(url, false).await
}

pub async fn connect_sub_with_opts(url: &url::Url, insecure: bool) -> Result<QuicSignalStream> {
    let params = parse_sub_params(url)?;
    let (endpoint, conn) = connect(url, insecure).await?;
    let (mut tx, mut rx) = conn.open_bi().await.with_context(|| "open bi stream failed")?;

    write_json(&mut tx, &SignalRequest::Sub(params)).await?;

    let rsp: SignalResponse = read_json(&mut rx).await?;
    match rsp {
        SignalResponse::Sub(rsp) => {
            if rsp.code != 0 {
                bail!("sub failed [{}] {}", rsp.code, rsp.reason);
            }
            Ok(QuicSignalStream::new(tx, rx, Some(endpoint)))
        }
        _ => bail!("invalid sub response"),
    }
}

pub async fn query_sessions(url: &url::Url) -> Result<Vec<AgentInfo>> {
    query_sessions_with_opts(url, false).await
}

pub async fn query_sessions_with_opts(url: &url::Url, insecure: bool) -> Result<Vec<AgentInfo>> {
    let (_endpoint, conn) = connect(url, insecure).await?;
    let (mut tx, mut rx) = conn.open_bi().await.with_context(|| "open bi stream failed")?;

    write_json(&mut tx, &SignalRequest::Sessions).await?;

    let rsp: SignalResponse = read_json(&mut rx).await?;
    match rsp {
        SignalResponse::Sessions(rsp) => {
            if rsp.code != 0 {
                bail!("query sessions failed [{}] {}", rsp.code, rsp.reason);
            }
            Ok(rsp.sessions)
        }
        _ => bail!("invalid sessions response"),
    }
}

async fn connect(url: &url::Url, insecure: bool) -> Result<(Endpoint, Connection)> {
    if !url.scheme().eq_ignore_ascii_case("quic") {
        bail!("unsupported scheme [{}]", url.scheme());
    }

    let (remote, server_name) = resolve_remote(url).await?;

    let bind_addr = match remote.ip() {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };

    let mut endpoint =
        Endpoint::client(bind_addr).with_context(|| format!("bind client udp [{}] failed", bind_addr))?;
    endpoint.set_default_client_config(configure_client(insecure));

    let conn = endpoint
        .connect(remote, &server_name)
        .with_context(|| format!("connect prepare failed [{}]", remote))?
        .await
        .with_context(|| format!("connect failed [{}]", remote))?;

    Ok((endpoint, conn))
}

fn configure_client(insecure: bool) -> quinn::ClientConfig {
    let mut cfg = if insecure {
        configure_insecure_client()
    } else {
        quinn::ClientConfig::with_native_roots()
    };
    let mut transport = quinn::TransportConfig::default();
    transport
        .keep_alive_interval(Some(SIGNAL_KEEPALIVE_INTERVAL))
        .max_idle_timeout(Some(quinn::VarInt::from_u32(SIGNAL_IDLE_TIMEOUT_MS).into()));
    cfg.transport_config(Arc::new(transport));
    cfg
}

fn configure_insecure_client() -> quinn::ClientConfig {
    let crypto = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(SkipServerVerification::new())
        .with_no_client_auth();
    quinn::ClientConfig::new(Arc::new(crypto))
}

struct SkipServerVerification;

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl rustls::client::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> std::result::Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}

async fn resolve_remote(url: &url::Url) -> Result<(SocketAddr, String)> {
    let host = url.host_str().with_context(|| "missing host in quic url")?;
    let port = url.port().with_context(|| "missing port in quic url")?;
    let mut addrs = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolve [{host}:{port}] failed"))?;
    let remote = addrs.next().with_context(|| "resolved empty address list")?;
    Ok((remote, host.to_string()))
}

fn parse_pub_params(url: &url::Url) -> Result<PubParams> {
    let mut agent = None;
    let mut token = None;
    let mut expire_in = None;
    let mut ver = None;

    for (k, v) in url.query_pairs() {
        if k == "agent" {
            agent = Some(v.to_string());
        } else if k == "token" {
            token = Some(v.to_string());
        } else if k == "expire_in" {
            let value = v
                .parse::<u64>()
                .with_context(|| format!("invalid expire_in [{}]", v))?;
            expire_in = Some(value);
        } else if k == "ver" {
            ver = Some(v.to_string());
        }
    }

    let token = token.with_context(|| "missing token in quic pub url")?;
    Ok(PubParams {
        agent,
        token,
        expire_in,
        ver,
    })
}

fn parse_sub_params(url: &url::Url) -> Result<SubParams> {
    let mut agent = None;
    let mut token = None;

    for (k, v) in url.query_pairs() {
        if k == "agent" {
            agent = Some(v.to_string());
        } else if k == "token" {
            token = Some(v.to_string());
        }
    }

    let token = token.with_context(|| "missing token in quic sub url")?;
    Ok(SubParams {
        agent,
        token,
        expire_in: None,
        ver: None,
    })
}

async fn write_json<T: ?Sized + Serialize>(tx: &mut SendStream, value: &T) -> Result<()> {
    let data = serde_json::to_vec(value)?;
    if data.len() > MAX_FRAME_SIZE {
        bail!("json payload too large [{}]", data.len());
    }
    tx.write_u32(data.len() as u32).await?;
    tx.write_all(&data).await?;
    Ok(())
}

async fn read_json<T>(rx: &mut RecvStream) -> Result<T>
where
    T: for<'a> Deserialize<'a>,
{
    let data = read_frame(rx).await?;
    let value = serde_json::from_slice(&data)?;
    Ok(value)
}

async fn read_frame(rx: &mut RecvStream) -> Result<Vec<u8>> {
    let len = rx.read_u32().await?;
    let len = len as usize;
    if len > MAX_FRAME_SIZE {
        bail!("frame too large [{}]", len);
    }
    let mut data = vec![0_u8; len];
    rx.read_exact(&mut data).await?;
    Ok(data)
}

async fn write_frame(tx: &mut SendStream, data: &[u8]) -> Result<()> {
    if data.len() > MAX_FRAME_SIZE {
        bail!("frame too large [{}]", data.len());
    }
    tx.write_u32(data.len() as u32).await?;
    tx.write_all(data).await?;
    Ok(())
}

pub struct QuicSignalStream {
    sink: QuicSink,
    source: QuicSource,
}

impl QuicSignalStream {
    pub fn new(tx: SendStream, rx: RecvStream, endpoint: Option<Endpoint>) -> Self {
        let (sink_tx, sink_rx) = fmpsc::channel(256);
        let (source_tx, source_rx) = mpsc::channel(256);

        tokio::spawn(async move {
            let r = writer_task(tx, sink_rx).await;
            tracing::debug!("quic signal writer finished {r:?}");
            r
        });

        tokio::spawn(async move {
            reader_task(rx, source_tx).await;
        });

        Self {
            sink: QuicSink {
                tx: sink_tx,
                _endpoint: endpoint.clone(),
            },
            source: QuicSource {
                rx: source_rx,
                _endpoint: endpoint,
            },
        }
    }

    pub fn split(self) -> (QuicSink, QuicSource) {
        (self.sink, self.source)
    }
}

async fn writer_task(mut tx: SendStream, mut rx: fmpsc::Receiver<ChPacket>) -> Result<()> {
    while let Some(packet) = rx.next().await {
        let data = RawPacket {
            ch_id: packet.ch_id.0,
            payload: packet.payload,
            ..Default::default()
        }
        .write_to_bytes()?;
        write_frame(&mut tx, &data).await?;
    }
    let _ = tx.finish().await;
    Ok(())
}

async fn reader_task(mut rx: RecvStream, tx: mpsc::Sender<Result<StreamPacket, StreamError>>) {
    loop {
        let data = read_frame(&mut rx).await;
        match data {
            Ok(data) => {
                if tx.send(Ok(data)).await.is_err() {
                    return;
                }
            }
            Err(err) => {
                let is_eof = err
                    .downcast_ref::<std::io::Error>()
                    .map(|e| e.kind() == std::io::ErrorKind::UnexpectedEof)
                    .unwrap_or(false);
                if !is_eof {
                    let _ = tx.send(Err(err)).await;
                }
                return;
            }
        }
    }
}

pub struct QuicSource {
    rx: mpsc::Receiver<Result<StreamPacket, StreamError>>,
    _endpoint: Option<Endpoint>,
}

impl Stream for QuicSource {
    type Item = Result<StreamPacket, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl PacketSource for QuicSource {}

pub struct QuicSink {
    tx: fmpsc::Sender<ChPacket>,
    _endpoint: Option<Endpoint>,
}

impl Sink<ChPacket> for QuicSink {
    type Error = SinkError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.tx.poll_ready_unpin(cx).map_err(|e| e.into())
    }

    fn start_send(mut self: Pin<&mut Self>, item: ChPacket) -> Result<(), Self::Error> {
        self.tx.start_send_unpin(item).map_err(|e| e.into())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.tx.poll_flush_unpin(cx).map_err(|e| e.into())
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.tx.poll_close_unpin(cx).map_err(|e| e.into())
    }
}

impl PacketSink for QuicSink {}
