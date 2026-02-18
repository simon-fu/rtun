use std::{
    io,
    pin::Pin,
    task::{self, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::{
    ch_recv_stream::ChRecvStream, ch_send_stream::ChSendStream, ChPair, ChReceiver, ChSender,
};

pub struct ChStream {
    tx: ChSendStream,
    rx: ChRecvStream,
}

impl ChStream {
    pub fn new(pair: ChPair) -> Self {
        let (tx, rx) = pair.split();
        Self {
            tx: tx.into(),
            rx: rx.into(),
        }
    }

    pub fn new2(tx: ChSender, rx: ChReceiver) -> Self {
        Self {
            tx: tx.into(),
            rx: rx.into(),
        }
    }

    pub async fn peek(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.rx.peek(buf).await
    }
}

impl AsyncRead for ChStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.rx).poll_read(cx, buf)
    }
}

impl AsyncWrite for ChStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.tx).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.tx).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.tx).poll_shutdown(cx)
    }
}
