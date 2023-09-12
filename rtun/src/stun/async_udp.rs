use std::{sync::Arc, net::SocketAddr, io::{self, IoSliceMut}, task::{Poll, self}, pin::Pin, fmt};

use bytes::Bytes;
use futures::{ready, Future};
use quinn::udp::{UdpState, RecvMeta, Transmit};
pub use quinn::AsyncUdpSocket as AsyncUdpSocketOps;

use crate::async_rt::dummy;

// pub trait AsyncUdpSocket: AsyncUdpSocketOps {

// }

// impl<T: AsyncUdpSocketOps> AsyncUdpSocket for T 
// {
// }

// impl<T: AsyncUdpSocket> AsyncUdpSocket for Box<T> 
// {
// }

pub trait AsyncUdpSocket: Send + fmt::Debug + 'static {
    /// Send UDP datagrams from `transmits`, or register to be woken if sending may succeed in the
    /// future
    fn poll_send(
        &self,
        state: &UdpState,
        cx: &mut task::Context,
        transmits: &[Transmit],
    ) -> Poll<Result<usize, io::Error>>;

    /// Receive UDP datagrams, or register to be woken if receiving may succeed in the future
    fn poll_recv(
        &self,
        cx: &mut task::Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>>;

    /// Look up the local IP address and port used by this socket
    fn local_addr(&self) -> io::Result<SocketAddr>;

    /// Whether datagrams might get fragmented into multiple parts
    ///
    /// Sockets should prevent this for best performance. See e.g. the `IPV6_DONTFRAG` socket
    /// option.
    fn may_fragment(&self) -> bool {
        true
    }
}

impl<T: AsyncUdpSocketOps> AsyncUdpSocket for T 
{
    #[inline]
    fn poll_send(
        &self,
        state: &UdpState,
        cx: &mut task::Context,
        transmits: &[Transmit],
    ) -> Poll<Result<usize, io::Error>> {
        self.poll_send(state, cx, transmits)
    }

    #[inline]
    fn poll_recv(
        &self,
        cx: &mut task::Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        self.poll_recv(cx, bufs, meta)
    }

    #[inline]
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.local_addr()
    }

    #[inline]
    fn may_fragment(&self) -> bool {
        self.may_fragment()
    }
}



impl AsyncUdpSocket for Box<dyn AsyncUdpSocket> {
    #[inline]
    fn poll_send(
        &self,
        state: &UdpState,
        cx: &mut task::Context,
        transmits: &[Transmit],
    ) -> Poll<Result<usize, io::Error>> {
        self.as_ref().poll_send(state, cx, transmits)
    }

    #[inline]
    fn poll_recv(
        &self,
        cx: &mut task::Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        self.as_ref().poll_recv(cx, bufs, meta)
    }

    #[inline]
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.as_ref().local_addr()
    }

    #[inline]
    fn may_fragment(&self) -> bool {
        self.as_ref().may_fragment()
    }
}


#[derive(Debug)]
pub struct DummyUdpSocket;

impl AsyncUdpSocket  for DummyUdpSocket {
    fn poll_send(
        &self,
        _state: &UdpState,
        _cx: &mut task::Context,
        _transmits: &[Transmit],
    ) -> Poll<Result<usize, io::Error>> {
        Poll::Pending
    }

    fn poll_recv(
        &self,
        _cx: &mut task::Context,
        _bufs: &mut [IoSliceMut<'_>],
        _meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        match "0.0.0.0:0".parse() {
            Ok(addr) => Ok(addr),
            Err(_e) => Err(io::ErrorKind::InvalidData.into()),
        }
    }
}

// struct DummyWaker;
// impl task::Wake for DummyWaker {
//     fn wake(self: Arc<Self>) {
        
//     }
//     // // Required method
//     // fn wake(self: Arc<Self>);

//     // // Provided method
//     // fn wake_by_ref(self: &Arc<Self>) { ... }
// }

#[inline]
pub fn udp_state() -> &'static Arc<UdpState> {
    lazy_static::lazy_static! {
        static ref UDP_STATE: Arc<UdpState> = Default::default();
    }
    &*UDP_STATE
}


pub trait AsUdpSocket<T> {
    fn as_socket<'a>(&'a self) -> UdpSocketWrapper<T>;
}

impl<T: AsyncUdpSocket + Unpin> AsUdpSocket<T> for T {
    fn as_socket<'a>(&'a self) -> UdpSocketWrapper<T> {
        UdpSocketWrapper(self)
    }
}

pub struct UdpSocketWrapper<'a, U>(&'a U);

