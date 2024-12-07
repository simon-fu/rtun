use std::{collections::HashMap, net::{Ipv4Addr, SocketAddr}, sync::Arc, time::{Duration, Instant}};
use anyhow::{Context, Result};
use bytes::{BufMut, BytesMut};
use tokio::{net::UdpSocket, sync::mpsc};

pub struct UdpAcceptor {
    socket: UdpSocket,
    local_addr: SocketAddr,
    latest: HashMap<SocketAddr, Latest>,
    buf: UdpBuf,
    last_cleanup_time: Instant,
}


impl UdpAcceptor {
    pub fn try_new(socket: UdpSocket) -> Result<Self> {
        let local_addr = socket.local_addr().with_context(||"get local addr failed")?;

        Ok(Self {
            last_cleanup_time: Instant::now(),
            latest: Default::default(),
            buf: Default::default(),
            socket,
            local_addr,
        })
    }

    pub async fn accept(&mut self) -> Result<(UdpConn, SocketAddr)> {

        loop {

            let (len, from) = self.socket.recv_buf_from(self.buf.get_mut()).await.with_context(||"udp recv failed")?;

            let packet = self.buf.get_mut().split_to(len);

            let now = Instant::now();
            self.try_clean_up(now);

            match self.latest.get_mut(&from) {
                Some(session) => {
                    session.update_time = now;
                    
                    let r = session.tx.send(packet).await;
                    if r.is_err() {
                        self.latest.remove(&from);
                    }
                },

                None => {
                    let peer_socket = new_udp_reuseport(self.local_addr).with_context(|| format!("new_udp_reuseport failed, addr [{}]", self.local_addr))?;
                    peer_socket.connect(from).await.with_context(||"connect to peer failed")?;

                    let conn = UdpConn::from_socket(peer_socket, 32);

                    // let (tx, rx) = mpsc::channel(32);

                    let session = Latest {
                        tx: conn.tx().clone(),
                        update_time: now,
                    };

                    let _r = session.tx.send(packet).await;

                    self.latest.insert(from, session);

                    

                    // let conn = UdpConn {
                    //     socket: peer_socket,
                    //     rx,
                    //     _tx: tx,
                    // };
                    
                    return Ok((conn, from))
                },
            }
            
        }

    }

    fn try_clean_up(&mut self, now: Instant) {
        if (now - self.last_cleanup_time) < Duration::from_millis(2000) {
            return
        }

        self.last_cleanup_time = now;

        self.latest.retain(|_k, v| {
            let r = (now - v.update_time) < Duration::from_millis(2000) ;
            // if !r {
            //     tracing::debug!("remove session [{_k}] ");
            // }
            r
        });

    }
}

pub struct UdpConn {
    socket: UdpSocket,
    tx: PSender,
    rx: PReceiver,
}


impl std::fmt::Debug for UdpConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdpConn")
        // .field("socket", &self.socket)
        // .field("rx", &self.rx)
        // .field("_tx", &self._tx)
        .finish()
    }
}

impl UdpConn {
    // pub fn new(socket: UdpSocket) -> Self {
    //     Self::from_socket(socket, 1)
    // }

    fn from_socket(socket: UdpSocket, ch_size: usize) -> Self {
        let (tx, rx) = mpsc::channel(ch_size);
        Self {
            tx,
            rx,
            socket
        }
    }


    fn tx(&self) -> &PSender {
        &self.tx
    }

    pub fn split(self) -> (UdpSender, UdpReceiver) {
        let socket = Arc::new(self.socket);
        (
            UdpSender {
                socket: socket.clone(),
            },
            UdpReceiver {
                socket,
                rx: self.rx,
                _tx: self.tx,
            }
        )
    }

    pub async fn send(&self, buf: &[u8]) -> Result<usize> {
        let len = self.socket.send(buf).await?;
        Ok(len)
    }

    pub async fn recv(&mut self, buf: &mut [u8]) -> Result<usize> {

        tokio::select! {
            r = self.socket.recv(buf) => {
                let len = r?;
                Ok(len)
            }

            r = self.rx.recv() => {
                let Some(packet) = r else {
                    unreachable!("never drop tx")
                };

                (&mut buf[..packet.len()]).copy_from_slice(&packet[..]);
                Ok(packet.len())
            }
        }
    }

