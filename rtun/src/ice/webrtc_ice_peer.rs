use std::sync::Arc;
use anyhow::{Result, Context, bail};

use futures::channel::oneshot;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use webrtc_dtls::{crypto::Certificate, config::{Config, ExtendedMasterSecretType, ClientAuthType}, conn::DTLSConn};
use webrtc_ice::{agent::{agent_config::AgentConfig, Agent as IceAgent}, state::ConnectionState, candidate::{Candidate, candidate_base::unmarshal_candidate}, network_type::NetworkType, udp_network::UDPNetwork, url::Url};
use webrtc_util::Conn;

use crate::ice::{ice_kcp::UpgradeToKcp, ice_mpsc::UpgradeToMpsc};

use super::{ice_kcp::IceKcpConnection, ice_mpsc::IceMpsc, ice_peer::IceArgs};

#[derive(Debug, Default)]
pub struct WebrtcIceConfig {
    pub servers: Vec<String>,
    pub disable_dtls: bool,
}

// #[derive(Debug)]
// pub struct IceArgs {
//     pub ufrag: String,
//     pub pwd: String,
//     pub candidates: Vec<String>,
//     pub cert_fingerprint: Option<String>,
// }

pub struct WebrtcIcePeer {
    config: WebrtcIceConfig,
    agent: Option<Arc<IceAgent>>,
    certificate: Option<Certificate>,
    conv: u32,
}

impl WebrtcIcePeer {
    pub fn new() -> Self {
        Self::with_config(WebrtcIceConfig::default())
    }

    pub fn with_config(config: WebrtcIceConfig) -> Self {
        Self {
            certificate: None,
            agent: None,
            config,
            conv: 0,
        }
    }

    pub async fn gather_until_done(&mut self) -> Result<IceArgs> {

        self.certificate = if !self.config.disable_dtls {
            Some(Certificate::generate_self_signed(vec!["localhost".to_owned()])?)
        } else {
            None
        };

        let mut urls = Vec::with_capacity(self.config.servers.len());
        for s in self.config.servers.iter() {
            let url = Url::parse_url(s).with_context(||"invalid ice server")?;
            urls.push(url);
        }

        let ice_agent = Arc::new(
            IceAgent::new(AgentConfig {
                urls,
                network_types: vec![NetworkType::Udp4],
                udp_network: UDPNetwork::Ephemeral(Default::default()),
                ..Default::default()
            })
            .await?,
        );

        ice_agent.on_connection_state_change(Box::new(move |c: ConnectionState| {
            tracing::debug!("ICE Connection State has changed: {c}");
            if c == ConnectionState::Failed {
                
            }
            Box::pin(async move {})
        }));
        

        let (tx, rx) = oneshot::channel();
        let mut tx = Some(tx);
        ice_agent.on_candidate(Box::new(move |c: Option<Arc<dyn Candidate + Send + Sync>>| {
            if c.is_none() {
                if let Some(tx) = tx.take() {
                    let _r = tx.send(());
                }
            }
            Box::pin(async move {})
        }));

        ice_agent.gather_candidates()?;
        let _r = rx.await;

        let local_candidates: Vec<String> = ice_agent
        .get_local_candidates().await?
        .iter()
        .map(|c|c.marshal())
        .collect();
        // tracing::debug!("local_candidates: {local_candidates:?}");
        
        let (local_ufrag, local_pwd) = ice_agent.get_local_user_credentials().await;
        // tracing::debug!("credentials: {local_ufrag:?}, {local_pwd:?}");

        self.agent = Some(ice_agent);
        self.conv = crc32c::crc32c(local_ufrag.as_bytes());

        Ok(IceArgs {
            ufrag: local_ufrag,
            pwd: local_pwd,
            candidates: local_candidates,
            cert_fingerprint: match &self.certificate {
                Some(certificate) => Some(make_fingerprint(SHA256_ALG, &certificate.certificate[0].0)?),
                None => None,
            },
        })
    }

    pub async fn dial(&mut self, remote: IceArgs) -> Result<IceConn> {
        self.conv = crc32c::crc32c(remote.ufrag.as_bytes());

        let agent = self.add_remote_candidates(&remote)?;

        let (_cancel_tx, cancel_rx) = mpsc::channel(1);
        let conn = agent.dial(cancel_rx, remote.ufrag, remote.pwd).await?;
        tracing::debug!("ice dial connected");

        self.make_conn(conn, true, remote.cert_fingerprint.as_deref()).await
    }

