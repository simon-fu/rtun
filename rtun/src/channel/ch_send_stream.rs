use std::{
    io::{self, ErrorKind},
    pin::Pin,
    task::{self, Poll},
};

use bytes::Bytes;
use tokio::io::AsyncWrite;
use tokio_util::sync::PollSender;

use super::{ChId, ChPacket, ChSender};

pub struct ChSendStream {
    ch_id: ChId,
    tx: PollSender<ChPacket>,
}

impl From<ChSender> for ChSendStream {
    fn from(tx: ChSender) -> Self {
        Self {
            ch_id: tx.ch_id,
            tx: PollSender::new(tx.outgoing_tx),
        }
    }
}

impl AsyncWrite for ChSendStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        match self.tx.poll_reserve(cx) {
            Poll::Ready(r) => match r {
                Ok(_r) => {
                    let packet = ChPacket {
                        ch_id: self.ch_id.clone(),
                        payload: Bytes::copy_from_slice(buf),
                    };
                    let r = self.tx.send_item(packet);
                    match r {
                        Ok(_r) => Poll::Ready(Ok(buf.len())),
                        Err(_e) => Poll::Ready(Err(ErrorKind::ConnectionAborted.into())),
                    }
                }
                Err(_e) => Poll::Ready(Err(ErrorKind::ConnectionAborted.into())),
            },
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        let r: Poll<Result<(), io::Error>> = self.poll_send(cx, &[][..]); // send zero packet
        match r {
            Poll::Ready(r) => {
                self.tx.close();
                Poll::Ready(r)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl ChSendStream {
    fn poll_send(&mut self, cx: &mut task::Context<'_>, buf: &[u8]) -> Poll<Result<(), io::Error>> {
        match self.tx.poll_reserve(cx) {
            Poll::Ready(r) => match r {
                Ok(_r) => {
                    let packet = ChPacket {
                        ch_id: self.ch_id.clone(),
                        payload: Bytes::copy_from_slice(buf),
                    };
                    let r = self.tx.send_item(packet);
                    match r {
                        Ok(_r) => Poll::Ready(Ok(())),
                        Err(_e) => Poll::Ready(Err(ErrorKind::ConnectionAborted.into())),
                    }
                }
                Err(_e) => Poll::Ready(Err(ErrorKind::ConnectionAborted.into())),
            },
            Poll::Pending => Poll::Pending,
        }
    }
}