    pub async fn recv_buf<B: BufMut>(&mut self, buf: &mut B) -> Result<usize> {

        tokio::select! {
            r = self.socket.recv_buf(buf) => {
                let len = r?;
                Ok(len)
            }

            r = self.rx.recv() => {
                let Some(packet) = r else {
                    unreachable!("never drop tx")
                };
                
                buf.put_slice(&packet[..]);
                Ok(packet.len())
            }
        }
    }
}

pub struct UdpSender {
    socket: Arc<UdpSocket>,
}

impl UdpSender {
    pub async fn send(&self, buf: &[u8]) -> Result<usize> {
        let len = self.socket.send(buf).await?;
        Ok(len)
    }
}


pub struct UdpReceiver {
    socket: Arc<UdpSocket>,
    _tx: PSender,
    rx: PReceiver,
}

impl UdpReceiver {
    pub async fn recv(&mut self, buf: &mut [u8]) -> Result<usize> {

        tokio::select! {
            r = self.socket.recv(buf) => {
                let len = r?;
                Ok(len)
            }

            r = self.rx.recv() => {
                let Some(packet) = r else {
                    unreachable!("never drop tx")
                };

                (&mut buf[..packet.len()]).copy_from_slice(&packet[..]);
                Ok(packet.len())
            }
        }
    }

    pub async fn recv_buf<B: BufMut>(&mut self, buf: &mut B) -> Result<usize> {

        tokio::select! {
            r = self.socket.recv_buf(buf) => {
                let len = r?;
                Ok(len)
            }

            r = self.rx.recv() => {
                let Some(packet) = r else {
                    unreachable!("never drop tx")
                };
                
                buf.put_slice(&packet[..]);
                Ok(packet.len())
            }
        }
    }
}


pub type PSender = mpsc::Sender<BytesMut>;
pub type PReceiver = mpsc::Receiver<BytesMut>;


struct Latest {
    update_time: Instant,
    tx: PSender,
}

#[derive(Default)]
pub struct UdpBuf {
    buf: BytesMut,
}

impl UdpBuf {

    #[inline]
    pub fn get_mut(&mut self) -> &mut BytesMut {
        
        const RESERV_BUF_SIZE: usize = 1024*8;

        let mut_len = self.buf.capacity() - self.buf.len();

        // let mut_len = self.buf.chunk_mut().len();
        if mut_len < MIN_BUF_SIZE {
            self.buf.reserve(RESERV_BUF_SIZE - mut_len);
            // self.buf = BytesMut::with_capacity(RESERV_BUF_SIZE);
        }

        &mut self.buf
    }
}

const MIN_BUF_SIZE: usize = 1700;



pub fn new_udp_reuseport_v4() -> Result<UdpSocket> {
    let listen_addr = SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), 0);
    new_udp_reuseport(listen_addr)   
}

pub fn new_udp_reuseport_str(listen_addr: &str) -> Result<UdpSocket> {
    let listen_addr: SocketAddr = listen_addr.parse().with_context(||format!("invalid addr [{listen_addr}]"))?;
    new_udp_reuseport(listen_addr)
}

pub fn new_udp_reuseport(listen_addr: SocketAddr) -> Result<UdpSocket> {
    let domain = if listen_addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };

    let udp_sock = socket2::Socket::new(
        domain,
        socket2::Type::DGRAM,
        None,
    ).with_context(|| format!("udp new socket domain [{domain:?}] failed"))?;

    udp_sock.set_reuse_port(true).with_context(||"udp set_reuse_port failed")?;
    // udp_sock.set_reuse_address(true).with_context(||"set_reuse_address failed")?;
    // from tokio-rs/mio/blob/master/src/sys/unix/net.rs
    udp_sock.set_cloexec(true).with_context(||"udp set_cloexec failed")?;
    udp_sock.set_nonblocking(true).with_context(||"udp set_nonblocking failed")?;
    udp_sock.bind(&socket2::SockAddr::from(listen_addr)).with_context(||"udp bind failed")?;
    let udp_sock: std::net::UdpSocket = udp_sock.into();
    Ok(udp_sock.try_into().with_context(||"udp convert failed")?)
}


