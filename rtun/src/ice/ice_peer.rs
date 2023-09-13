/* 
TODO:
    - ice nego 要有超时
    - 证书验证
    - ufrag, pwd 随机生成
    - 用 pwd 检验 stun 消息完整性
    - IfWatcher 在 github 上取到空列表
*/

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use anyhow::{Result, Context, bail};
use quinn::{Endpoint, ServerConfig, default_runtime, ClientConfig, Connection, SendStream, RecvStream};
use sha2::{Sha256, Digest};
use tracing::debug;

use crate::async_rt::spawn_with_name;
use crate::ice::ice_candidate::parse_candidate;
use crate::stun::async_udp::{AsyncUdpSocket, UdpSocketBridge, tokio_socket_bind, TokioUdpSocket, AsUdpSocket};


use crate::stun::stun::{Binding, StunSocket, BindingOutput, self};
use crate::huid::gen_huid::gen_huid;

use super::ice_candidate::{Candidate, server_reflexive};
use super::ice_ipnet::ipnet_iter;

#[derive(Debug, Default)]
pub struct IceConfig {
    pub servers: Vec<String>,
    pub disable_dtls: bool,
    pub compnent_id: Option<u16>,
}

#[derive(Debug)]
pub struct IceArgs {
    pub ufrag: String,
    pub pwd: String,
    pub candidates: Vec<String>,
    pub cert_fingerprint: Option<String>,
}

struct Local {
    pub ufrag: String,
    pub pwd: String,
    pub candidates: Vec<Candidate>,
    pub cert: rcgen::Certificate,
}

impl Local {
    fn to_args(&self) -> IceArgs {
        let mut candidates = Vec::with_capacity(self.candidates.len());
        for c in &self.candidates {
            candidates.push(c.to_cstr());
        }

        IceArgs {
            ufrag: self.ufrag.clone(),
            pwd: self.pwd.clone(),
            candidates,
            cert_fingerprint: self.cert_fingerprint(),
        }
    }

    fn cert_fingerprint(&self, ) -> Option<String> {
        let data = self.cert.get_key_pair().public_key_der();
        return make_fingerprint(SHA256_ALG, &data).ok()

        // if let Ok(data) = self.cert.serialize_der() {
        //     return make_fingerprint(SHA256_ALG, &data).ok()
        // }
        // None
    }
}

pub struct IcePeer {
    config: IceConfig,
    socket: Option<TokioUdpSocket>,
    local: Option<Local>,
    remote: Option<IceArgs>,
    targets: Vec<SocketAddr>,
}

impl IcePeer {
    pub fn new() -> Self {
        Self::with_config(IceConfig::default())
    }

    pub fn with_config(config: IceConfig) -> Self {
        Self {
            local: None,
            socket: None,
            remote: None,
            targets: Default::default(),
            config,
        }
    }

    pub fn local_args(&self) -> Option<IceArgs> {
        self.local.as_ref().map(|x|x.to_args())
    }

    pub async fn initiative(&mut self) -> Result<IceArgs> {
        self.gather_until_done().await
    }

    pub async fn passive(&mut self, remote: IceArgs) -> Result<IceArgs> {
        let local_args = self.gather_until_done().await?;
        
        self.set_remote(remote)?;

        let config = self.binding_config()?;
        if let Some(socket) = self.socket.as_mut() {
            socket.set_ttl(3)?;
            for target in self.targets.iter() {
                let data = config.gen_bind_req_bytes()?;
                socket.as_socket().send_to(data.into(), *target).await?;
            }
        }

        Ok(local_args)
    }

    fn set_remote(&mut self, remote: IceArgs) -> Result<()> {
        self.targets = Vec::with_capacity(remote.candidates.len());
        for c in remote.candidates.iter() {
            self.targets.push(parse_candidate(c)?.addr());
        }
        self.remote = Some(remote);
        Ok(())
    }

