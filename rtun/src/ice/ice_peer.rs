/* 
TODO:
    - ok ice nego 要有超时
    - ok (实现了校验服务端证书） quic 证书验证
    - ok ufrag, pwd 随机生成
    - ok 用 pwd 检验 stun 消息完整性
    - ok IfWatcher 在 github 上取到空列表
*/

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Duration;
use anyhow::{Result, Context};
use tokio::time::Instant;
use tracing::debug;

use crate::huid::HUId;
use crate::huid::gen_huid::gen_huid;
use crate::ice::ice_candidate::parse_candidate;
use crate::stun::async_udp::{AsyncUdpSocket, UdpSocketBridge, tokio_socket_bind, TokioUdpSocket};

use super::ice_socket::{IceChecker, IceCreds, udp_run_until_done, StunResolver, CheckerConfig, udp_flush, IceSocket};
use super::ice_candidate::{server_reflexive, Candidate, CandidateKind};
use super::ice_ipnet::ipnet_iter;

#[derive(Debug, Default)]
pub struct IceConfig {
    pub servers: Vec<String>,
    // pub disable_dtls: bool,
    pub compnent_id: Option<u16>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IceArgs {
    pub ufrag: String,
    pub pwd: String,
    pub candidates: Vec<String>,
    // pub cert_der: Option<Bytes>,
}

struct Local {
    pub ufrag: String,
    pub pwd: String,
    pub candidates: Vec<Candidate>,
    // pub nat4: Option<Nat4>,
    // pub cert: rcgen::Certificate,
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
            // cert_der: None,
        }
    }

    // fn cert_fingerprint(&self, ) -> Option<String> {
    //     let data = self.cert.get_key_pair().public_key_der();
    //     return make_fingerprint(SHA256_ALG, &data).ok()

    //     // if let Ok(data) = self.cert.serialize_der() {
    //     //     return make_fingerprint(SHA256_ALG, &data).ok()
    //     // }
    //     // None
    // }
}

// struct Nat4 {
//     targets: HashSet<SocketAddr>,
// }

// impl Nat4 {
//     fn from_candidates(candidates: &[Candidate]) -> Option<Self> {
//         let mut targets = HashSet::new();
//         for (index, cand) in candidates.iter().enumerate() {
//             if cand.raddr().is_some() && cand.kind() == CandidateKind::ServerReflexive && index < (candidates.len() - 1) {
//                 for other in (&candidates[index+1..]).iter() {
//                     if cand.raddr() == other.raddr() {
//                         targets.insert(cand.addr());
//                         targets.insert(other.addr());
//                     }
//                 }
//             }
//         }

//         if targets.len() > 0 {
//             Some(Self {
//                 targets,
//             })
//         } else {
//             None
//         }
//     }
// }

pub struct IcePeer {
    uid: HUId,
    config: IceConfig,
    socket: Option<TokioUdpSocket>,
    local: Option<Local>,
    remote: Option<IceArgs>,
    targets: Vec<Candidate>,
    // is_client: bool,
}

impl IcePeer {
    pub fn new() -> Self {
        Self::with_config(IceConfig::default())
    }

    pub fn with_config(config: IceConfig) -> Self {
        Self {
            uid: gen_huid(),
            local: None,
            socket: None,
            remote: None,
            targets: Default::default(),
            // is_client: true,
            config,
        }
    }

    pub fn uid(&self) -> HUId {
        self.uid
    }

    pub fn local_args(&self) -> Option<IceArgs> {
        self.local.as_ref().map(|x|x.to_args())
    }

    // pub fn is_client(&self) -> bool {
    //     self.is_client
    // }

    pub async fn client_gather(&mut self) -> Result<IceArgs> {
        // self.is_client = true;
        self.gather_until_done().await
    }

    // pub async fn server_gather(&mut self, remote: IceArgs) -> Result<IceArgs> {
    //     self.server_gather_with(remote, None).await
    // }

