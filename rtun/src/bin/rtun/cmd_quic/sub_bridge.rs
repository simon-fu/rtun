use bytes::{Buf, BufMut, BytesMut};
use clap::Parser;
use parking_lot::Mutex;
use quinn::{Connection, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tracing::{debug, info};

use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use rtun::{
    async_rt::spawn_with_name,
    huid::{gen_huid::gen_huid, HUId},
};

pub async fn run(args: CmdArgs) -> Result<()> {
    let listen_str = args.addr;

    let listen_addr = tokio::net::lookup_host(&listen_str)
        .await
        .with_context(|| format!("fail to lookup listen addr [{listen_str}]"))?
        .next()
        .with_context(|| format!("got empty when lookup listen addr [{listen_str}]"))?;

    let mut server_crypto =
        try_load_quic_cert(args.https_key.as_deref(), args.https_cert.as_deref())
            .await
            .with_context(|| "load quic key/cert file failed")?
            .with_context(|| "need key/cert for quic")?;

    const ALPN_QUIC_HTTP: &[&[u8]] = &[b"hq-29"];
    server_crypto.alpn_protocols = ALPN_QUIC_HTTP.iter().map(|&x| x.into()).collect();

    // server_crypto.key_log = Arc::new(rustls::KeyLogFile::new());

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));

    // server_config.use_retry(true);

    let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
    transport_config.max_concurrent_uni_streams(0_u8.into());

    let endpoint = quinn::Endpoint::server(server_config, listen_addr)
        .with_context(|| format!("failed to listen at [{listen_addr}]"))?;

    info!("bridge listening on [{listen_str}]");
    let shared: AShared = Arc::new(Shared {
        data: Default::default(),
    });

    while let Some(conn) = endpoint.accept().await {
        let uid = gen_huid();
        let shared = shared.clone();
        spawn_with_name(uid.to_string(), async move {
            let remote_addr = conn.remote_address();
            debug!("connection incoming from [{remote_addr}]");
            let r = handle_connection(shared, conn, uid).await;
            debug!("finished with [{r:?}]");
        });
    }

    Ok(())
}

async fn handle_connection(shared: AShared, conn: quinn::Connecting, uid: HUId) -> Result<()> {
    let connection = conn.await.with_context(|| "setup connection failed")?;

    {
        let (tx, rx) = connection
            .accept_bi()
            .await
            .with_context(|| "accept first stream failed")?;

        let mut session = Session {
            uid,
            conn: connection,
            tx,
            rx,
            rbuf: BytesMut::default(),
            wbuf: BytesMut::default(),
        };

        let req: BridgeRequest = read_short_json(&mut session.rx, &mut session.rbuf).await?;
        match req {
            BridgeRequest::Register(req) => session.handle_req_register(&shared, req).await,
            BridgeRequest::Forward(req) => session.handle_req_forward(&shared, req).await,
        }
    }

    // loop {
    //     // let conn2 = connection.clone();
    //     // connection.open_bi().await;
    //     let stream = connection.accept_bi().await;
    //     let stream = match stream {
    //         Err(quinn::ConnectionError::ApplicationClosed { .. }) => {
    //             debug!("connection closed");
    //             return Ok(());
    //         }
    //         Err(e) => {
    //             return Err(e.into());
    //         }
    //         Ok(s) => s,
    //     };
    // }
}

struct Session {
    uid: HUId,
    conn: Connection,
    tx: SendStream,
    rx: RecvStream,
    rbuf: BytesMut,
    wbuf: BytesMut,
}

