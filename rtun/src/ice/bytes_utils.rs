use bytes::{Buf, Bytes};
use tokio::io::ReadBuf;

pub struct BytesCursor {
    data: Bytes,
    pos: usize,
}

impl From<Bytes> for BytesCursor {
    fn from(data: Bytes) -> Self {
        Self { pos: 0, data }
    }
}

impl Buf for BytesCursor {
    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn chunk(&self) -> &[u8] {
        &self.data[self.pos..]
    }

    fn advance(&mut self, cnt: usize) {
        self.pos += cnt;
        self.pos = self.pos.min(self.data.len());
    }
}

pub struct BytesReader {
    data: Bytes,
    pos: usize,
}

impl From<Bytes> for BytesReader {
    fn from(data: Bytes) -> Self {
        Self { data, pos: 0 }
    }
}

impl BytesReader {
    pub fn peek_buf(&mut self, buf: &mut ReadBuf<'_>) -> usize {
        let num = self.remaining().min(buf.remaining());

        if num > 0 {
            buf.put_slice(&self.data[self.pos..num]);
        }

        num
    }

    pub fn read_buf(&mut self, buf: &mut ReadBuf<'_>) -> usize {
        let num = self.remaining().min(buf.remaining());

        if num > 0 {
            buf.put_slice(&self.data[self.pos..self.pos + num]);
            self.pos += num;
        }

        num
    }

    pub fn raw_data(&self) -> &[u8] {
        &self.data[..]
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
}
