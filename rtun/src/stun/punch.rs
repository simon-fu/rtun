

// use std::{sync::Arc, net::SocketAddr, io};
// use anyhow::{Result, Context};

// use futures::channel::oneshot;
// use tokio::{net::ToSocketAddrs, io::ReadBuf, sync::mpsc};
// use webrtc_ice::{agent::{agent_config::AgentConfig, Agent as IceAgent}, state::ConnectionState, candidate::{Candidate, candidate_base::unmarshal_candidate}, network_type::NetworkType, udp_network::UDPNetwork, url::Url};
// use webrtc_util::Conn;

// use crate::huid::gen_huid::gen_huid;

// use super::{udp_packet::{UdpSocketExt, udp_recv_from_data}, stun::{detect_nat_type1, NatDetectOutput, StunClient, decode_message, try_binding_response_bytes}};


// pub struct PunchPeer {
//     socket: Arc<UdpSocketExt>,
//     nat: Option<NatDetectOutput>,
//     local_ufrag: String,
// }

// impl PunchPeer {
//     pub async fn bind<A: ToSocketAddrs>(addr: A) -> Result<Self> {
//         let socket = Arc::new(UdpSocketExt::bind(addr).await?);
//         Ok(Self {
//             socket,
//             nat: None,
//             local_ufrag: gen_huid().to_string(),
//         })
//     }

//     pub async fn bind_and_detect<A: ToSocketAddrs>(addr: A) -> Result<(Self, NatDetectOutput)> {
//         let mut self0 = Self::bind(addr).await?;
//         let nat = self0.detect_my_nat().await?;
//         Ok((self0, nat))
//     }

//     pub async fn detect_my_nat(&mut self) -> Result<NatDetectOutput> {
//         match &self.nat {
//             Some(nat) => Ok(nat.clone()),
//             None => {
//                 let nat = detect_nat_type1(self.socket.clone()).await?;
//                 self.nat = Some(nat.clone());
//                 Ok(nat)
//             },
//         }
//     }

//     pub fn local_ufrag(&self) -> &str {
//         &self.local_ufrag
//     }

//     pub async fn punch(&mut self, remote_addr: SocketAddr, remote_ufrag: &str) -> Result<StunConnection> {
//         let username = format!("{}:{remote_ufrag}", self.local_ufrag);
        
//         let mut client = StunClient::from_socket(self.socket.clone());
        
//         client.req_binding2(remote_addr, username)?;
        
//         let output = client.exec().await?;
//         let rsp = output.result?;

//         Ok(StunConnection {
//             socket: client.into_socket(),
//             remote_addr: rsp.remote_addr,
//         })
//     }

// }


// pub struct StunConnection {
//     socket: Arc<UdpSocketExt>,
//     remote_addr: SocketAddr,
// }

// impl StunConnection {
//     pub fn local_addr(&self) -> &SocketAddr {
//         self.socket.local_addr()
//     }

//     pub fn remote_addr(&self) -> &SocketAddr {
//         &self.remote_addr
//     }

//     pub async fn recv(&mut self, mut buf: ReadBuf<'_>) -> io::Result<usize> {
//         // let mut buf = tokio::io::ReadBuf::new(buf);
        
//         loop {
//             let (len, remote_addr) = udp_recv_from_data(&self.socket, false, buf.initialize_unfilled()).await?;

//             if remote_addr != self.remote_addr {
//                 continue;
//             }

//             if len > 0 {
//                 let data = buf.filled();
//                 let first = data[0];

//                 if first & 0xC0 == 0 { 
//                     // >  0                   1                   2                   3
//                     // >  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//                     // > +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//                     // > |0 0|     STUN Message Type     |         Message Length        |
//                     // > +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+

//                     if let Ok(msg) = decode_message(data) {
//                         if let Some(data) = try_binding_response_bytes(&msg, &remote_addr) {
//                             let _r = self.socket.try_send_to_with(&data, remote_addr, false);
//                         }
//                     }
//                     continue;
//                 }
//             }

//             return Ok(len)
//         }
//     }