    pub async fn server_gather(&mut self, remote: IceArgs) -> Result<IceArgs> {
        // self.is_client = false;
        
        let local_args = self.gather_until_done().await?;
        
        self.set_remote(remote)?;

        // let config = self.binding_config(false)?;
        // tracing::debug!("{} aaa server gather config {config:?}", self.uid);

        // if let Some(socket) = self.socket.as_mut() {
        //     socket.set_ttl(3)?;
        //     for target in self.targets.iter() {
        //         tracing::debug!("aaa server gather send to {target:?}");
        //         let data = config.make_req_bytes(target, false)?;
        //         socket.as_socket().send_to(data.into(), target.addr()).await?;
        //     }
        //     // tokio::time::sleep(std::time::Duration::MAX/2).await; // aaa
        //     let rrr = 0;
        // }

        // tracing::debug!("prepare checking..");
        let mut checker = self.make_checker(false, None)?;
        if let Some(socket) = self.socket.as_mut() {
            checker.prepare_checking()?;
            udp_flush(socket, &mut checker).await?;
        }
        // wait for udp packet on wire
        tokio::time::sleep(Duration::from_millis(100)).await; 

        // tracing::debug!("prepare checking done");

        Ok(local_args)
    }

    fn set_remote(&mut self, remote: IceArgs) -> Result<()> {
        self.targets = Vec::with_capacity(remote.candidates.len());
        for c in remote.candidates.iter() {
            let cand = parse_candidate(c)?;
            self.targets.push(cand);
            // self.targets.push(BindingCandidate {
            //     addr: cand.addr(),
            //     use_cand: false,
            //     prio: Some(cand.prio()),
            // });
        }
        self.remote = Some(remote);
        Ok(())
    }

    // // fn binding_config(&self) -> Result<stun::Config> {
    // //     let local = self.local.as_ref().with_context(||"no local args")?;
    // //     let remote = self.remote.as_ref().with_context(||"no remote args")?;
    // //     let username = format!("{}:{}", local.ufrag, remote.ufrag);

    // //     let config = stun::Config::default()
    // //     .with_username(username)
    // //     .with_password(remote.pwd.clone());

    // //     Ok(config)
    // // }

    // fn binding_config(&self, is_client: bool) -> Result<BindingConfig> {
    //     let local = self.local.as_ref().with_context(||"no local args")?;
    //     let remote = self.remote.as_ref().with_context(||"no remote args")?;

    //     let config = BindingConfig::default()
    //     .with_software("rtun".into())
    //     .with_ice_controlling(is_client)
    //     .with_local_creds(
    //         format!("{}:{}", remote.ufrag, local.ufrag), 
    //         remote.pwd.to_string(),
    //     )
    //     .with_remote_creds(
    //         format!("{}:{}", local.ufrag, remote.ufrag), 
    //         local.pwd.to_string(),
    //     );

    //     Ok(config)
    // }