    fn binding_config(&self) -> Result<stun::Config> {
        let local = self.local.as_ref().with_context(||"no local args")?;
        let remote = self.remote.as_ref().with_context(||"no remote args")?;
        let username = format!("{}:{}", local.ufrag, remote.ufrag);

        let mut config = stun::Config::default();
        config.username = Some(username);
        config.password = Some(remote.pwd.clone());

        Ok(config)
    }

    async fn gather_until_done(&mut self) -> Result<IceArgs> {

        let mut servers = Vec::with_capacity(self.config.servers.len());
        for s in self.config.servers.iter() {
            // stun:stun1.l.google.com:19302
            if let Some(server) = s.strip_prefix("stun:") {
                servers.push(server);
            } else {

            }
        }


        let (socket, output) = if servers.is_empty() {
            (tokio_socket_bind("0.0.0.0:0").await?, BindingOutput::default())
        } else {
            Binding::bind("0.0.0.0:0").await?
            .exec(servers.into_iter()).await?
        };
        

        let local_addr = socket.local_addr()?;

        let mut candidates = Vec::new();

        for addr in output.mapped_iter() {
            candidates.push(server_reflexive(addr, local_addr, self.config.compnent_id));
        }

        if local_addr.ip().is_unspecified() {
            // for ifnet in IfWatcher::new()?.iter() {
            //     let if_addr = ifnet.addr();
            //     if (local_addr.is_ipv4() && if_addr.is_ipv4()) 
            //     || (local_addr.is_ipv6() && if_addr.is_ipv6()) {
            //         candidates.push(Candidate::host(SocketAddr::new(if_addr, local_addr.port()))?);
            //     }
            // }

            for r in ipnet_iter().await? {
                let if_addr = r?.addr();
                if (local_addr.is_ipv4() && if_addr.is_ipv4()) 
                || (local_addr.is_ipv6() && if_addr.is_ipv6()) {
                    candidates.push(Candidate::host(SocketAddr::new(if_addr, local_addr.port()))?);
                }
            }

        } else {
            candidates.push(Candidate::host(local_addr)?);
        }


        let local = Local {
            ufrag: gen_huid().to_string(),
            pwd: gen_huid().to_string(),
            candidates,
            cert: rcgen::generate_simple_self_signed(vec!["localhost".into()])?,
        };

        let args = local.to_args();

        self.local = Some(local);
        self.socket = Some(socket);
        
        Ok(args)
    }

    pub async fn dial(&mut self, remote: IceArgs) -> Result<IceConn> {
        self.set_remote(remote)?;
        let socket = self.socket.take().with_context(||"no socket")?;
        self.negotiate(socket, true).await
    }

    pub async fn accept(&mut self) -> Result<IceConn> {
        let socket = self.socket.take().with_context(||"no socket")?;
        self.negotiate(socket, false).await
    }

    async fn negotiate(&mut self, socket: TokioUdpSocket, is_client: bool) -> Result<IceConn> {

        let remote = self.remote.as_ref().with_context(||"no remote args")?;

        let local = self.local.as_ref().with_context(||"NOT gather yet")?;

        let config = self.binding_config()?;

        let (socket, output) = Binding::with_config(socket, config)
        .exec(self.targets.iter()).await?;

        let select_addr = output.target_iter()
        .next()
        .with_context(||"negotiate done but empty output")?
        .next()
        .with_context(||"negotiate done but empty target")?;

        debug!("setup connection to {select_addr}");

        let socket = UdpSocketBridge(StunSocket::new(socket)) ;

        let server_config = {
            // let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
            let cert = &local.cert;
            let cert_der = cert.serialize_der()?;
            let priv_key = cert.serialize_private_key_der();
            let priv_key = rustls::PrivateKey(priv_key);
            let cert_chain = vec![rustls::Certificate(cert_der.clone())];
        
            let mut server_config = ServerConfig::with_single_cert(cert_chain, priv_key)?;
            let transport_config = Arc::get_mut(&mut server_config.transport).with_context(||"get transport config failed")?;
            transport_config.max_concurrent_uni_streams(0_u8.into());
        
            server_config
        };


        // let (server_config, server_cert) = configure_server()?;

        let runtime = default_runtime().with_context(||"no async runtime")?;

        let mut endpoint = Endpoint::new_with_abstract_socket(
            Default::default(), 
            Some(server_config), 
            socket, 
            runtime,
        )?;
        
        let (conn, keepalive) = if is_client {
            let crypto = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(ServerFingerprintVerification::new(remote.cert_fingerprint.clone()))
            .with_no_client_auth();
            
            endpoint.set_default_client_config(ClientConfig::new(Arc::new(crypto)));

            let conn = endpoint.connect(select_addr, "localhost")?.await?;
            let keepalive = conn.open_bi().await?;
            (conn, keepalive)
        } else {
            let conn = endpoint.accept()
            .await.with_context(||"accept but enpoint none")?
            .await?;
            let keepalive = conn.accept_bi().await?;
            (conn, keepalive)
        };

        spawn_with_name("keepalive", async move {
            let r = keepalive_task(keepalive, is_client).await;
            debug!("finished {r:?}");
        });

        debug!("upgrade to quic");

        Ok(conn)
    }

}