    pub async fn accept(&mut self, remote: IceArgs) -> Result<IceConn> {
        let agent = self.add_remote_candidates(&remote)?;
        
        let (_cancel_tx, cancel_rx) = mpsc::channel(1);
        let conn = agent.accept(cancel_rx, remote.ufrag, remote.pwd).await?;
        
        tracing::debug!("ice accept connected");

        self.make_conn(conn, false, remote.cert_fingerprint.as_deref()).await
    }

    async fn make_conn(&mut self, conn: AConn, is_client: bool, remote_fingerprint: Option<&str>) -> Result<IceConn> {
        let conn = match self.certificate.take() {
            Some(certificate) => {
                let remote_fingerprint = remote_fingerprint.with_context(||"remote fingerprint empty")?;
                upgrade_to_dtls(conn, certificate, remote_fingerprint, is_client).await?
            },
            None => conn,
        };

        Ok(IceConn{ conn })
    }

    fn add_remote_candidates<'a>(&'a self, remote: &IceArgs) -> Result<&'a Arc<IceAgent>> {
        let agent = self.agent.as_ref().with_context(||"uninit gather")?;

        for candidate in remote.candidates.iter() {
            let c = unmarshal_candidate(candidate).with_context(||"invalid remote candidate")?;
            let c: Arc<dyn Candidate + Send + Sync> = Arc::new(c);
            agent.add_remote_candidate(&c).with_context(||"add remote candidate failed")?;
        }

        Ok(agent)
    }

    // // for test only 
    // pub async fn into_dial_and_chat(self, remote: IceArgs) -> Result<()> {
    //     let mut peer = self;
    //     crate::async_rt::spawn_with_name("peer-client", async move {
    //         let r = async move {
    //             let conn = peer.dial(remote).await?;
                
    //             // conn.send_data("I'am client".as_bytes()).await?;
    //             // let mut buf = vec![0; 1700];
    //             // let n = conn.recv_data(&mut buf).await?;
    //             // let msg = std::str::from_utf8(&buf[..n])?;
    //             // tracing::debug!("recv {msg:?}");

    //             let mut conn = conn.upgrade_to_kcp(peer.conv)?;
    //             tracing::debug!("upgrade to kcp");

    //             conn.write_all("I'am client 1 ".as_bytes()).await?;
    //             conn.write_all("I'am client 2 ".as_bytes()).await?;
    //             conn.write_all("I'am client 3 ".as_bytes()).await?;
    //             tracing::debug!("sent text");

    //             let mut buf = vec![0; 1700];
                
    //             loop {
    //                 let n = conn.read(&mut buf).await?;
    //                 let msg = std::str::from_utf8(&buf[..n])?;
    //                 tracing::debug!("recv {msg:?}");
    //             }
    //             Result::<()>::Ok(())
    //         }.await;
    //         tracing::debug!("finished {r:?}");
    //     });
    //     Ok(())
    // }

    // pub async fn into_accept_and_chat(self, remote: IceArgs) -> Result<()> {
    //     let mut peer = self;
    //     crate::async_rt::spawn_with_name("p2p-server", async move {
    //         let r = async move {
    //             let conn = peer.accept(remote).await?;

    //             let mut conn = conn.upgrade_to_kcp(peer.conv)?;
    //             tracing::debug!("upgrade to kcp");

    //             conn.write_all("I'am server 1 ".as_bytes()).await?;
    //             conn.write_all("I'am server 2 ".as_bytes()).await?;
    //             conn.write_all("I'am server 3 ".as_bytes()).await?;
    //             tracing::debug!("sent text");
                
    //             let mut buf = vec![0; 1700];
    //             loop {
    //                 let n = conn.read(&mut buf).await?;
    //                 let msg = std::str::from_utf8(&buf[..n])?;
    //                 tracing::debug!("recv {msg:?}");
    //             }
                
    //             Result::<()>::Ok(())
    //         }.await;
    //         tracing::debug!("finished {r:?}");
    //     });
    //     Ok(())
    // }

    pub async fn kick_and_ugrade_to_kcp(self, remote: IceArgs, is_client: bool) -> Result<IceKcpConnection> {
        let mut peer = self;
        let conn = if is_client {
            peer.dial(remote).await?
        } else {
            peer.accept(remote).await?
        };

        let conn = conn.upgrade_to_kcp(peer.conv)?;
        tracing::debug!("upgrade to kcp");

        Ok(conn)
    }

    pub async fn kick_and_ugrade_to_mpsc(self, remote: IceArgs, is_client: bool) -> Result<IceMpsc> {
        let mut peer = self;
        let conn = if is_client {
            peer.dial(remote).await?
        } else {
            peer.accept(remote).await?
        };

        let conn = conn.upgrade_to_mpsc(peer.conv)?;
        tracing::debug!("upgrade to mpsc");

        Ok(conn)
    }

}