impl Session {
    async fn handle_req_register(&mut self, shared: &AShared, req: RequestRegister) -> Result<()> {
        let agent = Arc::new(Agent {
            name: req.name,
            uid: self.uid,
            conn: self.conn.clone(),
        });

        let room = {
            let mut data = shared.data.lock();

            data.rooms
                .entry(req.room)
                .or_insert_with_key(|_key| {
                    Arc::new(Room {
                        // name: key.clone(),
                        data: Default::default(),
                    })
                })
                .clone()
        };

        self.send_short_json(&ResponseStatus::ok()).await?;

        {
            let mut data = room.data.lock();
            let old = data.agents.insert(agent.name.clone(), agent.clone());
            let old = old.map(|x| x.uid);
            info!("add agent [{}], old [{old:?}]", agent.name,);
        }

        let result = self.loop_next_req().await;

        {
            let mut data = room.data.lock();
            if let Some((key, exist)) = data.agents.remove_entry(&agent.name) {
                if exist.uid != self.uid {
                    data.agents.insert(key, exist);
                } else {
                    info!("remove agent");
                }
            }
        }

        result
    }

    async fn handle_req_forward(&mut self, shared: &AShared, req: RequestForward) -> Result<()> {
        let room = { shared.data.lock().rooms.get(&req.room).map(|x| x.clone()) };

        let room = {
            match room {
                Some(room) => room,
                None => {
                    let msg = format!("NOT found room [{}]->[{}]", req.room, req.target);
                    debug!("{msg}");
                    self.send_short_json(&ResponseStatus::new(404, msg)).await?;
                    return Ok(());
                }
            }
        };

        let agent = { room.data.lock().agents.get(&req.target).map(|x| x.clone()) };

        let agent = {
            match agent {
                Some(agent) => agent,
                None => {
                    let msg = format!("NOT found target [{}]->[{}]", req.room, req.target);
                    debug!("{msg}");
                    self.send_short_json(&ResponseStatus::new(404, msg)).await?;
                    return Ok(());
                }
            }
        };

        self.loop_next_req().await?;

        Ok(())
    }

    async fn loop_next_req(&mut self) -> Result<()> {
        let timeout = Duration::from_millis(PING_INTERVAL_MS * 3 / 2);

        loop {
            let fut = read_short_json(&mut self.rx, &mut self.rbuf);

            let req: NextRequest = tokio::time::timeout(timeout, fut)
                .await
                .with_context(|| "read next request timeout")?
                .with_context(|| "can't parse next request")?;

            match req {
                NextRequest::Ping(_ping) => {
                    self.send_short_json(&Pong {}).await?;
                }
            }
        }
    }

    async fn send_short_json<T>(&mut self, value: &T) -> Result<()>
    where
        T: ?Sized + Serialize,
    {
        ser_short_json(&mut self.wbuf, value)?;
        self.tx.write_all(&self.wbuf[..]).await?;
        self.wbuf.clear();
        Ok(())
    }
}

async fn read_short_json<R, T>(rx: &mut R, buf: &mut BytesMut) -> Result<T>
where
    R: AsyncReadExt + Unpin,
    T: for<'a> Deserialize<'a>,
{
    let packet_len = read_short_packet(rx, buf).await?;
    let obj: T = serde_json::from_slice(&buf[..packet_len])
        .with_context(|| "can't parsed bridge request")?;
    buf.advance(packet_len);
    Ok(obj)
}

async fn read_short_packet<R: AsyncReadExt + Unpin, B: BufMut>(
    rx: &mut R,
    buf: &mut B,
) -> Result<usize> {
    let len = rx.read_u16().await? as usize;
    let mut nread = 0;
    while nread < len {
        let n = rx.read_buf(buf).await?;
        nread += n;
    }
    Ok(nread)
}

pub fn ser_short_json<T>(buf: &mut BytesMut, value: &T) -> Result<usize>
where
    T: ?Sized + Serialize,
{
    let len_pos = buf.len();
    buf.put_u16(0); // reserve len field
    serde_json::to_writer((buf).writer(), value)?;
    let json_len = buf.len() - len_pos - 2;
    (&mut buf[len_pos..]).put_u16(json_len as u16);
    Ok(json_len)
}

type AShared = Arc<Shared>;

struct Shared {
    data: Mutex<SharedData>,
}