    fn make_checker(&self, is_client: bool, timeout: Option<Duration>) -> Result<IceChecker> {
        let local = self.local.as_ref().with_context(||"no local args")?;
        let remote = self.remote.as_ref().with_context(||"no remote args")?;

        let mut binding = CheckerConfig::default()
        .with_software("rtun".into())
        .with_controlling(is_client)
        .with_local_creds(IceCreds {
            username: format!("{}:{}", remote.ufrag, local.ufrag),
            password: remote.pwd.to_string(),
        })
        .with_remote_creds(IceCreds {
            username: format!("{}:{}", local.ufrag, remote.ufrag), 
            password: local.pwd.to_string(),
        })
        .with_timeout(timeout)
        .into_exec();

        for cand in self.targets.iter() {
            binding.add_remote_candidate(cand.clone());
        }

        Ok(binding)
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


        // // let (socket, output) = if servers.is_empty() {
        // //     (tokio_socket_bind("0.0.0.0:0").await?, BindingOutput::default())
        // // } else {
        // //     Binding::bind("0.0.0.0:0").await?
        // //     .exec(servers.into_iter()).await?
        // // };

        // // debug!("ice servers {servers:?}");
        // let (socket, output) = BindingConfig::default()
        // .resolve_mapped_addr(servers.into_iter()).await?;

        let socket = tokio_socket_bind("0.0.0.0:0").await.with_context(||"bind udp failed")?;
        let mut resolver = StunResolver::default();
        resolver.resolve(&socket, servers.into_iter()).await.with_context(||"stun resulve failed")?;
        

        let local_addr = socket.local_addr().with_context(||"get local addr failed")?;

        let mut candidates = Vec::new();

        for addr in resolver.mapped_addrs().mapped_iter() {
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

            for r in ipnet_iter().await.with_context(||"ipnet_iter failed")? {
                let if_addr = r.with_context(||"ipnet_iter next failed")?.addr();
                if (local_addr.is_ipv4() && if_addr.is_ipv4()) 
                || (local_addr.is_ipv6() && if_addr.is_ipv6()) {
                    candidates.push(Candidate::host(SocketAddr::new(if_addr, local_addr.port()))?);
                }
            }

        } else {
            candidates.push(Candidate::host(local_addr).with_context(||"host candidate failed")?);
        }


        let local = Local {
            ufrag: HUId::random().to_string(),
            pwd: HUId::random().to_string(),
            // nat4: Nat4::from_candidates(&candidates),
            candidates,
            // cert: rcgen::generate_simple_self_signed(vec!["localhost".into()])?,
        };

        let args = local.to_args();

        self.local = Some(local);
        self.socket = Some(socket);
        
        Ok(args)
    }

    pub async fn dial(&mut self, remote: IceArgs) -> Result<IceConn> {
        self.set_remote(remote)?;
        let socket = self.socket.take().with_context(||"no socket")?;
        self.negotiate(socket, true, None).await
    }

    pub async fn accept(&mut self) -> Result<IceConn> {
        let socket = self.socket.take().with_context(||"no socket")?;
        self.negotiate(socket, false, None).await
    }

    pub async fn accept_timeout(&mut self, timeout: Duration) -> Result<IceConn> {
        let socket = self.socket.take().with_context(||"no socket")?;
        self.negotiate(socket, false, Some(timeout)).await
    }

    async fn negotiate(&mut self, socket: TokioUdpSocket, is_client: bool, timeout: Option<Duration>) -> Result<IceConn> {



        // let config = self.binding_config(is_client)?;
        // tracing::debug!("{} aaa negotiate config {config:?}", self.uid);

        // let (socket, output) = config
        // .resolve_ice(socket, self.targets.iter().map(|x|x.clone())).await?;
        // let select_addr = output.addr();

        let mut checker = self.make_checker(is_client, timeout)?;
        debug!("start checking (is_client {is_client})...");
        // debug!("start checking... (is_client {is_client}), {:?}", checker.config());

        checker.kick_checking(Instant::now())?;
        udp_run_until_done(&socket, &mut checker).await?;
        let select_addr = checker.selected_addr().with_context(||"checking done but no selected addr")?;

        debug!("setup connection to {select_addr}");

        // let socket = UdpSocketBridge(StunSocket::new(socket)) ;
        let socket = UdpSocketBridge(checker.into_socket(socket)) ;

        let conn = IceConn {
            uid: self.uid,
            remote_addr: select_addr,
            is_client,
            socket
        };


        Ok(conn)
    }

}



pub struct IceConn {
    uid: HUId,
    socket: UdpSocketBridge<IceSocket<TokioUdpSocket>>,
    remote_addr: SocketAddr,
    is_client: bool,
}

impl IceConn {
    pub fn uid(&self) -> HUId {
        self.uid
    }

    pub fn is_client(&self) -> bool {
        self.is_client
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.0.local_addr()
    }