impl<'a, U> UdpSocketWrapper<'a, U> {
    pub fn new(socket: &'a U) -> Self {
        Self(socket)
    }

    // pub fn into_inner(self) -> U {
    //     self.0
    // }
}

impl<'a, U: AsyncUdpSocket + Unpin> UdpSocketWrapper<'a, U> {

    pub fn try_send_to(&self, data: Bytes, destination: SocketAddr) -> io::Result<usize> {
        let thiz = Pin::new(self);
        let waker = dummy::waker();
        let mut cx = dummy::context(&waker);
        // let waker = Arc::new(DummyWaker).into();
        // let mut cx = task::Context::from_waker(&waker);

        let transmits = [Transmit {
            destination,
            ecn: None,
            contents: data,
            segment_size: None,
            src_ip: None,
        }];

        let r = thiz.0.poll_send(udp_state(), &mut cx, &transmits);
        
        match r {
            Poll::Ready(r) => r.map(|_x|transmits[0].contents.len()),
            Poll::Pending => Err(io::ErrorKind::WouldBlock.into()),
        }
    }

    pub async fn send_to(&self, data: Bytes, destination: SocketAddr) -> io::Result<usize> {
        SendFut {
            socket: self.0,
            state: udp_state(),
            data,
            destination,
        }.await
    }

    pub fn poll_recv_from(
        &self,
        cx: &mut task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<SocketAddr>> {
        let mut bufs = [IoSliceMut::new(buf.initialize_unfilled())];
        let mut meta = [RecvMeta::default()];

        let r = self.0.poll_recv(cx, &mut bufs, &mut meta);
        match ready!(r) {
            Ok(_n) => {
                let len = meta[0].len;
                buf.set_filled(len);
                Poll::Ready(Ok(meta[0].addr))
            },
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

struct SendFut<'a, U> {
    socket: &'a U,
    state: &'a UdpState,
    data: Bytes,
    destination: SocketAddr
}

impl<'a, U: AsyncUdpSocket> Future for SendFut<'a, U> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        self.socket.poll_send(self.state, cx, &[Transmit {
            destination: self.destination,
            ecn: None,
            contents: self.data.clone(),
            segment_size: None,
            src_ip: None,
        }])
    }
}

pub struct BoxUdpSocket(pub Box<dyn AsyncUdpSocketOps>);

impl std::fmt::Debug for BoxUdpSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsyncUdpSocket for BoxUdpSocket {

    #[inline]
    fn poll_send(
        &self,
        state: &UdpState,
        cx: &mut task::Context,
        transmits: &[Transmit],
    ) -> Poll<Result<usize, io::Error>> {
        self.0.poll_send(state, cx, transmits)
    }

    #[inline]
    fn poll_recv(
        &self,
        cx: &mut task::Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        self.0.poll_recv(cx, bufs, meta)
    }

    #[inline]
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }

    #[inline]
    fn may_fragment(&self) -> bool {
        self.0.may_fragment()
    }
}


// pub type BoxUdpSocket = UdpSocketBridge<Box<dyn AsyncUdpSocketOps>>;

pub struct UdpSocketBridge<U>(pub U);

impl<U: AsyncUdpSocket> std::fmt::Debug for UdpSocketBridge<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<U: AsyncUdpSocket> AsyncUdpSocketOps for UdpSocketBridge<U> {

    #[inline]
    fn poll_send(
        &self,
        state: &UdpState,
        cx: &mut task::Context,
        transmits: &[Transmit],
    ) -> Poll<Result<usize, io::Error>> {
        self.0.poll_send(state, cx, transmits)
    }

    #[inline]
    fn poll_recv(
        &self,
        cx: &mut task::Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        self.0.poll_recv(cx, bufs, meta)
    }

    #[inline]
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }

    #[inline]
    fn may_fragment(&self) -> bool {
        self.0.may_fragment()
    }
}


pub async fn tokio_socket_bind<A>(addr: A) -> io::Result<BoxUdpSocket> 
where
    A: tokio::net::ToSocketAddrs,
{
    let socket = tokio::net::UdpSocket::bind(addr).await?;
    let socket = quinn::Runtime::wrap_udp_socket(&quinn::TokioRuntime, socket.into_std()?)?;
    Ok(BoxUdpSocket(socket))
}


// pub async fn send_udp<U: AsyncUdpSocket>(socket: &U, tokio_socket: tokio::net::UdpSocket) -> io::Result<()> {
//     // tokio_socket.send_to(buf, target)
//     // socket.poll_send(state, cx, transmits);
//     tokio_socket.try_send_to(buf, target)
//     Ok(())
// }