type AConn = Arc<dyn Conn + Send + Sync>;


async fn upgrade_to_dtls(
    conn: AConn, 
    certificate: Certificate, 
    remote_fingerprint: &str,
    is_client: bool,
) -> Result<AConn> {
    // 

    let config = Config {
        certificates: vec![certificate],
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        // client_auth: if is_client {ClientAuthType::default()} else {ClientAuthType::RequireAnyClientCert},
        client_auth: ClientAuthType::RequireAnyClientCert,
        ..Default::default()
    };

    let conn = Arc::new(DTLSConn::new(conn, config, is_client, None).await?);
    let state = conn.connection_state().await;
    if state.peer_certificates.is_empty() {
        bail!("remote certificates empty")
    }

    let fingerprint = make_fingerprint(SHA256_ALG, &state.peer_certificates[0])?;
    tracing::debug!("remote fingerprint {remote_fingerprint:?}", );
    if fingerprint != remote_fingerprint {
        bail!("verify remote fingerprint failed")
    }

    tracing::debug!("upgrade to dtls");

    Ok(conn)

}

// #[derive(Clone)]
pub struct IceConn {
    pub(crate) conn: AConn,
}

impl IceConn {
    pub async fn write_data(&mut self, data: &[u8]) -> Result<usize> {
        self.conn.send(data).await.map_err(|e|e.into())
    }

    pub async fn read_data(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.conn.recv(buf).await.map_err(|e|e.into())
    }

    pub fn split(self) -> (IceReadHalf, IceWriteHalf) {
        (
            IceReadHalf{ conn: self.conn.clone() },
            IceWriteHalf{ conn: self.conn },
        )
    }
}

pub struct IceReadHalf {
    conn: AConn,
}

impl IceReadHalf {
    pub async fn read_data(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.conn.recv(buf).await.map_err(|e|e.into())
    }
}

pub struct IceWriteHalf {
    conn: AConn,
}

impl IceWriteHalf {
    pub async fn write_data(&mut self, data: &[u8]) -> Result<usize> {
        self.conn.send(data).await.map_err(|e|e.into())
    }
}

const SHA256_ALG: &str = "sha-256";

fn make_fingerprint(algorithm: &str, remote_cert: &[u8]) -> Result<String> {
    if algorithm != SHA256_ALG {
        bail!("unsupported fingerprint algorithm [{algorithm}]")
    }

    let mut h = Sha256::new();
    h.update(remote_cert);
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
    let s1 = make_fingerprint(SHA256_ALG, cert_bytes).unwrap();
    assert_eq!(s1, "59:94:47:1a:bb:01:11:2a:fc:c1:81:59:f6:cc:74:b4:f5:11:b9:98:06:da:59:b3:ca:f5:a9:c1:73:ca:cf:c5")
}


#[tokio::test]
async fn test_ice_peer() -> Result<()> {
    let mut peer1 = WebrtcIcePeer::with_config(WebrtcIceConfig {
        servers: vec![
            "stun:stun1.l.google.com:19302".into(),
            "stun:stun2.l.google.com:19302".into(),
            "stun:stun.qq.com:3478".into(),
        ],
        ..Default::default()
    });

    let arg1 = peer1.gather_until_done().await?;

    let mut peer2 = WebrtcIcePeer::with_config(WebrtcIceConfig {
        servers: vec![
            "stun:stun1.l.google.com:19302".into(),
            "stun:stun2.l.google.com:19302".into(),
            "stun:stun.qq.com:3478".into(),
        ],
        ..Default::default()
    });
    
    let arg2 = peer2.gather_until_done().await?;

    let task1 = tokio::spawn(async move {
        let mut conn = peer1.dial(arg2).await?;
        conn.write_data("I'am conn1".as_bytes()).await?;
        let mut buf = vec![0; 1700];
        let n = conn.read_data(&mut buf).await?;
        let msg = std::str::from_utf8(&buf[..n])?;
        println!("recv {msg:?}");
        Result::<()>::Ok(())
    });

    let task2 = tokio::spawn(async move {
        let mut conn = peer2.accept(arg1).await?;
        conn.write_data("I'am conn2".as_bytes()).await?;
        let mut buf = vec![0; 1700];
        let n = conn.read_data(&mut buf).await?;
        let msg = std::str::from_utf8(&buf[..n])?;
        println!("recv {msg:?}");
        Result::<()>::Ok(())
    });

    task1.await??;
    task2.await??;

    Ok(())
}
