use std::{net::SocketAddr, sync::Arc, io};

use tokio::net::{UdpSocket, ToSocketAddrs};

// use crate::{tlv_one, tlv_custom::udp::{AppendTagUdpRecv, AppendTagUdpSend}};

pub struct UdpSocketExt {
    socket: UdpSocket,
    local_addr: SocketAddr,
}

impl UdpSocketExt {
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        let local_addr = socket.local_addr()?;
        Ok(Self{ socket, local_addr })
    }

    pub fn from_socket(socket: UdpSocket) -> io::Result<Self> {
        let local_addr = socket.local_addr()?;
        Ok(Self{ socket, local_addr })
    }

    #[inline]
    pub fn local_addr(&self) -> &SocketAddr {
        &self.local_addr
    }


    #[inline]
    pub fn try_send_to_with(&self, buf: &[u8], target: SocketAddr, _is_dump_tlv: bool) -> io::Result<usize> {
        self.socket.try_send_to(buf, target)

        // if !is_dump_tlv {
        //     self.socket.try_send_to(buf, target.clone())
        // } else {
        //     let r = self.socket.try_send_to(buf, target.clone());
        //     tlv_one::get_writer(|writer| { 
        //         writer.append_tag_udp_send(&self.local_addr, &target, buf, &r);
        //     });
        //     r
        // }
    }


    #[inline]
    pub async fn send_to_with(&self, buf: &[u8], remote_addr: &SocketAddr, _is_dump_tlv: bool) -> io::Result<usize> {
        let r = self.socket.send_to(buf, remote_addr).await;
        
        // if is_dump_tlv {
        //     tlv_one::get_writer(|writer| { 
        //         writer.append_tag_udp_send(&self.local_addr, remote_addr, buf, &r);
        //     });
        // }

        r
    }

    // pub fn socket(&self) -> &UdpSocket {
    //     &self.socket
    // }

    pub fn poll_recv_from(
        &self,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<SocketAddr>> {
        self.socket.poll_recv_from(cx, buf)
    }
}

// pub struct RtpUdpSockets {
//     video_rtp: Arc<UdpSocketExt>,
//     video_rtcp: Arc<UdpSocketExt>,
//     audio_rtp: Arc<UdpSocketExt>,
//     audio_rtcp: Arc<UdpSocketExt>,
// }

// impl RtpUdpSockets {
//     pub fn new(
//         video_rtp: Arc<UdpSocketExt>,
//         video_rtcp: Arc<UdpSocketExt>,
//         audio_rtp: Arc<UdpSocketExt>,
//         audio_rtcp: Arc<UdpSocketExt>,
//     ) -> Self {
//         Self {
//             video_rtp,
//             video_rtcp,
//             audio_rtp,
//             audio_rtcp,
//         }
//     }
    
//     pub fn video_rtp(&self) -> &Arc<UdpSocketExt> {
//         &self.video_rtp
//     }

//     pub fn video_rtcp(&self) -> &Arc<UdpSocketExt> {
//         &self.video_rtcp
//     }

//     pub fn audio_rtp(&self) -> &Arc<UdpSocketExt> {
//         &self.audio_rtp
//     }

//     pub fn audio_rtcp(&self) -> &Arc<UdpSocketExt> {
//         &self.audio_rtcp
//     }

//     pub async fn recv_from(&self, is_dump_tlv: bool) -> std::io::Result<UdpPacket> {
//         tokio::select! {
//             r = udp_recv_from_with(&self.video_rtp, is_dump_tlv) => r,
//             r = udp_recv_from_with(&self.video_rtcp, is_dump_tlv) => r,
//             r = udp_recv_from_with(&self.audio_rtp, is_dump_tlv) => r,
//             r = udp_recv_from_with(&self.audio_rtcp, is_dump_tlv) => r,
//         }
//     }
// }


// // #[inline]
// // pub async fn udp_recv_from(socket: &Arc<UdpSocketExt>) -> std::io::Result<UdpPacket> {
// //     udp_recv_from_with(socket, false).await
// // }

pub async fn udp_recv_from_data(socket: &Arc<UdpSocketExt>, _is_dump_tlv: bool, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    loop {
        socket.socket.readable().await?;

        let r = socket.socket.try_recv_from(buf);

        if let Err(e) = &r {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                continue;
            }
        }

        // if is_dump_tlv {
        //     tlv_one::get_writer(|writer| { 
        //         writer.append_tag_udp_recv(&socket.local_addr, &r, &buf.data);
        //     });
        // }
        
        return r
    }
}


pub async fn udp_recv_from_with(socket: &Arc<UdpSocketExt>, _is_dump_tlv: bool) -> io::Result<UdpPacket> {
    loop {
        socket.socket.readable().await?;

        let mut buf = UdpBuf::default();
        
        let r = socket.socket.try_recv_from(&mut buf.data);

        if let Err(e) = &r {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                continue;
            }
        }

        // if is_dump_tlv {
        //     tlv_one::get_writer(|writer| { 
        //         writer.append_tag_udp_recv(&socket.local_addr, &r, &buf.data);
        //     });
        // }
        
        match r {
            Ok(r) => {
                buf.len = r.0;
                return Ok(UdpPacket {
                    data: buf,
                    addr: r.1,
                    _socket: socket.clone(),
                })
            }
            // Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            //     continue;
            // }
            Err(e) => {
                return Err(e);
            }
        }
    }
}

// pub async fn udp_recv_from2(socket: &UdpSocketExt, buf: &mut [u8], is_dump_tlv: bool) -> io::Result<(usize, SocketAddr)> {
//     let r = socket.socket.recv_from(buf).await;
//     if is_dump_tlv {
//         tlv_one::get_writer(|writer| { 
//             writer.append_tag_udp_recv(&socket.local_addr, &r, &buf);
//         });
//     }
//     r
// }


pub struct UdpBuf {
    data: Box<[u8]>,
    len: usize,
}

// impl UdpBuf {
//     pub(super) fn set_len(&mut self, len: usize) {
//         self.len = len;
//     }
// }

impl Default for UdpBuf {
    fn default() -> Self {
        Self {
            data: vec![0_u8; 1700].into_boxed_slice(),
            len: 0,
        }
    }
}

pub struct UdpPacket {
    pub(super) data: UdpBuf,
    pub(super) addr: SocketAddr,
    pub(super) _socket: Arc<UdpSocketExt>, // TODO
}


impl UdpPacket {
    pub fn new(data: UdpBuf, addr: SocketAddr, socket: Arc<UdpSocketExt>) -> Self {
        Self {
            data,
            addr,
            _socket: socket,
        }
    }

    pub fn slice(&self) -> &[u8] {
        &self.data.data[..self.data.len]
    }

    pub fn remote_addr(&self) -> &SocketAddr {
        &self.addr
    }
}
