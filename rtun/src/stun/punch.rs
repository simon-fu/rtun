

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

