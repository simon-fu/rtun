use std::{
    pin::Pin,
    task::{self, Poll},
};

use anyhow::{anyhow, Result};
use futures::{
    stream::{SplitSink, SplitStream},
    Sink, SinkExt, Stream, StreamExt,
};
use protobuf::Message as PbMessage;
use tokio::net::TcpStream;
// use tokio_stream::StreamExt;
use crate::{
    channel::ChPacket,
    proto::RawPacket,
    switch::{
        switch_sink::{PacketSink, SinkError},
        switch_source::{PacketSource, StreamError, StreamPacket},
    },
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error as WsError, Message as WsMessage},
    MaybeTlsStream, WebSocketStream,
};

pub async fn ws_connect_to(
    url: &str,
) -> Result<(
    WsClientStream<WsRawStream>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
)> {
    let r = connect_async(url).await?;
    let (sink, source) = r.0.split();
    let stream = WsClientStream {
        sink: WsSink(sink),
        source: WsSource(source),
    };
    Ok((stream, r.1))
}

pub type WsRawStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct WsClientStream<S> {
    sink: WsSink<SplitSink<S, WsMessage>>,
    source: WsSource<SplitStream<S>>,
}

impl<S> WsClientStream<S> {
    pub fn split(self) -> (WsSink<SplitSink<S, WsMessage>>, WsSource<SplitStream<S>>) {
        (self.sink, self.source)
    }
}

impl<S> Stream for WsClientStream<S>
where
    S: StreamExt<Item = Result<WsMessage, WsError>> + Unpin,
{
    type Item = Result<StreamPacket, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        self.source.poll_next_unpin(cx)
    }
}

impl<S> Sink<ChPacket> for WsClientStream<S>
where
    S: Sink<WsMessage, Error = WsError> + Unpin,
{
    type Error = SinkError;

    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_ready_unpin(cx).map_err(|e| e.into())
    }

    fn start_send(mut self: Pin<&mut Self>, item: ChPacket) -> Result<(), Self::Error> {
        self.sink.start_send_unpin(item).map_err(|e| e.into())
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_flush_unpin(cx).map_err(|e| e.into())
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_close_unpin(cx).map_err(|e| e.into())
    }
}

// impl<S> PacketStream for WsClientStream<S>
// where
//     S: 'static
//     + Unpin
//     + Send
//     + StreamExt<Item = Result<WsMessage, WsError>>
//     + Sink<WsMessage, Error = WsError>
// { }

pub struct WsSource<S>(pub S);

impl<S> Stream for WsSource<S>
where
    S: StreamExt<Item = Result<WsMessage, WsError>> + Unpin,
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
                WsMessage::Text(s) => {
                    return Poll::Ready(Some(Err(anyhow!("got msg::text {s:?}").into())))
                }
                WsMessage::Close(c) => {
                    return Poll::Ready(Some(Err(anyhow!("got msg::close {c:?}"))))
                }
                WsMessage::Frame(_v) => return Poll::Ready(Some(Err(anyhow!("got msg::frame")))),
                WsMessage::Ping(_ping) => {} // bail!("got msg::ping"),
                WsMessage::Pong(_pong) => {} // bail!("got msg::pong"),
                WsMessage::Binary(d) => return Poll::Ready(Some(Ok(d))),
            }
        }
    }
}

impl<S> PacketSource for WsSource<S> where
    S: 'static + Unpin + Send + StreamExt<Item = Result<WsMessage, WsError>>
{
}

pub struct WsSink<S>(pub S);

impl<S> Sink<ChPacket> for WsSink<S>
where
    S: Sink<WsMessage, Error = WsError> + Unpin,
{
    type Error = SinkError;

    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready_unpin(cx).map_err(|e| e.into())
    }

    fn start_send(mut self: Pin<&mut Self>, item: ChPacket) -> Result<(), Self::Error> {
        let data = RawPacket {
            ch_id: item.ch_id.0,
            payload: item.payload,
            ..Default::default()
        }
        .write_to_bytes()?;
        self.0
            .start_send_unpin(WsMessage::Binary(data))
            .map_err(|e| e.into())
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.0.poll_flush_unpin(cx).map_err(|e| e.into())
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.0.poll_close_unpin(cx).map_err(|e| e.into())
    }
}

impl<S> PacketSink for WsSink<S> where S: 'static + Unpin + Send + Sink<WsMessage, Error = WsError> {}

// pub struct WsClientStream<S>(pub S);

// impl<S> WsClientStream<S> {
//     pub fn new(inner: S) -> Self {
//         Self(inner)
//     }
// }

// impl<S> Stream for WsClientStream<S>
// where
//     S: StreamExt<Item = Result<WsMessage, WsError>>
//         + Unpin,
// {
//     type Item = Result<StreamPacket, StreamError>;

//     fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
//         loop {
//             let r = match self.0.poll_next_unpin(cx) {
//                 Poll::Ready(v) => v,
//                 Poll::Pending => return Poll::Pending,
//             };

//             let r = match r {
//                 Some(r) => r,
//                 None => return Poll::Ready(None),
//             };

//             let msg = match r {
//                 Ok(r) => r,
//                 Err(e) => return Poll::Ready(Some(Err(e.into()))),
//             };

//             match msg {
//                 WsMessage::Text(s) => return Poll::Ready(Some(Err(anyhow!("got msg::text {s:?}").into()))),
//                 WsMessage::Close(c) => return Poll::Ready(Some(Err(anyhow!("got msg::close {c:?}")))),
//                 WsMessage::Frame(_v) => return Poll::Ready(Some(Err(anyhow!("got msg::frame")))),
//                 WsMessage::Ping(_ping) => {}, // bail!("got msg::ping"),
//                 WsMessage::Pong(_pong) => {}, // bail!("got msg::pong"),
//                 WsMessage::Binary(d) => {
//                     return Poll::Ready(Some(Ok(d)))
//                 },
//             }
//         }

//     }
// }

// impl<S> Sink<ChPacket> for WsClientStream<S>
// where
//     S: Sink<WsMessage, Error = WsError> + Unpin,
// {
//     type Error = SinkError;

//     fn poll_ready(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
//         self.0.poll_ready_unpin(cx).map_err(|e|e.into())
//     }

//     fn start_send(mut self: Pin<&mut Self>, item: ChPacket) -> Result<(), Self::Error> {
//         let data = RawPacket {
//             ch_id: item.ch_id.0,
//             payload: item.payload,
//             ..Default::default()
//         }.write_to_bytes()?;
//         self.0.start_send_unpin(WsMessage::Binary(data)).map_err(|e|e.into())
//     }

//     fn poll_flush(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
//         self.0.poll_flush_unpin(cx).map_err(|e|e.into())
//     }

//     fn poll_close(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
//         self.0.poll_close_unpin(cx).map_err(|e|e.into())
//     }

// }

// impl<S> PacketStream for WsClientStream<S>
// where
//     S: 'static
//     + Unpin
//     + Send
//     + StreamExt<Item = Result<WsMessage, WsError>>
//     + Sink<WsMessage, Error = WsError>
// { }