#[derive(Default)]
struct SharedData {
    rooms: HashMap<String, ARoom>,
}

type ARoom = Arc<Room>;

#[derive(Default)]
struct Room {
    // name: String,
    data: Mutex<RoomData>,
}

#[derive(Debug, Default)]
struct RoomData {
    // persist: bool,
    agents: HashMap<String, AAgent>,
}

type AAgent = Arc<Agent>;

#[derive(Debug)]
struct Agent {
    name: String,
    uid: HUId,
    conn: Connection,
}

#[derive(Debug, Serialize, Deserialize)]
enum BridgeRequest {
    Register(RequestRegister),
    Forward(RequestForward),
}

#[derive(Debug, Serialize, Deserialize)]
struct RequestRegister {
    name: String,
    room: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RequestForward {
    room: String,
    target: String,
}

#[derive(Debug, Serialize, Deserialize)]
enum NextRequest {
    Ping(Ping),
}

#[derive(Debug, Serialize, Deserialize)]
struct Ping {}

#[derive(Debug, Serialize, Deserialize)]
struct Pong {}

#[derive(Debug, Serialize, Deserialize)]
struct ResponseStatus {
    code: i32,
    msg: String,
}

impl ResponseStatus {
    pub fn new(code: i32, msg: String) -> Self {
        Self { code, msg }
    }

    pub fn ok() -> Self {
        Self::new(0, "".into())
    }
}

const PING_INTERVAL_MS: u64 = 30 * 1000;

async fn try_load_quic_cert(
    key_file: Option<&str>,
    cert_file: Option<&str>,
) -> Result<Option<rustls::ServerConfig>> {
    if let (Some(key_path), Some(cert_path)) = (key_file, cert_file) {
        let key_path: &Path = key_path.as_ref();
        let cert_path: &Path = cert_path.as_ref();

        let key = tokio::fs::read(key_path)
            .await
            .context("failed to read private key")?;
        let key = if key_path.extension().map_or(false, |x| x == "der") {
            rustls::PrivateKey(key)
        } else {
            let pkcs8 = rustls_pemfile::pkcs8_private_keys(&mut &*key)
                .context("malformed PKCS #8 private key")?;
            match pkcs8.into_iter().next() {
                Some(x) => rustls::PrivateKey(x),
                None => {
                    let rsa = rustls_pemfile::rsa_private_keys(&mut &*key)
                        .context("malformed PKCS #1 private key")?;
                    match rsa.into_iter().next() {
                        Some(x) => rustls::PrivateKey(x),
                        None => {
                            anyhow::bail!("no private keys found");
                        }
                    }
                }
            }
        };

        let cert_chain = tokio::fs::read(cert_path)
            .await
            .context("failed to read certificate chain")?;
        let cert_chain = if cert_path.extension().map_or(false, |x| x == "der") {
            vec![rustls::Certificate(cert_chain)]
        } else {
            rustls_pemfile::certs(&mut &*cert_chain)
                .context("invalid PEM-encoded certificate")?
                .into_iter()
                .map(rustls::Certificate)
                .collect()
        };

        let server_crypto = rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)?;

        return Ok(Some(server_crypto));
    }

    if key_file.is_some() || cert_file.is_some() {
        if key_file.is_none() {
            bail!("no key file")
        }

        if cert_file.is_none() {
            bail!("no cert file")
        }
    }

    Ok(None)
}

#[derive(Parser, Debug)]
#[clap(name = "quic_bridge", author, about, version)]
pub struct CmdArgs {
    #[clap(
        long = "addr",
        long_help = "listen address",
        default_value = "0.0.0.0:19888"
    )]
    addr: String,

    #[clap(long = "https-cert", long_help = "https cert file")]
    https_cert: Option<String>,

    #[clap(long = "https-key", long_help = "http key file")]
    https_key: Option<String>,

    #[clap(long = "access-secret", long_help = "access secret")]
    access_secret: Option<String>,
}
