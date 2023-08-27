use std::{io, task::{self, Poll}};

use bytes::Bytes;
use tokio::sync::mpsc;


#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChId(pub u64);

impl std::fmt::Display for ChId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

pub struct ChPacket {
    pub ch_id: ChId,
    pub payload: Bytes,
}

pub struct ChPair {
    pub tx: ChSender,
    pub rx: ChReceiver,
}

impl ChPair {
    pub fn new(ch_id: ChId) -> Self {
        let (tx, rx) = mpsc::channel(CHANNEL_SIZE);
        Self { 
            tx: ChSender::new(ch_id, tx), 
            rx: ChReceiver::new(rx),
        }
    }
}

impl ChPair {
    #[inline]
    pub fn split(self) -> (ChSender, ChReceiver) {
        (self.tx, self.rx)
    }
}

pub type ChTx = mpsc::Sender<ChPacket>;
pub type ChRx = mpsc::Receiver<ChPacket>;
pub type ChTxWeak = mpsc::WeakSender<ChPacket>;


#[derive(Debug)]
pub struct ChSender {
    pub(super) ch_id: ChId,
    pub(super) outgoing_tx: ChTx,
}

impl ChSender {
    pub fn new(ch_id: ChId, outgoing_tx: ChTx) -> Self {
        Self {
            ch_id,
            outgoing_tx,
        }
    }

    pub fn ch_id(&self) -> ChId {
        self.ch_id
    }

    pub fn downgrade(&self) -> ChSenderWeak{
        let tx = self.outgoing_tx.downgrade();
        ChSenderWeak { ch_id: self.ch_id, outgoing_tx: tx }
    }

    pub async fn send_data(&self, data: Bytes) -> Result<(), Bytes> {
        self.outgoing_tx.send(ChPacket { 
            ch_id: self.ch_id, 
            payload: data, 
        }).await
        .map_err(|x|x.0.payload)
    }

    pub fn try_send_zero(&self) -> Result<(), ()> {
        self.outgoing_tx.try_send(ChPacket { 
            ch_id: self.ch_id, 
            payload: Bytes::new(), 
        })
        .map_err(|_x|())
    }
}

#[derive(Debug)]
pub struct ChSenderWeak {
    pub(super) ch_id: ChId,
    pub(super) outgoing_tx: ChTxWeak,
}

impl ChSenderWeak {
    pub fn upgrade(&self) -> Option<ChSender> {
        self.outgoing_tx.upgrade()
        .map(|x|ChSender {
            ch_id: self.ch_id,
            outgoing_tx: x,
        })
    }
}

pub struct ChReceiver {
    rx: ChRx,
}

impl ChReceiver {
    pub fn new(rx: ChRx) -> Self {
        Self {
            rx,
        }
    }

    // pub async fn recv_data(&mut self) -> Option<ChPacket> {
    //     self.rx.recv().await
    // }

    pub async fn recv_packet(&mut self) -> Result<ChPacket, RecvError> {
        // self.rx.recv().await

        let r = self.rx.recv().await;
        match r {
            Some(packet) => {
                if packet.payload.len() > 0 {
                    Ok(packet)
                } else {
                    Err(RecvError::ZeroClosed)
                }
            },
            None => Err(RecvError::ChClosed),
        }
    }


    pub fn poll_recv(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<ChPacket, RecvError>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(r) => {
                match r {
                    Some(packet) => {
                        if packet.payload.len() > 0 {
                            Poll::Ready(Ok(packet))
                        } else {
                            Poll::Ready(Err(RecvError::ZeroClosed))
                        }
                    },
                    None => Poll::Ready(Err(RecvError::ChClosed)),
                }
            },
            Poll::Pending => Poll::Pending,
        }
    }
}


use thiserror::Error;

#[derive(Error, Debug)]
pub enum RecvError {
    // #[error("channel closed")]
    // Closed(#[from] io::Error),

    #[error("channel closed")]
    ChClosed,

    #[error("recv zero closed")]
    ZeroClosed,
}

impl From<RecvError> for io::Error {
    fn from(v: RecvError) -> io::Error {
        match v {
            RecvError::ChClosed => io::Error::new(
                io::ErrorKind::UnexpectedEof, 
                "ChClosed",
            ),
            RecvError::ZeroClosed => io::Error::new(
                io::ErrorKind::UnexpectedEof, 
                "ZeroClosed",
            ),
        }
    }
}

// impl Into<io::Error> for RecvError {
//     fn into(self) -> io::Error {
//         match self {
//             RecvError::ChClosed => io::Error::new(
//                 io::ErrorKind::UnexpectedEof, 
//                 "ChClosed",
//             ),
//             RecvError::ZeroClosed => io::Error::new(
//                 io::ErrorKind::UnexpectedEof, 
//                 "ZeroClosed",
//             ),
//         }
//     }
// }


pub const CHANNEL_SIZE: usize = 256;