    pub fn into_async_udp(self) -> UdpSocketBridge<impl AsyncUdpSocket> {
        self.socket
    }

    pub fn into_parts(self) -> (tokio::net::UdpSocket, CheckerConfig, SocketAddr) {
        let r = self.socket.0.into_parts();
        (r.0.into_inner(), r.1, self.remote_addr)
    }
}






#[cfg(test)]
mod test {
    use crate::stun::async_udp::AsUdpSocket;

    use super::*;
    use futures::{stream::FuturesUnordered, StreamExt};
    use tracing::debug;
    use anyhow::Result;

    #[tokio::test]
    async fn test_ice_peer_basic() -> Result<()> {
        use crate::async_rt::spawn_with_name;
    
        init_log();
    
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
    
        let msg1 = "I'am conn1";
        let msg2 = "I'am conn2";
    
        let task1 = spawn_with_name("client", async move {
            let conn = peer1.dial(arg2).await?;
            let remote_addr = conn.remote_addr();
            let socket = conn.into_async_udp();
            socket.as_socket().send_to(msg1.as_bytes().to_vec().into(), remote_addr).await?;
    
            let mut buf = vec![0; 1700];
            let (n, _addr) = socket.as_socket().recv_from(&mut buf).await
            .with_context(||"read stream failed")?;
            let msg = std::str::from_utf8(&buf[..n])?;
            debug!("recv {msg:?}");
            assert_eq!(msg, msg2);
            Result::<()>::Ok(())
        });
    
        let task2 = spawn_with_name("server", async move {
            let conn = peer2.accept().await?;
            let remote_addr = conn.remote_addr();
            let socket = conn.into_async_udp();
            socket.as_socket().send_to(msg2.as_bytes().to_vec().into(), remote_addr).await?;
    
            let mut buf = vec![0; 1700];
            let (n, _addr) = socket.as_socket().recv_from(&mut buf).await
            .with_context(||"read stream failed")?;
            let msg = std::str::from_utf8(&buf[..n])?;
            debug!("recv {msg:?}");
            assert_eq!(msg, msg1);
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
    async fn test_ice_peer_with_webrtc_as_client() -> Result<()> {
        use crate::async_rt::spawn_with_name;
        use crate::ice::webrtc_ice_peer::{WebrtcIcePeer, WebrtcIceConfig, DtlsIceArgs};
    
        init_log();
    
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
    
        let mut peer2 = WebrtcIcePeer::with_config(WebrtcIceConfig {
            servers: servers.clone(),
            disable_dtls: true,
            ..Default::default()
        });
        
        let arg2 = peer2.gather_until_done().await?;
        debug!("arg2 {arg2:?}");
    
        let msg1 = "I'am conn1";
        let msg2 = "I'am conn2";
    
        let task1 = spawn_with_name("client", async move {
            let arg2 = arg2.ice;

            let conn = peer1.dial(arg2).await?;
            let remote_addr = conn.remote_addr();
            let socket = conn.into_async_udp();
            socket.as_socket().send_to(msg1.as_bytes().to_vec().into(), remote_addr).await?;
            debug!("sent msg");
    
            let mut buf = vec![0; 1700];
            let (n, _addr) = socket.as_socket().recv_from(&mut buf).await
            .with_context(||"read stream failed")?;
            let msg = std::str::from_utf8(&buf[..n])?;
            debug!("recv {msg:?}");
            assert_eq!(msg, msg2);
            Result::<()>::Ok(())
        });
    
        let task2 = spawn_with_name("server", async move {
            let arg1 = DtlsIceArgs {
                ice: arg1,
                cert_fingerprint: None,
            };

            let mut conn = peer2.accept(arg1).await?;

            conn.write_data_all(msg2.as_bytes()).await?;
            debug!("sent msg");
    
            let mut buf = vec![0; 1700];
            let n = conn.read_data(&mut buf).await
            .with_context(||"read stream failed")?;
            let msg = std::str::from_utf8(&buf[..n])?;
            debug!("recv {msg:?}");
            assert_eq!(msg, msg1);
            Result::<()>::Ok(())
        });
        

        let mut futs = FuturesUnordered::new();
        futs.push(task1);
        futs.push(task2);
        
        while let Some(r) = futs.next().await {
            debug!("task finished {r:?}");
            r??;
        }

    
        // let r1 = task1.await?;
        // let r2 = task2.await?;
    
        // debug!("task1 finished {r1:?}");
        // debug!("task2 finished {r2:?}");
    
        // r1?;
        // r2?;
    
        Ok(())
    }

    #[tokio::test]
    async fn test_ice_peer_with_webrtc_as_server() -> Result<()> {
        use crate::async_rt::spawn_with_name;
        use crate::ice::webrtc_ice_peer::{WebrtcIcePeer, WebrtcIceConfig, DtlsIceArgs};
    
        init_log();
    
        let servers = vec![
            // "stun:stun1.l.google.com:19302".into(),
            // "stun:stun2.l.google.com:19302".into(),
            // "stun:stun.qq.com:3478".into(),
        ];
    

        let mut peer1 = WebrtcIcePeer::with_config(WebrtcIceConfig {
            servers: servers.clone(),
            disable_dtls: true,
            ..Default::default()
        });

        let arg1 = peer1.gather_until_done().await?;
        debug!("arg1 {arg1:?}");
    
        let mut peer2 = IcePeer::with_config(IceConfig {
            servers: servers.clone(),
            ..Default::default()
        });
        
        let arg2 = peer2.server_gather(arg1.ice).await?;
        debug!("arg2 {arg2:?}");
    
        let msg1 = "I'am conn1";
        let msg2 = "I'am conn2";
    
        let task1 = spawn_with_name("client", async move {
            let arg2 = DtlsIceArgs {
                ice: arg2,
                cert_fingerprint: None,
            };

            let mut conn = peer1.dial(arg2).await?;

            conn.write_data_all(msg1.as_bytes()).await?;
            debug!("sent msg");
    
            let mut buf = vec![0; 1700];
            let n = conn.read_data(&mut buf).await
            .with_context(||"read stream failed")?;
            let msg = std::str::from_utf8(&buf[..n])?;
            debug!("recv {msg:?}");
            assert_eq!(msg, msg2);
            Result::<()>::Ok(())
        });

        let task2 = spawn_with_name("server", async move {
            // let arg1 = arg1.ice;

            let conn = peer2.accept().await?;
            let remote_addr = conn.remote_addr();
            let socket = conn.into_async_udp();
            socket.as_socket().send_to(msg2.as_bytes().to_vec().into(), remote_addr).await?;
            debug!("sent msg");
    
            let mut buf = vec![0; 1700];
            let (n, _addr) = socket.as_socket().recv_from(&mut buf).await
            .with_context(||"read stream failed")?;
            let msg = std::str::from_utf8(&buf[..n])?;
            debug!("recv {msg:?}");
            assert_eq!(msg, msg1);
            Result::<()>::Ok(())
        });
            

        let mut futs = FuturesUnordered::new();
        futs.push(task1);
        futs.push(task2);
        
        while let Some(r) = futs.next().await {
            debug!("task finished {r:?}");
            r??;
        }
    
        Ok(())
    }

    fn init_log() {
        let _r = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_env_filter(tracing_subscriber::EnvFilter::from("rtun=debug"))
        .with_target(false)
        .try_init();

        // lazy_static::lazy_static! {
        //     static ref DUMMY: () = {
        //         tracing_subscriber::fmt()
        //         .with_max_level(tracing::Level::INFO)
        //         .with_env_filter(tracing_subscriber::EnvFilter::from("rtun=debug"))
        //         .with_target(false)
        //         .init();
        //         ()
        //     };
        // }
    }
}


