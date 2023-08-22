
use anyhow::{Result, Context, bail};
use axum::extract::ws::Message as WsMessage;
use futures::StreamExt;
use protobuf::Message as PbMessage;

pub async fn recv_ws_packet<S, P>(socket: &mut S) -> Result<P> 
where
    S: StreamExt<Item = Result<WsMessage, axum::Error>> + Unpin,
    P: PbMessage,
{
    let data = recv_ws_binary(socket).await?;
    P::parse_from_bytes(&data[..])
    .with_context(||"decode ws packet failed")
}

pub async fn recv_ws_binary<S>(socket: &mut S) -> Result<Vec<u8>> 
where
    S: StreamExt<Item = Result<WsMessage, axum::Error>> + Unpin,
{
    loop {
        let msg = socket.next().await
        .with_context(|| "recving handshake but closed")?
        .with_context(|| "recving handshake but failed")?;
    
        if let Some(data) = extract_ws_binary(msg)? {
            return Ok(data)
        }
    }
}

pub fn extract_ws_recv(next: Option<Result<WsMessage>>) -> Result<Option<Vec<u8>>> {
    match next {
        Some(r) => {
            let msg = r?;
            let r = extract_ws_binary(msg)?;
            match r {
                Some(msg) => Ok(Some(msg)),
                None => return Ok(None),
            }
        },
        None => {
            bail!("stream has closed")
        },
    }
}

pub fn extract_ws_binary(msg: WsMessage) -> Result<Option<Vec<u8>>> {
    match msg {
        WsMessage::Text(s) => bail!("got msg::text {s:?}"),
        WsMessage::Ping(_ping) => Ok(None), // bail!("got msg::ping"),
        WsMessage::Pong(_pong) => Ok(None), // bail!("got msg::pong"),
        WsMessage::Close(c) => bail!("got msg::close {c:?}"),
        WsMessage::Binary(d) => {
            Ok(Some(d))
        },
    }
}