async fn keepalive_task((mut wr, mut rd): (SendStream, RecvStream), _is_client: bool) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    let mut buf = vec![0; 2048];
    loop {
        tokio::select! {
            r = rd.read(&mut buf) => {
                let n = r?.with_context(||"stream closed")?;
                if n == 0 {
                    return Ok(())
                }
                // let s = std::str::from_utf8(&buf[..n]);
                // debug!("recv {s:?}");
            }
            _r = interval.tick() => {
                wr.write_all("hello".as_bytes()).await?;
            }
        }
    }
}

pub type IceConn = Connection;




// // #[derive(Clone)]
// pub struct IceConn {
//     pub(crate) conn: AConn,
// }

// impl IceConn {
//     pub async fn write_data(&mut self, data: &[u8]) -> Result<usize> {
//         self.conn.send(data).await.map_err(|e|e.into())
//     }

//     pub async fn read_data(&mut self, buf: &mut [u8]) -> Result<usize> {
//         self.conn.recv(buf).await.map_err(|e|e.into())
//     }

//     pub fn split(self) -> (IceReadHalf, IceWriteHalf) {
//         (
//             IceReadHalf{ conn: self.conn.clone() },
//             IceWriteHalf{ conn: self.conn },
//         )
//     }
// }

// pub struct IceReadHalf {
//     conn: AConn,
// }

// impl IceReadHalf {
//     pub async fn read_data(&mut self, buf: &mut [u8]) -> Result<usize> {
//         self.conn.recv(buf).await.map_err(|e|e.into())
//     }
// }

// pub struct IceWriteHalf {
//     conn: AConn,
// }

// impl IceWriteHalf {
//     pub async fn write_data(&mut self, data: &[u8]) -> Result<usize> {
//         self.conn.send(data).await.map_err(|e|e.into())
//     }
// }


const SHA256_ALG: &str = "sha-256";

fn make_fingerprint(algorithm: &str, cert_data: &[u8]) -> Result<String> {
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

// #[test]
// fn test_make_fingerprint() {
//     let cert_bytes = "12345".as_bytes();
//     let s1 = make_fingerprint(SHA256_ALG, cert_bytes).unwrap();
//     assert_eq!(s1, "59:94:47:1a:bb:01:11:2a:fc:c1:81:59:f6:cc:74:b4:f5:11:b9:98:06:da:59:b3:ca:f5:a9:c1:73:ca:cf:c5")
// }

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


#[tokio::test]
async fn test_ice_peer() -> Result<()> {
    use crate::async_rt::spawn_with_name;

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

    let arg1 = peer1.initiative().await?;
    debug!("arg1 {arg1:?}");

    let mut peer2 = IcePeer::with_config(IceConfig {
        servers: servers.clone(),
        ..Default::default()
    });
    
    let arg2 = peer2.passive(arg1).await?;
    debug!("arg2 {arg2:?}");


    let task1 = spawn_with_name("client", async move {
        let conn = peer1.dial(arg2).await?;
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