//     pub async fn try_send(&mut self, buf: &[u8]) -> io::Result<usize> {
//         self.socket.try_send_to_with(buf, self.remote_addr, false)
//     }
// }

use std::sync::Arc;
use anyhow::{Result, Context};

use futures::channel::oneshot;
use tokio::sync::mpsc;
use webrtc_ice::{agent::{agent_config::AgentConfig, Agent as IceAgent}, state::ConnectionState, candidate::{Candidate, candidate_base::unmarshal_candidate}, network_type::NetworkType, udp_network::UDPNetwork, url::Url};
use webrtc_util::Conn;

#[derive(Debug, Default)]
pub struct IceConfig {
    pub servers: Vec<String>,
}

#[derive(Debug)]
pub struct IceArgs {
    pub ufrag: String,
    pub pwd: String,
    pub candidates: Vec<String>,
}

pub struct IcePeer {
    config: IceConfig,
    agent: Option<Arc<IceAgent>>,
}

impl IcePeer {
    pub fn new() -> Self {
        Self::with_config(IceConfig::default())
    }

    pub fn with_config(config: IceConfig) -> Self {
        Self {
            agent: None,
            config,
        }
    }

    pub async fn gather_until_done(&mut self) -> Result<IceArgs> {

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

        Ok(IceArgs {
            ufrag: local_ufrag,
            pwd: local_pwd,
            candidates: local_candidates,
        })
    }

    pub async fn dial(&mut self, remote: IceArgs) -> Result<IceConn> {
        let agent = self.add_remote_candidates(&remote)?;

        let (_cancel_tx, cancel_rx) = mpsc::channel(1);
        let conn = agent.dial(cancel_rx, remote.ufrag, remote.pwd).await?;

        Ok(IceConn {
            conn,
        })
    }

    pub async fn accept(&mut self, remote: IceArgs) -> Result<IceConn> {
        let agent = self.add_remote_candidates(&remote)?;
        
        let (_cancel_tx, cancel_rx) = mpsc::channel(1);
        let conn = agent.accept(cancel_rx, remote.ufrag, remote.pwd).await?;

        Ok(IceConn {
            conn,
        })
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
}

pub struct IceConn {
    conn: Arc<dyn Conn + Send + Sync>,
}

impl IceConn {
    pub async fn send_data(&self, data: &[u8]) -> Result<usize> {
        self.conn.send(data).await.map_err(|e|e.into())
    }

    pub async fn recv_data(&self, buf: &mut [u8]) -> Result<usize> {
        self.conn.recv(buf).await.map_err(|e|e.into())
    }
}

#[tokio::test]
async fn test_ice_peer() -> Result<()> {
    let mut peer1 = IcePeer::with_config(IceConfig {
        servers: vec![
            "stun:stun1.l.google.com:19302".into(),
            "stun:stun2.l.google.com:19302".into(),
            "stun:stun.qq.com:3478".into(),
        ],
    });

    let arg1 = peer1.gather_until_done().await?;

    let mut peer2 = IcePeer::with_config(IceConfig {
        servers: vec![
            "stun:stun1.l.google.com:19302".into(),
            "stun:stun2.l.google.com:19302".into(),
            "stun:stun.qq.com:3478".into(),
        ],
    });
    
    let arg2 = peer2.gather_until_done().await?;

    let task1 = tokio::spawn(async move {
        let conn = peer1.dial(arg2).await?;
        conn.send_data("I'am conn1".as_bytes()).await?;
        let mut buf = vec![0; 1700];
        let n = conn.recv_data(&mut buf).await?;
        let msg = std::str::from_utf8(&buf[..n])?;
        println!("recv {msg:?}");
        Result::<()>::Ok(())
    });

    let task2 = tokio::spawn(async move {
        let conn = peer2.accept(arg1).await?;
        conn.send_data("I'am conn2".as_bytes()).await?;
        let mut buf = vec![0; 1700];
        let n = conn.recv_data(&mut buf).await?;
        let msg = std::str::from_utf8(&buf[..n])?;
        println!("recv {msg:?}");
        Result::<()>::Ok(())
    });

    task1.await??;
    task2.await??;

    Ok(())
}
