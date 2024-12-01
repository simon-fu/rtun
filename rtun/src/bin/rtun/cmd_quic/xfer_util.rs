use anyhow::{Result, Context};
use serde::{Serialize, Deserialize};
use bytes::{BytesMut, BufMut, Buf};
use tokio::io::AsyncReadExt;

pub async fn read_short_json<R, T>(rx: &mut R, buf: &mut BytesMut) -> Result<T> 
where
    R: AsyncReadExt + Unpin,
    T: for<'a> Deserialize<'a>,
{
    let packet_len = read_short_packet(rx, buf).await?;
    let obj: T = serde_json::from_slice(&buf[..packet_len])
    .with_context(||"can't parsed bridge request")?;
    buf.advance(packet_len);
    Ok(obj)
}


pub async fn read_short_packet<R: AsyncReadExt + Unpin, B: BufMut>(rx: &mut R, buf: &mut B) -> Result<usize> {
    let len = rx.read_u16().await? as usize;
    let mut nread = 0;
    while nread < len {
        let n = rx.read_buf(buf).await?;
        nread += n;
    }
    Ok(nread)
}



pub fn ser_short_json<T>(buf: &mut BytesMut, value: &T) -> Result<usize>
where
    T: ?Sized + Serialize,
{
    let len_pos = buf.len();
    buf.put_u16(0); // reserve len field
    serde_json::to_writer((buf).writer(), value)?;
    let json_len = buf.len() - len_pos - 2;
    (&mut buf[len_pos..]).put_u16(json_len as u16);
    Ok(json_len)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResponseStatus {
    pub code: i32,
    pub msg: String,
}

impl ResponseStatus {
    pub fn new(code: i32, msg: String ) -> Self {
        Self {
            code,
            msg,
        }
    }

    pub fn ok() -> Self {
        Self::new(0, "".into())
    }
}

