use std::{task::{Poll, self}, pin::Pin, io::{self, ErrorKind}};

use bytes::Bytes;
use tokio::io::{AsyncRead, ReadBuf};

use super::ChReceiver;


pub struct ChRecvStream {
    rx: ChReceiver,
    reader: Option<Reader>,
}

impl ChRecvStream {
    pub async fn peek(&mut self, buf: &mut [u8]) -> io::Result<usize> {

        loop {
            if let Some(reader) = &mut self.reader {
                if reader.data.len() == 0 {
                    return Ok(0)
                }

                let n = reader.peek_buf(&mut ReadBuf::new(buf));
                if n > 0 {
                    return Ok(n)
                }
                self.reader = None;
            }

            self.recv_next().await?;
        }
    }

    async fn recv_next(&mut self) -> io::Result<()> {
        let r = self.rx.recv_data().await;
        match r {
            Some(packet) => {
                self.reader = Some(Reader { 
                    data: packet.payload, 
                    pos: 0,
                });
                Ok(())
            },
            None => Err(ErrorKind::ConnectionAborted.into()),
        }
    }
}

impl From<ChReceiver> for ChRecvStream {
    fn from(rx: ChReceiver) -> Self {
        Self {
            rx,
            reader: None,
        }
    }
}


impl AsyncRead for ChRecvStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {

        if let Some(reader) = &mut self.reader {
            let num = reader.read_buf(buf);
            if num > 0 {
                return Poll::Ready(Ok(()))
            }

            self.reader = None;
        }

        match self.rx.rx.poll_recv(cx) {
            Poll::Ready(r) => {
                match r {
                    Some(packet) => {
                        let mut reader = Reader { 
                            data: packet.payload, 
                            pos: 0,
                        };

                        reader.read_buf(buf);

                        self.reader = Some(reader);
                    },
                    None => { },
                }
                Poll::Ready(Ok(()))
            },
            Poll::Pending => Poll::Pending,
        }
        
    }
}

struct Reader {
    data: Bytes,
    pos: usize,
}

impl Reader {

    fn peek_buf(&mut self, buf: &mut ReadBuf<'_>) -> usize {
        
        let num = self.remaining().min(buf.remaining());

        if num > 0 {
            buf.put_slice(&self.data[self.pos..num]);
        }
        
        num
    }

    fn read_buf(&mut self, buf: &mut ReadBuf<'_>) -> usize {
        
        let num = self.remaining().min(buf.remaining());

        if num > 0 {
            buf.put_slice(&self.data[self.pos..self.pos+num]);
            self.pos += num;
        }
        
        num
    }

    #[inline]
    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
}
