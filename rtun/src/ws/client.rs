use std::{task::{self, Poll}, pin::Pin};

use anyhow::{Result, anyhow};
use futures::{StreamExt, Sink, SinkExt, Stream};
use protobuf::Message as PbMessage;
use tokio::net::TcpStream;
// use tokio_stream::StreamExt;
use crate::{proto::RawPacket, channel::ChPacket, switch::switch_stream::{SinkError, StreamPacket, StreamError}};
use tokio_tungstenite::{connect_async, tungstenite::{Message as WsMessage, Error as WsError}, WebSocketStream, MaybeTlsStream};


pub async fn ws_connect_to(url: &str) -> Result<WsClientStream<WsRawStream>> {
    let (stream, _response) = connect_async(url).await?;
    Ok(WsClientStream(stream))
}

pub type WsRawStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct WsClientStream<S>(pub S);

impl<S> WsClientStream<S> {
    pub fn new(inner: S) -> Self {
        Self(inner)
    }
}

impl<S> Stream for WsClientStream<S> 
where 
    S: StreamExt<Item = Result<WsMessage, WsError>> 
        + Unpin,
{
    type Item = Result<StreamPacket, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let r = match self.0.poll_next_unpin(cx) {
                Poll::Ready(v) => v,
                Poll::Pending => return Poll::Pending,
            };
    
            let r = match r {
                Some(r) => r,
                None => return Poll::Ready(None),
            };
    
            let msg = match r {
                Ok(r) => r,
                Err(e) => return Poll::Ready(Some(Err(e.into()))),
            };
    
            match msg {
                WsMessage::Text(s) => return Poll::Ready(Some(Err(anyhow!("got msg::text {s:?}").into()))),
                WsMessage::Close(c) => return Poll::Ready(Some(Err(anyhow!("got msg::close {c:?}")))),
                WsMessage::Frame(_v) => return Poll::Ready(Some(Err(anyhow!("got msg::frame")))),
                WsMessage::Ping(_ping) => {}, // bail!("got msg::ping"),
                WsMessage::Pong(_pong) => {}, // bail!("got msg::pong"),
                WsMessage::Binary(d) => {
                    return Poll::Ready(Some(Ok(d)))
                },
            }
        }
        
    }
}

impl<S> Sink<ChPacket> for WsClientStream<S> 
where
    S: Sink<WsMessage, Error = WsError> + Unpin,
{
    type Error = SinkError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready_unpin(cx).map_err(|e|e.into())
    }

    fn start_send(mut self: Pin<&mut Self>, item: ChPacket) -> Result<(), Self::Error> {
        let data = RawPacket {
            ch_id: item.ch_id.0,
            payload: item.payload,
            ..Default::default()
        }.write_to_bytes()?;
        self.0.start_send_unpin(WsMessage::Binary(data)).map_err(|e|e.into())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_flush_unpin(cx).map_err(|e|e.into())
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_close_unpin(cx).map_err(|e|e.into())
    }

}

