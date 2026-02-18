use std::{
    io::{self, ErrorKind},
    pin::Pin,
    task::{self, Poll},
};

use anyhow::Result;
use bytes::{BufMut, Bytes, BytesMut};
use futures::{AsyncRead, AsyncWrite};
use tokio::{io::ReadBuf, sync::mpsc};
use tokio_util::sync::PollSender;

use super::bytes_utils::BytesReader;

pub struct MpscPair {
    pub writer: MpscWriter,
    pub reader: MpscReader,
}

impl MpscPair {
    pub fn split(self) -> (MpscReader, MpscWriter) {
        (self.reader, self.writer)
    }
}

impl AsyncWrite for MpscPair {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.writer).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.writer).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.writer).poll_close(cx)
    }
}

impl AsyncRead for MpscPair {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}

pub struct MpscWriter {
    tx: PollSender<Bytes>,
    buf: BytesMut,
}

impl From<mpsc::Sender<Bytes>> for MpscWriter {
    fn from(tx: mpsc::Sender<Bytes>) -> Self {
        Self {
            buf: Default::default(),
            tx: PollSender::new(tx),
        }
    }
}

impl MpscWriter {
    fn poll_send(
        self: &mut Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<(), io::Error>> {
        match self.tx.poll_reserve(cx) {
            Poll::Ready(r) => match r {
                Ok(_r) => {
                    self.buf.put_slice(buf);
                    let data = self.buf.split().freeze();

                    let r = self.tx.send_item(data);
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

impl AsyncWrite for MpscWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        match self.poll_send(cx, buf) {
            Poll::Ready(Ok(_r)) => Poll::Ready(Ok(buf.len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        let r = self.poll_send(cx, &[][..]); // send zero packet
        match r {
            Poll::Ready(r) => {
                self.tx.close();
                Poll::Ready(r)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub struct MpscReader {
    rx: mpsc::Receiver<Bytes>,
    reader: Option<BytesReader>,
}

impl From<mpsc::Receiver<Bytes>> for MpscReader {
    fn from(rx: mpsc::Receiver<Bytes>) -> Self {
        Self { rx, reader: None }
    }
}

impl AsyncRead for MpscReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut buf = ReadBuf::new(buf);
        let buf = &mut buf;

        if let Some(reader) = &mut self.reader {
            if reader.raw_data().len() == 0 {
                return Poll::Ready(Ok(0));
            }

            let num = reader.read_buf(buf);
            if num > 0 {
                return Poll::Ready(Ok(num));
            }

            self.reader = None;
        }

        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let mut reader = BytesReader::from(data);

                let num = reader.read_buf(buf);

                self.reader = Some(reader);
                Poll::Ready(Ok(num))
            }
            Poll::Ready(None) => Poll::Ready(Err(ErrorKind::ConnectionAborted.into())),
            Poll::Pending => Poll::Pending,
        }
    }
}

// type KcpCtrl = Kcp<KcpWriteBuf>;

// fn kcp_update(kcp: &mut KcpCtrl) -> io::Result<Instant> {
//     let now = now_millis();
//     kcp.update(now).map_err(into_io)?;
//     let next = kcp.check(now);
//     let next = Instant::now() + Duration::from_millis(next as u64);
//     Ok(next)
// }

// #[inline]
// pub fn now_millis() -> u32 {
//     let start = std::time::SystemTime::now();
//     let since_the_epoch = start.duration_since(std::time::UNIX_EPOCH).expect("time went afterwards");
//     // (since_the_epoch.as_secs() * 1000 + since_the_epoch.subsec_millis() as u64) as u32
//     since_the_epoch.as_millis() as u32
// }

// fn into_io<E: std::fmt::Debug>(e: E) -> io::Error {
//     io::Error::new(ErrorKind::ConnectionAborted, format!("{:?}", e))
// }
