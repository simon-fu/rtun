use std::{sync::Arc, net::UdpSocket, os::fd::{AsFd, AsRawFd}, marker::PhantomData};
use tokio::sync::mpsc;
use anyhow::Result;

pub fn unbound<T>() -> (Sender<T>, Receiver<T>) {
    do_unbound::<T>().unwrap()
}

type UnboundSender<T> = mpsc::UnboundedSender<T>;
type UnboundedReceiver<T> = mpsc::UnboundedReceiver<T>;
pub type UnboundedSendError<T> = mpsc::error::SendError<T>;
pub type UnboundedTryRecvError = mpsc::error::TryRecvError;

fn do_unbound<T>() -> Result<(Sender<T>, Receiver<T>)>{
    let server = UdpSocket::bind("127.0.0.1:0")?;
    let client = UdpSocket::bind("127.0.0.1:0")?;
    
    client.connect(server.local_addr()?)?;
    server.set_nonblocking(true)?;

    let (tx, rx) = mpsc::unbounded_channel::<T>();

    let shared = Arc::new(Shared {
        client,
        _none: Default::default(),
        // data: Mutex::new(SharedData {
        //     que: VecDeque::<T>::new(),
        // }),
    });

    Ok((
        Sender {
            shared,
            tx,
        },
        Receiver {
            dummy: vec![0_u8; 8],
            rx,
            server,
        }
    ))
}

// #[derive(Clone)]
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
    tx: UnboundSender<T>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self { shared: self.shared.clone(), tx: self.tx.clone() }
    }
}

impl<T> Sender<T> {
    pub fn blocking_send(&self, t: T) -> Result<(), UnboundedSendError<T>> {
        let r = self.tx.send(t);
        if r.is_ok() {
            let _r = self.shared.client.send(&[0][..]);
        }
        r
    }
}

pub struct Receiver<T> {
    rx: UnboundedReceiver<T>,
    server: UdpSocket,
    dummy: Vec<u8>,
}

impl<T> Receiver<T> {
    // pub fn recv(&self) -> Result<T, mpsc::RecvError> {
    //     self.rx.recv()
    // }

    pub fn try_recv(&mut self) -> Result<T, UnboundedTryRecvError> {
        self.rx.try_recv()
    }

    pub fn prepared_recv(&mut self) -> Result<()> {
        while let Ok(_n) = self.server.recv(&mut self.dummy) {
        }
        Ok(())
    }

    pub fn fd(&self) -> std::os::fd::RawFd {
        self.server.as_fd().as_raw_fd()
    }
}

struct Shared<T> {
    client: UdpSocket,
    // data: Mutex<SharedData<T>>,
    _none: PhantomData<T>,
}

// struct SharedData<T> {
//     que: VecDeque<T>,
// }
